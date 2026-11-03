//! `cargo xtask migrate-oracle <project>`: migration oracle on a real
//! declarative project (PLAN.md P7).
//!
//! The project migrations are applied in order to an empty database, the
//! declarative schema at once to another; both databases are compared
//! through what PostgreSQL says about their objects
//! (`pgweave_oracle::schema`). The report only contains aggregates; gaps,
//! names included, are only printed with `--details`.

use std::path::{Path, PathBuf};

use pgweave_schema::{FileRole, ProjectConfig, SchemaSource};

pub fn run(project: &Path, pg_url: Option<String>, details: bool) -> Result<(), String> {
    let url = pg_url
        .or_else(|| std::env::var(pgweave_oracle::URL_ENV).ok())
        .ok_or_else(|| {
            format!(
                "PostgreSQL server expected: --pg-url or {}",
                pgweave_oracle::URL_ENV
            )
        })?;
    let (root, config) = ProjectConfig::find_and_load(project)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no {} under {}", ProjectConfig::FILENAME, project.display()))?;
    if !config.schema.declarative {
        return Err("project is not declarative (`[schema] declarative = true` expected)".into());
    }
    let chunks = config.to_source(&root).chunks();
    let rel = |p: &PathBuf| p.strip_prefix(&root).unwrap_or(p).display().to_string();
    let migrations: Vec<(String, String)> = chunks
        .iter()
        .filter(|c| matches!(c.role, FileRole::Migration { .. }))
        .map(|c| (rel(&c.origin.path), c.sql.clone()))
        .collect();
    let schema: String = chunks
        .iter()
        .filter(|c| c.role == FileRole::Schema)
        .map(|c| c.sql.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let cmp = rt.block_on(pgweave_oracle::schema::compare(
        &url,
        "xtask",
        &migrations,
        &schema,
    ))?;
    println!(
        "{} migration(s) applied, {} description line(s) compared: {}",
        migrations.len(),
        cmp.objects,
        if cmp.is_identical() {
            "identical databases".to_string()
        } else {
            format!(
                "{} only in migrations, {} only in schema",
                cmp.only_migrations.len(),
                cmp.only_schema.len()
            )
        }
    );
    if details {
        for l in &cmp.only_migrations {
            eprintln!("- migrations: {l}");
        }
        for l in &cmp.only_schema {
            eprintln!("+ schema: {l}");
        }
    }
    if cmp.is_identical() {
        Ok(())
    } else {
        Err("the migrations do not reach the declarative schema".into())
    }
}
