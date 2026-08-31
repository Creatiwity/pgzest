use pgsql_codegen::{check_migration_safety, diff_schemas};

fn parse(sql: &str) -> Vec<pgsql_ast::Statement> {
    pgsql_parser::parse(sql).expect("parse")
}

#[test]
fn m001_add_not_null_without_default() {
    let old = parse("CREATE TABLE tasks (id UUID PRIMARY KEY, status TEXT);");
    let new =
        parse("CREATE TABLE tasks (id UUID PRIMARY KEY, status TEXT, priority TEXT NOT NULL);");
    let diff = diff_schemas(&old, &new);
    let diags = check_migration_safety(&diff);
    assert!(
        diags.iter().any(|d| d.code == "M001"),
        "expected M001, got: {:?}",
        diags.iter().map(|d| &d.code).collect::<Vec<_>>()
    );
}

#[test]
fn m001_no_warn_with_default() {
    let old = parse("CREATE TABLE tasks (id UUID PRIMARY KEY, status TEXT);");
    let new = parse(
        "CREATE TABLE tasks (id UUID PRIMARY KEY, status TEXT, priority TEXT NOT NULL DEFAULT 'low');",
    );
    let diff = diff_schemas(&old, &new);
    let diags = check_migration_safety(&diff);
    assert!(
        !diags.iter().any(|d| d.code == "M001"),
        "M001 should not fire with DEFAULT, got: {:?}",
        diags.iter().map(|d| &d.code).collect::<Vec<_>>()
    );
}

#[test]
fn m002_drop_column_warns() {
    let old = parse("CREATE TABLE tasks (id UUID PRIMARY KEY, title TEXT, description TEXT);");
    let new = parse("CREATE TABLE tasks (id UUID PRIMARY KEY, title TEXT);");
    let diff = diff_schemas(&old, &new);
    let diags = check_migration_safety(&diff);
    assert!(
        diags.iter().any(|d| d.code == "M002"),
        "expected M002, got: {:?}",
        diags.iter().map(|d| &d.code).collect::<Vec<_>>()
    );
}

#[test]
fn m003_drop_table_warns() {
    let old = parse("CREATE TABLE logs (id UUID PRIMARY KEY, msg TEXT);");
    let new = parse("");
    let diff = diff_schemas(&old, &new);
    let diags = check_migration_safety(&diff);
    assert!(
        diags.iter().any(|d| d.code == "M003"),
        "expected M003, got: {:?}",
        diags.iter().map(|d| &d.code).collect::<Vec<_>>()
    );
}
