//! `CREATE TABLE … (LIKE source …)` : les colonnes copiées existent, à leur
//! position ; une source inconnue ne fabrique pas de table à moitié.
//! `ANALYZE` / `VACUUM` vérifient leurs tables et colonnes.

use pgweave_ast::Statement;
use pgweave_checker::check_sql_self_contained;
use pgweave_diagnostics::{Diagnostic, codes};
use pgweave_schema::provider::{DdlSchemaProvider, SchemaProvider};

fn errors<'a>(diags: &'a [Diagnostic], code: &str) -> Vec<&'a Diagnostic> {
    diags.iter().filter(|d| d.code == code).collect()
}

#[test]
fn les_colonnes_copiees_sont_a_leur_position() {
    let stmts: Vec<Statement> = pgweave_parser::parse(
        "CREATE TABLE src (a INT NOT NULL, b TEXT);\n\
         CREATE TABLE copie (id BIGINT, LIKE src INCLUDING ALL, extra BOOLEAN);",
    )
    .expect("parse");
    let schema = DdlSchemaProvider::from_statements(&stmts);
    let t = schema.get_table("copie").expect("table copie");
    let names: Vec<&str> = t.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["id", "a", "b", "extra"]);
    // NOT NULL est toujours copié.
    assert!(!t.columns[1].nullable);
}

#[test]
fn une_colonne_copiee_est_visible_des_requetes() {
    let d = check_sql_self_contained(
        "CREATE TABLE src (a INT, b TEXT);\n\
         CREATE TABLE copie (LIKE src);\n\
         SELECT a, b FROM copie;\n\
         SELECT nope FROM copie;",
    );
    assert_eq!(errors(&d, codes::COLUMN_NOT_FOUND).len(), 1, "{d:?}");
}

#[test]
fn une_source_inconnue_ne_fabrique_pas_de_table() {
    let stmts = pgweave_parser::parse("CREATE TABLE copie (LIKE absente, id INT);").expect("parse");
    let schema = DdlSchemaProvider::from_statements(&stmts);
    assert!(schema.get_table("copie").is_none());
}

#[test]
fn analyze_et_vacuum_verifient_tables_et_colonnes() {
    let d = check_sql_self_contained(
        "CREATE TABLE t (a INT, b TEXT);\n\
         ANALYZE t (a, nope);\n\
         VACUUM (VERBOSE) absente;\n\
         ANALYZE;",
    );
    assert_eq!(errors(&d, codes::COLUMN_NOT_FOUND).len(), 1, "{d:?}");
    assert_eq!(errors(&d, codes::TABLE_NOT_FOUND).len(), 1, "{d:?}");
}
