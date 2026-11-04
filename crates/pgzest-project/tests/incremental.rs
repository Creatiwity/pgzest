//! P2 exit criterion: editing a file only recomputes what depends on it.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pgzest_project::{Database, Project, SourceFile, Workspace, check_file};
use pgzest_schema::{FileRole, SourceOrigin, SqlChunk};
use salsa::Setter;

struct Fixture {
    db: Database,
    project: Project,
    schema: SourceFile,
    q1: SourceFile,
    q2: SourceFile,
}

fn fixture() -> Fixture {
    let db = Database::default();
    let schema = SourceFile::new(
        &db,
        PathBuf::from("schema.sql"),
        "CREATE TABLE t (a INT, b TEXT);\n".into(),
        FileRole::Schema,
        0,
    );
    let q1 = SourceFile::new(
        &db,
        PathBuf::from("q1.sql"),
        "SELECT a FROM t;".into(),
        FileRole::Query,
        0,
    );
    let q2 = SourceFile::new(
        &db,
        PathBuf::from("q2.sql"),
        "SELECT c FROM t;".into(),
        FileRole::Query,
        0,
    );
    let project = Project::new(
        &db,
        vec![schema, q1, q2],
        Arc::new(pgzest_macro::MacroSet::builtin()),
        false,
    );
    Fixture {
        db,
        project,
        schema,
        q1,
        q2,
    }
}

fn check_all(f: &Fixture) -> (usize, usize) {
    let n1 = check_file(&f.db, f.project, f.q1).0.len();
    let n2 = check_file(&f.db, f.project, f.q2).0.len();
    (n1, n2)
}

#[test]
fn first_computation() {
    let f = fixture();
    // q2 references a missing column.
    assert_eq!(check_all(&f), (0, 1));
}

#[test]
fn editing_a_query_only_reruns_that_query() {
    let mut f = fixture();
    check_all(&f);
    f.db.take_executions();

    f.q1.set_text(&mut f.db).to("SELECT b FROM t;".into());
    assert_eq!(check_all(&f), (0, 1));
    let executed = f.db.take_executions();
    assert_eq!(
        executed,
        ["parse_file(q1.sql)", "check_file(q1.sql)"],
        "neither the catalog nor q2 is recomputed"
    );
}

#[test]
fn a_substantially_unchanged_schema_stops_propagation() {
    let mut f = fixture();
    check_all(&f);
    f.db.take_executions();

    // Same DDL, different formatting and comment.
    f.schema
        .set_text(&mut f.db)
        .to("-- tables\nCREATE TABLE t (\n  a INT,\n  b TEXT\n);\n".into());
    assert_eq!(check_all(&f), (0, 1));
    let executed = f.db.take_executions();
    assert!(
        executed.contains(&"project_catalog".to_string()),
        "{executed:?}"
    );
    assert!(
        !executed.iter().any(|e| e.starts_with("check_file")),
        "identical catalog: no check rerun — {executed:?}"
    );
}

#[test]
fn changing_macros_rereads_every_file() {
    let mut f = fixture();
    check_all(&f);
    f.db.take_executions();

    f.project
        .set_macros(&mut f.db)
        .to(Arc::new(pgzest_macro::MacroSet::builtin()));
    check_all(&f);
    let executed = f.db.take_executions();
    for file in ["schema.sql", "q1.sql", "q2.sql"] {
        assert!(
            executed.contains(&format!("parse_file({file})")),
            "{executed:?}"
        );
    }
}

fn chunk(path: &str, sql: &str, role: FileRole) -> SqlChunk {
    SqlChunk {
        origin: SourceOrigin {
            path: PathBuf::from(path),
            byte_range: 0..sql.len(),
        },
        sql: sql.into(),
        role,
    }
}

#[test]
fn workspace_follows_editor_text() {
    let mut ws = Workspace::new(
        &[
            chunk("schema.sql", "CREATE TABLE t (a INT);", FileRole::Schema),
            chunk(
                "m1.sql",
                "ALTER TABLE t ADD COLUMN b nope;",
                FileRole::Migration { sequence: 1 },
            ),
        ],
        Arc::new(pgzest_macro::MacroSet::builtin()),
    );
    let has_b = |ws: &Workspace| {
        ws.catalog()
            .catalog
            .relations
            .values()
            .any(|r| r.columns.iter().any(|c| c.name == "b"))
    };
    assert!(has_b(&ws));
    let not_found = |ws: &Workspace| {
        ws.replay_diagnostics(Path::new("m1.sql"))
            .iter()
            .filter(|d| d.code == "type-not-found")
            .count()
    };
    assert_eq!(not_found(&ws), 1);
    assert!(ws.replay_diagnostics(Path::new("schema.sql")).is_empty());

    // Unsaved text: taken into account without re-reading the disk.
    assert!(ws.set_text(Path::new("m1.sql"), "SELECT 1;"));
    assert!(!has_b(&ws));
    assert_eq!(not_found(&ws), 0);
    assert!(!ws.set_text(Path::new("unknown.sql"), "SELECT 1;"));

    // Same text: nothing is recomputed.
    ws.take_executions();
    assert!(ws.set_text(Path::new("m1.sql"), "SELECT 1;"));
    ws.catalog();
    assert!(ws.take_executions().is_empty());
}

#[test]
fn a_schema_change_reruns_dependents() {
    let mut f = fixture();
    check_all(&f);
    f.db.take_executions();

    f.schema
        .set_text(&mut f.db)
        .to("CREATE TABLE t (a INT, b TEXT, c INT);\n".into());
    assert_eq!(check_all(&f), (0, 0));
    let executed = f.db.take_executions();
    assert!(
        executed.contains(&"check_file(q1.sql)".to_string()),
        "{executed:?}"
    );
    assert!(
        executed.contains(&"check_file(q2.sql)".to_string()),
        "{executed:?}"
    );
}
