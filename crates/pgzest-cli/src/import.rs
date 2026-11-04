//! `pgzest schema import`: brownfield import (PLAN.md, P2).
//!
//! The existing migrations (Prisma, Diesel, or a directory of `.sql` files)
//! are replayed onto the catalog; the resulting catalog is emitted as
//! declarative schema files, one per PostgreSQL schema
//! (`<out>/<schema>/schema.sql`), and as a baseline migration carrying the
//! `-- pgzest:` header.
//!
//! If the replay reports an error, the rebuilt state is not the database's:
//! nothing is written, unless `--force`.

use std::path::{Path, PathBuf};

use pgzest_diagnostics::Severity;
use pgzest_schema::{FileRole, SourceOrigin, SqlChunk};

/// Layout of the migrations directory. Explicit: guessing it could mistake
/// a rollback script for a migration.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum Layout {
    /// All `.sql` files, recursively, sorted by path.
    Flat,
    /// Prisma: `<timestamp>_<name>/migration.sql`.
    Prisma,
    /// Diesel: `<timestamp>_<name>/up.sql`.
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
        eprintln!("error: no migration under {}", migrations.display());
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

    let macros = pgzest_macro::MacroSet::builtin();
    let mut builder = pgzest_catalog::project::ProjectBuilder::new()
        .with_typer(Some(pgzest_sema::ViewTyper::shared()))
        .keep_sources(true);
    for chunk in &chunks {
        let parsed = pgzest_catalog::project::parse_source(&chunk.sql, 0, &macros);
        builder.add_file(&chunk.origin.path, &chunk.sql, &chunk.role, parsed.as_ref());
    }
    let project = builder.finish();
    let mut errors = 0;
    for (path, sk) in &project.skipped {
        eprintln!(
            "warning: {}: statement not replayed ({:?}) — the rebuilt state may be incomplete",
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
            "error: {errors} replay error(s): the rebuilt state is not the database's, nothing is written (--force to write anyway)"
        );
        return 1;
    }

    let dump =
        pgzest_catalog::dump::dump(&project.catalog, pgzest_catalog::project::DEFAULT_POSTGRES);
    for (schema, sql) in &dump.schemas {
        let dir = out.join(schema);
        let file = dir.join("schema.sql");
        if let Err(e) = std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&file, sql)) {
            eprintln!("error: {}: {e}", file.display());
            return 2;
        }
        eprintln!("written: {}", file.display());
    }
    let baseline = baseline
        .map(Path::to_path_buf)
        .unwrap_or_else(|| out.join("baseline.sql"));
    if let Err(e) = std::fs::write(&baseline, &dump.baseline) {
        eprintln!("error: {}: {e}", baseline.display());
        return 2;
    }
    eprintln!("written: {} (baseline migration)", baseline.display());
    0
}
