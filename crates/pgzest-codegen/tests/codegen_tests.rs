use pgzest_ast::expr::ExprKind;
use pgzest_ast::*;
use pgzest_codegen::emit_statement;

// ── Helpers ───────────────────────────────────────────────────────────────────

fn make_select(columns: Vec<SelectItem>, from: Vec<TableRef>) -> SelectStmt {
    SelectStmt {
        id: NodeId::DUMMY,
        span: Span::DUMMY,
        with: vec![],
        with_recursive: false,
        distinct: false,
        distinct_on: vec![],
        columns,
        from,
        joins: vec![],
        where_clause: None,
        group_by: vec![],
        having: None,
        windows: vec![],
        order_by: vec![],
        limit: None,
        offset: None,
        set_ops: vec![],
        values: vec![],
    }
}

fn col(name: &str) -> SelectItem {
    SelectItem::Expr {
        expr: Expr::bare(ExprKind::Column(ColumnRef {
            schema: None,
            table: None,
            column: Ident::new(name),
        })),
        alias: None,
    }
}

fn table_ref(name: &str) -> TableRef {
    TableRef {
        source: TableSource::Table(ObjectName::unquoted(&[name])),
        alias: None,
        column_aliases: vec![],
        column_types: vec![],
        span: Default::default(),
    }
}

// ── emit_select_roundtrip ─────────────────────────────────────────────────────

/// Build a SELECT AST by hand, emit it to SQL text, then verify that the
/// emitted string contains all the structural keywords we expect.
///
/// Note: we don't have a parser in scope here, so we verify structural
/// properties of the emitted SQL rather than doing a full re-parse roundtrip.
#[test]
fn emit_select_roundtrip() {
    let stmt = Statement::Select(make_select(
        vec![col("id"), col("name")],
        vec![table_ref("users")],
    ));

    let sql = emit_statement(&stmt);

    // Must start with SELECT
    assert!(
        sql.starts_with("SELECT "),
        "expected SELECT prefix, got: {}",
        sql
    );
    // Must reference both projected columns
    assert!(sql.contains("id"), "missing column id in: {}", sql);
    assert!(sql.contains("name"), "missing column name in: {}", sql);
    // Must reference the FROM table
    assert!(sql.contains("FROM"), "missing FROM in: {}", sql);
    assert!(sql.contains("users"), "missing table users in: {}", sql);
}

#[test]
fn emit_select_with_where_and_limit() {
    let stmt = Statement::Select(SelectStmt {
        id: NodeId::DUMMY,
        span: Span::DUMMY,
        with: vec![],
        with_recursive: false,
        distinct: false,
        distinct_on: vec![],
        columns: vec![SelectItem::Wildcard],
        from: vec![table_ref("orders")],
        joins: vec![],
        where_clause: Some(Expr::bare(ExprKind::BinaryOp {
            left: Box::new(Expr::bare(ExprKind::Column(ColumnRef {
                schema: None,
                table: None,
                column: Ident::new("status"),
            }))),
            op: BinaryOp::Eq,
            right: Box::new(Expr::bare(ExprKind::Literal(Literal::String(
                "active".to_string(),
            )))),
        })),
        group_by: vec![],
        having: None,
        windows: vec![],
        order_by: vec![],
        limit: Some(Expr::bare(ExprKind::Literal(Literal::Number(
            "10".to_string(),
        )))),
        offset: None,
        set_ops: vec![],
        values: vec![],
    });

    let sql = emit_statement(&stmt);
    assert!(sql.contains("WHERE"), "missing WHERE");
    assert!(sql.contains("LIMIT 10"), "missing LIMIT 10");
    assert!(sql.contains("'active'"), "missing quoted string");
}
