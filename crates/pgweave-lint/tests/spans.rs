//! Lint diagnostics must carry the span of the statement concerned.
//!
//! Regression: every rule returned a diagnostic without a span, hence
//! reported at `1:1`. Useless in the editor (underline at the top of the
//! file) and impossible to target with `-- pgweave-disable-next-line`.

use pgweave_lint::{LintConfig, lint_statements};

/// 1-indexed line of the byte offset, as the CLI computes it.
fn line_of(sql: &str, offset: usize) -> usize {
    sql[..offset].matches('\n').count() + 1
}

/// (code, line) of each diagnostic, sorted — for stable assertions.
fn diag_lines(sql: &str) -> Vec<(String, usize)> {
    let stmts = pgweave_parser::parse(sql).expect("parse");
    let mut out: Vec<(String, usize)> = lint_statements(&stmts, &LintConfig::default())
        .diagnostics
        .iter()
        .map(|d| {
            let line = d.span.as_ref().map(|s| line_of(sql, s.start)).unwrap_or(0); // 0 = no span, what we want to eliminate
            (d.code.to_string(), line)
        })
        .collect();
    out.sort();
    out
}

#[test]
fn every_diagnostic_has_a_span() {
    let sql = "\
CREATE TABLE t (a INT PRIMARY KEY);

SELECT * FROM t;

CREATE TABLE no_pk (id UUID);

SELECT a FROM t WHERE a::text LIKE '%x';
";
    let stmts = pgweave_parser::parse(sql).expect("parse");
    let diags = lint_statements(&stmts, &LintConfig::default()).diagnostics;
    assert!(!diags.is_empty(), "diagnostics are expected");
    for d in &diags {
        assert!(
            d.span.is_some(),
            "diagnostic {} has no span — it would be reported at 1:1",
            d.code
        );
    }
}

#[test]
fn the_span_points_to_the_right_line() {
    let sql = "\
CREATE TABLE t (a INT PRIMARY KEY);

SELECT * FROM t;
";
    // SELECT * is on line 3.
    assert_eq!(diag_lines(sql), vec![("L001".to_string(), 3)]);
}

#[test]
fn leading_comments_do_not_shift() {
    let sql = "\
CREATE TABLE t (a INT PRIMARY KEY);

-- a comment
-- and a second one
SELECT * FROM t;
";
    // The span must start at SELECT (line 5), not at the comment.
    assert_eq!(diag_lines(sql), vec![("L001".to_string(), 5)]);
}

#[test]
fn an_accent_does_not_shift_the_span() {
    let sql = "\
CREATE TABLE t (a INT PRIMARY KEY);

-- accented comment: é à ù → ok
SELECT * FROM t;
";
    assert_eq!(diag_lines(sql), vec![("L001".to_string(), 4)]);
}

#[test]
fn several_statements_each_on_its_line() {
    let sql = "\
CREATE TABLE a_t (x INT PRIMARY KEY);
SELECT * FROM a_t;
CREATE TABLE no_pk (y UUID);
SELECT * FROM no_pk;
";
    let got = diag_lines(sql);
    assert!(
        got.contains(&("L001".to_string(), 2)),
        "first SELECT * on line 2, got: {got:?}"
    );
    assert!(
        got.contains(&("L001".to_string(), 4)),
        "second SELECT * on line 4, got: {got:?}"
    );
    assert!(
        got.contains(&("L002".to_string(), 3)),
        "table without PK on line 3, got: {got:?}"
    );
}

#[test]
fn cross_statement_rule_l005_carries_its_span() {
    // L005 is computed over all statements: the driver cannot attach the
    // span, the rule must carry it itself.
    let sql = "\
CREATE TABLE parent (id UUID PRIMARY KEY);

CREATE TABLE child (
    id     UUID PRIMARY KEY,
    par_id UUID REFERENCES parent(id)
);
";
    let got = diag_lines(sql);
    let l005: Vec<_> = got.iter().filter(|(c, _)| c == "L005").collect();
    assert_eq!(l005.len(), 1, "one L005 expected, got: {got:?}");
    assert_eq!(
        l005[0].1, 3,
        "the span must point to CREATE TABLE child (line 3), got: {got:?}"
    );
}

#[test]
fn a_directive_can_target_a_lint_rule() {
    // This was impossible before: without a span, positional suppression
    // could not know which line to apply to.
    let sql = "\
CREATE TABLE t (a INT PRIMARY KEY);

-- pgweave-disable-next-line L001
SELECT * FROM t;

SELECT * FROM t;
";
    let stmts = pgweave_parser::parse(sql).expect("parse");
    let mut diags = lint_statements(&stmts, &LintConfig::default()).diagnostics;
    pgweave_diagnostics::Suppressions::scan(sql).apply(&mut diags);

    let l001: Vec<_> = diags.iter().filter(|d| d.code == "L001").collect();
    assert_eq!(
        l001.len(),
        1,
        "only one L001 must survive, got: {:?}",
        diags.iter().map(|d| d.code).collect::<Vec<_>>()
    );
    let line = line_of(sql, l001[0].span.as_ref().unwrap().start);
    assert_eq!(line, 6, "the survivor is the one on line 6");
}
