//! Requêtes nommées compilées dans un crate Rust, depuis `build.rs`
//! (PLAN.md §5, P5) :
//!
//! ```ignore
//! // build.rs
//! fn main() {
//!     pgweave_build::Builder::new().generate().unwrap_or_else(|e| panic!("{e}"));
//! }
//!
//! // src/lib.rs
//! pub mod queries {
//!     include!(concat!(env!("OUT_DIR"), "/pgweave_queries.rs"));
//! }
//! ```
//!
//! Le projet est celui de `pgweave.toml` (en remontant depuis le crate) :
//! son catalogue est rejoué, chaque requête de `[queries] directory` est
//! décrite puis traduite. Aucune connexion à une base. Une requête que
//! l'analyse ne décrit pas fait échouer la compilation, avec sa position.
//! Les fichiers lus sont déclarés à Cargo : modifier le schéma, une
//! migration ou une requête relance la génération.

use std::path::{Path, PathBuf};

use pgweave_catalog::project::{ProjectBuilder, parse_source};
use pgweave_codegen::rust::{RustOptions, Temporal, generate_rust};
use pgweave_diagnostics::Severity;
use pgweave_schema::{ProjectConfig, SchemaSource};

pub struct Builder {
    root: PathBuf,
    out_file: String,
    options: RustOptions,
}

impl Default for Builder {
    fn default() -> Self {
        Self::new()
    }
}

impl Builder {
    /// Projet trouvé depuis le crate en cours de compilation.
    pub fn new() -> Self {
        Builder {
            root: std::env::var_os("CARGO_MANIFEST_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(".")),
            out_file: "pgweave_queries.rs".into(),
            options: RustOptions::default(),
        }
    }

    /// Répertoire depuis lequel chercher `pgweave.toml`.
    pub fn root(mut self, root: impl Into<PathBuf>) -> Self {
        self.root = root.into();
        self
    }

    /// Dates et heures : `time` (défaut) ou `chrono`.
    pub fn temporal(mut self, t: Temporal) -> Self {
        self.options.temporal = t;
        self
    }

    /// Type Rust d'un type PostgreSQL (`"public.geometry"`, `"geo_types::Geometry"`).
    pub fn type_override(mut self, pg: &str, rust: &str) -> Self {
        self.options.overrides.insert(pg.into(), rust.into());
        self
    }

    /// Chemin du runtime dans le crate (défaut : `pgweave_runtime`).
    pub fn runtime_path(mut self, path: &str) -> Self {
        self.options.runtime = path.into();
        self
    }

    /// Génère `$OUT_DIR/pgweave_queries.rs`.
    pub fn generate(self) -> Result<PathBuf, String> {
        let out_dir = std::env::var_os("OUT_DIR")
            .map(PathBuf::from)
            .ok_or("OUT_DIR absent : `generate` s'appelle depuis build.rs")?;
        let code = self.generate_code(&mut |path| {
            println!("cargo:rerun-if-changed={}", path.display());
        })?;
        let out = out_dir.join(&self.out_file);
        std::fs::write(&out, code).map_err(|e| format!("{}: {e}", out.display()))?;
        Ok(out)
    }

    /// Code Rust des requêtes du projet ; `watch` reçoit chaque fichier lu.
    pub fn generate_code(&self, watch: &mut dyn FnMut(&Path)) -> Result<String, String> {
        let project = load_project(&self.root)?;
        for f in &project.files {
            watch(f);
        }
        let (root, config, catalog) = (project.root, project.config, project.catalog);
        let mut errors = Vec::new();

        let Some(dir) = &config.queries.directory else {
            return Err(format!(
                "aucun répertoire de requêtes : `[queries] directory` dans {}",
                ProjectConfig::FILENAME
            ));
        };
        let dir = root.join(dir);
        watch(&dir);
        let mut files = Vec::new();
        collect_sql(&dir, &mut files).map_err(|e| format!("{}: {e}", dir.display()))?;
        files.sort();

        let mut descriptors = Vec::new();
        for file in &files {
            watch(file);
            let text =
                std::fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
            let parsed = pgweave_query::parse_query_file(&text);
            let mut report = |d: &pgweave_diagnostics::Diagnostic| {
                let (line, col) = d.span.as_ref().map_or((1, 1), |s| line_col(&text, s.start));
                errors.push(format!(
                    "{}:{line}:{col}: error[{}]: {}",
                    file.display(),
                    d.code,
                    d.message
                ));
            };
            for d in parsed
                .diagnostics
                .iter()
                .filter(|d| d.severity == Severity::Error)
            {
                report(d);
            }
            for q in &parsed.queries {
                match pgweave_query::describe_named(&catalog, q) {
                    Ok(d) => descriptors.push(d),
                    Err(diags) => diags.iter().for_each(&mut report),
                }
            }
        }
        if !errors.is_empty() {
            return Err(format!("requêtes non décrites :\n{}", errors.join("\n")));
        }
        generate_rust(&descriptors, &catalog, &self.options)
            .map_err(|e| format!("génération Rust :\n{}", e.join("\n")))
    }
}

/// Projet chargé : sa configuration, son catalogue rejoué, et les fichiers
/// lus pour le construire (à surveiller).
pub struct Project {
    pub root: PathBuf,
    pub config: ProjectConfig,
    pub catalog: pgweave_catalog::ProjectCatalog,
    pub files: Vec<PathBuf>,
}

/// Projet de `pgweave.toml` trouvé en remontant depuis `start`, catalogue
/// rejoué. Un catalogue incomplet (statement non lu, rejeu en erreur) est
/// une erreur : il donnerait des types faux.
pub fn load_project(start: &Path) -> Result<Project, String> {
    let (root, config) = ProjectConfig::find_and_load(start)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| {
            format!(
                "pas de {} au-dessus de {}",
                ProjectConfig::FILENAME,
                start.display()
            )
        })?;
    let mut files = vec![root.join(ProjectConfig::FILENAME)];

    let mut macros = pgweave_macro::MacroSet::builtin();
    let macro_dir = root.join(&config.macros.directory);
    if macro_dir.is_dir() {
        files.push(macro_dir.clone());
        let errors = macros.load_dir(&macro_dir);
        if !errors.is_empty() {
            return Err(errors.join("\n"));
        }
    }

    let mut builder = ProjectBuilder::new()
        .with_typer(Some(pgweave_sema::ViewTyper::shared()))
        .declarative(config.schema.declarative);
    for chunk in SchemaSource::chunks(&config.to_source(&root)) {
        if !files.contains(&chunk.origin.path) {
            files.push(chunk.origin.path.clone());
        }
        let parsed = parse_source(&chunk.sql, chunk.origin.byte_range.start, &macros);
        builder.add_file(&chunk.origin.path, &chunk.sql, &chunk.role, parsed.as_ref());
    }
    let catalog = builder.finish();
    let mut errors: Vec<String> = catalog
        .diagnostics
        .iter()
        .filter(|(_, d)| d.severity == Severity::Error)
        .map(|(p, d)| format!("{}: error[{}]: {}", p.display(), d.code, d.message))
        .collect();
    errors.extend(
        catalog
            .skipped
            .iter()
            .map(|(p, s)| format!("{}: statement non lu : {:?}", p.display(), s.reason)),
    );
    if !errors.is_empty() {
        return Err(format!("catalogue incomplet :\n{}", errors.join("\n")));
    }
    Ok(Project {
        root,
        config,
        catalog,
        files,
    })
}

fn collect_sql(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_sql(&path, out)?;
        } else if path.extension().is_some_and(|e| e == "sql") {
            out.push(path);
        }
    }
    Ok(())
}

fn line_col(text: &str, offset: usize) -> (usize, usize) {
    let before = &text[..offset.min(text.len())];
    let line = before.matches('\n').count() + 1;
    let col = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
    (line, col)
}
