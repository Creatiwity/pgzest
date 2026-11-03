//! Format des requêtes nommées (`docs/named-queries.md`).

use pgweave_query::{Cardinality, Params, QueryFile, parse_query_file};

fn codes(f: &QueryFile) -> Vec<&'static str> {
    f.diagnostics.iter().map(|d| d.code).collect()
}

fn ok(src: &str) -> QueryFile {
    let f = parse_query_file(src);
    assert!(
        f.diagnostics.is_empty(),
        "aucun diagnostic attendu, obtenu : {:?}",
        f.diagnostics
    );
    f
}

#[test]
fn un_fichier_se_decoupe_en_requetes() {
    let f = ok("\
-- Requêtes sur les pages.

-- name: page_by_id :optional
-- Une page par son identifiant.
SELECT id, title FROM pages WHERE id = $id;

-- name: rename_page
UPDATE pages SET title = $title WHERE id = $id RETURNING id;
");
    assert_eq!(f.queries.len(), 2);

    let q = &f.queries[0];
    assert_eq!(q.name, "page_by_id");
    assert_eq!(q.cardinality, Some(Cardinality::Optional));
    assert_eq!(q.doc, vec!["Une page par son identifiant."]);
    assert_eq!(q.sql, "SELECT id, title FROM pages WHERE id = $id");
    assert_eq!(
        q.positional_sql(),
        "SELECT id, title FROM pages WHERE id = $1"
    );

    let q = &f.queries[1];
    assert_eq!(q.name, "rename_page");
    assert_eq!(q.cardinality, None);
    let Params::Named(params) = &q.params else {
        panic!("paramètres nommés attendus : {:?}", q.params)
    };
    let names: Vec<&str> = params.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["title", "id"], "ordre de première apparition");
    assert_eq!(
        q.positional_sql(),
        "UPDATE pages SET title = $1 WHERE id = $2 RETURNING id"
    );
}

#[test]
fn les_plages_visent_le_fichier() {
    let src = "-- name: q\nSELECT $id;\n";
    let f = ok(src);
    let q = &f.queries[0];
    assert_eq!(&src[q.name_range.clone()], "q");
    assert_eq!(&src[q.sql_range.clone()], "SELECT $id");
    let Params::Named(p) = &q.params else {
        panic!()
    };
    assert_eq!(&src[p[0].occurrences[0].clone()], "$id");
}

#[test]
fn un_parametre_repete_est_un_seul_parametre() {
    let f = ok("-- name: q\nSELECT * FROM t WHERE a = $x OR b = $x OR c = $y;\n");
    assert_eq!(
        f.queries[0].positional_sql(),
        "SELECT * FROM t WHERE a = $1 OR b = $1 OR c = $2"
    );
}

#[test]
fn cast_et_accents_ne_decalent_pas_la_reecriture() {
    let f = ok("-- name: q\nSELECT 'éèà' AS x, $id::bigint, $name;\n");
    assert_eq!(
        f.queries[0].positional_sql(),
        "SELECT 'éèà' AS x, $1::bigint, $2"
    );
}

#[test]
fn chaines_et_corps_dollar_ne_sont_ni_en_tetes_ni_parametres() {
    let f = ok("\
-- name: q
SELECT '-- name: piege', '$pas_un_param', $tag$ $non plus $tag$, $vrai;
");
    assert_eq!(f.queries.len(), 1);
    let Params::Named(p) = &f.queries[0].params else {
        panic!()
    };
    assert_eq!(p.len(), 1);
    assert_eq!(p[0].name, "vrai");
}

#[test]
fn les_parametres_positionnels_sont_acceptes_sans_trou() {
    let f = ok("-- name: q\nSELECT $2, $1;\n");
    assert_eq!(f.queries[0].params, Params::Positional { count: 2 });
    assert_eq!(f.queries[0].positional_sql(), "SELECT $2, $1");

    let f = parse_query_file("-- name: q\nSELECT $1, $3;\n");
    assert_eq!(codes(&f), ["query-parameter-gap"]);
    assert!(
        f.queries.is_empty(),
        "requête aux paramètres invalides non rendue"
    );
}

#[test]
fn melanger_nomme_et_positionnel_est_une_erreur() {
    let f = parse_query_file("-- name: q\nSELECT $id, $1;\n");
    assert_eq!(codes(&f), ["query-mixed-parameters"]);
    assert!(f.queries.is_empty());
}

#[test]
fn parametres_mal_formes() {
    let f = parse_query_file("-- name: q\nSELECT $pageId;\n");
    assert_eq!(codes(&f), ["query-parameter-invalid"]);

    let f = parse_query_file("-- name: q\nSELECT $0;\n");
    assert_eq!(codes(&f), ["query-parameter-invalid"]);
}

#[test]
fn sans_parametre() {
    let f = ok("-- name: q\nSELECT 1\n");
    assert_eq!(f.queries[0].params, Params::None);
    assert_eq!(f.queries[0].sql, "SELECT 1", "le `;` final est optionnel");
}

#[test]
fn sql_hors_requete_signale_une_fois() {
    let f = parse_query_file("SELECT 1;\nSELECT 2;\n-- name: q\nSELECT 3;\n");
    assert_eq!(codes(&f), ["query-without-name"]);
    assert_eq!(f.queries.len(), 1, "la requête nommée reste utilisable");
}

#[test]
fn un_seul_statement_par_requete() {
    let f = parse_query_file("-- name: q\nSELECT 1;\nSELECT 2;\nSELECT 3;\n");
    assert_eq!(codes(&f), ["query-multiple-statements"]);
}

#[test]
fn requete_vide() {
    let f = parse_query_file("-- name: a\n-- name: b\nSELECT 1;\n");
    assert_eq!(codes(&f), ["query-empty"]);
    assert_eq!(f.queries.len(), 1);
    assert_eq!(f.queries[0].name, "b");
}

#[test]
fn noms_en_double() {
    let src = "-- name: q\nSELECT 1;\n-- name: q\nSELECT 2;\n";
    let f = parse_query_file(src);
    assert_eq!(codes(&f), ["query-duplicate-name"]);
    assert_eq!(f.queries.len(), 1);
    assert_eq!(
        f.queries[0].sql, "SELECT 1",
        "la première définition est gardée"
    );
    let at = f.diagnostics[0].span.as_ref().unwrap().start;
    assert_eq!(at, src.rfind("q\n").unwrap(), "la seconde est signalée");
}

#[test]
fn en_tetes_invalides() {
    for (src, why) in [
        ("-- name: PageById\nSELECT 1;\n", "casse"),
        ("-- name: 2pages\nSELECT 1;\n", "chiffre initial"),
        ("-- name:\nSELECT 1;\n", "sans nom"),
        ("-- name: q :first\nSELECT 1;\n", "cardinalité inconnue"),
        ("-- name: q :one extra\nSELECT 1;\n", "texte en trop"),
        ("SELECT 1; -- name: q\n", "pas en début de ligne"),
    ] {
        let f = parse_query_file(src);
        assert!(
            codes(&f).contains(&"query-header-invalid"),
            "{why} : {:?}",
            f.diagnostics
        );
    }
}

#[test]
fn toutes_les_cardinalites() {
    for c in Cardinality::ALL {
        let f = ok(&format!("-- name: q :{}\nSELECT 1;\n", c.as_str()));
        assert_eq!(f.queries[0].cardinality, Some(c));
    }
}

#[test]
fn un_fichier_illisible_est_signale() {
    let f = parse_query_file("-- name: q\nSELECT 'non terminée;\n");
    assert_eq!(codes(&f), ["parse-error"]);
}

#[test]
fn en_tete_avec_ligne_de_reference() {
    let src = "-- name: a :many -> card\nSELECT 1 AS x;\n-- name: b -> card\nSELECT 1 AS x;\n";
    let f = parse_query_file(src);
    assert!(f.diagnostics.is_empty(), "{:?}", f.diagnostics);
    assert_eq!(f.queries[0].cardinality, Some(Cardinality::Many));
    assert_eq!(f.queries[0].row.as_ref().unwrap().0, "card");
    let (name, range) = f.queries[1].row.clone().unwrap();
    assert_eq!(name, "card");
    assert_eq!(range.start, src.rfind("card").unwrap());
    assert_eq!(&src[range], "card");
}

#[test]
fn ligne_de_reference_invalide() {
    for (src, attendu) in [
        ("-- name: a ->\nSELECT 1;\n", "sans nom"),
        ("-- name: a -> Card\nSELECT 1;\n", "nom de requête invalide"),
        ("-- name: a -> card en_trop\nSELECT 1;\n", "texte inattendu"),
    ] {
        let f = parse_query_file(src);
        assert!(
            f.diagnostics.iter().any(|d| d.message.contains(attendu)),
            "{src:?} : {:?}",
            f.diagnostics
        );
    }
}
