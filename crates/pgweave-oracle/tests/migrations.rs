//! Oracle des migrations (PLAN.md P7).
//!
//! Chaque cas de `tests/migrations/<cas>/` est une évolution de schéma
//! déclaratif en versions (`v1.sql`, `v2.sql`…). Chaque migration est
//! générée depuis les précédentes, comme `pgweave migrate create` :
//!
//! - sans base : elle est vérifiée par rejeu (l'état atteint est la cible) ;
//! - avec PostgreSQL (`make oracle`) : les migrations appliquées dans l'ordre
//!   et la version déclarée appliquée d'un bloc donnent deux bases
//!   identiques, selon PostgreSQL lui-même.

use std::path::{Path, PathBuf};

use pgweave_catalog::ProjectCatalog;
use pgweave_catalog::diff::diff;
use pgweave_catalog::project::{ProjectBuilder, parse_source};
use pgweave_diagnostics::Severity;
use pgweave_schema::FileRole;

fn cases() -> Vec<(String, Vec<String>)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/migrations");
    let mut out = Vec::new();
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&root)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    for dir in dirs {
        let mut versions: Vec<PathBuf> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|x| x == "sql"))
            .collect();
        versions.sort();
        out.push((
            dir.file_name().unwrap().to_string_lossy().into_owned(),
            versions
                .iter()
                .map(|p| std::fs::read_to_string(p).unwrap())
                .collect(),
        ));
    }
    out
}

/// Rejeu déclaratif : `history` comme migrations, `schema` comme cible.
fn project(history: &[String], schema: &str) -> ProjectCatalog {
    let macros = pgweave_macro::MacroSet::builtin();
    let mut b = ProjectBuilder::new()
        .with_typer(Some(pgweave_sema::ViewTyper::shared()))
        .with_checker(Some(pgweave_sema::check::SemaChecker::shared()))
        .declarative(true)
        .keep_sources(true);
    for (i, sql) in history.iter().enumerate() {
        let path = PathBuf::from(format!("migrations/{:04}.sql", i + 1));
        let parsed = parse_source(sql, 0, &macros);
        b.add_file(
            &path,
            sql,
            &FileRole::Migration {
                sequence: i as u64 + 1,
            },
            parsed.as_ref(),
        );
    }
    let parsed = parse_source(schema, 0, &macros);
    b.add_file(
        Path::new("schema.sql"),
        schema,
        &FileRole::Schema,
        parsed.as_ref(),
    );
    let p = b.finish();
    let errors: Vec<String> = p
        .diagnostics
        .iter()
        .filter(|(_, d)| d.severity == Severity::Error)
        .map(|(f, d)| format!("{}: [{}] {}", f.display(), d.code, d.message))
        .chain(
            p.skipped
                .iter()
                .map(|(f, s)| format!("{}: non lu {:?}", f.display(), s.reason)),
        )
        .collect();
    assert!(
        errors.is_empty(),
        "rejeu en erreur :\n{}",
        errors.join("\n")
    );
    p
}

/// Migrations successives d'un cas, chacune vérifiée par rejeu.
fn generate(case: &str, versions: &[String]) -> Vec<String> {
    let mut history: Vec<String> = Vec::new();
    for (i, target) in versions.iter().enumerate() {
        let p = project(&history, target);
        let m = diff(p.history.as_ref().unwrap(), &p.catalog);
        assert!(
            m.unsupported.is_empty(),
            "{case} v{} : non pris en charge {:?}",
            i + 1,
            m.unsupported
        );
        let sql = format!(
            "{}\n{}\n",
            pgweave_catalog::dump::header(&p.catalog, 18),
            m.to_sql()
        );
        history.push(sql);
        let check = project(&history, target);
        let rest = diff(check.history.as_ref().unwrap(), &check.catalog);
        assert!(
            rest.is_empty(),
            "{case} v{} : la migration n'atteint pas la cible\n--- migration\n{}\n--- reste\n{}",
            i + 1,
            history.last().unwrap(),
            rest.to_sql()
        );
    }
    history
}

#[test]
fn evolutions_generees_et_verifiees_par_rejeu() {
    for (case, versions) in cases() {
        let history = generate(&case, &versions);
        assert_eq!(history.len(), versions.len());
    }
}

#[tokio::test]
#[ignore = "nécessite PostgreSQL : make oracle"]
async fn evolutions_appliquees_sur_postgresql() {
    let url = std::env::var(pgweave_oracle::URL_ENV).expect("PGWEAVE_ORACLE_URL");
    for (case, versions) in cases() {
        let history = generate(&case, &versions);
        for (i, target) in versions.iter().enumerate() {
            let migrations: Vec<(String, String)> = history[..=i]
                .iter()
                .enumerate()
                .map(|(j, sql)| (format!("migration {}", j + 1), sql.clone()))
                .collect();
            let cmp = pgweave_oracle::schema::compare(
                &url,
                &format!("{case}_v{}", i + 1),
                &migrations,
                target,
            )
            .await
            .unwrap_or_else(|e| panic!("{case} v{} : {e}\n--- migration\n{}", i + 1, history[i]));
            assert!(
                cmp.is_identical(),
                "{case} v{} : bases différentes\n--- migrations seules\n{}\n--- schéma seul\n{}\n--- migration\n{}",
                i + 1,
                cmp.only_migrations.join("\n"),
                cmp.only_schema.join("\n"),
                history[i]
            );
            eprintln!("{case} v{} : {} objets identiques", i + 1, cmp.objects);
        }
    }
}
