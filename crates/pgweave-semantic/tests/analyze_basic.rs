use pgweave_catalog::ProjectCatalog;
use pgweave_semantic::analyze;

#[test]
fn analyze_simple_select() {
    let schema_sql = "
        CREATE TABLE users (
            id UUID PRIMARY KEY,
            email TEXT NOT NULL,
            created_at TIMESTAMPTZ
        );
    ";
    let schema = ProjectCatalog::from_sql(schema_sql).expect("parse schema");

    let sql = "SELECT id, email FROM users WHERE id = id;";
    let stmts = pgweave_parser::parse(sql).expect("parse sql");
    let result = analyze(&stmts, &schema);

    // Should have output columns for the SELECT
    assert!(
        !result.select_columns.is_empty(),
        "expected output columns for SELECT"
    );

    // The root scope should have columns from 'users'
    let scope_id = pgweave_semantic::scope::ScopeId(1); // first real scope
    let cols = result.scopes.available_columns(scope_id);
    assert!(
        !cols.is_empty(),
        "scope should have columns from users table"
    );
}

/// Noms des colonnes de sortie de l'unique SELECT de `sql`.
fn star_columns(schema_sql: &str, sql: &str) -> Vec<String> {
    let schema = ProjectCatalog::from_sql(schema_sql).expect("parse schema");
    let stmts = pgweave_parser::parse(sql).expect("parse sql");
    let result = analyze(&stmts, &schema);
    let pgweave_ast::Statement::Select(sel) = &stmts[0] else {
        panic!("SELECT attendu")
    };
    result.select_columns[&sel.id]
        .iter()
        .map(|c| c.name.clone().unwrap_or_default())
        .collect()
}

const TWO_TABLES: &str = "
    CREATE TABLE authors (id UUID, name TEXT);
    CREATE TABLE pages (id BIGINT, title TEXT, author_id UUID);
";

#[test]
fn star_suit_l_ordre_de_postgresql_sans_doublon() {
    // Régression (oracle) : l'expansion suivait l'ordre d'une HashMap — un
    // résultat différent à chaque exécution, avec des doublons.
    for _ in 0..20 {
        assert_eq!(
            star_columns(
                TWO_TABLES,
                "SELECT * FROM pages p JOIN authors a ON a.id = p.author_id;"
            ),
            ["id", "title", "author_id", "id", "name"],
            "relations dans l'ordre du FROM, colonnes dans l'ordre de déclaration"
        );
    }
}

#[test]
fn star_qualifie_ne_prend_que_sa_relation() {
    assert_eq!(
        star_columns(
            TWO_TABLES,
            "SELECT a.* FROM pages p JOIN authors a ON a.id = p.author_id;"
        ),
        ["id", "name"]
    );
    assert_eq!(
        star_columns(TWO_TABLES, "SELECT pages.* FROM pages;"),
        ["id", "title", "author_id"]
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
    let schema = ProjectCatalog::from_sql(schema_sql).expect("parse schema");
    let stmts = pgweave_parser::parse(schema_sql).expect("parse stmts");
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
    let schema = ProjectCatalog::from_sql(schema_sql).expect("parse schema");
    let sql = "SELECT COUNT(*) AS total_tasks, COUNT(*) FILTER (WHERE status = 'done') AS done_tasks FROM tasks;";
    let stmts = pgweave_parser::parse(sql).expect("parse");
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
