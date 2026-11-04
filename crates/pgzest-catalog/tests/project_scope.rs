//! The project indexing scope must be consistent with file classification,
//! and cover subdirectories.

use std::fs;
use std::path::Path;

use pgzest_catalog::ProjectCatalog;
use pgzest_schema::{FileRole, ProjectConfig, SchemaProvider, SchemaSource};

/// Creates a throwaway project tree and returns its root.
fn scaffold(name: &str, files: &[(&str, &str)]) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!("pgzest-scope-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    for (rel, content) in files {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, content).unwrap();
    }
    root
}

fn load(root: &Path) -> ProjectConfig {
    ProjectConfig::find_and_load(root)
        .expect("valid config")
        .expect("pgzest.toml present")
        .1
}

#[test]
fn subdirectories_are_indexed() {
    // Regression: the scan was `read_dir` without recursion, so
    // schema/tables/users.sql was invisible.
    let root = scaffold(
        "recursive",
        &[
            (
                "pgzest.toml",
                "[postgres]\nversion = 18\n[schema]\ndirectory = \"schema\"\n",
            ),
            (
                "schema/tables/users.sql",
                "CREATE TABLE users (id UUID, email TEXT);",
            ),
            ("schema/functions/f.sql", "CREATE TABLE helper (a INT);"),
        ],
    );
    let cfg = load(&root);
    let resolver = ProjectCatalog::from_chunks(&cfg.to_source(&root).chunks());

    assert!(
        resolver.get_table("users").is_some(),
        "schema/tables/users.sql must be indexed"
    );
    assert!(
        resolver.get_table("helper").is_some(),
        "schema/functions/f.sql must be indexed"
    );
}

#[test]
fn without_schema_directory_the_root_is_the_scope() {
    // Consistent with classify_file, which treats every non-migration as Schema.
    let root = scaffold(
        "rootscope",
        &[
            ("pgzest.toml", "[postgres]\nversion = 18\n[schema]\n"),
            ("db/users.sql", "CREATE TABLE users (id UUID);"),
            ("elsewhere/deep/t.sql", "CREATE TABLE deep (a INT);"),
        ],
    );
    let cfg = load(&root);
    let resolver = ProjectCatalog::from_chunks(&cfg.to_source(&root).chunks());

    assert!(resolver.get_table("users").is_some());
    assert!(
        resolver.get_table("deep").is_some(),
        "without schema.directory, the whole project is indexed"
    );
    // And the classification agrees.
    assert_eq!(
        cfg.classify_file(&root.join("db/users.sql"), &root),
        FileRole::Schema
    );
}

#[test]
fn migrations_are_not_indexed_twice() {
    // Without exclusion, the root scan would pick up migrations/ as Schema on
    // top of Migration, replaying its ALTERs.
    let root = scaffold(
        "nodouble",
        &[
            ("pgzest.toml", "[postgres]\nversion = 18\n[schema]\n"),
            ("migrations/000001_init.sql", "CREATE TABLE t (a INT);"),
        ],
    );
    let cfg = load(&root);
    let chunks = cfg.to_source(&root).chunks();

    let from_migrations: Vec<_> = chunks
        .iter()
        .filter(|c| c.origin.path.to_string_lossy().contains("migrations"))
        .collect();
    assert_eq!(
        from_migrations.len(),
        1,
        "the migration file must appear only once, got {:?}",
        from_migrations
            .iter()
            .map(|c| &c.origin.path)
            .collect::<Vec<_>>()
    );
    assert!(
        from_migrations[0].role.is_migration(),
        "and with the Migration role"
    );
}

#[test]
fn node_modules_and_target_are_ignored() {
    let root = scaffold(
        "ignored",
        &[
            ("pgzest.toml", "[postgres]\nversion = 18\n[schema]\n"),
            ("real.sql", "CREATE TABLE real_t (a INT);"),
            (
                "node_modules/pkg/schema.sql",
                "CREATE TABLE junk_a (a INT);",
            ),
            ("target/debug/x.sql", "CREATE TABLE junk_b (a INT);"),
            (".schema-snapshots/old.sql", "CREATE TABLE junk_c (a INT);"),
        ],
    );
    let cfg = load(&root);
    let resolver = ProjectCatalog::from_chunks(&cfg.to_source(&root).chunks());

    assert!(resolver.get_table("real_t").is_some());
    for junk in ["junk_a", "junk_b", "junk_c"] {
        assert!(
            resolver.get_table(junk).is_none(),
            "{junk} should not be indexed"
        );
    }
}

#[test]
fn schema_include_adds_directories() {
    let root = scaffold(
        "include",
        &[
            (
                "pgzest.toml",
                "[postgres]\nversion = 18\n[schema]\ndirectory = \"schema\"\ninclude = [\"extra\"]\n",
            ),
            ("schema/a.sql", "CREATE TABLE a_t (a INT);"),
            ("extra/b.sql", "CREATE TABLE b_t (b INT);"),
            ("elsewhere/c.sql", "CREATE TABLE c_t (c INT);"),
        ],
    );
    let cfg = load(&root);
    let resolver = ProjectCatalog::from_chunks(&cfg.to_source(&root).chunks());

    assert!(resolver.get_table("a_t").is_some(), "schema/");
    assert!(resolver.get_table("b_t").is_some(), "extra/ via include");
    assert!(
        resolver.get_table("c_t").is_none(),
        "out of scope when schema.directory is explicit"
    );
}

#[test]
fn named_queries_are_not_indexed_as_schema() {
    // Without schema.directory, the root is the scope: the queries directory
    // must be excluded from it, and its files classified as Query.
    let root = scaffold(
        "queries",
        &[
            (
                "pgzest.toml",
                "[postgres]\nversion = 18\n[queries]\ndirectory = \"queries\"\n",
            ),
            ("db/t.sql", "CREATE TABLE t (a INT);"),
            (
                "queries/pages.sql",
                "-- name: decoy\nCREATE TABLE decoy (a INT);",
            ),
        ],
    );
    let cfg = load(&root);
    let resolver = ProjectCatalog::from_chunks(&cfg.to_source(&root).chunks());

    assert!(resolver.get_table("t").is_some());
    assert!(
        resolver.get_table("decoy").is_none(),
        "a query file does not declare schema objects"
    );
    assert_eq!(
        cfg.classify_file(&root.join("queries/pages.sql"), &root),
        FileRole::Query
    );
}

#[test]
fn schema_include_is_classified_schema() {
    // Regression: with schema.directory present, a schema.include file was
    // indexed as schema but classified as Query.
    let root = scaffold(
        "include-role",
        &[
            (
                "pgzest.toml",
                "[postgres]\nversion = 18\n[schema]\ndirectory = \"schema\"\ninclude = [\"extra\"]\n",
            ),
            ("schema/a.sql", "CREATE TABLE a_t (a INT);"),
            ("extra/b.sql", "CREATE TABLE b_t (b INT);"),
        ],
    );
    let cfg = load(&root);
    assert_eq!(
        cfg.classify_file(&root.join("extra/b.sql"), &root),
        FileRole::Schema
    );
}

#[test]
fn invalid_config_is_an_error_not_a_default() {
    let root = scaffold(
        "invalid",
        &[("pgzest.toml", "[postgres]\nversion = 18\n[shema]\n")],
    );
    let err = ProjectConfig::find_and_load(&root).expect_err("unknown key");
    assert!(err.to_string().contains("pgzest.toml"), "{err}");
}
