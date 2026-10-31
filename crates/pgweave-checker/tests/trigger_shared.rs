//! Une fonction trigger partagée entre plusieurs tables.
//!
//! Régression : `set_updated_at` attaché à quatre tables déclenchait une erreur
//! `trigger-ambiguous-table`, alors que c'est un motif PostgreSQL idiomatique —
//! NEW et OLD prennent le type de la table qui déclenche. `examples/schema.sql`
//! ne passait donc pas `check`.
//!
//! Le corps est désormais vérifié une fois par table. Un diagnostic qui ne
//! dépend pas de la table n'est émis qu'une fois ; un diagnostic propre à une
//! table la nomme.

use pgweave_checker::check_sql_self_contained;
use pgweave_diagnostics::{Diagnostic, Severity, codes};

fn shared_trigger(body: &str) -> Vec<Diagnostic> {
    let sql = format!(
        "CREATE TABLE users (id UUID PRIMARY KEY, updated_at TIMESTAMPTZ);\n\
         CREATE TABLE posts (id UUID PRIMARY KEY, updated_at TIMESTAMPTZ, title TEXT);\n\
         CREATE OR REPLACE FUNCTION touch() RETURNS TRIGGER\n\
         LANGUAGE plpgsql AS $$\n\
         BEGIN\n\
         {body}\n\
         END;\n\
         $$;\n\
         CREATE TRIGGER t_users BEFORE UPDATE ON users FOR EACH ROW EXECUTE FUNCTION touch();\n\
         CREATE TRIGGER t_posts BEFORE UPDATE ON posts FOR EACH ROW EXECUTE FUNCTION touch();\n"
    );
    check_sql_self_contained(&sql)
}

fn with_code<'a>(diags: &'a [Diagnostic], code: &str) -> Vec<&'a Diagnostic> {
    diags.iter().filter(|d| d.code == code).collect()
}

#[test]
fn une_colonne_commune_a_toutes_les_tables_ne_produit_rien() {
    let d = shared_trigger("    NEW.updated_at = now();\n    RETURN NEW;");
    assert!(
        with_code(&d, codes::TRIGGER_AMBIGUOUS_TABLE).is_empty(),
        "plus d'ambiguïté signalée : {d:?}"
    );
    assert!(
        !d.iter().any(|d| d.severity == Severity::Error),
        "aucune erreur attendue : {d:?}"
    );
    assert!(
        with_code(&d, codes::TRIGGER_NO_CONTEXT).is_empty(),
        "le contexte trigger est résolu : {d:?}"
    );
}

#[test]
fn une_colonne_absente_dune_seule_table_est_signalee_sur_cette_table() {
    let d = shared_trigger("    NEW.title = 'x';\n    RETURN NEW;");
    let missing = with_code(&d, codes::COLUMN_NOT_FOUND);
    assert_eq!(missing.len(), 1, "un seul signalement attendu : {d:?}");
    assert!(
        missing[0].message.contains("users"),
        "le message doit nommer la table fautive : {}",
        missing[0].message
    );
    assert!(
        !missing[0].message.contains("posts"),
        "posts a bien la colonne : {}",
        missing[0].message
    );
}

#[test]
fn une_condition_composite_est_verifiee_sur_chaque_table() {
    let d = shared_trigger(
        "    IF OLD.title IS DISTINCT FROM NEW.title THEN\n\
         \x20       NEW.updated_at = now();\n\
         \x20   END IF;\n\
         \x20   RETURN NEW;",
    );
    let missing = with_code(&d, codes::COLUMN_NOT_FOUND);
    assert!(!missing.is_empty(), "users n'a pas de title : {d:?}");
    assert!(
        missing.iter().all(|m| m.message.contains("users")),
        "seule users est fautive : {missing:?}"
    );
}

#[test]
fn un_diagnostic_independant_de_la_table_nest_emis_quune_fois() {
    let d = shared_trigger("    RETURN inconnue;");
    let undeclared = with_code(&d, codes::UNDECLARED_VARIABLE);
    assert_eq!(
        undeclared.len(),
        1,
        "pas de doublon par table : {undeclared:?}"
    );
    assert!(
        !undeclared[0].message.contains("trigger on"),
        "rien de propre à une table : {}",
        undeclared[0].message
    );
}

#[test]
fn le_schema_dexemple_ne_produit_plus_dambiguite() {
    let sql = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/schema.sql"
    ))
    .expect("examples/schema.sql");
    let d = check_sql_self_contained(&sql);
    assert!(
        !d.iter().any(|d| d.severity == Severity::Error),
        "examples/schema.sql doit passer check : {d:?}"
    );
}
