//! Tables de transition d'un trigger niveau statement
//! (`REFERENCING NEW TABLE AS inserted OLD TABLE AS removed`).
//!
//! Régression (Mesalia) : dans le corps de la fonction, `inserted` et
//! `removed` étaient signalés comme tables inexistantes. Ce sont des tables
//! ayant les colonnes de la table du trigger, visibles du seul corps.

use pgweave_checker::check_sql_self_contained;
use pgweave_diagnostics::{Diagnostic, codes};

fn transition_trigger(body: &str) -> Vec<Diagnostic> {
    let sql = format!(
        "CREATE TABLE stock (id BIGINT PRIMARY KEY, quantity INTEGER NOT NULL);\n\
         CREATE TABLE stock_log (id BIGINT, delta INTEGER);\n\
         CREATE FUNCTION log_stock() RETURNS TRIGGER LANGUAGE plpgsql AS $$\n\
         BEGIN\n\
         {body}\n\
         RETURN NULL;\n\
         END;\n\
         $$;\n\
         CREATE TRIGGER stock_changes AFTER UPDATE ON stock\n\
         REFERENCING OLD TABLE AS removed NEW TABLE AS inserted\n\
         FOR EACH STATEMENT EXECUTE FUNCTION log_stock();\n"
    );
    check_sql_self_contained(&sql)
}

fn errors<'a>(diags: &'a [Diagnostic], code: &str) -> Vec<&'a Diagnostic> {
    diags.iter().filter(|d| d.code == code).collect()
}

#[test]
fn les_tables_de_transition_sont_en_portee() {
    let d = transition_trigger(
        "INSERT INTO stock_log (id, delta)\n\
         SELECT i.id, i.quantity - r.quantity FROM inserted i JOIN removed r ON r.id = i.id;",
    );
    assert!(errors(&d, codes::TABLE_NOT_FOUND).is_empty(), "{d:?}");
    assert!(errors(&d, codes::COLUMN_NOT_FOUND).is_empty(), "{d:?}");
}

#[test]
fn une_colonne_absente_d_une_table_de_transition_est_signalee() {
    let d = transition_trigger("INSERT INTO stock_log (id) SELECT i.nope FROM inserted i;");
    assert_eq!(errors(&d, codes::COLUMN_NOT_FOUND).len(), 1, "{d:?}");
}

#[test]
fn hors_du_corps_les_tables_de_transition_n_existent_pas() {
    let d = transition_trigger("NULL;");
    let outside = "SELECT id FROM inserted;";
    let d2 = check_sql_self_contained(outside);
    assert!(errors(&d, codes::TABLE_NOT_FOUND).is_empty());
    assert_eq!(errors(&d2, codes::TABLE_NOT_FOUND).len(), 1, "{d2:?}");
}
