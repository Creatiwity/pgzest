//! Resolution of the sources of a SELECT: CTEs, derived tables, functions,
//! LATERAL, correlated subqueries, set operations.

use pgzest_checker::check_sql_self_contained;
use pgzest_diagnostics::{Diagnostic, codes};

const SCHEMA: &str = "CREATE TABLE authors (id UUID PRIMARY KEY, name TEXT);\n\
                      CREATE TABLE pages (id BIGINT PRIMARY KEY, author_id UUID, title TEXT);\n";

fn check(query: &str) -> Vec<Diagnostic> {
    check_sql_self_contained(&format!("{SCHEMA}{query}"))
}

fn errors(d: &[Diagnostic]) -> Vec<&Diagnostic> {
    d.iter()
        .filter(|d| d.severity == pgzest_diagnostics::Severity::Error)
        .collect()
}

#[test]
fn ctes_are_sources() {
    let d = check("WITH recent AS (SELECT id, title FROM pages) SELECT r.title FROM recent r;");
    assert!(errors(&d).is_empty(), "{d:?}");
    let d = check("WITH recent AS (SELECT id FROM pages) SELECT r.title FROM recent r;");
    assert_eq!(errors(&d).len(), 1, "title missing from the CTE: {d:?}");
}

#[test]
fn a_recursive_cte_references_itself() {
    let d = check(
        "WITH RECURSIVE r (n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 3) SELECT n FROM r;",
    );
    assert!(errors(&d).is_empty(), "{d:?}");
}

#[test]
fn correlated_subquery() {
    let d = check(
        "SELECT a.name, (SELECT count(*) FROM pages p WHERE p.author_id = a.id) AS n FROM authors a;",
    );
    assert!(
        errors(&d).is_empty(),
        "reference to the enclosing query: {d:?}"
    );
    let d = check(
        "SELECT a.name FROM authors a WHERE EXISTS (SELECT 1 FROM pages p WHERE p.nope = a.id);",
    );
    assert_eq!(errors(&d).len(), 1, "{d:?}");
    assert_eq!(errors(&d)[0].code, codes::COLUMN_NOT_FOUND);
}

#[test]
fn columns_of_a_derived_table() {
    let d = check("SELECT s.n, s.t FROM (SELECT id AS n, title AS t FROM pages) s;");
    assert!(errors(&d).is_empty(), "{d:?}");
    let d = check("SELECT s.title FROM (SELECT id AS n FROM pages) s;");
    assert_eq!(errors(&d).len(), 1, "{d:?}");
    let d = check("SELECT s.x FROM (SELECT id FROM pages) AS s (x);");
    assert!(errors(&d).is_empty(), "column aliases: {d:?}");
}

#[test]
fn lateral_sees_preceding_sources() {
    let d = check(
        "SELECT a.name, l.n FROM authors a CROSS JOIN LATERAL \
         (SELECT count(*) AS n FROM pages p WHERE p.author_id = a.id) l;",
    );
    assert!(errors(&d).is_empty(), "{d:?}");
}

#[test]
fn a_function_without_known_columns_stays_opaque() {
    let d = check("SELECT g.i, i FROM generate_series(1, 3) g (i);");
    assert!(errors(&d).is_empty(), "{d:?}");
    let d = check("SELECT x.anything FROM some_set_function() x;");
    assert!(
        d.iter().all(|d| d.code != codes::COLUMN_NOT_FOUND),
        "unknown columns: nothing to assert: {d:?}"
    );
}

#[test]
fn each_union_branch_is_checked() {
    let d = check("SELECT id FROM pages UNION SELECT nope FROM authors;");
    assert_eq!(errors(&d).len(), 1, "{d:?}");
}

#[test]
fn order_by_can_reference_an_output_alias() {
    let d = check("SELECT count(*) AS total FROM pages ORDER BY total;");
    assert!(errors(&d).is_empty(), "{d:?}");
}
