//! Migration directories of common tools: numbered files, Prisma
//! (`<timestamp>_<name>/migration.sql`), Diesel (`<date>_<name>/up.sql` and
//! `down.sql`).

use pgweave_schema::{FileRole, MigrationDirSource, SchemaSource};

fn layout(name: &str, files: &[(&str, &str)]) -> Vec<(String, u64)> {
    let dir = std::env::temp_dir().join(format!("pgweave-layout-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for (path, sql) in files {
        let p = dir.join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, sql).unwrap();
    }
    let out = MigrationDirSource::new(&dir)
        .chunks()
        .into_iter()
        .map(|c| {
            let rel = c
                .origin
                .path
                .strip_prefix(&dir)
                .unwrap()
                .display()
                .to_string();
            let FileRole::Migration { sequence } = c.role else {
                panic!("expected a migration")
            };
            (rel, sequence)
        })
        .collect();
    let _ = std::fs::remove_dir_all(&dir);
    out
}

#[test]
fn prisma() {
    let got = layout(
        "prisma",
        &[
            ("20220601000000_b/migration.sql", "SELECT 2;"),
            ("20220507000000_a/migration.sql", "SELECT 1;"),
            ("migration_lock.toml", "provider = \"postgresql\""),
        ],
    );
    assert_eq!(
        got,
        [
            ("20220507000000_a/migration.sql".to_string(), 20220507000000),
            ("20220601000000_b/migration.sql".to_string(), 20220601000000),
        ]
    );
}

#[test]
fn diesel_without_down_files() {
    let got = layout(
        "diesel",
        &[
            ("2023-01-02-000000_b/up.sql", "SELECT 2;"),
            ("2023-01-02-000000_b/down.sql", "SELECT -2;"),
            ("2023-01-01-000000_a/up.sql", "SELECT 1;"),
            ("2023-01-01-000000_a/down.sql", "SELECT -1;"),
        ],
    );
    let files: Vec<&str> = got.iter().map(|(f, _)| f.as_str()).collect();
    assert_eq!(
        files,
        ["2023-01-01-000000_a/up.sql", "2023-01-02-000000_b/up.sql"]
    );
}

#[test]
fn numbered_files() {
    let got = layout("flat", &[("0002_b.sql", ""), ("0001_a.sql", "")]);
    assert_eq!(
        got,
        [("0001_a.sql".to_string(), 1), ("0002_b.sql".to_string(), 2)]
    );
}
