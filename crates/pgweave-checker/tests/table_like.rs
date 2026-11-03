//! `CREATE TABLE … (LIKE source …)`: the copied columns exist, at their
//! position; an unknown source does not produce a half-built table.
//! `ANALYZE` / `VACUUM` check their tables and columns.

use pgweave_ast::Statement;
use pgweave_catalog::ProjectCatalog;
use pgweave_checker::check_sql_self_contained;
use pgweave_diagnostics::{Diagnostic, codes};
use pgweave_schema::provider::SchemaProvider;

fn errors<'a>(diags: &'a [Diagnostic], code: &str) -> Vec<&'a Diagnostic> {
    diags.iter().filter(|d| d.code == code).collect()
}

#[test]
fn copied_columns_are_at_their_position() {
    let stmts: Vec<Statement> = pgweave_parser::parse(
        "CREATE TABLE src (a INT NOT NULL, b TEXT);\n\
         CREATE TABLE copy_t (id BIGINT, LIKE src INCLUDING ALL, extra BOOLEAN);",
    )
    .expect("parse");
    let schema = ProjectCatalog::from_statements(&stmts);
    let t = schema.get_table("copy_t").expect("table copy_t");
    let names: Vec<&str> = t.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["id", "a", "b", "extra"]);
    // NOT NULL is always copied.
    assert!(!t.columns[1].nullable);
}

#[test]
fn a_copied_column_is_visible_to_queries() {
    let d = check_sql_self_contained(
        "CREATE TABLE src (a INT, b TEXT);\n\
         CREATE TABLE copy_t (LIKE src);\n\
         SELECT a, b FROM copy_t;\n\
         SELECT nope FROM copy_t;",
    );
    assert_eq!(errors(&d, codes::COLUMN_NOT_FOUND).len(), 1, "{d:?}");
}

#[test]
fn an_unknown_source_does_not_produce_a_table() {
    let stmts =
        pgweave_parser::parse("CREATE TABLE copy_t (LIKE missing, id INT);").expect("parse");
    let schema = ProjectCatalog::from_statements(&stmts);
    assert!(schema.get_table("copy_t").is_none());
}

#[test]
fn analyze_and_vacuum_check_tables_and_columns() {
    let d = check_sql_self_contained(
        "CREATE TABLE t (a INT, b TEXT);\n\
         ANALYZE t (a, nope);\n\
         VACUUM (VERBOSE) missing;\n\
         ANALYZE;",
    );
    assert_eq!(errors(&d, codes::COLUMN_NOT_FOUND).len(), 1, "{d:?}");
    assert_eq!(errors(&d, codes::TABLE_NOT_FOUND).len(), 1, "{d:?}");
}
