//! The CLI must see the same schema as the LSP.
//!
//! Historically `pgweave check` ignored pgweave.toml and required explicit
//! `--schema` flags, while the LSP indexed the whole project. Same file,
//! different diagnostics between the editor and CI.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_pgweave");

fn scaffold(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "pgweave-parity-{name}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = fs::remove_dir_all(&root);
    for (rel, content) in files {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, content).unwrap();
    }
    root
}

/// Runs `pgweave check` from `root`, returns (code, stdout+stderr).
fn check(root: &Path, args: &[&str]) -> (i32, String) {
    let out = Command::new(BIN)
        .arg("check")
        .args(args)
        .current_dir(root)
        .output()
        .expect("the pgweave binary must run");
    let mut combined = String::from_utf8_lossy(&out.stdout).to_string();
    combined.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.code().unwrap_or(-1), combined)
}

const PROJECT: &[(&str, &str)] = &[
    (
        "pgweave.toml",
        "[postgres]\nversion = 18\n[schema]\ndirectory = \"schema\"\n",
    ),
    (
        "schema/tables/users.sql",
        "CREATE TABLE users (id UUID PRIMARY KEY, email TEXT NOT NULL);",
    ),
    ("queries/ok.sql", "SELECT email FROM users;"),
    ("queries/bad.sql", "SELECT nope FROM users;"),
];

#[test]
fn project_scope_resolves_without_explicit_schema() {
    let root = scaffold("resolve", PROJECT);
    let (code, out) = check(&root, &["queries/ok.sql"]);
    assert_eq!(
        code, 0,
        "users must resolve from the project scope, output:\n{out}"
    );
}

#[test]
fn a_missing_column_is_still_detected() {
    let root = scaffold("badcol", PROJECT);
    let (code, out) = check(&root, &["queries/bad.sql"]);
    assert_eq!(code, 1, "output:\n{out}");
    assert!(
        out.contains("column-not-found"),
        "column-not-found expected, output:\n{out}"
    );
}

#[test]
fn no_project_falls_back_to_isolated_mode() {
    let root = scaffold("noproject", PROJECT);
    let (code, out) = check(&root, &["queries/ok.sql", "--no-project"]);
    assert_eq!(code, 1, "without a project index, users is unknown:\n{out}");
    assert!(out.contains("table-not-found"), "output:\n{out}");
}

#[test]
fn scope_subdirectories_count() {
    // schema/tables/users.sql is in a subdirectory: the scan must recurse,
    // otherwise the table stays invisible.
    let root = scaffold("subdir", PROJECT);
    let (code, out) = check(&root, &["queries/ok.sql"]);
    assert_eq!(code, 0, "output:\n{out}");
    assert!(!out.contains("table-not-found"), "output:\n{out}");
}

#[test]
fn an_unindexable_statement_is_reported() {
    // A table defined in an unreadable statement disappears from the index;
    // the CLI must warn rather than pretend the index is complete.
    let root = scaffold(
        "skipidx",
        &[
            (
                "pgweave.toml",
                "[postgres]\nversion = 18\n[schema]\ndirectory = \"schema\"\n",
            ),
            ("schema/broken.sql", "CREATE TABL users (id UUID);"),
            ("queries/q.sql", "SELECT id FROM users;"),
        ],
    );
    let (_code, out) = check(&root, &["queries/q.sql"]);
    assert!(
        out.contains("not indexed"),
        "the indexing gap must be reported, output:\n{out}"
    );
}

#[test]
fn explicit_schema_still_takes_precedence() {
    // Backward compatibility: --schema must keep working and take precedence.
    let root = scaffold(
        "explicit",
        &[
            (
                "pgweave.toml",
                "[postgres]\nversion = 18\n[schema]\ndirectory = \"schema\"\n",
            ),
            ("schema/a.sql", "CREATE TABLE t (a INT);"),
            ("other.sql", "CREATE TABLE other_t (b INT);"),
            ("q.sql", "SELECT b FROM other_t;"),
        ],
    );
    let (code, out) = check(&root, &["q.sql", "--schema", "other.sql"]);
    assert_eq!(code, 0, "output:\n{out}");
}

#[test]
fn without_config_self_contained_mode_is_kept() {
    // No pgweave.toml: the historical behavior
    // (DDL of the file itself) must stay intact.
    let root = scaffold(
        "standalone",
        &[("q.sql", "CREATE TABLE t (a INT);\nSELECT a FROM t;")],
    );
    let (code, out) = check(&root, &["q.sql"]);
    assert_eq!(code, 0, "output:\n{out}");
}

const QUERIES_PROJECT: &[(&str, &str)] = &[
    (
        "pgweave.toml",
        "[postgres]\nversion = 18\n[schema]\ndirectory = \"schema\"\n\
         [queries]\ndirectory = \"queries\"\n",
    ),
    (
        "schema/pages.sql",
        "CREATE TABLE pages (id BIGINT PRIMARY KEY, title TEXT NOT NULL);",
    ),
    (
        "queries/pages.sql",
        "-- name: page_by_id :optional\nSELECT id, title FROM pages WHERE id = $id;\n\n\
         -- name: rename_page\nUPDATE pages SET title = $title WHERE id = $id;\n",
    ),
    (
        "queries/mixed.sql",
        "-- name: mixed\nSELECT id FROM pages WHERE id = $id OR id = $1;\n",
    ),
    ("queries/unnamed.sql", "SELECT id FROM pages;\n"),
    (
        "queries/bad_column.sql",
        "-- name: q\nSELECT nope FROM pages;\n",
    ),
];

#[test]
fn a_valid_named_query_file_passes() {
    let root = scaffold("queries-ok", QUERIES_PROJECT);
    let (code, out) = check(&root, &["queries/pages.sql"]);
    assert_eq!(code, 0, "output:\n{out}");
}

#[test]
fn named_query_format_is_checked() {
    let root = scaffold("queries-format", QUERIES_PROJECT);
    let (code, out) = check(&root, &["queries/mixed.sql"]);
    assert_eq!(code, 1, "output:\n{out}");
    assert!(out.contains("query-mixed-parameters"), "output:\n{out}");

    let (code, out) = check(&root, &["queries/unnamed.sql"]);
    assert_eq!(code, 1, "output:\n{out}");
    assert!(out.contains("query-without-name"), "output:\n{out}");
}

#[test]
fn a_named_query_is_also_checked_against_the_schema() {
    let root = scaffold("queries-schema", QUERIES_PROJECT);
    let (code, out) = check(&root, &["queries/bad_column.sql"]);
    assert_eq!(code, 1, "output:\n{out}");
    assert!(out.contains("column-not-found"), "output:\n{out}");
}

#[test]
fn format_only_applies_to_the_queries_directory() {
    // A file outside [queries] directory need not carry a header.
    let root = scaffold("queries-scope", QUERIES_PROJECT);
    fs::write(root.join("adhoc.sql"), "SELECT id FROM pages;\n").unwrap();
    let (code, out) = check(&root, &["adhoc.sql"]);
    assert_eq!(code, 0, "output:\n{out}");
}

#[test]
fn an_invalid_config_stops_check() {
    let root = scaffold(
        "badconfig",
        &[
            ("pgweave.toml", "[postgres]\nversion = 18\n[shema]\n"),
            ("q.sql", "SELECT 1;"),
        ],
    );
    let (code, out) = check(&root, &["q.sql"]);
    assert_eq!(code, 2, "output:\n{out}");
    assert!(out.contains("pgweave.toml"), "output:\n{out}");
}
