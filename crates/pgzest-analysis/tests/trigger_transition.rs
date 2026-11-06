//! Transition tables of a statement-level trigger
//! (`REFERENCING NEW TABLE AS inserted OLD TABLE AS removed`).
//!
//! Regression (Mesalia): in the function body, `inserted` and
//! `removed` were reported as nonexistent tables. They are tables
//! with the columns of the trigger's table, visible only from the body.

use pgzest_analysis::check_sql_self_contained;
use pgzest_diagnostics::{Diagnostic, codes};

fn transition_trigger(body: &str) -> Vec<Diagnostic> {
    let sql = format!(
        "CREATE TABLE stock (id BIGINT PRIMARY KEY, quantity INTEGER NOT NULL);\n\
         CREATE TABLE stock_log (id BIGINT, delta INTEGER);\n\
         CREATE FUNCTION log_stock() RETURNS TRIGGER LANGUAGE plpgsql AS $$\n\
         BEGIN\n\
         {body}\n\
         RETURN NULL;\n\
         END;\n\
         $$;\n\
         CREATE TRIGGER stock_changes AFTER UPDATE ON stock\n\
         REFERENCING OLD TABLE AS removed NEW TABLE AS inserted\n\
         FOR EACH STATEMENT EXECUTE FUNCTION log_stock();\n"
    );
    check_sql_self_contained(&sql)
}

fn errors<'a>(diags: &'a [Diagnostic], code: &str) -> Vec<&'a Diagnostic> {
    diags.iter().filter(|d| d.code == code).collect()
}

#[test]
fn transition_tables_are_in_scope() {
    let d = transition_trigger(
        "INSERT INTO stock_log (id, delta)\n\
         SELECT i.id, i.quantity - r.quantity FROM inserted i JOIN removed r ON r.id = i.id;",
    );
    assert!(errors(&d, codes::TABLE_NOT_FOUND).is_empty(), "{d:?}");
    assert!(errors(&d, codes::COLUMN_NOT_FOUND).is_empty(), "{d:?}");
}

#[test]
fn a_column_missing_from_a_transition_table_is_reported() {
    let d = transition_trigger("INSERT INTO stock_log (id) SELECT i.nope FROM inserted i;");
    assert_eq!(errors(&d, codes::COLUMN_NOT_FOUND).len(), 1, "{d:?}");
}

#[test]
fn outside_the_body_transition_tables_do_not_exist() {
    let d = transition_trigger("NULL;");
    let outside = "SELECT id FROM inserted;";
    let d2 = check_sql_self_contained(outside);
    assert!(errors(&d, codes::TABLE_NOT_FOUND).is_empty());
    assert_eq!(errors(&d2, codes::TABLE_NOT_FOUND).len(), 1, "{d2:?}");
}

#[test]
fn a_do_block_is_checked_like_a_procedure() {
    let body = "BEGIN INSERT INTO missing (id) VALUES (1); END";
    let codes_of = |sql: String| {
        let mut c: Vec<&'static str> = check_sql_self_contained(&sql)
            .iter()
            .map(|d| d.code)
            .collect();
        c.sort_unstable();
        c
    };
    let as_do = codes_of(format!("DO $$ {body} $$;"));
    let as_procedure = codes_of(format!(
        "CREATE PROCEDURE p() LANGUAGE plpgsql AS $$ {body} $$;"
    ));
    assert_eq!(as_do, as_procedure);
    assert!(as_do.contains(&codes::TABLE_NOT_FOUND), "{as_do:?}");
}

#[test]
fn copy_checks_table_and_columns() {
    let d = check_sql_self_contained(
        "CREATE TABLE stock (id BIGINT, quantity INTEGER);\n\
         COPY stock (id, nope) FROM STDIN WITH (FORMAT csv);\n\
         COPY missing FROM STDIN;",
    );
    assert_eq!(errors(&d, codes::COLUMN_NOT_FOUND).len(), 1, "{d:?}");
    assert_eq!(errors(&d, codes::TABLE_NOT_FOUND).len(), 1, "{d:?}");
}
