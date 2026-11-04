//! `pgzest migrate create` writes the simplest migration that reaches the
//! declarative schema, even when it loses data or may fail on a populated
//! database: the developer adapts it, guided by its header. Changes pgzest
//! cannot generate are left to write, and `migrate check` fails until then.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_pgzest");

fn scaffold(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "pgzest-migrate-{name}-{}-{:?}",
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

/// Runs `pgzest migrate <args>` from `root`: (exit code, stdout + stderr).
fn migrate(root: &Path, args: &[&str]) -> (i32, String) {
    let out = Command::new(BIN)
        .arg("migrate")
        .args(args)
        .current_dir(root)
        .output()
        .expect("the pgzest binary runs");
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
            ("pgzest.toml", CONFIG),
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
            ("pgzest.toml", CONFIG),
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

#[test]
fn the_log_level_is_generated_and_recorded_in_the_header() {
    let routine = "@function bump(k INT): INT {\n    UPDATE t SET n = n + 1 WHERE id <= k;\n    RETURN k;\n}\n";
    let root = scaffold(
        "log",
        &[
            ("pgzest.toml", &format!("{CONFIG}[log]\nlevel = \"step\"\n")),
            (
                "schema/t.sql",
                "CREATE TABLE t (id INT PRIMARY KEY, n INT);\n",
            ),
            ("schema/bump.sql", routine),
        ],
    );
    let (code, out) = migrate(&root, &["create", "init"]);
    assert_eq!(code, 0, "{out}");
    let sql = fs::read_to_string(root.join("migrations/0001_init.sql")).unwrap();
    assert!(
        sql.starts_with("-- pgzest: postgres=18 log=step\n"),
        "{sql}"
    );
    // The tables the routine writes to come with it.
    for expected in [
        "CREATE SCHEMA pgzest_log",
        "CREATE UNLOGGED TABLE pgzest_log.run",
        "_pgzest_steps jsonb[]",
    ] {
        assert!(sql.contains(expected), "{expected}\n{sql}");
    }
    let (code, out) = migrate(&root, &["check"]);
    assert_eq!(code, 0, "{out}");

    // Another level: the routine is generated again, the header says so.
    fs::write(
        root.join("pgzest.toml"),
        format!("{CONFIG}[log]\nlevel = \"info\"\n"),
    )
    .unwrap();
    let (code, out) = migrate(&root, &["check"]);
    assert_eq!(code, 1, "{out}");
    let (code, out) = migrate(&root, &["create", "info"]);
    assert_eq!(code, 0, "{out}");
    let sql = fs::read_to_string(root.join("migrations/0002_info.sql")).unwrap();
    assert!(
        sql.starts_with("-- pgzest: postgres=18 log=info\n"),
        "{sql}"
    );
    assert!(
        sql.contains("run log level changed (log=step in 0001_init.sql, log=info"),
        "{sql}"
    );
    assert!(sql.contains("bump"), "{sql}");
    assert!(!sql.contains("_pgzest_steps"), "{sql}");
    let (code, out) = migrate(&root, &["check"]);
    assert_eq!(code, 0, "{out}");

    // A header that does not match its routines is an error.
    fs::write(
        root.join("migrations/0002_info.sql"),
        sql.replace("log=info", "log=debug"),
    )
    .unwrap();
    let (code, out) = migrate(&root, &["check"]);
    assert_eq!(code, 1, "{out}");
    assert!(
        out.contains("routine bump is generated with `log=info`, the header declares `log=debug`"),
        "{out}"
    );
    let _ = fs::remove_dir_all(&root);
}
