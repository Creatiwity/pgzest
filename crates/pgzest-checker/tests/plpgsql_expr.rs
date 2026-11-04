//! Expressions in PL/pgSQL bodies are checked against the declared variables.
//!
//! Regression: `PlExpr::Sql` was not checked at all, so
//! `RETURN nonexistent_column;` passed silently — whereas PostgreSQL
//! answers `column "…" does not exist`.
//!
//! The stake of this check is the signal/noise ratio: a bare identifier
//! in PL/pgSQL can be a variable, a parameter, a special variable
//! (`TG_OP`), or a row variable (`NEW`). Every unrecognized case becomes a
//! false positive on correct code, so each one has its test.

use pgzest_checker::check_sql_self_contained;
use pgzest_diagnostics::{Diagnostic, codes};

fn diags(body: &str) -> Vec<Diagnostic> {
    let sql = format!(
        "CREATE TABLE users (id UUID PRIMARY KEY, email TEXT);\n\
         CREATE OR REPLACE FUNCTION f(p_arg INTEGER) RETURNS INTEGER\n\
         LANGUAGE plpgsql AS $$\n\
         DECLARE\n\
         \x20   total INTEGER;\n\
         BEGIN\n\
         {body}\n\
         END;\n\
         $$;\n"
    );
    check_sql_self_contained(&sql)
}

fn trigger_diags(body: &str) -> Vec<Diagnostic> {
    let sql = format!(
        "CREATE TABLE users (id UUID PRIMARY KEY, email TEXT);\n\
         CREATE OR REPLACE FUNCTION tf() RETURNS TRIGGER\n\
         LANGUAGE plpgsql AS $$\n\
         BEGIN\n\
         {body}\n\
         END;\n\
         $$;\n\
         CREATE TRIGGER t BEFORE UPDATE ON users FOR EACH ROW EXECUTE FUNCTION tf();\n"
    );
    check_sql_self_contained(&sql)
}

fn undeclared(diags: &[Diagnostic]) -> Vec<&str> {
    diags
        .iter()
        .filter(|d| d.code == codes::UNDECLARED_VARIABLE)
        .map(|d| d.message.as_str())
        .collect()
}

// ── What must be reported ─────────────────────────────────────────────────────

#[test]
fn returning_an_unknown_variable_is_reported() {
    let d = diags("    RETURN bogus_column;");
    let msgs = undeclared(&d);
    assert_eq!(msgs.len(), 1, "exactly one report expected: {msgs:?}");
    assert!(msgs[0].contains("bogus_column"), "got: {msgs:?}");
}

#[test]
fn the_hint_lists_the_available_variables() {
    let d = diags("    RETURN bogus_column;");
    let hint = d
        .iter()
        .find(|d| d.code == codes::UNDECLARED_VARIABLE)
        .and_then(|d| d.hint.clone())
        .expect("hint expected");
    assert!(hint.contains("total"), "the declared variable: {hint}");
    assert!(hint.contains("p_arg"), "the parameter: {hint}");
}

#[test]
fn an_unknown_variable_in_a_subexpression_is_reported() {
    let d = diags("    RETURN total + unknown_var;");
    let msgs = undeclared(&d);
    assert!(
        msgs.iter().any(|m| m.contains("unknown_var")),
        "the recursion must descend into the operands: {msgs:?}"
    );
    assert!(
        !msgs.iter().any(|m| m.contains("total")),
        "the declared variable must not be reported: {msgs:?}"
    );
}

#[test]
fn an_unknown_variable_as_function_argument_is_reported() {
    let d = diags("    RETURN abs(unknown_var);");
    assert!(
        undeclared(&d).iter().any(|m| m.contains("unknown_var")),
        "got: {:?}",
        undeclared(&d)
    );
}

#[test]
fn an_unknown_variable_in_a_case_is_reported() {
    let d = diags("    RETURN CASE WHEN unknown_var > 0 THEN 1 ELSE 0 END;");
    assert!(
        undeclared(&d).iter().any(|m| m.contains("unknown_var")),
        "got: {:?}",
        undeclared(&d)
    );
}

#[test]
fn new_in_an_ordinary_function_is_reported() {
    // `RETURN NEW` outside a trigger is a real error: NEW does not exist.
    // The identifier is named the way PostgreSQL reads it: folded to lower case.
    let d = diags("    RETURN NEW;");
    assert!(
        undeclared(&d).iter().any(|m| m.contains("'new'")),
        "got: {:?}",
        undeclared(&d)
    );
}

// ── What must NOT be reported ─────────────────────────────────────────────────

#[test]
fn a_declared_variable_produces_nothing() {
    assert!(undeclared(&diags("    RETURN total;")).is_empty());
}

#[test]
fn a_parameter_produces_nothing() {
    assert!(undeclared(&diags("    RETURN p_arg;")).is_empty());
}

#[test]
fn a_literal_produces_nothing() {
    assert!(undeclared(&diags("    RETURN 42;")).is_empty());
    assert!(undeclared(&diags("    RETURN 'text'::INTEGER;")).is_empty());
}

#[test]
fn an_empty_return_produces_nothing() {
    assert!(undeclared(&diags("    RETURN;")).is_empty());
}

#[test]
fn trigger_special_variables_produce_nothing() {
    for v in ["TG_OP", "TG_TABLE_NAME", "TG_WHEN", "TG_LEVEL", "TG_NARGS"] {
        let d = trigger_diags(&format!("    RAISE NOTICE '%', {v};\n    RETURN NEW;"));
        assert!(
            undeclared(&d).is_empty(),
            "{v} is a valid special variable, got: {:?}",
            undeclared(&d)
        );
    }
}

#[test]
fn bare_new_and_old_produce_nothing_in_a_trigger() {
    // The parser only makes a `NewField` of them if a `.column` follows; bare,
    // they arrive as ordinary identifiers.
    for body in ["    RETURN NEW;", "    RETURN OLD;"] {
        let d = trigger_diags(body);
        assert!(
            undeclared(&d).is_empty(),
            "{body} is valid in a trigger, got: {:?}",
            undeclared(&d)
        );
    }
}

#[test]
fn found_produces_nothing() {
    let d = diags("    IF FOUND THEN RETURN 1; END IF;\n    RETURN 0;");
    assert!(undeclared(&d).is_empty(), "got: {:?}", undeclared(&d));
}

#[test]
fn a_qualified_reference_produces_nothing() {
    // `rec.field`: the record's type is not tracked, deciding would produce
    // false positives.
    let d = diags("    RETURN some_rec.some_field;");
    assert!(undeclared(&d).is_empty(), "got: {:?}", undeclared(&d));
}

#[test]
fn a_subquery_produces_nothing() {
    // The columns of a subquery have their own scope.
    let d = diags("    RETURN (SELECT count(*) FROM users);");
    assert!(undeclared(&d).is_empty(), "got: {:?}", undeclared(&d));
}

#[test]
fn a_real_schema_produces_no_false_positive() {
    // Safeguard on the richest fixture of the repository: several PL/pgSQL
    // functions, triggers, RLS.
    let sql = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/schema.sql"
    ))
    .expect("examples/schema.sql");
    let d = check_sql_self_contained(&sql);
    assert!(
        undeclared(&d).is_empty(),
        "no false positive expected on examples/schema.sql, got: {:?}",
        undeclared(&d)
    );
}
