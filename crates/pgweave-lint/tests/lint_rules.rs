use pgweave_ast::ddl::{ColumnDef, CreateTable, DdlStatement, TableConstraint};
use pgweave_ast::expr::{Ident, ObjectName};
use pgweave_ast::node::{NodeId, Span};
use pgweave_ast::stmt::{SelectItem, SelectStmt, Statement};
use pgweave_ast::types::DataType;
use pgweave_diagnostics::Severity;
use pgweave_lint::config::LintConfig;
use pgweave_lint::lint_statements;

// ── helpers ──────────────────────────────────────────────────────────────────

fn empty_select(columns: Vec<SelectItem>) -> Statement {
    Statement::Select(SelectStmt {
        id: NodeId::DUMMY,
        span: Span::DUMMY,
        with: vec![],
        distinct: false,
        columns,
        from: vec![],
        joins: vec![],
        where_clause: None,
        group_by: vec![],
        having: None,
        order_by: vec![],
        limit: None,
        offset: None,
    })
}

fn create_table(
    name: &str,
    columns: Vec<ColumnDef>,
    constraints: Vec<TableConstraint>,
) -> Statement {
    Statement::Ddl(DdlStatement::CreateTable(CreateTable {
        id: NodeId::DUMMY,
        span: Span::DUMMY,
        name: ObjectName::unquoted(&[name]),
        if_not_exists: false,
        temporary: false,
        columns,
        constraints,
    }))
}

fn quoted_table(name: &str) -> Statement {
    Statement::Ddl(DdlStatement::CreateTable(CreateTable {
        id: NodeId::DUMMY,
        span: Span::DUMMY,
        name: ObjectName(vec![Ident {
            value: name.to_string(),
            quoted: true,
        }]),
        if_not_exists: false,
        temporary: false,
        columns: vec![],
        constraints: vec![TableConstraint::PrimaryKey { columns: vec![] }],
    }))
}

fn simple_col(name: &str) -> ColumnDef {
    ColumnDef {
        name: Ident::new(name),
        data_type: DataType::Text,
        nullable: true,
        default: None,
        constraints: vec![],
    }
}

fn col_codes(diags: &[pgweave_diagnostics::Diagnostic]) -> Vec<&str> {
    diags.iter().map(|d| d.code).collect()
}

// ── L001 · no_select_star ─────────────────────────────────────────────────────

#[test]
fn select_star_triggers_warning() {
    let stmt = empty_select(vec![SelectItem::Wildcard]);
    let config = LintConfig::default();
    let result = lint_statements(&[stmt], &config);
    let codes = col_codes(&result.diagnostics);
    assert!(codes.contains(&"L001"), "expected L001, got: {codes:?}");
    // Should be a warning, not an error
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.code == "L001" && matches!(d.severity, Severity::Warning))
    );
}

#[test]
fn named_columns_no_warning() {
    use pgweave_ast::expr::{ColumnRef, Expr, ExprKind};
    let col_expr = SelectItem::Expr {
        expr: Expr::bare(ExprKind::Column(ColumnRef {
            table: None,
            column: Ident::new("id"),
        })),
        alias: None,
    };
    let stmt = empty_select(vec![col_expr]);
    let config = LintConfig::default();
    let result = lint_statements(&[stmt], &config);
    let l001: Vec<_> = result
        .diagnostics
        .iter()
        .filter(|d| d.code == "L001")
        .collect();
    assert!(
        l001.is_empty(),
        "expected no L001 diagnostics, got: {l001:?}"
    );
}

#[test]
fn qualified_wildcard_triggers_warning() {
    let stmt = empty_select(vec![SelectItem::QualifiedWildcard(Ident::new("t"))]);
    let config = LintConfig::default();
    let result = lint_statements(&[stmt], &config);
    let codes = col_codes(&result.diagnostics);
    assert!(
        codes.contains(&"L001"),
        "expected L001 for qualified wildcard, got: {codes:?}"
    );
}

// ── L002 · require_pk ────────────────────────────────────────────────────────

#[test]
fn table_without_pk_triggers_warning() {
    let stmt = create_table("no_pk", vec![simple_col("id")], vec![]);
    let config = LintConfig::default();
    let result = lint_statements(&[stmt], &config);
    let codes = col_codes(&result.diagnostics);
    assert!(codes.contains(&"L002"), "expected L002, got: {codes:?}");
}

#[test]
fn table_with_table_pk_no_warning() {
    let pk = TableConstraint::PrimaryKey {
        columns: vec![Ident::new("id")],
    };
    let stmt = create_table("with_pk", vec![simple_col("id")], vec![pk]);
    let config = LintConfig::default();
    let result = lint_statements(&[stmt], &config);
    let l002: Vec<_> = result
        .diagnostics
        .iter()
        .filter(|d| d.code == "L002")
        .collect();
    assert!(l002.is_empty(), "expected no L002, got: {l002:?}");
}

#[test]
fn table_with_column_pk_no_warning() {
    use pgweave_ast::ddl::ColumnConstraint;
    let mut col = simple_col("id");
    col.constraints.push(ColumnConstraint::PrimaryKey);
    let stmt = create_table("with_col_pk", vec![col], vec![]);
    let config = LintConfig::default();
    let result = lint_statements(&[stmt], &config);
    let l002: Vec<_> = result
        .diagnostics
        .iter()
        .filter(|d| d.code == "L002")
        .collect();
    assert!(l002.is_empty(), "expected no L002, got: {l002:?}");
}

// ── L003 · naming ────────────────────────────────────────────────────────────

#[test]
fn non_snake_case_triggers_warning() {
    // "MyTable" — mixed case, should trigger L003
    let stmt = quoted_table("MyTable");
    let config = LintConfig::default();
    let result = lint_statements(&[stmt], &config);
    let codes = col_codes(&result.diagnostics);
    assert!(
        codes.contains(&"L003"),
        "expected L003 for 'MyTable', got: {codes:?}"
    );
}

#[test]
fn snake_case_table_no_warning() {
    let pk = TableConstraint::PrimaryKey {
        columns: vec![Ident::new("id")],
    };
    let stmt = create_table("my_table", vec![], vec![pk]);
    let config = LintConfig::default();
    let result = lint_statements(&[stmt], &config);
    let l003: Vec<_> = result
        .diagnostics
        .iter()
        .filter(|d| d.code == "L003")
        .collect();
    assert!(
        l003.is_empty(),
        "expected no L003 for 'my_table', got: {l003:?}"
    );
}

#[test]
fn non_snake_case_column_triggers_warning() {
    let pk = TableConstraint::PrimaryKey {
        columns: vec![Ident::new("id")],
    };
    let bad_col = ColumnDef {
        name: Ident {
            value: "FirstName".to_string(),
            quoted: true,
        },
        data_type: DataType::Text,
        nullable: true,
        default: None,
        constraints: vec![],
    };
    let stmt = create_table("users", vec![bad_col], vec![pk]);
    let config = LintConfig::default();
    let result = lint_statements(&[stmt], &config);
    let l003: Vec<_> = result
        .diagnostics
        .iter()
        .filter(|d| d.code == "L003")
        .collect();
    assert!(
        !l003.is_empty(),
        "expected L003 for column 'FirstName', got no warnings"
    );
}

// ── L004 · no_not_in ─────────────────────────────────────────────────────────

#[test]
fn not_in_subquery_triggers_warning() {
    use pgweave_ast::expr::{ColumnRef, Expr, ExprKind};
    let subquery = Box::new(SelectStmt {
        id: NodeId::DUMMY,
        span: Span::DUMMY,
        with: vec![],
        distinct: false,
        columns: vec![SelectItem::Expr {
            expr: Expr::bare(ExprKind::Column(ColumnRef {
                table: None,
                column: Ident::new("id"),
            })),
            alias: None,
        }],
        from: vec![],
        joins: vec![],
        where_clause: None,
        group_by: vec![],
        having: None,
        order_by: vec![],
        limit: None,
        offset: None,
    });

    let where_clause = Expr::bare(ExprKind::InSubquery {
        expr: Box::new(Expr::bare(ExprKind::Column(ColumnRef {
            table: None,
            column: Ident::new("user_id"),
        }))),
        subquery,
        negated: true,
    });

    let stmt = Statement::Select(SelectStmt {
        id: NodeId::DUMMY,
        span: Span::DUMMY,
        with: vec![],
        distinct: false,
        columns: vec![SelectItem::Wildcard],
        from: vec![],
        joins: vec![],
        where_clause: Some(where_clause),
        group_by: vec![],
        having: None,
        order_by: vec![],
        limit: None,
        offset: None,
    });

    let config = LintConfig::default();
    let result = lint_statements(&[stmt], &config);
    let codes = col_codes(&result.diagnostics);
    assert!(codes.contains(&"L004"), "expected L004, got: {codes:?}");
}

// ── rule-level off ────────────────────────────────────────────────────────────

#[test]
fn rule_off_suppresses_diagnostic() {
    use pgweave_lint::config::RuleLevel;

    let stmt = empty_select(vec![SelectItem::Wildcard]);
    let mut config = LintConfig::default();
    config.rules.no_select_star = RuleLevel::Off;

    let result = lint_statements(&[stmt], &config);
    let l001: Vec<_> = result
        .diagnostics
        .iter()
        .filter(|d| d.code == "L001")
        .collect();
    assert!(
        l001.is_empty(),
        "L001 should be suppressed when rule is Off"
    );
}
