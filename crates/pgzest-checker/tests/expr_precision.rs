//! Diagnostics target the faulty expression, not the start of the statement.

use pgzest_checker::check_sql_self_contained;
use pgzest_diagnostics::{Diagnostic, codes};

/// 1-based (line, column) of a diagnostic, as the CLI displays them.
fn line_col(sql: &str, d: &Diagnostic) -> (usize, usize) {
    let Some(sp) = d.span.as_ref().filter(|s| s.start > 0 || s.end > 0) else {
        return (1, 1);
    };
    let before = &sql[..sp.start];
    let line = before.matches('\n').count() + 1;
    let col = before
        .rfind('\n')
        .map(|nl| before[nl + 1..].chars().count() + 1)
        .unwrap_or_else(|| before.chars().count() + 1);
    (line, col)
}

/// Text the diagnostic's span cuts out of the source.
fn slice<'a>(sql: &'a str, d: &Diagnostic) -> Option<&'a str> {
    d.span
        .as_ref()
        .filter(|s| s.end > s.start)
        .map(|s| &sql[s.start..s.end])
}

fn only(sql: &str, code: &str) -> Diagnostic {
    let diags = check_sql_self_contained(sql);
    let mut it = diags.into_iter().filter(|d| d.code == code);
    let d = it.next().unwrap_or_else(|| panic!("no {code} for {sql:?}"));
    assert!(it.next().is_none(), "exactly one {code} expected");
    d
}

#[test]
fn the_diagnostic_targets_the_faulty_column_in_select() {
    let sql = "CREATE TABLE users (id UUID, email TEXT);\nSELECT id, nope, email FROM users;";
    let d = only(sql, codes::COLUMN_NOT_FOUND);
    assert_eq!(slice(sql, &d), Some("nope"));
    assert_eq!(line_col(sql, &d), (2, 12));
}

#[test]
fn the_diagnostic_targets_the_faulty_column_in_where() {
    let sql =
        "CREATE TABLE users (id UUID, email TEXT);\nSELECT email FROM users WHERE missing = 'x';";
    let d = only(sql, codes::COLUMN_NOT_FOUND);
    assert_eq!(slice(sql, &d), Some("missing"));
    assert_eq!(line_col(sql, &d), (2, 31));
}

#[test]
fn precision_survives_an_accent_upstream() {
    // Combined regression: sqlparser column in characters + offsets in bytes.
    let sql = "CREATE TABLE users (id UUID);\n-- accented comment: é à ù\nSELECT nope FROM users;";
    let d = only(sql, codes::COLUMN_NOT_FOUND);
    assert_eq!(
        slice(sql, &d),
        Some("nope"),
        "an accent upstream must not shift the span"
    );
}

#[test]
fn a_nested_expression_is_targeted_precisely() {
    let sql = "CREATE TABLE t (a INT);\nSELECT a FROM t WHERE (a + bad) > 1;";
    let d = only(sql, codes::COLUMN_NOT_FOUND);
    assert_eq!(
        slice(sql, &d),
        Some("bad"),
        "the finest node must win, not the enclosing expression"
    );
}

#[test]
fn a_missing_qualified_column_is_targeted() {
    let sql = "CREATE TABLE users (id UUID);\nSELECT users.nope FROM users;";
    let d = only(sql, codes::COLUMN_NOT_FOUND);
    assert_eq!(slice(sql, &d), Some("users.nope"));
}

#[test]
fn the_statement_span_remains_the_fallback() {
    // `table-not-found` is about the statement, not an expression:
    // the statement backfill must keep applying.
    let sql = "CREATE TABLE t (a INT);\nSELECT a FROM nonexistent;";
    let d = only(sql, codes::TABLE_NOT_FOUND);
    let (line, _) = line_col(sql, &d);
    assert_eq!(line, 2, "at least the right line");
}

#[test]
fn several_errors_each_target_their_column() {
    let sql = "CREATE TABLE t (a INT);\nSELECT bad1 FROM t;\nSELECT bad2 FROM t;";
    let diags = check_sql_self_contained(sql);
    let cols: Vec<_> = diags
        .iter()
        .filter(|d| d.code == codes::COLUMN_NOT_FOUND)
        .map(|d| slice(sql, d))
        .collect();
    assert_eq!(cols, vec![Some("bad1"), Some("bad2")]);
}
