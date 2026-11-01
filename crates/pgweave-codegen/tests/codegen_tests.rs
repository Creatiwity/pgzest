use pgweave_ast::expr::ExprKind;
use pgweave_ast::*;
use pgweave_codegen::{DiffOp, diff_schemas, emit_statement};

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
    }
}

fn simple_col_def(name: &str, dt: DataType) -> ColumnDef {
    ColumnDef {
        name: Ident::new(name),
        data_type: dt,
        nullable: true,
        default: None,
        generated: None,
        identity: None,
        constraints: vec![],
    }
}

fn make_create_table(table_name: &str, cols: Vec<ColumnDef>) -> Statement {
    Statement::Ddl(DdlStatement::CreateTable(CreateTable {
        id: NodeId::DUMMY,
        span: Span::DUMMY,
        name: ObjectName::unquoted(&[table_name]),
        if_not_exists: false,
        temporary: false,
        unlogged: false,
        on_commit: None,
        columns: cols,
        constraints: vec![],
        likes: vec![],
    }))
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

// ── diff_adds_table ───────────────────────────────────────────────────────────

#[test]
fn diff_adds_table() {
    let old: Vec<Statement> = vec![];
    let new = vec![make_create_table(
        "organizations",
        vec![
            simple_col_def("id", DataType::Uuid),
            simple_col_def("name", DataType::Text),
        ],
    )];

    let diff = diff_schemas(&old, &new);

    assert_eq!(diff.operations.len(), 1, "expected exactly one diff op");
    match &diff.operations[0] {
        DiffOp::CreateTable(ct) => {
            assert_eq!(
                ct.name.0[0].value, "organizations",
                "expected organizations table"
            );
        }
        other => panic!("expected CreateTable, got {:?}", other),
    }
}

// ── diff_adds_column ──────────────────────────────────────────────────────────

#[test]
fn diff_adds_column() {
    let base_cols = vec![
        simple_col_def("id", DataType::Uuid),
        simple_col_def("name", DataType::Text),
        simple_col_def("email", DataType::Text),
    ];
    let extended_cols = {
        let mut cols = base_cols.clone();
        cols.push(simple_col_def("phone", DataType::Varchar(Some(20))));
        cols
    };

    let old = vec![make_create_table("users", base_cols)];
    let new = vec![make_create_table("users", extended_cols)];

    let diff = diff_schemas(&old, &new);

    assert_eq!(diff.operations.len(), 1, "expected exactly one diff op");
    match &diff.operations[0] {
        DiffOp::AddColumn { table, column } => {
            assert_eq!(table, "users");
            assert_eq!(column.name.value, "phone");
            assert_eq!(column.data_type, DataType::Varchar(Some(20)));
        }
        other => panic!("expected AddColumn, got {:?}", other),
    }
}

// ── diff_no_change ────────────────────────────────────────────────────────────

#[test]
fn diff_no_change() {
    let schema = vec![make_create_table(
        "products",
        vec![
            simple_col_def("id", DataType::Uuid),
            simple_col_def("title", DataType::Text),
            simple_col_def(
                "price",
                DataType::Numeric {
                    precision: Some(10),
                    scale: Some(2),
                },
            ),
        ],
    )];

    let diff = diff_schemas(&schema, &schema);

    assert!(
        diff.operations.is_empty(),
        "expected empty diff for identical schemas, got {:?}",
        diff.operations
    );
}

// ── diff drops table ──────────────────────────────────────────────────────────

#[test]
fn diff_drops_table() {
    let old = vec![make_create_table(
        "legacy",
        vec![simple_col_def("id", DataType::Integer)],
    )];
    let new: Vec<Statement> = vec![];

    let diff = diff_schemas(&old, &new);

    assert_eq!(diff.operations.len(), 1);
    match &diff.operations[0] {
        DiffOp::DropTable { name } => assert_eq!(name, "legacy"),
        other => panic!("expected DropTable, got {:?}", other),
    }
}

// ── diff alters column type ───────────────────────────────────────────────────

#[test]
fn diff_alters_column_type() {
    let old = vec![make_create_table(
        "items",
        vec![simple_col_def("quantity", DataType::Integer)],
    )];
    let new = vec![make_create_table(
        "items",
        vec![simple_col_def("quantity", DataType::BigInt)],
    )];

    let diff = diff_schemas(&old, &new);

    assert_eq!(diff.operations.len(), 1);
    match &diff.operations[0] {
        DiffOp::AlterColumnType {
            table,
            column,
            new_type,
        } => {
            assert_eq!(table, "items");
            assert_eq!(column, "quantity");
            assert_eq!(*new_type, DataType::BigInt);
        }
        other => panic!("expected AlterColumnType, got {:?}", other),
    }
}

// ── MigrationDiff::to_sql ─────────────────────────────────────────────────────

#[test]
fn migration_diff_to_sql_create_table() {
    let old: Vec<Statement> = vec![];
    let new = vec![make_create_table(
        "tenants",
        vec![
            simple_col_def("id", DataType::Uuid),
            simple_col_def("slug", DataType::Text),
        ],
    )];

    let diff = diff_schemas(&old, &new);
    let sql = diff.to_sql();

    assert!(sql.contains("CREATE TABLE"), "expected CREATE TABLE in SQL");
    assert!(sql.contains("tenants"), "expected table name in SQL");
}
