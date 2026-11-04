//! Principle 8: an unquoted identifier is folded to lowercase, a quoted
//! identifier is kept as is — like PostgreSQL. And what emission writes
//! reads back as designating the same objects.

use pgzest_ast::{DdlStatement, Ident, Statement, types::DataType};
use pgzest_codegen::emit::{emit_ident, emit_statement};

fn table(sql: &str) -> pgzest_ast::ddl::CreateTable {
    match pgzest_parser::parse(sql).expect("parse").remove(0) {
        Statement::Ddl(DdlStatement::CreateTable(ct)) => ct,
        other => panic!("CREATE TABLE expected: {other:?}"),
    }
}

/// Two `CREATE TABLE`s describe the same table (node identifiers and
/// positions aside).
fn same_table(a: &pgzest_ast::ddl::CreateTable, b: &pgzest_ast::ddl::CreateTable) -> bool {
    a.name == b.name
        && a.columns.len() == b.columns.len()
        && a.columns
            .iter()
            .zip(&b.columns)
            .all(|(x, y)| x.name == y.name && x.data_type == y.data_type)
}

#[test]
fn unquoted_is_folded_quoted_is_kept() {
    let ct = table(r#"CREATE TABLE "OrderLine" (Id INT, "Label" TEXT)"#);
    assert_eq!(ct.name.0[0].value, "OrderLine");
    assert!(ct.name.0[0].quoted);
    assert_eq!(ct.columns[0].name.value, "id");
    assert_eq!(ct.columns[1].name.value, "Label");

    let ct = table("CREATE TABLE OrderLine (a INT)");
    assert_eq!(ct.name.0[0].value, "orderline");
}

#[test]
fn types_follow_the_same_rule() {
    let ct = table(r#"CREATE TABLE t (a "MyEnum", b MyEnum, c public."Other")"#);
    assert_eq!(ct.columns[0].data_type, DataType::Custom("MyEnum".into()));
    assert_eq!(ct.columns[1].data_type, DataType::Custom("myenum".into()));
    assert_eq!(
        ct.columns[2].data_type,
        DataType::Custom("public.Other".into())
    );
}

#[test]
fn emission_quotes_what_would_not_read_back_identically() {
    let plain = |v: &str| Ident {
        value: v.into(),
        quoted: false,
    };
    assert_eq!(emit_ident(&plain("users")), "users");
    assert_eq!(emit_ident(&plain("OrderLine")), "\"OrderLine\"");
    assert_eq!(emit_ident(&plain("with space")), "\"with space\"");

    let sql = r#"CREATE TABLE "OrderLine" ("Label" "MyEnum", n INT)"#;
    let out = emit_statement(&pgzest_parser::parse(sql).unwrap()[0]);
    assert!(out.contains(r#""OrderLine""#), "{out}");
    assert!(out.contains(r#""Label" "MyEnum""#), "{out}");
    assert!(
        same_table(&table(&out), &table(sql)),
        "read back differently:\n{out}"
    );
}

#[test]
fn type_modifiers_are_kept() {
    let ct = table("CREATE TABLE places (pos geometry(Point, 4326), emb vector(3))");
    assert_eq!(
        ct.columns[0].data_type,
        DataType::Modified {
            name: "geometry".into(),
            modifiers: vec!["Point".into(), "4326".into()],
        }
    );
    let out = emit_statement(&Statement::Ddl(DdlStatement::CreateTable(ct.clone())));
    assert!(out.contains("geometry(Point, 4326)"), "{out}");
    assert!(
        same_table(&table(&out), &ct),
        "read back differently:\n{out}"
    );
}
