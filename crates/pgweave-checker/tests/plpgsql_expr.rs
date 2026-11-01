//! Les expressions des corps PL/pgSQL sont confrontées aux variables déclarées.
//!
//! Régression : `PlExpr::Sql` n'était pas vérifié du tout, donc
//! `RETURN colonne_inexistante;` passait en silence — alors que PostgreSQL
//! répond `column "…" does not exist`.
//!
//! L'enjeu de cette vérification est le rapport signal/bruit : un identifiant
//! nu en PL/pgSQL peut être une variable, un paramètre, une variable spéciale
//! (`TG_OP`), ou une variable ligne (`NEW`). Chaque cas non reconnu devient un
//! faux positif sur du code correct, donc chacun a son test.

use pgweave_checker::check_sql_self_contained;
use pgweave_diagnostics::{Diagnostic, codes};

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

// ── Ce qui doit être signalé ──────────────────────────────────────────────────

#[test]
fn un_return_dune_variable_inconnue_est_signale() {
    let d = diags("    RETURN colonne_bidon;");
    let msgs = undeclared(&d);
    assert_eq!(msgs.len(), 1, "un seul signalement attendu : {msgs:?}");
    assert!(msgs[0].contains("colonne_bidon"), "obtenu : {msgs:?}");
}

#[test]
fn le_hint_liste_les_variables_disponibles() {
    let d = diags("    RETURN colonne_bidon;");
    let hint = d
        .iter()
        .find(|d| d.code == codes::UNDECLARED_VARIABLE)
        .and_then(|d| d.hint.clone())
        .expect("hint attendu");
    assert!(hint.contains("total"), "la variable déclarée : {hint}");
    assert!(hint.contains("p_arg"), "le paramètre : {hint}");
}

#[test]
fn une_variable_inconnue_dans_une_sous_expression_est_signalee() {
    let d = diags("    RETURN total + inconnue;");
    let msgs = undeclared(&d);
    assert!(
        msgs.iter().any(|m| m.contains("inconnue")),
        "la récursion doit descendre dans les opérandes : {msgs:?}"
    );
    assert!(
        !msgs.iter().any(|m| m.contains("total")),
        "la variable déclarée ne doit pas remonter : {msgs:?}"
    );
}

#[test]
fn une_variable_inconnue_en_argument_de_fonction_est_signalee() {
    let d = diags("    RETURN abs(inconnue);");
    assert!(
        undeclared(&d).iter().any(|m| m.contains("inconnue")),
        "obtenu : {:?}",
        undeclared(&d)
    );
}

#[test]
fn une_variable_inconnue_dans_un_case_est_signalee() {
    let d = diags("    RETURN CASE WHEN inconnue > 0 THEN 1 ELSE 0 END;");
    assert!(
        undeclared(&d).iter().any(|m| m.contains("inconnue")),
        "obtenu : {:?}",
        undeclared(&d)
    );
}

#[test]
fn new_dans_une_fonction_ordinaire_est_signale() {
    // `RETURN NEW` hors trigger est une vraie erreur : NEW n'existe pas.
    // L'identifiant est nommé comme PostgreSQL le lit : replié en minuscules.
    let d = diags("    RETURN NEW;");
    assert!(
        undeclared(&d).iter().any(|m| m.contains("'new'")),
        "obtenu : {:?}",
        undeclared(&d)
    );
}

// ── Ce qui ne doit PAS être signalé ───────────────────────────────────────────

#[test]
fn une_variable_declaree_ne_produit_rien() {
    assert!(undeclared(&diags("    RETURN total;")).is_empty());
}

#[test]
fn un_parametre_ne_produit_rien() {
    assert!(undeclared(&diags("    RETURN p_arg;")).is_empty());
}

#[test]
fn un_litteral_ne_produit_rien() {
    assert!(undeclared(&diags("    RETURN 42;")).is_empty());
    assert!(undeclared(&diags("    RETURN 'texte'::INTEGER;")).is_empty());
}

#[test]
fn un_return_vide_ne_produit_rien() {
    assert!(undeclared(&diags("    RETURN;")).is_empty());
}

#[test]
fn les_variables_speciales_de_trigger_ne_produisent_rien() {
    for v in ["TG_OP", "TG_TABLE_NAME", "TG_WHEN", "TG_LEVEL", "TG_NARGS"] {
        let d = trigger_diags(&format!("    RAISE NOTICE '%', {v};\n    RETURN NEW;"));
        assert!(
            undeclared(&d).is_empty(),
            "{v} est une variable spéciale valide, obtenu : {:?}",
            undeclared(&d)
        );
    }
}

#[test]
fn new_et_old_nus_ne_produisent_rien_dans_un_trigger() {
    // Le parser n'en fait un `NewField` que si un `.colonne` suit ; nus, ils
    // arrivent comme identifiants ordinaires.
    for body in ["    RETURN NEW;", "    RETURN OLD;"] {
        let d = trigger_diags(body);
        assert!(
            undeclared(&d).is_empty(),
            "{body} est valide dans un trigger, obtenu : {:?}",
            undeclared(&d)
        );
    }
}

#[test]
fn found_ne_produit_rien() {
    let d = diags("    IF FOUND THEN RETURN 1; END IF;\n    RETURN 0;");
    assert!(undeclared(&d).is_empty(), "obtenu : {:?}", undeclared(&d));
}

#[test]
fn une_reference_qualifiee_ne_produit_rien() {
    // `rec.champ` : le type du record n'est pas suivi, trancher produirait des
    // faux positifs.
    let d = diags("    RETURN some_rec.some_field;");
    assert!(undeclared(&d).is_empty(), "obtenu : {:?}", undeclared(&d));
}

#[test]
fn une_sous_requete_ne_produit_rien() {
    // Les colonnes d'une sous-requête ont leur propre portée.
    let d = diags("    RETURN (SELECT count(*) FROM users);");
    assert!(undeclared(&d).is_empty(), "obtenu : {:?}", undeclared(&d));
}

#[test]
fn un_schema_reel_ne_produit_aucun_faux_positif() {
    // Garde-fou sur la fixture la plus riche du dépôt : plusieurs fonctions
    // PL/pgSQL, des triggers, du RLS.
    let sql = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/schema.sql"
    ))
    .expect("examples/schema.sql");
    let d = check_sql_self_contained(&sql);
    assert!(
        undeclared(&d).is_empty(),
        "aucun faux positif attendu sur examples/schema.sql, obtenu : {:?}",
        undeclared(&d)
    );
}
