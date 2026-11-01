//! Contributeurs du contexte global (PLAN.md, D9) vus depuis un projet :
//! extension manquante suggérée, en-têtes de migration confrontés à l'état
//! rejoué, migrations sans en-tête signalées une fois.

use std::path::PathBuf;

use pgweave_catalog::ProjectCatalog;
use pgweave_diagnostics::codes;
use pgweave_schema::{FileRole, SourceOrigin, SqlChunk};

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
fn un_type_d_extension_non_activee_est_suggere() {
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
fn en_tetes_confrontes_a_l_etat_rejoue() {
    let p = ProjectCatalog::from_chunks(&[
        migration(
            1,
            "-- pgweave: postgres=18 extensions=citext@1.8\nCREATE EXTENSION citext;",
        ),
        // Annonce une extension jamais créée, et oublie citext.
        migration(
            2,
            "-- pgweave: postgres=18 extensions=btree_gist@1.8\nSELECT 1;",
        ),
        migration(
            3,
            "-- pgweave: postgres=18 extensions=citext@1.8\nSELECT 1;",
        ),
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
fn migrations_sans_en_tete_signalees_une_fois() {
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
