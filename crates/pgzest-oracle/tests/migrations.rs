//! Migration oracle (PLAN.md P7).
//!
//! Each case of `tests/migrations/<case>/` is a declarative schema evolution
//! in versions (`v1.sql`, `v2.sql`...). Each migration is generated from the
//! previous ones, like `pgzest migrate create`:
//!
//! - without a database: it is verified by replay (the reached state is the
//!   target);
//! - with PostgreSQL (`make oracle`): the migrations applied in order and the
//!   declared version applied at once yield two identical databases,
//!   according to PostgreSQL itself.

use std::path::{Path, PathBuf};

use pgzest_catalog::ProjectCatalog;
use pgzest_catalog::diff::diff;
use pgzest_catalog::project::{ProjectBuilder, parse_source};
use pgzest_diagnostics::Severity;
use pgzest_schema::FileRole;

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

/// Declarative replay: `history` as migrations, `schema` as target.
fn project(history: &[String], schema: &str) -> ProjectCatalog {
    let macros = pgzest_macro::MacroSet::builtin();
    let mut b = ProjectBuilder::new()
        .with_typer(Some(pgzest_sema::ViewTyper::shared()))
        .with_checker(Some(pgzest_sema::check::SemaChecker::shared()))
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
                .map(|(f, s)| format!("{}: not read {:?}", f.display(), s.reason)),
        )
        .collect();
    assert!(errors.is_empty(), "replay failed:\n{}", errors.join("\n"));
    p
}

/// Successive migrations of a case, each verified by replay.
fn generate(case: &str, versions: &[String]) -> Vec<String> {
    let mut history: Vec<String> = Vec::new();
    for (i, target) in versions.iter().enumerate() {
        let p = project(&history, target);
        let m = diff(p.history.as_ref().unwrap(), &p.catalog);
        assert!(
            m.unsupported.is_empty(),
            "{case} v{}: unsupported {:?}",
            i + 1,
            m.unsupported
        );
        let sql = format!(
            "{}\n{}\n",
            pgzest_catalog::dump::header(&p.catalog, 18, Default::default()),
            m.to_sql()
        );
        history.push(sql);
        let check = project(&history, target);
        let rest = diff(check.history.as_ref().unwrap(), &check.catalog);
        assert!(
            rest.is_empty(),
            "{case} v{}: the migration does not reach the target\n--- migration\n{}\n--- remainder\n{}",
            i + 1,
            history.last().unwrap(),
            rest.to_sql()
        );
    }
    history
}

#[test]
fn evolutions_generated_and_verified_by_replay() {
    for (case, versions) in cases() {
        let history = generate(&case, &versions);
        assert_eq!(history.len(), versions.len());
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL: make oracle"]
async fn evolutions_applied_on_postgresql() {
    let url = std::env::var(pgzest_oracle::URL_ENV).expect("PGZEST_ORACLE_URL");
    for (case, versions) in cases() {
        let history = generate(&case, &versions);
        for (i, target) in versions.iter().enumerate() {
            let migrations: Vec<(String, String)> = history[..=i]
                .iter()
                .enumerate()
                .map(|(j, sql)| (format!("migration {}", j + 1), sql.clone()))
                .collect();
            let cmp = pgzest_oracle::schema::compare(
                &url,
                &format!("{case}_v{}", i + 1),
                &migrations,
                target,
            )
            .await
            .unwrap_or_else(|e| panic!("{case} v{}: {e}\n--- migration\n{}", i + 1, history[i]));
            assert!(
                cmp.is_identical(),
                "{case} v{}: databases differ\n--- migrations only\n{}\n--- schema only\n{}\n--- migration\n{}",
                i + 1,
                cmp.only_migrations.join("\n"),
                cmp.only_schema.join("\n"),
                history[i]
            );
            eprintln!("{case} v{}: {} identical objects", i + 1, cmp.objects);
        }
    }
}
