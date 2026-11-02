//! `pgweave schema import` : import brownfield (PLAN.md, P2).
//!
//! Les migrations existantes (Prisma, Diesel, ou un répertoire de `.sql`)
//! sont rejouées sur le catalogue ; le catalogue obtenu est émis en fichiers
//! de schéma déclaratifs, un par schéma PostgreSQL
//! (`<out>/<schéma>/schema.sql`), et en une migration de référence portant
//! l'en-tête `-- pgweave:`.
//!
//! Si le rejeu signale une erreur, l'état reconstruit n'est pas celui de la
//! base : rien n'est écrit, sauf `--force`.

use std::path::{Path, PathBuf};

use pgweave_diagnostics::Severity;
use pgweave_schema::{FileRole, SourceOrigin, SqlChunk};

/// Organisation du répertoire de migrations. Explicite : la deviner ferait
/// prendre un script de retour arrière pour une migration.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum Layout {
    /// Tous les `.sql`, récursivement, triés par chemin.
    Flat,
    /// Prisma : `<horodatage>_<nom>/migration.sql`.
    Prisma,
    /// Diesel : `<horodatage>_<nom>/up.sql`.
    Diesel,
}

impl Layout {
    fn accepts(self, path: &Path) -> bool {
        match self {
            Layout::Flat => path.extension().is_some_and(|e| e == "sql"),
            Layout::Prisma => path.file_name().is_some_and(|n| n == "migration.sql"),
            Layout::Diesel => path.file_name().is_some_and(|n| n == "up.sql"),
        }
    }
}

fn collect(dir: &Path, layout: Layout, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect(&path, layout, out)?;
        } else if layout.accepts(&path) {
            out.push(path);
        }
    }
    Ok(())
}

pub fn run(
    migrations: &Path,
    layout: Layout,
    out: &Path,
    baseline: Option<&Path>,
    force: bool,
) -> i32 {
    let mut files = Vec::new();
    if let Err(e) = collect(migrations, layout, &mut files) {
        eprintln!("error: {}: {e}", migrations.display());
        return 2;
    }
    files.sort();
    if files.is_empty() {
        eprintln!("error: aucune migration sous {}", migrations.display());
        return 2;
    }
    let mut chunks = Vec::new();
    for (i, path) in files.iter().enumerate() {
        let sql = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("error: {}: {e}", path.display());
                return 2;
            }
        };
        chunks.push(SqlChunk {
            origin: SourceOrigin {
                path: path.clone(),
                byte_range: 0..sql.len(),
            },
            sql,
            role: FileRole::Migration { sequence: i as u64 },
        });
    }

    let macros = pgweave_macro::MacroSet::builtin();
    let mut builder = pgweave_catalog::project::ProjectBuilder::new()
        .with_typer(Some(pgweave_sema::ViewTyper::shared()))
        .keep_sources(true);
    for chunk in &chunks {
        let parsed = pgweave_catalog::project::parse_source(&chunk.sql, 0, &macros);
        builder.add_file(&chunk.origin.path, &chunk.sql, &chunk.role, parsed.as_ref());
    }
    let project = builder.finish();
    let mut errors = 0;
    for (path, sk) in &project.skipped {
        eprintln!(
            "warning: {}: statement non rejoué ({:?}) — l'état reconstruit peut être incomplet",
            path.display(),
            sk.reason
        );
    }
    for (path, d) in &project.diagnostics {
        let sev = match d.severity {
            Severity::Error => {
                errors += 1;
                "error"
            }
            Severity::Warning => "warning",
            Severity::Info => "info",
        };
        eprintln!("{sev}[{}]: {}: {}", d.code, path.display(), d.message);
    }
    if errors > 0 && !force {
        eprintln!(
            "error: {errors} erreur(s) de rejeu : l'état reconstruit n'est pas celui de la base, rien n'est écrit (--force pour écrire quand même)"
        );
        return 1;
    }

    let dump =
        pgweave_catalog::dump::dump(&project.catalog, pgweave_catalog::project::DEFAULT_POSTGRES);
    for (schema, sql) in &dump.schemas {
        let dir = out.join(schema);
        let file = dir.join("schema.sql");
        if let Err(e) = std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&file, sql)) {
            eprintln!("error: {}: {e}", file.display());
            return 2;
        }
        eprintln!("écrit : {}", file.display());
    }
    let baseline = baseline
        .map(Path::to_path_buf)
        .unwrap_or_else(|| out.join("baseline.sql"));
    if let Err(e) = std::fs::write(&baseline, &dump.baseline) {
        eprintln!("error: {}: {e}", baseline.display());
        return 2;
    }
    eprintln!("écrit : {} (migration de référence)", baseline.display());
    0
}
