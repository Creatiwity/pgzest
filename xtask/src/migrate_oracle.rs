//! `cargo xtask migrate-oracle <projet>` : oracle des migrations sur un
//! projet déclaratif réel (PLAN.md P7).
//!
//! Les migrations du projet sont appliquées dans l'ordre sur une base
//! vierge, le schéma déclaratif d'un bloc sur une autre ; les deux bases
//! sont comparées par ce que PostgreSQL dit de leurs objets
//! (`pgweave_oracle::schema`). Le rapport ne contient que des agrégats ; les
//! écarts, noms compris, ne sont affichés qu'avec `--details`.

use std::path::{Path, PathBuf};

use pgweave_schema::{FileRole, ProjectConfig, SchemaSource};

pub fn run(project: &Path, pg_url: Option<String>, details: bool) -> Result<(), String> {
    let url = pg_url
        .or_else(|| std::env::var(pgweave_oracle::URL_ENV).ok())
        .ok_or_else(|| {
            format!(
                "serveur PostgreSQL attendu : --pg-url ou {}",
                pgweave_oracle::URL_ENV
            )
        })?;
    let (root, config) = ProjectConfig::find_and_load(project)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| {
            format!(
                "pas de {} sous {}",
                ProjectConfig::FILENAME,
                project.display()
            )
        })?;
    if !config.schema.declarative {
        return Err("projet non déclaratif (`[schema] declarative = true` attendu)".into());
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
        "{} migration(s) appliquée(s), {} ligne(s) de description comparée(s) : {}",
        migrations.len(),
        cmp.objects,
        if cmp.is_identical() {
            "bases identiques".to_string()
        } else {
            format!(
                "{} propre(s) aux migrations, {} propre(s) au schéma",
                cmp.only_migrations.len(),
                cmp.only_schema.len()
            )
        }
    );
    if details {
        for l in &cmp.only_migrations {
            eprintln!("- migrations : {l}");
        }
        for l in &cmp.only_schema {
            eprintln!("+ schéma : {l}");
        }
    }
    if cmp.is_identical() {
        Ok(())
    } else {
        Err("les migrations n'atteignent pas le schéma déclaratif".into())
    }
}
