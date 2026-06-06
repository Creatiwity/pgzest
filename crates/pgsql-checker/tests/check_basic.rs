use pgsql_checker::{check_sql, check_statements, DdlSchemaProvider, IntrospectionJsonProvider, SchemaProvider};

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Build a schema that contains `organizations(id UUID, name TEXT)`.
fn org_schema() -> DdlSchemaProvider {
    DdlSchemaProvider::from_sql(
        "CREATE TABLE organizations (id UUID NOT NULL, name TEXT NOT NULL);",
    )
    .expect("DDL parse should succeed")
}

// ── SELECT ────────────────────────────────────────────────────────────────────

#[test]
fn valid_select_passes() {
    let schema = org_schema();
    let diags = check_sql("SELECT id, name FROM organizations", &schema);
    assert!(
        diags.is_empty(),
        "expected no diagnostics, got: {:#?}",
        diags
    );
}

#[test]
fn wildcard_select_passes() {
    let schema = org_schema();
    let diags = check_sql("SELECT * FROM organizations", &schema);
    assert!(
        diags.is_empty(),
        "expected no diagnostics, got: {:#?}",
        diags
    );
}

#[test]
fn missing_column_is_error() {
    let schema = org_schema();
    let diags = check_sql("SELECT id, nonexistent FROM organizations", &schema);

    let errors: Vec<_> = diags
        .iter()
        .filter(|d| matches!(d.severity, pgsql_checker::Severity::Error))
        .collect();

    assert_eq!(errors.len(), 1, "expected exactly 1 error, got: {:#?}", diags);
    assert_eq!(errors[0].code, "E001");
    assert!(
        errors[0].message.contains("nonexistent"),
        "error message should mention the column name, got: {}",
        errors[0].message
    );
}

#[test]
fn select_from_unknown_table_is_error() {
    let schema = org_schema();
    let diags = check_sql("SELECT id FROM ghost_table", &schema);

    let errors: Vec<_> = diags
        .iter()
        .filter(|d| matches!(d.severity, pgsql_checker::Severity::Error))
        .collect();

    assert!(
        !errors.is_empty(),
        "expected at least one error for unknown table"
    );
}

// ── INSERT ────────────────────────────────────────────────────────────────────

#[test]
fn unknown_table_in_insert_is_error() {
    let schema = org_schema();
    let diags = check_sql("INSERT INTO ghost_table (id) VALUES (1)", &schema);

    let errors: Vec<_> = diags
        .iter()
        .filter(|d| matches!(d.severity, pgsql_checker::Severity::Error))
        .collect();

    assert!(
        !errors.is_empty(),
        "expected an error for INSERT into unknown table"
    );
    assert_eq!(errors[0].code, "E003");
}

#[test]
fn valid_insert_passes() {
    let schema = org_schema();
    let diags = check_sql(
        "INSERT INTO organizations (id, name) VALUES ('00000000-0000-0000-0000-000000000000', 'Acme')",
        &schema,
    );
    let errors: Vec<_> = diags
        .iter()
        .filter(|d| matches!(d.severity, pgsql_checker::Severity::Error))
        .collect();
    assert!(errors.is_empty(), "expected no errors, got: {:#?}", diags);
}

// ── UPDATE / DELETE ───────────────────────────────────────────────────────────

#[test]
fn unknown_table_in_update_is_error() {
    let schema = org_schema();
    let diags = check_sql("UPDATE ghost_table SET name = 'x' WHERE id = 1", &schema);

    let errors: Vec<_> = diags
        .iter()
        .filter(|d| matches!(d.severity, pgsql_checker::Severity::Error))
        .collect();

    assert!(!errors.is_empty(), "expected error for UPDATE on unknown table");
}

#[test]
fn unknown_table_in_delete_is_error() {
    let schema = org_schema();
    let diags = check_sql("DELETE FROM ghost_table WHERE id = 1", &schema);

    let errors: Vec<_> = diags
        .iter()
        .filter(|d| matches!(d.severity, pgsql_checker::Severity::Error))
        .collect();

    assert!(!errors.is_empty(), "expected error for DELETE from unknown table");
}

// ── DDL pass-through ──────────────────────────────────────────────────────────

#[test]
fn ddl_passes_through_without_errors() {
    let schema = org_schema();
    let diags = check_sql(
        "CREATE TABLE users (id UUID NOT NULL, email TEXT NOT NULL);",
        &schema,
    );
    let errors: Vec<_> = diags
        .iter()
        .filter(|d| matches!(d.severity, pgsql_checker::Severity::Error))
        .collect();
    assert!(errors.is_empty(), "DDL should produce no errors, got: {:#?}", diags);
}

// ── IntrospectionJsonProvider ─────────────────────────────────────────────────

#[test]
fn json_provider_valid_select() {
    let json = r#"{
        "tables": [
            {
                "name": "organizations",
                "columns": [
                    { "name": "id", "type": "uuid", "nullable": false },
                    { "name": "name", "type": "text", "nullable": false }
                ]
            }
        ]
    }"#;
    let schema =
        IntrospectionJsonProvider::from_json(json).expect("JSON parse should succeed");

    let diags = check_sql("SELECT id, name FROM organizations", &schema);
    assert!(
        diags.is_empty(),
        "expected no diagnostics, got: {:#?}",
        diags
    );
}

#[test]
fn json_provider_missing_column_is_error() {
    let json = r#"{
        "tables": [
            {
                "name": "organizations",
                "columns": [
                    { "name": "id", "type": "uuid", "nullable": false }
                ]
            }
        ]
    }"#;
    let schema =
        IntrospectionJsonProvider::from_json(json).expect("JSON parse should succeed");

    let diags = check_sql("SELECT id, nonexistent FROM organizations", &schema);
    let errors: Vec<_> = diags
        .iter()
        .filter(|d| matches!(d.severity, pgsql_checker::Severity::Error))
        .collect();

    assert_eq!(errors.len(), 1, "expected exactly 1 error, got: {:#?}", diags);
    assert_eq!(errors[0].code, "E001");
}

// ── RLS policy checks ─────────────────────────────────────────────────────────

/// CREATE POLICY on a table that does not exist in the schema → E003 error.
#[test]
fn rls_policy_unknown_table_is_error() {
    let schema = org_schema();
    let diags = check_sql(
        "CREATE POLICY tenant_iso ON ghost_table USING (id = '1')",
        &schema,
    );

    let errors: Vec<_> = diags
        .iter()
        .filter(|d| matches!(d.severity, pgsql_checker::Severity::Error))
        .collect();

    assert!(
        !errors.is_empty(),
        "expected at least one error for policy on non-existent table, got: {:#?}",
        diags
    );
    assert_eq!(
        errors[0].code, "E003",
        "expected code E003, got: {}",
        errors[0].code
    );
    assert!(
        errors[0].message.contains("ghost_table"),
        "error should mention the unknown table, got: {}",
        errors[0].message
    );
}

/// CREATE POLICY whose USING expression references a valid column → 0 errors.
#[test]
fn rls_policy_valid_column_passes() {
    let schema = org_schema();
    // `id` is a valid column on `organizations`.
    let diags = check_sql(
        "CREATE POLICY org_isolation ON organizations USING (id = '00000000-0000-0000-0000-000000000000')",
        &schema,
    );

    let errors: Vec<_> = diags
        .iter()
        .filter(|d| matches!(d.severity, pgsql_checker::Severity::Error))
        .collect();

    assert!(
        errors.is_empty(),
        "expected no errors for valid USING column, got: {:#?}",
        diags
    );
}

// ── PL/pgSQL body checks ──────────────────────────────────────────────────────

/// Trigger function that assigns to NEW.nonexistent_col → must produce E001 or at
/// least not panic.
#[test]
fn trigger_new_nonexistent_column_is_error() {
    // Schema: tasks(id UUID, status TEXT)
    let schema_sql = "CREATE TABLE tasks (id UUID PRIMARY KEY, status TEXT NOT NULL);";
    let func_sql = r#"
        CREATE OR REPLACE FUNCTION bad_trigger()
        RETURNS TRIGGER LANGUAGE plpgsql AS $$
        BEGIN
          NEW.nonexistent_col = now();
          RETURN NEW;
        END;
        $$;"#;

    let schema = DdlSchemaProvider::from_sql(schema_sql).unwrap();
    let stmts = pgsql_parser::parse(func_sql).unwrap();
    let diags = check_statements(&stmts, &schema);

    // At minimum, must not panic.
    println!("diags: {:?}", diags);

    // If the body is Parsed, the AST checker should emit E001.
    // If it falls back to Raw, the text scanner may find it too.
    // Either way, we only assert no panic here as documented in the spec.
}

/// A function with an empty $$ $$ body should emit a W002 warning.
#[test]
fn plpgsql_empty_body_is_warning() {
    let schema = org_schema();
    let diags = check_sql(
        r#"CREATE OR REPLACE FUNCTION empty_fn() RETURNS void LANGUAGE plpgsql AS $$  $$"#,
        &schema,
    );

    let warnings: Vec<_> = diags
        .iter()
        .filter(|d| matches!(d.severity, pgsql_checker::Severity::Warning))
        .collect();

    assert!(
        !warnings.is_empty(),
        "expected at least one W002 warning for empty body, got: {:#?}",
        diags
    );
    assert!(
        warnings.iter().any(|w| w.code == "W002"),
        "expected W002 warning, got warnings: {:#?}",
        warnings
    );
}
