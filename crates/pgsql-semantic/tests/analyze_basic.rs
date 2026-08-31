use pgsql_schema::provider::DdlSchemaProvider;
use pgsql_semantic::analyze;

#[test]
fn analyze_simple_select() {
    let schema_sql = "
        CREATE TABLE users (
            id UUID PRIMARY KEY,
            email TEXT NOT NULL,
            created_at TIMESTAMPTZ
        );
    ";
    let schema = DdlSchemaProvider::from_sql(schema_sql).expect("parse schema");

    let sql = "SELECT id, email FROM users WHERE id = id;";
    let stmts = pgsql_parser::parse(sql).expect("parse sql");
    let result = analyze(&stmts, &schema);

    // Should have output columns for the SELECT
    assert!(
        !result.select_columns.is_empty(),
        "expected output columns for SELECT"
    );

    // The root scope should have columns from 'users'
    let scope_id = pgsql_semantic::scope::ScopeId(1); // first real scope
    let cols = result.scopes.available_columns(scope_id);
    assert!(
        !cols.is_empty(),
        "scope should have columns from users table"
    );
}

#[test]
fn analyze_registers_function() {
    let schema_sql = "
        CREATE TABLE tasks (id UUID PRIMARY KEY, status TEXT);
        CREATE OR REPLACE FUNCTION get_open_tasks()
        RETURNS SETOF UUID
        LANGUAGE sql AS $$ SELECT id FROM tasks WHERE status = 'open' $$;
    ";
    let schema = DdlSchemaProvider::from_sql(schema_sql).expect("parse schema");
    let stmts = pgsql_parser::parse(schema_sql).expect("parse stmts");
    let result = analyze(&stmts, &schema);

    // Function should be registered in symbol table
    assert!(
        result.symbols.get_function("get_open_tasks").is_some(),
        "get_open_tasks should be in symbol table"
    );
}

#[test]
fn output_columns_for_schema_sql() {
    // Test on a realistic query from examples/schema.sql
    let schema_sql = "
        CREATE TABLE tasks (
            id UUID PRIMARY KEY,
            status TEXT NOT NULL DEFAULT 'open',
            project_id UUID NOT NULL
        );
    ";
    let schema = DdlSchemaProvider::from_sql(schema_sql).expect("parse schema");
    let sql = "SELECT COUNT(*) AS total_tasks, COUNT(*) FILTER (WHERE status = 'done') AS done_tasks FROM tasks;";
    let stmts = pgsql_parser::parse(sql).expect("parse");
    let result = analyze(&stmts, &schema);

    // Should have 2 output columns
    let cols: Vec<_> = result.select_columns.values().flatten().collect();
    // total_tasks + done_tasks
    let names: Vec<_> = cols.iter().filter_map(|c| c.name.as_deref()).collect();
    assert!(
        names.contains(&"total_tasks"),
        "expected total_tasks column, got {:?}",
        names
    );
    assert!(
        names.contains(&"done_tasks"),
        "expected done_tasks column, got {:?}",
        names
    );
}
