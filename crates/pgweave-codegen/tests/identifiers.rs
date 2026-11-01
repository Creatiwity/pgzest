//! Principe 8 : un identifiant non quoté est replié en minuscules, un
//! identifiant quoté est conservé tel quel — comme PostgreSQL. Et ce que
//! l'émission écrit se relit en désignant les mêmes objets.

use pgweave_ast::{DdlStatement, Ident, Statement, types::DataType};
use pgweave_codegen::emit::{emit_ident, emit_statement};

fn table(sql: &str) -> pgweave_ast::ddl::CreateTable {
    match pgweave_parser::parse(sql).expect("parse").remove(0) {
        Statement::Ddl(DdlStatement::CreateTable(ct)) => ct,
        other => panic!("CREATE TABLE attendu : {other:?}"),
    }
}

/// Deux `CREATE TABLE` décrivent la même table (identifiants de nœuds et
/// positions mis à part).
fn same_table(a: &pgweave_ast::ddl::CreateTable, b: &pgweave_ast::ddl::CreateTable) -> bool {
    a.name == b.name
        && a.columns.len() == b.columns.len()
        && a.columns
            .iter()
            .zip(&b.columns)
            .all(|(x, y)| x.name == y.name && x.data_type == y.data_type)
}

#[test]
fn non_quote_replie_quote_conserve() {
    let ct = table(r#"CREATE TABLE "MenuComponent" (Id INT, "Label" TEXT)"#);
    assert_eq!(ct.name.0[0].value, "MenuComponent");
    assert!(ct.name.0[0].quoted);
    assert_eq!(ct.columns[0].name.value, "id");
    assert_eq!(ct.columns[1].name.value, "Label");

    let ct = table("CREATE TABLE MenuComponent (a INT)");
    assert_eq!(ct.name.0[0].value, "menucomponent");
}

#[test]
fn les_types_suivent_la_meme_regle() {
    let ct = table(r#"CREATE TABLE t (a "MyEnum", b MyEnum, c public."Other")"#);
    assert_eq!(ct.columns[0].data_type, DataType::Custom("MyEnum".into()));
    assert_eq!(ct.columns[1].data_type, DataType::Custom("myenum".into()));
    assert_eq!(
        ct.columns[2].data_type,
        DataType::Custom("public.Other".into())
    );
}

#[test]
fn l_emission_quote_ce_qui_ne_se_relirait_pas_a_l_identique() {
    let plain = |v: &str| Ident {
        value: v.into(),
        quoted: false,
    };
    assert_eq!(emit_ident(&plain("users")), "users");
    assert_eq!(emit_ident(&plain("MenuComponent")), "\"MenuComponent\"");
    assert_eq!(emit_ident(&plain("with space")), "\"with space\"");

    let sql = r#"CREATE TABLE "MenuComponent" ("Label" "MyEnum", n INT)"#;
    let out = emit_statement(&pgweave_parser::parse(sql).unwrap()[0]);
    assert!(out.contains(r#""MenuComponent""#), "{out}");
    assert!(out.contains(r#""Label" "MyEnum""#), "{out}");
    assert!(
        same_table(&table(&out), &table(sql)),
        "relu autrement :\n{out}"
    );
}

#[test]
fn les_modificateurs_de_type_sont_conserves() {
    let ct = table("CREATE TABLE lieux (pos geometry(Point, 4326), emb vector(3))");
    assert_eq!(
        ct.columns[0].data_type,
        DataType::Modified {
            name: "geometry".into(),
            modifiers: vec!["Point".into(), "4326".into()],
        }
    );
    let out = emit_statement(&Statement::Ddl(DdlStatement::CreateTable(ct.clone())));
    assert!(out.contains("geometry(Point, 4326)"), "{out}");
    assert!(same_table(&table(&out), &ct), "relu autrement :\n{out}");
}
