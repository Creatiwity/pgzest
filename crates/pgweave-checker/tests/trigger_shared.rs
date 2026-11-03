//! A trigger function shared between several tables.
//!
//! Regression: `set_updated_at` attached to four tables raised a
//! `trigger-ambiguous-table` error, whereas it is an idiomatic PostgreSQL
//! pattern — NEW and OLD take the type of the firing table. `examples/schema.sql`
//! therefore did not pass `check`.
//!
//! The body is now checked once per table. A diagnostic that does not
//! depend on the table is emitted only once; a diagnostic specific to a
//! table names it.

use pgweave_checker::check_sql_self_contained;
use pgweave_diagnostics::{Diagnostic, Severity, codes};

fn shared_trigger(body: &str) -> Vec<Diagnostic> {
    let sql = format!(
        "CREATE TABLE users (id UUID PRIMARY KEY, updated_at TIMESTAMPTZ);\n\
         CREATE TABLE posts (id UUID PRIMARY KEY, updated_at TIMESTAMPTZ, title TEXT);\n\
         CREATE OR REPLACE FUNCTION touch() RETURNS TRIGGER\n\
         LANGUAGE plpgsql AS $$\n\
         BEGIN\n\
         {body}\n\
         END;\n\
         $$;\n\
         CREATE TRIGGER t_users BEFORE UPDATE ON users FOR EACH ROW EXECUTE FUNCTION touch();\n\
         CREATE TRIGGER t_posts BEFORE UPDATE ON posts FOR EACH ROW EXECUTE FUNCTION touch();\n"
    );
    check_sql_self_contained(&sql)
}

fn with_code<'a>(diags: &'a [Diagnostic], code: &str) -> Vec<&'a Diagnostic> {
    diags.iter().filter(|d| d.code == code).collect()
}

#[test]
fn a_column_common_to_all_tables_produces_nothing() {
    let d = shared_trigger("    NEW.updated_at = now();\n    RETURN NEW;");
    assert!(
        with_code(&d, codes::TRIGGER_AMBIGUOUS_TABLE).is_empty(),
        "no more ambiguity reported: {d:?}"
    );
    assert!(
        !d.iter().any(|d| d.severity == Severity::Error),
        "no error expected: {d:?}"
    );
    assert!(
        with_code(&d, codes::TRIGGER_NO_CONTEXT).is_empty(),
        "the trigger context is resolved: {d:?}"
    );
}

#[test]
fn a_column_missing_from_a_single_table_is_reported_on_that_table() {
    let d = shared_trigger("    NEW.title = 'x';\n    RETURN NEW;");
    let missing = with_code(&d, codes::COLUMN_NOT_FOUND);
    assert_eq!(missing.len(), 1, "exactly one report expected: {d:?}");
    assert!(
        missing[0].message.contains("users"),
        "the message must name the faulty table: {}",
        missing[0].message
    );
    assert!(
        !missing[0].message.contains("posts"),
        "posts does have the column: {}",
        missing[0].message
    );
}

#[test]
fn a_composite_condition_is_checked_on_each_table() {
    let d = shared_trigger(
        "    IF OLD.title IS DISTINCT FROM NEW.title THEN\n\
         \x20       NEW.updated_at = now();\n\
         \x20   END IF;\n\
         \x20   RETURN NEW;",
    );
    let missing = with_code(&d, codes::COLUMN_NOT_FOUND);
    assert!(!missing.is_empty(), "users has no title: {d:?}");
    assert!(
        missing.iter().all(|m| m.message.contains("users")),
        "only users is faulty: {missing:?}"
    );
}

#[test]
fn a_table_independent_diagnostic_is_emitted_only_once() {
    let d = shared_trigger("    RETURN unknown_var;");
    let undeclared = with_code(&d, codes::UNDECLARED_VARIABLE);
    assert_eq!(
        undeclared.len(),
        1,
        "no duplicate per table: {undeclared:?}"
    );
    assert!(
        !undeclared[0].message.contains("trigger on"),
        "nothing table-specific: {}",
        undeclared[0].message
    );
}

#[test]
fn the_example_schema_no_longer_produces_ambiguity() {
    let sql = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/schema.sql"
    ))
    .expect("examples/schema.sql");
    let d = check_sql_self_contained(&sql);
    assert!(
        !d.iter().any(|d| d.severity == Severity::Error),
        "examples/schema.sql must pass check: {d:?}"
    );
}
