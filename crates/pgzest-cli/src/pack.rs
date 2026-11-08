//! `pgzest db pack`: the catalog pack of an extension pgzest does not ship,
//! generated from a PostgreSQL server where it is installed, for the
//! project's `[postgres] packs`.
//!
//! The extension is created in a temporary database, then the database is
//! dropped: nothing else is modified on the server.

use std::path::{Path, PathBuf};

use pgzest_schema::ProjectConfig;

pub struct Options {
    pub url: String,
    pub extension: String,
    pub version: Option<String>,
    /// Directory to write into (default: the first of `[postgres] packs`).
    pub out: Option<PathBuf>,
}

pub fn run(opts: Options) -> i32 {
    match generate(opts) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {e}");
            2
        }
    }
}

fn generate(opts: Options) -> Result<(), String> {
    let project = crate::discover_project()?;
    let dir = match (&opts.out, &project) {
        (Some(out), _) => out.clone(),
        (None, Some((root, config))) => match config.postgres.packs.first() {
            Some(d) => root.join(d),
            None => {
                return Err(format!(
                    "no pack directory: pass --out, or list one in `[postgres] packs` of {}",
                    root.join(ProjectConfig::FILENAME).display()
                ));
            }
        },
        (None, None) => {
            return Err(format!(
                "no {} found: pass --out to say where to write the pack",
                ProjectConfig::FILENAME
            ));
        }
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let pack = runtime.block_on(pgzest_oracle::pack_gen::generate(
        &opts.url,
        Some(&opts.extension),
        opts.version.as_deref(),
    ))?;
    // A pack describes the extension on one PostgreSQL version: the project
    // would refuse one generated on another.
    if let Some((_, config)) = &project {
        let target = config.postgres.version.to_string();
        if pack.postgres.as_deref() != Some(target.as_str()) {
            return Err(format!(
                "the server runs postgres@{}, the project targets postgres@{target}: \
                 generate the pack on a PostgreSQL {target} server",
                pack.postgres.as_deref().unwrap_or("?")
            ));
        }
    }
    let path = write(&dir, &pack)?;
    eprintln!(
        "{}: {} → {}",
        pack.contributor,
        pgzest_oracle::pack_gen::summary(&pack),
        path.display()
    );
    if let Some((root, config)) = &project
        && !config.postgres.packs.iter().any(|d| root.join(d) == dir)
    {
        eprintln!(
            "note: {} is not in `[postgres] packs`: the project will not use this pack",
            dir.display()
        );
    }
    Ok(())
}

fn write(dir: &Path, pack: &pgzest_catalog::Pack) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let path = dir.join(format!(
        "{}-{}.json",
        pack.contributor.name, pack.contributor.version
    ));
    std::fs::write(&path, pack.to_json()).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(path)
}
