//! Expressions carry their exact position in the document.

use pgweave_ast::{ExprKind, SelectItem, Statement};

/// Text that an expression's span cuts out of the source.
fn slice<'a>(sql: &'a str, e: &pgweave_ast::Expr) -> Option<&'a str> {
    e.real_span()
        .map(|sp| &sql[sp.start as usize..sp.end as usize])
}

fn select_of(sql: &str) -> pgweave_ast::stmt::SelectStmt {
    match pgweave_parser::parse(sql).expect("parse").remove(0) {
        Statement::Select(s) => s,
        other => panic!("SELECT expected, got {other:?}"),
    }
}

fn projections(sql: &str) -> Vec<String> {
    select_of(sql)
        .columns
        .iter()
        .filter_map(|c| match c {
            SelectItem::Expr { expr, .. } => Some(
                slice(sql, expr)
                    .map(str::to_string)
                    .unwrap_or_else(|| "<no span>".into()),
            ),
            _ => None,
        })
        .collect()
}

#[test]
fn projected_columns_carry_their_span() {
    let sql = "SELECT a + 1, nope FROM t;";
    assert_eq!(projections(sql), vec!["a + 1", "nope"]);
}

#[test]
fn the_where_clause_carries_its_span() {
    let sql = "SELECT a FROM t WHERE b LIKE '%x';";
    let sel = select_of(sql);
    let w = sel.where_clause.expect("WHERE expected");
    assert_eq!(slice(sql, &w), Some("b LIKE '%x'"));
}

#[test]
fn sub_expressions_carry_their_span() {
    let sql = "SELECT a FROM t WHERE b = 42;";
    let sel = select_of(sql);
    let w = sel.where_clause.expect("WHERE");
    let ExprKind::BinaryOp { left, right, .. } = &w.kind else {
        panic!("BinaryOp expected");
    };
    assert_eq!(slice(sql, left), Some("b"));
    assert_eq!(slice(sql, right), Some("42"));
}

/// The case that trapped the parser: a multi-byte character before the expression.
/// The column returned by sqlparser is in characters, the expected offset in bytes.
#[test]
fn an_accented_comment_does_not_shift_spans() {
    let sql = "-- accented comment: ï ü ñ → ok\nSELECT a + 1, nope FROM t;";
    assert_eq!(projections(sql), vec!["a + 1", "nope"]);
}

#[test]
fn an_accented_literal_does_not_shift_what_follows() {
    let sql = "SELECT 'naïve', nope FROM t;";
    assert_eq!(projections(sql), vec!["'naïve'", "nope"]);
}

#[test]
fn spans_are_relative_to_the_document_not_the_statement() {
    let sql = "CREATE TABLE t (a INT);\nSELECT nope FROM t;";
    let stmts = pgweave_parser::parse(sql).expect("parse");
    let Statement::Select(sel) = &stmts[1] else {
        panic!("SELECT expected in 2nd position")
    };
    let SelectItem::Expr { expr, .. } = &sel.columns[0] else {
        panic!("expression expected")
    };
    // Must cut the full document, not the isolated 2nd statement.
    assert_eq!(slice(sql, expr), Some("nope"));
}

#[test]
fn parse_at_also_shifts_expression_spans() {
    // SQL extracted from a host document: spans must target the document.
    let doc = "const q = sql`SELECT nope FROM t`;";
    let start = doc.find("SELECT").unwrap();
    let stmts = pgweave_parser::parse_at("SELECT nope FROM t;", start).expect("parse");
    let Statement::Select(sel) = &stmts[0] else {
        panic!("SELECT expected")
    };
    let SelectItem::Expr { expr, .. } = &sel.columns[0] else {
        panic!("expression expected")
    };
    let sp = expr.real_span().expect("span expected");
    assert_eq!(&doc[sp.start as usize..sp.end as usize], "nope");
}

#[test]
fn a_synthesized_expression_has_no_span() {
    // `IS TRUE` is desugared into `= true`: the literal does not exist in the
    // source, so it must not claim a position.
    let sql = "SELECT a FROM t WHERE b IS TRUE;";
    let sel = select_of(sql);
    let w = sel.where_clause.expect("WHERE");
    let ExprKind::BinaryOp { right, .. } = &w.kind else {
        panic!("BinaryOp expected")
    };
    assert!(
        right.real_span().is_none(),
        "the synthesized literal must not carry a span, got {:?}",
        slice(sql, right)
    );
}
