//! Résolution des sources d'un SELECT : CTE, tables dérivées, fonctions,
//! LATERAL, sous-requêtes corrélées, opérations ensemblistes.

use pgweave_checker::check_sql_self_contained;
use pgweave_diagnostics::{Diagnostic, codes};

const SCHEMA: &str = "CREATE TABLE authors (id UUID PRIMARY KEY, name TEXT);\n\
                      CREATE TABLE pages (id BIGINT PRIMARY KEY, author_id UUID, title TEXT);\n";

fn check(query: &str) -> Vec<Diagnostic> {
    check_sql_self_contained(&format!("{SCHEMA}{query}"))
}

fn errors(d: &[Diagnostic]) -> Vec<&Diagnostic> {
    d.iter()
        .filter(|d| d.severity == pgweave_diagnostics::Severity::Error)
        .collect()
}

#[test]
fn les_cte_sont_des_sources() {
    let d = check("WITH recent AS (SELECT id, title FROM pages) SELECT r.title FROM recent r;");
    assert!(errors(&d).is_empty(), "{d:?}");
    let d = check("WITH recent AS (SELECT id FROM pages) SELECT r.title FROM recent r;");
    assert_eq!(errors(&d).len(), 1, "title absente de la CTE : {d:?}");
}

#[test]
fn une_cte_recursive_se_cite_elle_meme() {
    let d = check(
        "WITH RECURSIVE r (n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 3) SELECT n FROM r;",
    );
    assert!(errors(&d).is_empty(), "{d:?}");
}

#[test]
fn sous_requete_correlee() {
    let d = check(
        "SELECT a.name, (SELECT count(*) FROM pages p WHERE p.author_id = a.id) AS n FROM authors a;",
    );
    assert!(
        errors(&d).is_empty(),
        "référence à la requête englobante : {d:?}"
    );
    let d = check(
        "SELECT a.name FROM authors a WHERE EXISTS (SELECT 1 FROM pages p WHERE p.nope = a.id);",
    );
    assert_eq!(errors(&d).len(), 1, "{d:?}");
    assert_eq!(errors(&d)[0].code, codes::COLUMN_NOT_FOUND);
}

#[test]
fn colonnes_d_une_table_derivee() {
    let d = check("SELECT s.n, s.t FROM (SELECT id AS n, title AS t FROM pages) s;");
    assert!(errors(&d).is_empty(), "{d:?}");
    let d = check("SELECT s.title FROM (SELECT id AS n FROM pages) s;");
    assert_eq!(errors(&d).len(), 1, "{d:?}");
    let d = check("SELECT s.x FROM (SELECT id FROM pages) AS s (x);");
    assert!(errors(&d).is_empty(), "alias de colonnes : {d:?}");
}

#[test]
fn lateral_voit_les_sources_precedentes() {
    let d = check(
        "SELECT a.name, l.n FROM authors a CROSS JOIN LATERAL \
         (SELECT count(*) AS n FROM pages p WHERE p.author_id = a.id) l;",
    );
    assert!(errors(&d).is_empty(), "{d:?}");
}

#[test]
fn une_fonction_sans_colonnes_connues_reste_opaque() {
    let d = check("SELECT g.i, i FROM generate_series(1, 3) g (i);");
    assert!(errors(&d).is_empty(), "{d:?}");
    let d = check("SELECT x.anything FROM some_set_function() x;");
    assert!(
        d.iter().all(|d| d.code != codes::COLUMN_NOT_FOUND),
        "colonnes inconnues : rien à affirmer : {d:?}"
    );
}

#[test]
fn chaque_branche_d_union_est_verifiee() {
    let d = check("SELECT id FROM pages UNION SELECT nope FROM authors;");
    assert_eq!(errors(&d).len(), 1, "{d:?}");
}

#[test]
fn order_by_peut_citer_un_alias_de_sortie() {
    let d = check("SELECT count(*) AS total FROM pages ORDER BY total;");
    assert!(errors(&d).is_empty(), "{d:?}");
}
