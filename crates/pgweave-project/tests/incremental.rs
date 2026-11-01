//! Critère de sortie de P2 : modifier un fichier ne recalcule que ce qui en
//! dépend.

use std::path::PathBuf;

use pgweave_project::{Database, Project, SourceFile, check_file};
use pgweave_schema::FileRole;
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
    );
    let q1 = SourceFile::new(
        &db,
        PathBuf::from("q1.sql"),
        "SELECT a FROM t;".into(),
        FileRole::Query,
    );
    let q2 = SourceFile::new(
        &db,
        PathBuf::from("q2.sql"),
        "SELECT c FROM t;".into(),
        FileRole::Query,
    );
    let project = Project::new(&db, vec![schema, q1, q2]);
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
fn premier_calcul() {
    let f = fixture();
    // q2 cite une colonne absente.
    assert_eq!(check_all(&f), (0, 1));
}

#[test]
fn modifier_une_requete_ne_relance_qu_elle() {
    let mut f = fixture();
    check_all(&f);
    f.db.take_executions();

    f.q1.set_text(&mut f.db).to("SELECT b FROM t;".into());
    assert_eq!(check_all(&f), (0, 1));
    let executed = f.db.take_executions();
    assert_eq!(
        executed,
        [
            "parse_file(q1.sql)",
            "trigger_definitions",
            "check_file(q1.sql)"
        ],
        "ni le catalogue ni q2 ne sont recalculés"
    );
}

#[test]
fn un_schema_inchange_en_substance_coupe_la_propagation() {
    let mut f = fixture();
    check_all(&f);
    f.db.take_executions();

    // Même DDL, mise en forme et commentaire différents.
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
        "catalogue identique : aucune vérification relancée — {executed:?}"
    );
}

#[test]
fn un_changement_de_schema_relance_les_dependants() {
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
