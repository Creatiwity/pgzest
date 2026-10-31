//! Les diagnostics visent l'expression fautive, pas le début du statement.

use pgweave_checker::check_sql_self_contained;
use pgweave_diagnostics::{Diagnostic, codes};

/// (ligne, colonne) 1-basées d'un diagnostic, comme les affiche la CLI.
fn line_col(sql: &str, d: &Diagnostic) -> (usize, usize) {
    let Some(sp) = d.span.as_ref().filter(|s| s.start > 0 || s.end > 0) else {
        return (1, 1);
    };
    let before = &sql[..sp.start];
    let line = before.matches('\n').count() + 1;
    let col = before
        .rfind('\n')
        .map(|nl| before[nl + 1..].chars().count() + 1)
        .unwrap_or_else(|| before.chars().count() + 1);
    (line, col)
}

/// Texte que le span du diagnostic découpe dans la source.
fn slice<'a>(sql: &'a str, d: &Diagnostic) -> Option<&'a str> {
    d.span
        .as_ref()
        .filter(|s| s.end > s.start)
        .map(|s| &sql[s.start..s.end])
}

fn only(sql: &str, code: &str) -> Diagnostic {
    let diags = check_sql_self_contained(sql);
    let mut it = diags.into_iter().filter(|d| d.code == code);
    let d = it
        .next()
        .unwrap_or_else(|| panic!("aucun {code} pour {sql:?}"));
    assert!(it.next().is_none(), "un seul {code} attendu");
    d
}

#[test]
fn le_diagnostic_vise_la_colonne_fautive_dans_le_select() {
    let sql = "CREATE TABLE users (id UUID, email TEXT);\nSELECT id, nope, email FROM users;";
    let d = only(sql, codes::COLUMN_NOT_FOUND);
    assert_eq!(slice(sql, &d), Some("nope"));
    assert_eq!(line_col(sql, &d), (2, 12));
}

#[test]
fn le_diagnostic_vise_la_colonne_fautive_dans_le_where() {
    let sql =
        "CREATE TABLE users (id UUID, email TEXT);\nSELECT email FROM users WHERE missing = 'x';";
    let d = only(sql, codes::COLUMN_NOT_FOUND);
    assert_eq!(slice(sql, &d), Some("missing"));
    assert_eq!(line_col(sql, &d), (2, 31));
}

#[test]
fn la_precision_survit_a_un_accent_en_amont() {
    // Régression combinée : colonne sqlparser en caractères + offsets en octets.
    let sql =
        "CREATE TABLE users (id UUID);\n-- commentaire accentué : é à ù\nSELECT nope FROM users;";
    let d = only(sql, codes::COLUMN_NOT_FOUND);
    assert_eq!(
        slice(sql, &d),
        Some("nope"),
        "un accent en amont ne doit pas décaler le span"
    );
}

#[test]
fn une_expression_imbriquee_est_visee_precisement() {
    let sql = "CREATE TABLE t (a INT);\nSELECT a FROM t WHERE (a + bad) > 1;";
    let d = only(sql, codes::COLUMN_NOT_FOUND);
    assert_eq!(
        slice(sql, &d),
        Some("bad"),
        "le nœud le plus fin doit gagner, pas l'expression englobante"
    );
}

#[test]
fn une_colonne_qualifiee_absente_est_visee() {
    let sql = "CREATE TABLE users (id UUID);\nSELECT users.nope FROM users;";
    let d = only(sql, codes::COLUMN_NOT_FOUND);
    assert_eq!(slice(sql, &d), Some("users.nope"));
}

#[test]
fn le_span_statement_reste_le_repli() {
    // `table-not-found` porte sur le statement, pas sur une expression :
    // le backfill statement doit continuer de s'appliquer.
    let sql = "CREATE TABLE t (a INT);\nSELECT a FROM nonexistent;";
    let d = only(sql, codes::TABLE_NOT_FOUND);
    let (line, _) = line_col(sql, &d);
    assert_eq!(line, 2, "au moins la bonne ligne");
}

#[test]
fn plusieurs_erreurs_visent_chacune_leur_colonne() {
    let sql = "CREATE TABLE t (a INT);\nSELECT bad1 FROM t;\nSELECT bad2 FROM t;";
    let diags = check_sql_self_contained(sql);
    let cols: Vec<_> = diags
        .iter()
        .filter(|d| d.code == codes::COLUMN_NOT_FOUND)
        .map(|d| slice(sql, d))
        .collect();
    assert_eq!(cols, vec![Some("bad1"), Some("bad2")]);
}
