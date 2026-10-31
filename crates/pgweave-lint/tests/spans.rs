//! Les diagnostics de lint doivent porter le span du statement concerné.
//!
//! Régression : toutes les règles renvoyaient un diagnostic sans span, donc
//! rapporté en `1:1`. Inexploitable dans l'éditeur (soulignement en tête de
//! fichier) et impossible à cibler avec `-- pgweave-disable-next-line`.

use pgweave_lint::{LintConfig, lint_statements};

/// Ligne 1-indexée de l'offset octet, comme le fait la CLI.
fn line_of(sql: &str, offset: usize) -> usize {
    sql[..offset].matches('\n').count() + 1
}

/// (code, ligne) de chaque diagnostic, trié — pour des assertions stables.
fn diag_lines(sql: &str) -> Vec<(String, usize)> {
    let stmts = pgweave_parser::parse(sql).expect("parse");
    let mut out: Vec<(String, usize)> = lint_statements(&stmts, &LintConfig::default())
        .diagnostics
        .iter()
        .map(|d| {
            let line = d.span.as_ref().map(|s| line_of(sql, s.start)).unwrap_or(0); // 0 = pas de span, ce qu'on veut éliminer
            (d.code.to_string(), line)
        })
        .collect();
    out.sort();
    out
}

#[test]
fn tous_les_diagnostics_ont_un_span() {
    let sql = "\
CREATE TABLE t (a INT PRIMARY KEY);

SELECT * FROM t;

CREATE TABLE no_pk (id UUID);

SELECT a FROM t WHERE a::text LIKE '%x';
";
    let stmts = pgweave_parser::parse(sql).expect("parse");
    let diags = lint_statements(&stmts, &LintConfig::default()).diagnostics;
    assert!(!diags.is_empty(), "des diagnostics sont attendus");
    for d in &diags {
        assert!(
            d.span.is_some(),
            "le diagnostic {} n'a pas de span — il serait rapporté en 1:1",
            d.code
        );
    }
}

#[test]
fn le_span_pointe_la_bonne_ligne() {
    let sql = "\
CREATE TABLE t (a INT PRIMARY KEY);

SELECT * FROM t;
";
    // SELECT * est ligne 3.
    assert_eq!(diag_lines(sql), vec![("L001".to_string(), 3)]);
}

#[test]
fn les_commentaires_en_tete_ne_decalent_pas() {
    let sql = "\
CREATE TABLE t (a INT PRIMARY KEY);

-- un commentaire
-- et un deuxième
SELECT * FROM t;
";
    // Le span doit démarrer sur SELECT (ligne 5), pas sur le commentaire.
    assert_eq!(diag_lines(sql), vec![("L001".to_string(), 5)]);
}

#[test]
fn un_accent_ne_decale_pas_le_span() {
    let sql = "\
CREATE TABLE t (a INT PRIMARY KEY);

-- commentaire accentué : é à ù → ok
SELECT * FROM t;
";
    assert_eq!(diag_lines(sql), vec![("L001".to_string(), 4)]);
}

#[test]
fn plusieurs_statements_chacun_sa_ligne() {
    let sql = "\
CREATE TABLE a_t (x INT PRIMARY KEY);
SELECT * FROM a_t;
CREATE TABLE no_pk (y UUID);
SELECT * FROM no_pk;
";
    let got = diag_lines(sql);
    assert!(
        got.contains(&("L001".to_string(), 2)),
        "premier SELECT * ligne 2, obtenu: {got:?}"
    );
    assert!(
        got.contains(&("L001".to_string(), 4)),
        "second SELECT * ligne 4, obtenu: {got:?}"
    );
    assert!(
        got.contains(&("L002".to_string(), 3)),
        "table sans PK ligne 3, obtenu: {got:?}"
    );
}

#[test]
fn la_regle_cross_statement_l005_porte_son_span() {
    // L005 est calculée sur l'ensemble des statements : le driver ne peut pas
    // lui attacher le span, elle doit le porter elle-même.
    let sql = "\
CREATE TABLE parent (id UUID PRIMARY KEY);

CREATE TABLE child (
    id     UUID PRIMARY KEY,
    par_id UUID REFERENCES parent(id)
);
";
    let got = diag_lines(sql);
    let l005: Vec<_> = got.iter().filter(|(c, _)| c == "L005").collect();
    assert_eq!(l005.len(), 1, "un L005 attendu, obtenu: {got:?}");
    assert_eq!(
        l005[0].1, 3,
        "le span doit pointer le CREATE TABLE child (ligne 3), obtenu: {got:?}"
    );
}

#[test]
fn une_directive_peut_cibler_une_regle_de_lint() {
    // C'était impossible avant : sans span, la suppression positionnelle
    // ne pouvait pas savoir sur quelle ligne s'appliquer.
    let sql = "\
CREATE TABLE t (a INT PRIMARY KEY);

-- pgweave-disable-next-line L001
SELECT * FROM t;

SELECT * FROM t;
";
    let stmts = pgweave_parser::parse(sql).expect("parse");
    let mut diags = lint_statements(&stmts, &LintConfig::default()).diagnostics;
    pgweave_diagnostics::Suppressions::scan(sql).apply(&mut diags);

    let l001: Vec<_> = diags.iter().filter(|d| d.code == "L001").collect();
    assert_eq!(
        l001.len(),
        1,
        "un seul L001 doit survivre, obtenu: {:?}",
        diags.iter().map(|d| d.code).collect::<Vec<_>>()
    );
    let line = line_of(sql, l001[0].span.as_ref().unwrap().start);
    assert_eq!(line, 6, "le survivant est celui de la ligne 6");
}
