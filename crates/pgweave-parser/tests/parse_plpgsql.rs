use pgweave_ast::{
    Statement,
    ddl::{DdlStatement, FunctionBody},
    plpgsql::PlExpr,
};
use pgweave_parser::parse;

const SCHEMA: &str = include_str!("../../../examples/schema.sql");

/// Parse the full schema.sql and count FunctionBody::Parsed vs FunctionBody::Raw.
/// All PL/pgSQL functions and procedures should parse to Parsed (not Raw).
#[test]
fn parse_full_schema_plpgsql_functions() {
    let stmts = parse(SCHEMA).expect("schema.sql should parse without fatal errors");

    let mut parsed_count = 0usize;
    let mut raw_count = 0usize;
    let mut sql_query_count = 0usize;

    for stmt in &stmts {
        let body = match stmt {
            Statement::Ddl(DdlStatement::CreateFunction(f)) => Some((&f.body, &f.language)),
            Statement::Ddl(DdlStatement::CreateProcedure(p)) => Some((&p.body, &p.language)),
            _ => None,
        };
        if let Some((body, _lang)) = body {
            match body {
                FunctionBody::Parsed(_) => parsed_count += 1,
                FunctionBody::Raw(_) => raw_count += 1,
                FunctionBody::SqlQuery(_) | FunctionBody::Sql(_) | FunctionBody::SqlReturn(_) => {
                    sql_query_count += 1
                }
            }
        }
    }

    println!(
        "schema.sql function bodies: {} Parsed, {} Raw, {} SqlQuery",
        parsed_count, raw_count, sql_query_count
    );

    // The schema has:
    //   - set_updated_at()                    → plpgsql → Parsed
    //   - enforce_task_status_transition()     → plpgsql → Parsed
    //   - current_user_org_ids()              → sql     → SqlQuery
    //   - assign_task() (procedure)            → plpgsql → Parsed
    //   - project_summary()                   → sql     → SqlQuery
    // Expect at least 3 Parsed (the two trigger functions + procedure)
    assert!(
        parsed_count >= 3,
        "expected at least 3 Parsed PL/pgSQL bodies, got {} (raw={}, sqlquery={})",
        parsed_count,
        raw_count,
        sql_query_count
    );
    // Expect 0 Raw (graceful degradation should not be needed for these functions)
    assert_eq!(
        raw_count, 0,
        "expected 0 Raw bodies (all PL/pgSQL should parse), got {}",
        raw_count
    );
    // Expect at least 2 SqlQuery (the two sql-language functions)
    assert!(
        sql_query_count >= 2,
        "expected at least 2 SqlQuery bodies, got {}",
        sql_query_count
    );
}

#[test]
fn parse_if_not_exists_subquery() {
    // The expression NOT EXISTS (...) must NOT produce a PlExpr::Var
    let sql = r#"
        CREATE OR REPLACE PROCEDURE test_proc(p_id UUID)
        LANGUAGE plpgsql AS $$
        BEGIN
          IF NOT EXISTS (SELECT 1 FROM memberships WHERE user_id = p_id) THEN
            RAISE EXCEPTION 'not found';
          END IF;
        END;
        $$;"#;
    let stmts = pgweave_parser::parse(sql).expect("parse failed");
    match &stmts[0] {
        Statement::Ddl(DdlStatement::CreateProcedure(p)) => {
            if let FunctionBody::Parsed(body) = &p.body {
                use pgweave_ast::plpgsql::PlStatement;
                if let Some(PlStatement::If { condition, .. }) = body.block.statements.first() {
                    assert!(
                        !matches!(condition, PlExpr::Var(v) if v.value.contains("EXISTS")),
                        "NOT EXISTS incorrectly parsed as variable: {:?}",
                        condition
                    );
                }
            }
        }
        _ => panic!("expected procedure"),
    }
}

fn get_function_body(sql: &str) -> FunctionBody {
    let stmts = parse(sql).expect("parse failed");
    match &stmts[0] {
        Statement::Ddl(DdlStatement::CreateFunction(f)) => f.body.clone(),
        _ => panic!("not a function"),
    }
}

#[test]
fn parse_set_updated_at() {
    let sql = r#"
        CREATE OR REPLACE FUNCTION set_updated_at()
        RETURNS TRIGGER LANGUAGE plpgsql AS $$
        BEGIN
          NEW.updated_at = now();
          RETURN NEW;
        END;
        $$;"#;
    let body = get_function_body(sql);
    assert!(
        matches!(body, FunctionBody::Parsed(_)),
        "expected Parsed, got {:?}",
        body
    );
    if let FunctionBody::Parsed(b) = body {
        assert_eq!(b.declare.len(), 0);
        assert_eq!(b.block.statements.len(), 2);
    }
}

#[test]
fn parse_if_raise() {
    let sql = r#"
        CREATE OR REPLACE FUNCTION enforce_task_status_transition()
        RETURNS TRIGGER LANGUAGE plpgsql AS $$
        BEGIN
          IF OLD.status = 'cancelled' AND NEW.status != 'cancelled' THEN
            RAISE EXCEPTION 'Cannot reopen a cancelled task (id=%)', OLD.id;
          END IF;
          RETURN NEW;
        END;
        $$;"#;
    let body = get_function_body(sql);
    assert!(
        matches!(body, FunctionBody::Parsed(_)),
        "expected Parsed, got {:?}",
        body
    );
}

#[test]
fn parse_declare_select_into() {
    let sql = r#"
        CREATE OR REPLACE PROCEDURE assign_task(p_task_id UUID, p_user_id UUID)
        LANGUAGE plpgsql AS $$
        DECLARE
          v_org_id UUID;
        BEGIN
          SELECT p.org_id INTO v_org_id
          FROM tasks t JOIN projects p ON p.id = t.project_id
          WHERE t.id = p_task_id;
          IF NOT FOUND THEN
            RAISE EXCEPTION 'Task not found: %', p_task_id;
          END IF;
        END;
        $$;"#;
    let stmts = parse(sql).expect("parse failed");
    match &stmts[0] {
        Statement::Ddl(DdlStatement::CreateProcedure(p)) => {
            assert!(
                matches!(p.body, FunctionBody::Parsed(_)),
                "expected Parsed, got {:?}",
                p.body
            );
        }
        _ => panic!("not a procedure"),
    }
}
