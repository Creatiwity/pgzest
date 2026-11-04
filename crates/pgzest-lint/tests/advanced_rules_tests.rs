use pgzest_lint::{LintConfig, lint_statements};

fn parse(sql: &str) -> Vec<pgzest_ast::stmt::Statement> {
    pgzest_parser::parse(sql).expect("parse")
}

fn lint(sql: &str) -> Vec<String> {
    let stmts = parse(sql);
    lint_statements(&stmts, &LintConfig::default())
        .diagnostics
        .iter()
        .map(|d| format!("{}: {}", d.code, d.message))
        .collect()
}

#[test]
fn l005_fk_without_index() {
    let sql = "
        CREATE TABLE projects (id UUID PRIMARY KEY);
        CREATE TABLE tasks (
            id UUID PRIMARY KEY,
            project_id UUID NOT NULL REFERENCES projects(id)
        );
    ";
    let diags = lint(sql);
    assert!(
        diags.iter().any(|d| d.starts_with("L005")),
        "expected L005, got: {:?}",
        diags
    );
}

#[test]
fn l005_fk_with_index_no_warn() {
    let sql = "
        CREATE TABLE projects (id UUID PRIMARY KEY);
        CREATE TABLE tasks (
            id UUID PRIMARY KEY,
            project_id UUID NOT NULL REFERENCES projects(id)
        );
        CREATE INDEX ON tasks(project_id);
    ";
    let diags = lint(sql);
    assert!(
        !diags.iter().any(|d| d.starts_with("L005")),
        "L005 should not fire when index exists, got: {:?}",
        diags
    );
}

#[test]
fn l006_leading_wildcard() {
    let sql = "
        CREATE TABLE users (email TEXT);
        SELECT email FROM users WHERE email LIKE '%@example.com';
    ";
    let diags = lint(sql);
    assert!(
        diags.iter().any(|d| d.starts_with("L006")),
        "expected L006, got: {:?}",
        diags
    );
}

#[test]
fn l006_trailing_wildcard_no_warn() {
    let sql = "
        CREATE TABLE users (email TEXT);
        SELECT email FROM users WHERE email LIKE 'admin%';
    ";
    let diags = lint(sql);
    assert!(
        !diags.iter().any(|d| d.starts_with("L006")),
        "L006 should not fire on trailing wildcard, got: {:?}",
        diags
    );
}

#[test]
fn l008_add_not_null_no_default() {
    let sql = "ALTER TABLE tasks ADD COLUMN priority TEXT NOT NULL;";
    let diags = lint(sql);
    assert!(
        diags.iter().any(|d| d.starts_with("L008")),
        "expected L008, got: {:?}",
        diags
    );
}

#[test]
fn l008_add_not_null_with_default_no_warn() {
    let sql = "ALTER TABLE tasks ADD COLUMN priority TEXT NOT NULL DEFAULT 'medium';";
    let diags = lint(sql);
    assert!(
        !diags.iter().any(|d| d.starts_with("L008")),
        "L008 should not fire when DEFAULT is provided, got: {:?}",
        diags
    );
}
