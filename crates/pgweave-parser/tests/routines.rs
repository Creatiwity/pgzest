//! Routine grammar (`grammar::routine`): PostgreSQL 18, without
//! text rewriting.

use pgweave_ast::Statement;
use pgweave_ast::ddl::{
    CreateFunction, CreateProcedure, DdlStatement, FunctionBody, FunctionLanguage, Parallel,
    ParamMode, ReturnType, SettingValue, Volatility,
};
use pgweave_ast::types::DataType;
use pgweave_parser::{SkipReason, parse_with_outcome};

fn one(sql: &str) -> Statement {
    let out = parse_with_outcome(sql).expect("parse");
    assert!(
        out.skipped.is_empty(),
        "unexpected skips: {:?}",
        out.skipped
    );
    assert_eq!(out.statements.len(), 1, "{sql}");
    out.statements.into_iter().next().unwrap()
}

fn function(sql: &str) -> CreateFunction {
    match one(sql) {
        Statement::Ddl(DdlStatement::CreateFunction(f)) => f,
        other => panic!("function expected: {other:?}"),
    }
}

fn procedure(sql: &str) -> CreateProcedure {
    match one(sql) {
        Statement::Ddl(DdlStatement::CreateProcedure(p)) => p,
        other => panic!("procedure expected: {other:?}"),
    }
}

#[test]
fn procedure_with_dollar_body() {
    let p = procedure(
        "CREATE OR REPLACE PROCEDURE reset_views(p_id BIGINT) LANGUAGE plpgsql AS $$
         BEGIN UPDATE pages SET views = 0 WHERE id = p_id; END $$",
    );
    assert!(p.or_replace);
    assert_eq!(p.params[0].name.as_ref().unwrap().value, "p_id");
    assert!(matches!(p.body, FunctionBody::Parsed(_)));
}

#[test]
fn options_in_any_order() {
    let f = function(
        "CREATE FUNCTION f() RETURNS int SET search_path = public, pg_temp COST 10
         SECURITY DEFINER LANGUAGE plpgsql STRICT PARALLEL SAFE ROWS 5 STABLE LEAKPROOF
         AS $$ BEGIN RETURN 1; END $$",
    );
    assert!(f.security_definer && f.strict && f.leakproof);
    assert_eq!(f.cost, Some(10));
    assert_eq!(f.rows, Some(5));
    assert_eq!(f.parallel, Some(Parallel::Safe));
    assert_eq!(f.volatility, Volatility::Stable);
    assert_eq!(f.settings[0].name, "search_path");
    assert_eq!(
        f.settings[0].value,
        SettingValue::Values(vec!["public".into(), "pg_temp".into()])
    );
}

#[test]
fn set_from_current_and_returns_null_on_null_input() {
    let f = function(
        "CREATE FUNCTION f(a int) RETURNS int LANGUAGE sql RETURNS NULL ON NULL INPUT
         SET work_mem FROM CURRENT AS 'SELECT a'",
    );
    assert!(f.strict);
    assert_eq!(f.settings[0].value, SettingValue::FromCurrent);
}

#[test]
fn ambiguous_params_type_alone_or_name_and_type() {
    let f = function(
        "CREATE FUNCTION f(double precision, p_id uuid, character varying(10),
                           IN OUT total numeric = 0, VARIADIC xs int[])
         RETURNS void LANGUAGE plpgsql AS $$ BEGIN END $$",
    );
    let p = &f.params;
    assert!(p[0].name.is_none());
    assert_eq!(p[0].data_type, DataType::DoublePrecision);
    assert_eq!(p[1].name.as_ref().unwrap().value, "p_id");
    assert_eq!(p[1].data_type, DataType::Uuid);
    assert!(p[2].name.is_none());
    assert_eq!(p[3].mode, ParamMode::InOut);
    assert!(p[3].default.is_some());
    assert_eq!(p[4].mode, ParamMode::Variadic);
    assert_eq!(f.return_type, ReturnType::Void);
}

#[test]
fn return_forms() {
    let f = function(
        "CREATE FUNCTION f(m int) RETURNS TABLE (page_id bigint, title text) LANGUAGE sql
         AS $$ SELECT 1::bigint, 'x' $$",
    );
    let ReturnType::Table(cols) = &f.return_type else {
        panic!("{:?}", f.return_type)
    };
    assert_eq!(cols[0].0.value, "page_id");
    assert!(matches!(f.body, FunctionBody::SqlQuery(_)));

    let f = function(
        "CREATE FUNCTION f() RETURNS SETOF pages LANGUAGE sql AS $$ SELECT * FROM pages $$",
    );
    assert!(matches!(f.return_type, ReturnType::SetOf(_)));

    let f = function(
        "CREATE FUNCTION f() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$",
    );
    assert_eq!(f.return_type, ReturnType::Trigger);
}

#[test]
fn out_params_without_returns() {
    let f = function(
        "CREATE FUNCTION f(a int, OUT doubled int, OUT label text) LANGUAGE sql
         AS $$ SELECT a * 2, 'x' $$",
    );
    // Neither `RETURNS TABLE` nor `SETOF`: a single row, typed by the OUTs.
    assert_eq!(f.return_type, ReturnType::OutParams);
}

#[test]
fn standard_sql_body() {
    let f = function(
        "CREATE FUNCTION f(a int) RETURNS int LANGUAGE sql IMMUTABLE BEGIN ATOMIC
         SELECT CASE WHEN a > 0 THEN a END; SELECT a + 1; END",
    );
    let FunctionBody::Sql(stmts) = &f.body else {
        panic!("{:?}", f.body)
    };
    assert_eq!(stmts.len(), 2);

    let f = function("CREATE FUNCTION f(a int) RETURNS int RETURN a * 2");
    assert_eq!(f.language, FunctionLanguage::Sql);
    assert!(matches!(f.body, FunctionBody::SqlReturn(_)));
}

#[test]
fn sql_body_with_several_statements() {
    let f = function(
        "CREATE FUNCTION f(p bigint) RETURNS bigint LANGUAGE sql AS $$
            UPDATE pages SET views = views + 1 WHERE id = p;
            SELECT views::bigint FROM pages WHERE id = p;
         $$",
    );
    let FunctionBody::Sql(stmts) = &f.body else {
        panic!("{:?}", f.body)
    };
    assert!(matches!(stmts[0], Statement::Update(_)));
    assert!(matches!(stmts[1], Statement::Select(_)));
}

#[test]
fn an_unreadable_body_is_reported_with_its_routine() {
    let out = parse_with_outcome(
        "CREATE FUNCTION f() RETURNS int LANGUAGE sql AS $$ SELEC 1 $$;\nSELECT 2;",
    )
    .unwrap();
    assert_eq!(out.statements.len(), 2, "the function stays declared");
    match &out.skipped[..] {
        [s] => assert!(
            matches!(&s.reason, SkipReason::UnparsedBody { name, .. } if name == "f"),
            "{:?}",
            s.reason
        ),
        other => panic!("{other:?}"),
    }
}

#[test]
fn sql_body_expressions_point_into_the_file() {
    let sql =
        "-- head\nCREATE FUNCTION f() RETURNS int LANGUAGE sql AS $$\n  SELECT nope FROM t\n$$;";
    let f = function(sql);
    let FunctionBody::SqlQuery(sel) = &f.body else {
        panic!("{:?}", f.body)
    };
    let pgweave_ast::stmt::SelectItem::Expr { expr, .. } = &sel.columns[0] else {
        panic!()
    };
    assert_eq!(
        &sql[expr.span.start as usize..expr.span.end as usize],
        "nope"
    );
    assert_eq!(
        &sql[f.body_start as usize..f.body_start as usize + 9],
        "\n  SELECT",
        "body_start points at the body's first character"
    );
}

#[test]
fn routine_without_language_is_explicitly_rejected() {
    let out = parse_with_outcome("CREATE FUNCTION f() RETURNS int AS $$ SELECT 1 $$").unwrap();
    assert!(out.statements.is_empty());
    assert!(matches!(out.skipped[0].reason, SkipReason::Unsupported(_)));
}
