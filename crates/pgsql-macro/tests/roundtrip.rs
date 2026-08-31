//! Le SQL généré doit être du PostgreSQL valide, analysable par notre parser.
//!
//! C'est le test qui compte : un désucrage qui produit du SQL que l'outil
//! lui-même refuse ne sert à rien.

use pgsql_macro::{MacroSet, expand_source};

fn expand(src: &str) -> String {
    let e = expand_source(src, &MacroSet::builtin());
    let errors: Vec<_> = e
        .diagnostics
        .iter()
        .filter(|d| d.severity == pgsql_diagnostics::Severity::Error)
        .collect();
    assert!(errors.is_empty(), "erreurs de macro : {errors:?}");
    e.sql
}

/// Le SQL généré parse-t-il sans statement écarté ?
fn assert_parses(sql: &str) {
    let out = pgsql_parser::parse_with_outcome(sql)
        .unwrap_or_else(|e| panic!("parse impossible : {e}\n---\n{sql}"));
    assert!(
        out.skipped.is_empty(),
        "statements écartés : {:?}\n---\n{sql}",
        out.skipped
            .iter()
            .map(|s| (&s.sql, &s.reason))
            .collect::<Vec<_>>()
    );
    assert!(!out.statements.is_empty(), "aucun statement produit");
}

#[test]
fn une_fonction_avec_declare_produit_du_plpgsql_valide() {
    let sql = expand(
        "@function user_count(target_org UUID): INTEGER {\n\
         \x20   DECLARE total INTEGER;\n\
         \x20   SELECT count(*) INTO total FROM users WHERE org_id = target_org;\n\
         \x20   RETURN total;\n\
         }",
    );
    // La section DECLARE doit précéder BEGIN, sinon PostgreSQL refuse.
    let decl = sql.find("DECLARE").expect("section DECLARE");
    let begin = sql.find("BEGIN").expect("BEGIN");
    assert!(decl < begin, "DECLARE doit précéder BEGIN :\n{sql}");
    assert_eq!(sql.matches("DECLARE").count(), 1, "une seule section");
    assert_parses(&sql);
}

#[test]
fn plusieurs_declare_se_regroupent_en_une_section() {
    let sql = expand(
        "@function f(): INTEGER {\n\
         \x20   DECLARE a INTEGER;\n\
         \x20   DECLARE b TEXT;\n\
         \x20   RETURN 1;\n\
         }",
    );
    assert_eq!(
        sql.matches("DECLARE").count(),
        1,
        "une seule section :\n{sql}"
    );
    assert!(sql.contains("a INTEGER;"));
    assert!(sql.contains("b TEXT;"));
    assert_parses(&sql);
}

#[test]
fn une_fonction_sans_declare_nemet_pas_de_section() {
    let sql = expand("@function f(): INTEGER {\n    RETURN 1;\n}");
    assert!(
        !sql.contains("DECLARE"),
        "pas de section vide attendue :\n{sql}"
    );
    assert_parses(&sql);
}

#[test]
fn un_trigger_produit_deux_statements_valides() {
    let sql = expand(
        "@trigger touch before update on users for each row {\n\
         \x20   NEW.updated_at = now();\n\
         \x20   RETURN NEW;\n\
         }",
    );
    let out = pgsql_parser::parse_with_outcome(&sql).expect("parse");
    assert!(
        out.statements.len() >= 2,
        "fonction + trigger attendus, obtenu {} :\n{sql}",
        out.statements.len()
    );
    assert!(out.skipped.is_empty(), "écartés : {:?}", out.skipped);
}

#[test]
fn le_multi_tables_produit_du_sql_valide() {
    let sql = expand(
        "@trigger touch before update on (organizations, users, projects) for each row {\n\
         \x20   NEW.updated_at = now();\n\
         \x20   RETURN NEW;\n\
         }",
    );
    assert_parses(&sql);
    assert_eq!(sql.matches("CREATE TRIGGER").count(), 3);
}

#[test]
fn une_procedure_produit_du_sql_valide() {
    let sql =
        expand("@procedure archive(target UUID) {\n    DELETE FROM users WHERE id = target;\n}");
    assert!(sql.contains("CREATE OR REPLACE PROCEDURE archive(target UUID)"));
    assert_parses(&sql);
}

#[test]
fn les_attributs_produisent_du_sql_valide() {
    let sql = expand("@function slug(t TEXT): TEXT immutable strict {\n    RETURN lower(t);\n}");
    assert!(sql.contains("IMMUTABLE"));
    assert!(sql.contains("STRICT"));
    assert_parses(&sql);
}

#[test]
fn du_sql_melange_a_des_macros_parse_entierement() {
    let sql = expand(
        "CREATE TABLE users (id UUID PRIMARY KEY, updated_at TIMESTAMPTZ);\n\
         \n\
         @function count_all(): INTEGER {\n\
         \x20   DECLARE n INTEGER;\n\
         \x20   SELECT count(*) INTO n FROM users;\n\
         \x20   RETURN n;\n\
         }\n\
         \n\
         @trigger touch before update on users for each row {\n\
         \x20   NEW.updated_at = now();\n\
         \x20   RETURN NEW;\n\
         }\n\
         \n\
         SELECT id FROM users;",
    );
    assert_parses(&sql);
    assert!(sql.contains("CREATE TABLE users"));
    assert!(sql.contains("SELECT id FROM users;"));
}
