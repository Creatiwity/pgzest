//! `pgweave migrate create` writes the simplest migration that reaches the
//! declarative schema, even when it loses data or may fail on a populated
//! database: the developer adapts it, guided by its header. Changes pgweave
//! cannot generate are left to write, and `migrate check` fails until then.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_pgweave");

fn scaffold(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "pgweave-migrate-{name}-{}-{:?}",
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

/// Runs `pgweave migrate <args>` from `root`: (exit code, stdout + stderr).
fn migrate(root: &Path, args: &[&str]) -> (i32, String) {
    let out = Command::new(BIN)
        .arg("migrate")
        .args(args)
        .current_dir(root)
        .output()
        .expect("the pgweave binary runs");
    let mut combined = String::from_utf8_lossy(&out.stdout).to_string();
    combined.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.code().unwrap_or(-1), combined)
}

const CONFIG: &str = "[postgres]\nversion = 18\n[migrations]\ndirectory = \"migrations\"\n\
                      [schema]\ndirectory = \"schema\"\ndeclarative = true\n";

#[test]
fn destructive_migration_is_written_with_a_review_header() {
    let root = scaffold(
        "destructive",
        &[
            ("pgweave.toml", CONFIG),
            (
                "migrations/0001_init.sql",
                "CREATE TABLE t (id INT PRIMARY KEY, label TEXT);\n",
            ),
            (
                "schema/t.sql",
                "CREATE TABLE t (id INT PRIMARY KEY, name TEXT, price INT NOT NULL);\n",
            ),
        ],
    );
    let (code, out) = migrate(&root, &["create", "rename"]);
    assert_eq!(code, 0, "{out}");
    let sql = fs::read_to_string(root.join("migrations/0002_rename.sql")).unwrap();
    let header = sql.find("TO ADAPT").expect(&sql);
    let drop = sql.find("DROP COLUMN label").expect(&sql);
    assert!(header < drop, "{sql}");
    // Both risks are listed in the header, before any statement.
    let first_statement = sql.find("ALTER TABLE").unwrap();
    for code in ["M001", "M002"] {
        assert!(sql[..first_statement].contains(code), "{code}\n{sql}");
    }
    // As generated, the migration reaches the schema.
    let (code, out) = migrate(&root, &["check"]);
    assert_eq!(code, 0, "{out}");
}

#[test]
fn change_left_to_write_keeps_check_failing() {
    let root = scaffold(
        "unsupported",
        &[
            ("pgweave.toml", CONFIG),
            (
                "migrations/0001_init.sql",
                "CREATE TYPE k AS ENUM ('a', 'b');\nCREATE TABLE t (id INT PRIMARY KEY);\n",
            ),
            (
                "schema/t.sql",
                "CREATE TYPE k AS ENUM ('a');\nCREATE TABLE t (id INT PRIMARY KEY, n INT);\n",
            ),
        ],
    );
    let (code, out) = migrate(&root, &["create", "enum"]);
    assert_eq!(code, 0, "{out}");
    let sql = fs::read_to_string(root.join("migrations/0002_enum.sql")).unwrap();
    assert!(sql.contains("TODO type public.k"), "{sql}");
    assert!(sql.contains("ADD COLUMN n integer"), "{sql}");
    let (code, out) = migrate(&root, &["check"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("public.k"), "{out}");
}
