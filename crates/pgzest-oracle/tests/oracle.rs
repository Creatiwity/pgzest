//! Differential oracle: the `cases/` corpus checked against a real PostgreSQL 18.
//!
//! `make oracle` starts a PostgreSQL 18 in Docker and runs these tests. In
//! CI, a `postgres:18` service plays that role.

use std::path::{Path, PathBuf};

use pgzest_oracle::{
    CaseReport, URL_ENV, describe_case, describe_ours, load_cases, render, server_major_version,
};

fn cases_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("cases")
}

/// Without a server: the corpus must at least load and be analyzed on the
/// pgzest side. A malformed case breaks ordinary CI, not only the oracle.
#[test]
fn corpus_loads() {
    let cases = load_cases(&cases_dir()).expect("valid corpus");
    assert!(!cases.is_empty(), "empty corpus");
    for case in &cases {
        assert!(
            !case.queries.is_empty(),
            "case `{}` has no query",
            case.name
        );
        assert_eq!(describe_ours(case).len(), case.queries.len());
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL 18: `make oracle` (PGZEST_ORACLE_URL variable)"]
async fn corpus_checked_against_postgresql() {
    let url = std::env::var(URL_ENV)
        .unwrap_or_else(|_| panic!("{URL_ENV} must be set: run `make oracle`"));

    let major = server_major_version(&url).await.expect("reachable server");
    assert_eq!(
        major, 18,
        "the reference oracle runs on PostgreSQL 18 (server: {major})"
    );

    let mut reports = Vec::new();
    for case in load_cases(&cases_dir()).expect("valid corpus") {
        let pg = describe_case(&url, &case)
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let ours = describe_ours(&case);
        reports.push(CaseReport { case, pg, ours });
    }

    let (report, totals) = render(&reports, major);
    assert_eq!(
        totals.rejected_by_pg, 0,
        "the corpus contains invalid queries:\n{report}"
    );
    insta::assert_snapshot!("report", report);
}
