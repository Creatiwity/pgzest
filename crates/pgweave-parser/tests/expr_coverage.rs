//! Coverage of common PostgreSQL expressions.
//!
//! Two distinct regressions are locked down here.
//!
//! **Skipped statements.** `convert_binary_op` returned `Unsupported` for 28
//! operators that sqlparser can read, including `||`. A `SELECT a || b` was thus
//! removed from the AST — and by default `unsupported = "ignore"`, so silently.
//!
//! **Lying AST.** The catch-all `_other => Literal(Null)` turned
//! any unmodeled expression into `NULL`. A `WHERE EXISTS (…)` became
//! `WHERE NULL`, which means "no rows": the AST did not omit the
//! information, it asserted a false one.

use pgweave_ast::{BinaryOp, ExprKind, SelectItem, Statement};

const SCHEMA: &str = "CREATE TABLE t (a INT, b TEXT, j JSONB, ts TIMESTAMPTZ, arr INT[]);\n";

/// Parses `SCHEMA` + `sql`, returns the last statement.
fn last_stmt(sql: &str) -> Statement {
    let full = format!("{SCHEMA}{sql}");
    let out = pgweave_parser::parse_with_outcome(&full).expect("parse");
    assert!(
        out.skipped.is_empty(),
        "no statement should be skipped, got: {:?}",
        out.skipped
            .iter()
            .map(|s| (&s.sql, &s.reason))
            .collect::<Vec<_>>()
    );
    out.statements.into_iter().next_back().expect("a statement")
}

fn select_of(sql: &str) -> pgweave_ast::stmt::SelectStmt {
    match last_stmt(sql) {
        Statement::Select(s) => s,
        other => panic!("SELECT expected, got {other:?}"),
    }
}

/// First projected expression.
fn projection(sql: &str) -> ExprKind {
    let sel = select_of(sql);
    match sel.columns.into_iter().next().expect("a column") {
        SelectItem::Expr { expr, .. } => expr.kind,
        other => panic!("expression expected, got {other:?}"),
    }
}

fn where_kind(sql: &str) -> ExprKind {
    select_of(sql).where_clause.expect("WHERE expected").kind
}

/// True if the expression, or one of its sub-expressions, is `Unsupported`.
fn has_unsupported(k: &ExprKind) -> bool {
    match k {
        ExprKind::Unsupported(_) => true,
        ExprKind::BinaryOp { left, right, .. } => {
            has_unsupported(&left.kind) || has_unsupported(&right.kind)
        }
        ExprKind::UnaryOp { expr, .. } | ExprKind::Cast { expr, .. } => has_unsupported(&expr.kind),
        _ => false,
    }
}

// ── Formerly fatal operators ──────────────────────────────────────────────────

#[test]
fn concatenation_produces_a_dedicated_operator() {
    // `||` has its own variant and not `Custom`: its result type is known.
    let k = projection("SELECT b || b FROM t;");
    assert!(
        matches!(
            &k,
            ExprKind::BinaryOp {
                op: BinaryOp::Concat,
                ..
            }
        ),
        "got: {k:?}"
    );
}

#[test]
fn unknown_operators_become_custom_not_an_error() {
    // The catch-all routes to `Custom(symbol)` thanks to sqlparser's
    // `Display`, instead of skipping the statement.
    for (sql, symbol) in [
        ("SELECT a << 2 FROM t;", "<<"),
        ("SELECT a >> 2 FROM t;", ">>"),
        ("SELECT a # 2 FROM t;", "#"),
        ("SELECT a ^ 2 FROM t;", "^"),
    ] {
        let k = projection(sql);
        match &k {
            ExprKind::BinaryOp {
                op: BinaryOp::Custom(s),
                ..
            } => assert_eq!(s, symbol, "expected symbol for {sql}"),
            other => panic!("Custom({symbol}) expected for {sql}, got {other:?}"),
        }
    }
}

#[test]
fn regex_operators_pass() {
    for sql in [
        "SELECT a FROM t WHERE b ~ 'x';",
        "SELECT a FROM t WHERE b ~* 'x';",
        "SELECT a FROM t WHERE b !~ 'x';",
    ] {
        let k = where_kind(sql);
        assert!(matches!(&k, ExprKind::BinaryOp { .. }), "{sql} → {k:?}");
    }
}

#[test]
fn a_bound_parameter_no_longer_fails_the_statement() {
    // No parameterized application query used to get through.
    let k = where_kind("SELECT a FROM t WHERE a = $1;");
    let ExprKind::BinaryOp { right, .. } = &k else {
        panic!("BinaryOp expected, got {k:?}")
    };
    assert!(
        matches!(&right.kind, ExprKind::Parameter(p) if p == "$1"),
        "got: {:?}",
        right.kind
    );
}

// ── Expressions formerly turned into NULL ─────────────────────────────────────

#[test]
fn exists_is_no_longer_turned_into_null() {
    // The most serious case: `WHERE NULL` means "no rows".
    let k = where_kind("SELECT a FROM t WHERE EXISTS (SELECT 1 FROM t);");
    assert!(
        matches!(&k, ExprKind::Exists { negated: false, .. }),
        "got: {k:?}"
    );
}

#[test]
fn not_exists_conserve_sa_negation() {
    let k = where_kind("SELECT a FROM t WHERE NOT EXISTS (SELECT 1 FROM t);");
    assert!(
        matches!(&k, ExprKind::Exists { negated: true, .. }),
        "got: {k:?}"
    );
}

#[test]
fn at_time_zone_is_kept() {
    let k = projection("SELECT ts AT TIME ZONE 'UTC' FROM t;");
    assert!(matches!(&k, ExprKind::AtTimeZone { .. }), "got: {k:?}");
}

#[test]
fn collate_is_kept() {
    let k = projection("SELECT b COLLATE \"C\" FROM t;");
    match &k {
        ExprKind::Collate { collation, .. } => assert_eq!(collation, "C"),
        other => panic!("Collate expected, got {other:?}"),
    }
}

#[test]
fn a_subscript_access_is_kept() {
    let k = projection("SELECT arr[1] FROM t;");
    assert!(matches!(&k, ExprKind::FieldAccess { .. }), "got: {k:?}");
}

#[test]
fn extract_becomes_a_function_call() {
    // Brought back to `FunctionCall` rather than a variant per keyword: the name is
    // enough for inference, and the machinery already exists.
    let k = projection("SELECT EXTRACT(YEAR FROM ts) FROM t;");
    match &k {
        ExprKind::FunctionCall { name, .. } => {
            let rendered = name
                .0
                .iter()
                .map(|i| i.value.as_str())
                .collect::<Vec<_>>()
                .join(".");
            assert!(rendered.contains("extract"), "got: {rendered}");
        }
        other => panic!("FunctionCall expected, got {other:?}"),
    }
}

#[test]
fn an_interval_is_kept() {
    let k = projection("SELECT ts + INTERVAL '1 day' FROM t;");
    let ExprKind::BinaryOp { right, .. } = &k else {
        panic!("BinaryOp expected, got {k:?}")
    };
    assert!(
        matches!(&right.kind, ExprKind::Interval(_)),
        "got: {:?}",
        right.kind
    );
}

// ── The escape hatch does not lie ─────────────────────────────────────────────

#[test]
fn an_unmodeled_expression_declares_itself_as_such() {
    // `GROUPING SETS` is not modeled. What matters is that it does not become
    // `NULL`: the node says "I don't know", which lets consumers
    // skip it instead of drawing wrong conclusions.
    let sel = select_of("SELECT b, count(*) FROM t GROUP BY GROUPING SETS ((b),());");
    let unsupported = sel.group_by.iter().any(|e| has_unsupported(&e.kind));
    let null_literal = sel
        .group_by
        .iter()
        .any(|e| matches!(&e.kind, ExprKind::Literal(pgweave_ast::Literal::Null)));
    assert!(
        unsupported || !null_literal,
        "must not pass itself off as NULL: {:?}",
        sel.group_by
    );
    assert!(
        !null_literal,
        "a GROUP BY GROUPING SETS must not become GROUP BY NULL: {:?}",
        sel.group_by
    );
}

#[test]
fn a_common_expression_produces_no_unsupported() {
    // Safeguard: everyday forms must all be modeled.
    for sql in [
        "SELECT a + 1 FROM t;",
        "SELECT b || 'x' FROM t;",
        "SELECT a FROM t WHERE a IN (1, 2);",
        "SELECT a FROM t WHERE b LIKE 'x%';",
        "SELECT CASE WHEN a > 0 THEN 'p' ELSE 'n' END FROM t;",
        "SELECT a FROM t WHERE EXISTS (SELECT 1 FROM t);",
        "SELECT lower(b) FROM t;",
        "SELECT a::TEXT FROM t;",
    ] {
        let sel = select_of(sql);
        let bad = sel
            .columns
            .iter()
            .filter_map(|c| match c {
                SelectItem::Expr { expr, .. } => Some(&expr.kind),
                _ => None,
            })
            .any(has_unsupported)
            || sel
                .where_clause
                .as_ref()
                .is_some_and(|w| has_unsupported(&w.kind));
        assert!(!bad, "{sql} must not produce any Unsupported");
    }
}
