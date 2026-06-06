use pgsql_parser::parse;

const SCHEMA_SQL: &str = include_str!("../../../examples/schema.sql");

/// Parse just the supported subset of the schema (no CREATE EXTENSION).
#[test]
fn parse_schema_supported_subset() {
    let supported_sql = r#"
        CREATE TABLE organizations (
          id          UUID        PRIMARY KEY DEFAULT uuid_generate_v4(),
          name        TEXT        NOT NULL,
          slug        TEXT        NOT NULL UNIQUE,
          created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
          updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
        );

        CREATE TABLE users (
          id           UUID        PRIMARY KEY DEFAULT uuid_generate_v4(),
          email        TEXT        NOT NULL UNIQUE,
          display_name TEXT        NOT NULL,
          created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
          updated_at   TIMESTAMPTZ NOT NULL DEFAULT now()
        );

        CREATE TABLE memberships (
          user_id   UUID  NOT NULL REFERENCES users(id)         ON DELETE CASCADE,
          org_id    UUID  NOT NULL REFERENCES organizations(id)  ON DELETE CASCADE,
          role      TEXT  NOT NULL CHECK (role IN ('owner', 'admin', 'member')),
          joined_at TIMESTAMPTZ NOT NULL DEFAULT now(),
          PRIMARY KEY (user_id, org_id)
        );

        CREATE INDEX tasks_project_id_idx   ON tasks(project_id);
        CREATE INDEX tasks_assigned_to_idx  ON tasks(assigned_to);

        ALTER TABLE organizations ENABLE ROW LEVEL SECURITY;
        ALTER TABLE memberships   ENABLE ROW LEVEL SECURITY;

        CREATE POLICY orgs_select ON organizations
          FOR SELECT
          USING (id IN (SELECT current_user_org_ids()));

        CREATE POLICY memberships_select ON memberships
          FOR SELECT
          USING (org_id IN (SELECT current_user_org_ids()));

        SELECT id, name FROM organizations WHERE id = '123';
    "#;

    let stmts = parse(supported_sql).expect("supported SQL should parse without errors");
    assert!(
        !stmts.is_empty(),
        "expected at least 1 statement, got 0"
    );
    println!("parsed {} statements from supported subset", stmts.len());
    println!("first statement type: {:?}", std::mem::discriminant(&stmts[0]));
}

/// Parse the full schema statement-by-statement (splitting on semicolons at the
/// top-level). We use pgsql_parser::parse() on each individual statement SQL text
/// so that failures in one statement do not affect others.
/// Note: some schema statements use RETURNS SETOF which sqlparser 0.54 does not
/// support; those are counted as parse-level failures and are expected.
#[test]
fn parse_full_schema_counts_statements() {
    use pgsql_parser::ParseError;

    // Simple split on top-level semicolons. We don't need to be perfect here —
    // the goal is to count how many statements convert OK.
    let stmts_sql: Vec<&str> = SCHEMA_SQL
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.starts_with("--"))
        .collect();

    let total = stmts_sql.len();
    assert!(total > 0, "schema should have statements after splitting");
    println!("found {} candidate statements in schema.sql", total);

    let mut ok_count = 0usize;
    let mut unsupported_count = 0usize;
    let mut syntax_count = 0usize;

    for sql in &stmts_sql {
        // Add semicolon back for the parser
        let full = format!("{};", sql);
        match parse(&full) {
            Ok(converted) if !converted.is_empty() => {
                ok_count += 1;
            }
            Ok(_) => {
                // parser produced no statements (e.g. comment-only)
                syntax_count += 1;
            }
            Err(ParseError::Unsupported(_)) => {
                unsupported_count += 1;
            }
            Err(ParseError::Syntax(_)) => {
                // sqlparser couldn't parse this statement at all (e.g. SETOF)
                syntax_count += 1;
            }
        }
    }

    println!(
        "conversion results: {} ok, {} unsupported, {} syntax/empty",
        ok_count, unsupported_count, syntax_count
    );

    assert!(
        ok_count > 0,
        "expected at least one successfully converted statement, got 0 out of {}",
        total
    );
}

/// The target SQL from the spec: RETURNS SETOF UUID + SECURITY DEFINER must parse
/// without error and produce the correct AST fields.
#[test]
fn parse_create_function_setof_security_definer() {
    use pgsql_ast::{
        ddl::{DdlStatement, ReturnType},
        types::DataType,
        Statement,
    };

    let sql = r#"
        CREATE OR REPLACE FUNCTION current_user_org_ids()
        RETURNS SETOF UUID
        LANGUAGE sql STABLE SECURITY DEFINER AS $$
          SELECT org_id FROM memberships WHERE user_id = auth.uid()
        $$;
    "#;

    let stmts = parse(sql).expect("should parse SETOF UUID + SECURITY DEFINER without error");
    assert_eq!(stmts.len(), 1, "expected exactly one statement");

    match &stmts[0] {
        Statement::Ddl(DdlStatement::CreateFunction(cf)) => {
            assert!(cf.security_definer, "security_definer should be true");
            assert!(cf.or_replace, "or_replace should be true");
            assert!(matches!(
                &cf.return_type,
                ReturnType::SetOf(DataType::Uuid)
            ), "return_type should be SetOf(Uuid), got {:?}", cf.return_type);
        }
        other => panic!("expected CreateFunction, got {:?}", other),
    }
}

/// Test COST n and ROWS n extraction.
#[test]
fn parse_create_function_cost_rows() {
    use pgsql_ast::{
        ddl::DdlStatement,
        Statement,
    };

    let sql = r#"
        CREATE OR REPLACE FUNCTION my_fn()
        RETURNS INTEGER
        LANGUAGE sql STABLE COST 100 ROWS 1000 AS $$
          SELECT 42
        $$;
    "#;

    let stmts = parse(sql).expect("should parse COST and ROWS without error");
    assert_eq!(stmts.len(), 1);

    match &stmts[0] {
        Statement::Ddl(DdlStatement::CreateFunction(cf)) => {
            assert_eq!(cf.cost, Some(100), "cost should be 100");
            assert_eq!(cf.rows, Some(1000), "rows should be 1000");
        }
        other => panic!("expected CreateFunction, got {:?}", other),
    }
}
