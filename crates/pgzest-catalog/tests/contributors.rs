//! Global context contributors (PLAN.md, D9) seen from a project: missing
//! extension suggested, migration headers checked against the replayed
//! state, migrations without a header reported once.

use std::path::PathBuf;

use pgzest_catalog::ProjectCatalog;
use pgzest_diagnostics::codes;
use pgzest_schema::{FileRole, SourceOrigin, SqlChunk};

fn migration(i: u64, sql: &str) -> SqlChunk {
    SqlChunk {
        origin: SourceOrigin {
            path: PathBuf::from(format!("migrations/{i:03}.sql")),
            byte_range: 0..sql.len(),
        },
        sql: sql.to_string(),
        role: FileRole::Migration { sequence: i },
    }
}

fn codes_of(p: &ProjectCatalog) -> Vec<(&str, &'static str)> {
    p.diagnostics
        .iter()
        .map(|(path, d)| (path.to_str().unwrap(), d.code))
        .collect()
}

#[test]
fn type_of_non_enabled_extension_is_suggested() {
    let p = ProjectCatalog::from_chunks(&[migration(1, "CREATE TABLE t (email citext);")]);
    let d = p
        .diagnostics
        .iter()
        .find(|(_, d)| d.code == codes::TYPE_NOT_FOUND)
        .expect("type-not-found");
    assert!(
        d.1.hint
            .as_deref()
            .unwrap_or("")
            .contains("CREATE EXTENSION citext"),
        "{:?}",
        d.1
    );
}

#[test]
fn headers_checked_against_replayed_state() {
    let p = ProjectCatalog::from_chunks(&[
        migration(
            1,
            "-- pgzest: postgres=18 extensions=citext@1.8\nCREATE EXTENSION citext;",
        ),
        // Declares an extension never created, and forgets citext.
        migration(
            2,
            "-- pgzest: postgres=18 extensions=btree_gist@1.8\nSELECT 1;",
        ),
        migration(3, "-- pgzest: postgres=18 extensions=citext@1.8\nSELECT 1;"),
    ]);
    let c = codes_of(&p);
    let mismatches: Vec<_> = c
        .iter()
        .filter(|(_, code)| *code == codes::MIGRATION_HEADER_MISMATCH)
        .map(|(p, _)| *p)
        .collect();
    assert_eq!(
        mismatches,
        ["migrations/002.sql", "migrations/002.sql"],
        "{c:?}"
    );
}

#[test]
fn migrations_without_header_reported_once() {
    let p = ProjectCatalog::from_chunks(&[
        migration(1, "CREATE TABLE a (id INT);"),
        migration(2, "CREATE TABLE b (id INT);"),
    ]);
    let without: Vec<_> = p
        .diagnostics
        .iter()
        .filter(|(_, d)| d.code == codes::MIGRATION_WITHOUT_HEADER)
        .collect();
    assert_eq!(without.len(), 1);
    assert!(without[0].1.message.starts_with("2 migration(s)"));
}
