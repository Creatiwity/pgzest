//! Critère de sortie de P2 : modifier un fichier ne recalcule que ce qui en
//! dépend.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pgweave_project::{Database, Project, SourceFile, Workspace, check_file};
use pgweave_schema::{FileRole, SourceOrigin, SqlChunk};
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
        Arc::new(pgweave_macro::MacroSet::builtin()),
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
        ["parse_file(q1.sql)", "check_file(q1.sql)"],
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
fn changer_les_macros_relit_tous_les_fichiers() {
    let mut f = fixture();
    check_all(&f);
    f.db.take_executions();

    f.project
        .set_macros(&mut f.db)
        .to(Arc::new(pgweave_macro::MacroSet::builtin()));
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
fn espace_de_travail_suit_le_texte_de_l_editeur() {
    let mut ws = Workspace::new(
        &[
            chunk("schema.sql", "CREATE TABLE t (a INT);", FileRole::Schema),
            chunk(
                "m1.sql",
                "ALTER TABLE t ADD COLUMN b nope;",
                FileRole::Migration { sequence: 1 },
            ),
        ],
        Arc::new(pgweave_macro::MacroSet::builtin()),
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

    // Texte non sauvegardé : pris en compte sans relire le disque.
    assert!(ws.set_text(Path::new("m1.sql"), "SELECT 1;"));
    assert!(!has_b(&ws));
    assert_eq!(not_found(&ws), 0);
    assert!(!ws.set_text(Path::new("inconnu.sql"), "SELECT 1;"));

    // Même texte : rien n'est recalculé.
    ws.take_executions();
    assert!(ws.set_text(Path::new("m1.sql"), "SELECT 1;"));
    ws.catalog();
    assert!(ws.take_executions().is_empty());
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
