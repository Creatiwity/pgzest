//! Named queries compiled into a Rust crate, from `build.rs`
//! (PLAN.md §5, P5):
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
//! The project is the one of `pgweave.toml` (searched upwards from the
//! crate): its catalog is replayed, each query of `[queries] directory` is
//! described then translated. No database connection. A query the
//! analysis does not describe fails the build, with its position.
//! Files read are declared to Cargo: changing the schema, a migration or a
//! query reruns the generation.

use std::path::{Path, PathBuf};

use pgweave_catalog::project::{ProjectBuilder, parse_source};
use pgweave_codegen::rust::{QueryModule, RustOptions, Temporal, generate_rust_modules};
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
    /// Project found from the crate being compiled.
    pub fn new() -> Self {
        Builder {
            root: std::env::var_os("CARGO_MANIFEST_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(".")),
            out_file: "pgweave_queries.rs".into(),
            options: RustOptions::default(),
        }
    }

    /// Directory from which to search for `pgweave.toml`.
    pub fn root(mut self, root: impl Into<PathBuf>) -> Self {
        self.root = root.into();
        self
    }

    /// Dates and times: `time` (default) or `chrono`.
    pub fn temporal(mut self, t: Temporal) -> Self {
        self.options.temporal = t;
        self
    }

    /// Rust type of a PostgreSQL type (`"public.geometry"`, `"geo_types::Geometry"`).
    pub fn type_override(mut self, pg: &str, rust: &str) -> Self {
        self.options.overrides.insert(pg.into(), rust.into());
        self
    }

    /// Path of the runtime in the crate (default: `pgweave_runtime`).
    pub fn runtime_path(mut self, path: &str) -> Self {
        self.options.runtime = path.into();
        self
    }

    /// Generates `$OUT_DIR/pgweave_queries.rs`.
    pub fn generate(self) -> Result<PathBuf, String> {
        let out_dir = std::env::var_os("OUT_DIR")
            .map(PathBuf::from)
            .ok_or("OUT_DIR not set: `generate` must be called from build.rs")?;
        let code = self.generate_code(&mut |path| {
            println!("cargo:rerun-if-changed={}", path.display());
        })?;
        let out = out_dir.join(&self.out_file);
        std::fs::write(&out, code).map_err(|e| format!("{}: {e}", out.display()))?;
        Ok(out)
    }

    /// Rust code of the project's queries; `watch` receives each file read.
    pub fn generate_code(&self, watch: &mut dyn FnMut(&Path)) -> Result<String, String> {
        let project = load_project(&self.root)?;
        for f in &project.files {
            watch(f);
        }
        let Some(dir) = &project.config.queries.directory else {
            return Err(format!(
                "no query directory: `[queries] directory` in {}",
                ProjectConfig::FILENAME
            ));
        };
        let dir = project.root.join(dir);
        let mut options = self.options.clone();
        if options.types_path.is_none() {
            options.types_path = project.config.rust.types.clone();
        }
        generate_path(&project, &dir, &options, watch)
    }
}

/// Rust code of the queries of `path`: a directory (one module per file,
/// `order/book.sql` → `order::book`) or a file (its queries at the root).
/// `watch` receives each file and directory read. A query that is not
/// described is an error positioned as `file:line:column`.
pub fn generate_path(
    project: &Project,
    path: &Path,
    options: &RustOptions,
    watch: &mut dyn FnMut(&Path),
) -> Result<String, String> {
    let files = if path.is_dir() {
        watch(path);
        let mut files = Vec::new();
        collect_sql(path, &mut files).map_err(|e| format!("{}: {e}", path.display()))?;
        files.sort();
        files
    } else if path.is_file() {
        vec![path.to_path_buf()]
    } else {
        return Err(format!(
            "{}: neither a file nor a directory",
            path.display()
        ));
    };

    let mut errors = Vec::new();
    let mut modules = Vec::new();
    for file in &files {
        watch(file);
        let text = std::fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
        let parsed = pgweave_query::parse_query_file_with(&text, &project.macros);
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
        let mut descriptors = Vec::new();
        for q in &parsed.queries {
            match pgweave_query::describe_named(&project.catalog, q) {
                Ok(d) => descriptors.push(d),
                Err(diags) => diags.iter().for_each(&mut report),
            }
        }
        let lines = parsed
            .queries
            .iter()
            .map(|q| (q.name.clone(), line_col(&text, q.name_range.start).0))
            .collect();
        let label = file
            .strip_prefix(&project.root)
            .unwrap_or(file)
            .display()
            .to_string();
        modules.push(QueryModule {
            path: if path.is_dir() {
                module_path(path, file)
            } else {
                Vec::new()
            },
            descriptors,
            source: Some(pgweave_query::QuerySource {
                label,
                path: file.canonicalize().unwrap_or_else(|_| file.clone()),
                lines,
            }),
        });
    }
    if !errors.is_empty() {
        return Err(format!("undescribed queries:\n{}", errors.join("\n")));
    }
    generate_rust_modules(&modules, &project.catalog, options)
        .map_err(|e| format!("Rust generation:\n{}", e.join("\n")))
}

/// Loaded project: its configuration, its replayed catalog, and the files
/// read to build it (to watch).
pub struct Project {
    pub root: PathBuf,
    pub config: ProjectConfig,
    pub catalog: pgweave_catalog::ProjectCatalog,
    /// Project macros and fragments (`[macros] directory`).
    pub macros: pgweave_macro::MacroSet,
    pub files: Vec<PathBuf>,
}

/// Project of the `pgweave.toml` found upwards from `start`, catalog
/// replayed. An incomplete catalog (unread statement, failed replay) is an
/// error: it would yield wrong types.
pub fn load_project(start: &Path) -> Result<Project, String> {
    let (root, config) = ProjectConfig::find_and_load(start)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no {} above {}", ProjectConfig::FILENAME, start.display()))?;
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
    let macros = macros.with_instrumentation(config.log.instrumentation());

    let mut builder = ProjectBuilder::new()
        .with_typer(Some(pgweave_sema::ViewTyper::shared()))
        .with_instrumentation(macros.instrumentation())
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
            .map(|(p, s)| format!("{}: unread statement: {:?}", p.display(), s.reason)),
    );
    if !errors.is_empty() {
        return Err(format!("incomplete catalog:\n{}", errors.join("\n")));
    }
    Ok(Project {
        root,
        config,
        catalog,
        macros,
        files,
    })
}

/// Module of a query file in the tree of `dir`: `order/book.sql`
/// → `["order", "book"]`.
pub fn module_path(dir: &Path, file: &Path) -> Vec<String> {
    file.strip_prefix(dir)
        .unwrap_or(file)
        .with_extension("")
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect()
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
