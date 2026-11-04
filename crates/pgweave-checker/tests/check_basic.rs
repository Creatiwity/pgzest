use pgweave_catalog::ProjectCatalog;
use pgweave_checker::{check_sql, check_sql_self_contained};
use pgweave_diagnostics::codes;

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Build a schema that contains `organizations(id UUID, name TEXT)`.
fn org_schema() -> ProjectCatalog {
    ProjectCatalog::from_sql("CREATE TABLE organizations (id UUID NOT NULL, name TEXT NOT NULL);")
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
        .filter(|d| matches!(d.severity, pgweave_diagnostics::Severity::Error))
        .collect();

    assert_eq!(
        errors.len(),
        1,
        "expected exactly 1 error, got: {:#?}",
        diags
    );
    assert_eq!(errors[0].code, codes::COLUMN_NOT_FOUND);
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
        .filter(|d| matches!(d.severity, pgweave_diagnostics::Severity::Error))
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
        .filter(|d| matches!(d.severity, pgweave_diagnostics::Severity::Error))
        .collect();

    assert!(
        !errors.is_empty(),
        "expected an error for INSERT into unknown table"
    );
    assert_eq!(errors[0].code, codes::TABLE_NOT_FOUND);
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
        .filter(|d| matches!(d.severity, pgweave_diagnostics::Severity::Error))
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
        .filter(|d| matches!(d.severity, pgweave_diagnostics::Severity::Error))
        .collect();

    assert!(
        !errors.is_empty(),
        "expected error for UPDATE on unknown table"
    );
}

#[test]
fn unknown_table_in_delete_is_error() {
    let schema = org_schema();
    let diags = check_sql("DELETE FROM ghost_table WHERE id = 1", &schema);

    let errors: Vec<_> = diags
        .iter()
        .filter(|d| matches!(d.severity, pgweave_diagnostics::Severity::Error))
        .collect();

    assert!(
        !errors.is_empty(),
        "expected error for DELETE from unknown table"
    );
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
        .filter(|d| matches!(d.severity, pgweave_diagnostics::Severity::Error))
        .collect();
    assert!(
        errors.is_empty(),
        "DDL should produce no errors, got: {:#?}",
        diags
    );
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
        .filter(|d| matches!(d.severity, pgweave_diagnostics::Severity::Error))
        .collect();

    assert!(
        !errors.is_empty(),
        "expected at least one error for policy on non-existent table, got: {:#?}",
        diags
    );
    assert_eq!(
        errors[0].code,
        codes::TABLE_NOT_FOUND,
        "expected code table-not-found, got: {}",
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
        .filter(|d| matches!(d.severity, pgweave_diagnostics::Severity::Error))
        .collect();

    assert!(
        errors.is_empty(),
        "expected no errors for valid USING column, got: {:#?}",
        diags
    );
}

// ── PL/pgSQL body checks ──────────────────────────────────────────────────────

/// Trigger function assigning `NEW.nonexistent_col`: the field does not
/// exist on the trigger's table.
#[test]
fn trigger_new_nonexistent_column_is_error() {
    let sql = r#"
        CREATE TABLE tasks (id UUID PRIMARY KEY, status TEXT NOT NULL);
        CREATE OR REPLACE FUNCTION bad_trigger()
        RETURNS TRIGGER LANGUAGE plpgsql AS $$
        BEGIN
          NEW.nonexistent_col = now();
          RETURN NEW;
        END;
        $$;
        CREATE TRIGGER t BEFORE UPDATE ON tasks FOR EACH ROW EXECUTE FUNCTION bad_trigger();"#;
    let diags = check_sql_self_contained(sql);
    assert!(
        diags
            .iter()
            .any(|d| d.code == codes::COLUMN_NOT_FOUND && d.message.contains("nonexistent_col")),
        "{diags:#?}"
    );
}

// ── self-contained mode ───────────────────────────────────────────────────────

#[test]
fn self_contained_schema_resolves_policies() {
    // A single SQL file containing CREATE TABLE + CREATE POLICY.
    // The checker must resolve the table from the DDL of the same file.
    let sql = "
        CREATE TABLE users (id UUID PRIMARY KEY, email TEXT NOT NULL);
        CREATE POLICY users_select ON users
            FOR SELECT USING (id = id);
    ";
    let diags = check_sql_self_contained(sql);
    let errors: Vec<_> = diags
        .iter()
        .filter(|d| matches!(d.severity, pgweave_diagnostics::Severity::Error))
        .collect();
    assert!(errors.is_empty(), "unexpected errors: {:?}", errors);
}

/// Built-in PostgreSQL functions (COUNT, ROUND, NULLIF, now(), etc.) must not emit W001.
#[test]
fn builtin_functions_no_warning() {
    let sql = "
        CREATE TABLE metrics (value NUMERIC NOT NULL);
        SELECT COUNT(*), ROUND(AVG(value), 2), NULLIF(MAX(value), 0) FROM metrics;
    ";
    let diags = pgweave_checker::check_sql_self_contained(sql);
    let w001: Vec<_> = diags
        .iter()
        .filter(|d| d.code == codes::FUNCTION_NOT_FOUND)
        .collect();
    assert!(
        w001.is_empty(),
        "unexpected function-not-found for builtin functions: {:?}",
        w001
    );
}

/// INSERT into a temp table created within the same function body must not fire E003.
#[test]
fn plpgsql_temp_table_no_false_positive() {
    let schema = org_schema();
    let diags = check_sql(
        r#"
CREATE OR REPLACE FUNCTION fn_with_temp()
RETURNS void LANGUAGE plpgsql AS $$
DECLARE
  v_id UUID;
BEGIN
  CREATE TEMP TABLE tmp_results (id UUID, name TEXT);
  INSERT INTO tmp_results SELECT id, name FROM organizations;
END;
$$"#,
        &schema,
    );
    let e003: Vec<_> = diags
        .iter()
        .filter(|d| d.code == codes::TABLE_NOT_FOUND)
        .collect();
    assert!(
        e003.is_empty(),
        "unexpected table-not-found for temp table reference: {:#?}",
        e003
    );
}

/// SELECT inside a PL/pgSQL function that uses a declared variable in WHERE
/// must not fire E002 for that variable reference.
#[test]
fn plpgsql_plvar_in_select_no_false_positive() {
    let schema = org_schema();
    let diags = check_sql(
        r#"
CREATE OR REPLACE FUNCTION fn_with_param(p_id UUID)
RETURNS TEXT LANGUAGE plpgsql AS $$
DECLARE
  v_name TEXT;
BEGIN
  SELECT name INTO v_name FROM organizations WHERE id = p_id;
  RETURN v_name;
END;
$$"#,
        &schema,
    );
    let e002: Vec<_> = diags
        .iter()
        .filter(|d| d.code == "table-not-in-scope")
        .collect();
    assert!(
        e002.is_empty(),
        "unexpected table-not-in-scope for plvar reference: {:#?}",
        e002
    );
}

/// A function referencing a non-existent real table must still fire E003.
#[test]
fn plpgsql_unknown_real_table_fires_e003() {
    let schema = org_schema();
    let diags = check_sql(
        r#"
CREATE OR REPLACE FUNCTION fn_bad_table()
RETURNS void LANGUAGE plpgsql AS $$
BEGIN
  INSERT INTO no_such_table (id) VALUES (gen_random_uuid());
END;
$$"#,
        &schema,
    );
    assert!(
        diags.iter().any(|d| d.code == codes::TABLE_NOT_FOUND),
        "expected table-not-found for unknown table, got: {:#?}",
        diags
    );
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
        .filter(|d| matches!(d.severity, pgweave_diagnostics::Severity::Warning))
        .collect();

    assert!(
        !warnings.is_empty(),
        "expected at least one W002 warning for empty body, got: {:#?}",
        diags
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.code == codes::EMPTY_FUNCTION_BODY),
        "expected empty-function-body warning, got warnings: {:#?}",
        warnings
    );
}
