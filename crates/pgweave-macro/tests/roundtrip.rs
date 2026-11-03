//! The generated SQL must be valid PostgreSQL, parseable by our parser.
//!
//! This is the test that matters: a desugaring that produces SQL the tool
//! itself rejects is useless.

use pgweave_macro::{MacroSet, expand_source};

fn expand(src: &str) -> String {
    let e = expand_source(src, &MacroSet::builtin());
    let errors: Vec<_> = e
        .diagnostics
        .iter()
        .filter(|d| d.severity == pgweave_diagnostics::Severity::Error)
        .collect();
    assert!(errors.is_empty(), "macro errors: {errors:?}");
    e.sql
}

/// Does the generated SQL parse without any skipped statement?
fn assert_parses(sql: &str) {
    let out = pgweave_parser::parse_with_outcome(sql)
        .unwrap_or_else(|e| panic!("parse failed: {e}\n---\n{sql}"));
    assert!(
        out.skipped.is_empty(),
        "skipped statements: {:?}\n---\n{sql}",
        out.skipped
            .iter()
            .map(|s| (&s.sql, &s.reason))
            .collect::<Vec<_>>()
    );
    assert!(!out.statements.is_empty(), "no statement produced");
}

#[test]
fn function_with_declare_produces_valid_plpgsql() {
    let sql = expand(
        "@function user_count(target_org UUID): INTEGER {\n\
         \x20   DECLARE total INTEGER;\n\
         \x20   SELECT count(*) INTO total FROM users WHERE org_id = target_org;\n\
         \x20   RETURN total;\n\
         }",
    );
    // The DECLARE section must precede BEGIN, otherwise PostgreSQL rejects it.
    let decl = sql.find("DECLARE").expect("DECLARE section");
    let begin = sql.find("BEGIN").expect("BEGIN");
    assert!(decl < begin, "DECLARE must precede BEGIN:\n{sql}");
    assert_eq!(sql.matches("DECLARE").count(), 1, "a single section");
    assert_parses(&sql);
}

#[test]
fn several_declares_merge_into_one_section() {
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
        "a single section:\n{sql}"
    );
    assert!(sql.contains("a INTEGER;"));
    assert!(sql.contains("b TEXT;"));
    assert_parses(&sql);
}

#[test]
fn function_without_declare_emits_no_section() {
    let sql = expand("@function f(): INTEGER {\n    RETURN 1;\n}");
    assert!(
        !sql.contains("DECLARE"),
        "no empty section expected:\n{sql}"
    );
    assert_parses(&sql);
}

#[test]
fn trigger_produces_two_valid_statements() {
    let sql = expand(
        "@trigger touch before update on users for each row {\n\
         \x20   NEW.updated_at = now();\n\
         \x20   RETURN NEW;\n\
         }",
    );
    let out = pgweave_parser::parse_with_outcome(&sql).expect("parse");
    assert!(
        out.statements.len() >= 2,
        "function + trigger expected, got {}:\n{sql}",
        out.statements.len()
    );
    assert!(out.skipped.is_empty(), "skipped: {:?}", out.skipped);
}

#[test]
fn multi_tables_produces_valid_sql() {
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
fn procedure_produces_valid_sql() {
    let sql =
        expand("@procedure archive(target UUID) {\n    DELETE FROM users WHERE id = target;\n}");
    assert!(sql.contains("CREATE OR REPLACE PROCEDURE archive(target UUID)"));
    assert_parses(&sql);
}

#[test]
fn attributes_produce_valid_sql() {
    let sql = expand("@function slug(t TEXT): TEXT immutable strict {\n    RETURN lower(t);\n}");
    assert!(sql.contains("IMMUTABLE"));
    assert!(sql.contains("STRICT"));
    assert_parses(&sql);
}

#[test]
fn sql_mixed_with_macros_parses_fully() {
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

/// Regression: detection looked backwards and hit the comment preceding the
/// macro, so only the first one of a file was desugared.
#[test]
fn macros_separated_by_comments_are_all_desugared() {
    let src = "\
-- first comment
@function a(): INT {
    RETURN 1;
}

-- second comment, preceding the next macro
@function b(): INT {
    RETURN 2;
}

/* block comment */
@function c(): INT {
    RETURN 3;
}
";
    let e = expand_source(src, &MacroSet::builtin());
    assert_eq!(e.expanded, 3, "all three macros must be desugared");
    assert!(
        !e.sql.contains("@function"),
        "no macro must remain:\n{}",
        e.sql
    );
    assert_parses(&expand(src));
}

#[test]
fn macro_immediately_following_another_is_desugared() {
    let src = "@function a(): INT {\n RETURN 1;\n}\n@function b(): INT {\n RETURN 2;\n}";
    let e = expand_source(src, &MacroSet::builtin());
    assert_eq!(e.expanded, 2);
}

#[test]
fn absolute_value_operator_stays_intact_after_a_statement() {
    let src = "SELECT 1;\nSELECT @ -5;";
    let e = expand_source(src, &MacroSet::builtin());
    assert_eq!(e.expanded, 0, "no expansion expected");
    assert_eq!(e.sql, src);
}

// ── Locating an expansion ─────────────────────────────────────────────────────

#[test]
fn expansion_at_finds_the_macro_under_the_cursor() {
    let src = "\
CREATE TABLE t (a INT);

@function one(): INT {
    RETURN 1;
}

@function two(): INT {
    RETURN 2;
}
";
    let e = expand_source(src, &MacroSet::builtin());
    assert_eq!(e.regions.len(), 2);

    // Cursor in the first macro.
    let in_first = src.find("RETURN 1").unwrap();
    let first = e.expansion_at(in_first).expect("expansion expected");
    assert!(first.contains("FUNCTION one()"), "got:\n{first}");
    assert!(
        !first.contains("FUNCTION two()"),
        "must not spill over into the next macro:\n{first}"
    );

    // Cursor in the second one.
    let in_second = src.find("RETURN 2").unwrap();
    let second = e.expansion_at(in_second).expect("expansion expected");
    assert!(second.contains("FUNCTION two()"), "got:\n{second}");
    assert!(!second.contains("FUNCTION one()"));
}

#[test]
fn expansion_at_returns_nothing_outside_macro() {
    let src = "CREATE TABLE t (a INT);\n\n@function f(): INT {\n RETURN 1;\n}\n";
    let e = expand_source(src, &MacroSet::builtin());
    let in_table = src.find("CREATE TABLE").unwrap();
    assert!(
        e.expansion_at(in_table).is_none(),
        "a cursor outside a macro must return nothing"
    );
}

#[test]
fn region_names_the_macro() {
    let src = "@trigger t before update on users for each row {\n RETURN NEW;\n}\n";
    let e = expand_source(src, &MacroSet::builtin());
    assert_eq!(e.regions[0].macro_name, "trigger");
    assert!(e.regions[0].source.contains(&0));
}
