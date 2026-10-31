//! Oracle différentiel : le corpus `cases/` confronté à un vrai PostgreSQL 18.
//!
//! `make oracle` démarre un PostgreSQL 18 dans Docker et lance ces tests. En
//! CI, un service `postgres:18` joue ce rôle.

use std::path::{Path, PathBuf};

use pgweave_oracle::{
    CaseReport, URL_ENV, describe_case, describe_ours, load_cases, render, server_major_version,
};

fn cases_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("cases")
}

/// Sans serveur : le corpus doit au moins se charger et s'analyser côté
/// pgweave. Un cas mal formé casse la CI ordinaire, pas seulement l'oracle.
#[test]
fn le_corpus_se_charge() {
    let cases = load_cases(&cases_dir()).expect("corpus valide");
    assert!(!cases.is_empty(), "corpus vide");
    for case in &cases {
        assert!(!case.queries.is_empty(), "cas `{}` sans requête", case.name);
        assert_eq!(describe_ours(case).len(), case.queries.len());
    }
}

#[tokio::test]
#[ignore = "nécessite PostgreSQL 18 : `make oracle` (variable PGWEAVE_ORACLE_URL)"]
async fn le_corpus_confronte_a_postgresql() {
    let url = std::env::var(URL_ENV)
        .unwrap_or_else(|_| panic!("{URL_ENV} doit être défini : lancer `make oracle`"));

    let major = server_major_version(&url).await.expect("serveur joignable");
    assert_eq!(
        major, 18,
        "l'oracle de référence tourne sur PostgreSQL 18 (serveur : {major})"
    );

    let mut reports = Vec::new();
    for case in load_cases(&cases_dir()).expect("corpus valide") {
        let pg = describe_case(&url, &case)
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let ours = describe_ours(&case);
        reports.push(CaseReport { case, pg, ours });
    }

    let (report, totals) = render(&reports, major);
    assert_eq!(
        totals.rejected_by_pg, 0,
        "le corpus contient des requêtes invalides :\n{report}"
    );
    insta::assert_snapshot!("rapport", report);
}
