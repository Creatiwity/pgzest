//! `cargo xtask catalog-gen`: generates a pack shipped with pgzest (PLAN.md,
//! D9) from a real PostgreSQL, with `pgzest_oracle::pack_gen`.

use std::path::PathBuf;

pub fn run(
    url: Option<String>,
    extension: Option<String>,
    version: Option<String>,
    out: Option<PathBuf>,
) -> Result<(), String> {
    let url = url
        .or_else(|| std::env::var("PGZEST_ORACLE_URL").ok())
        .ok_or("--pg-url or PGZEST_ORACLE_URL required")?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let pack = rt.block_on(pgzest_oracle::pack_gen::generate(
        &url,
        extension.as_deref(),
        version.as_deref(),
    ))?;
    let path = out.unwrap_or_else(|| {
        PathBuf::from(format!(
            "crates/pgzest-catalog/packs/{}-{}.json",
            pack.contributor.name, pack.contributor.version
        ))
    });
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(&path, pack.to_json()).map_err(|e| format!("{}: {e}", path.display()))?;
    eprintln!(
        "{}: {} → {}",
        pack.contributor,
        pgzest_oracle::pack_gen::summary(&pack),
        path.display()
    );
    Ok(())
}
