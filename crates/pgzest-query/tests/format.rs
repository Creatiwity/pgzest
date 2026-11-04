//! Named-query format (`site/src/content/docs/reference/named-queries.md`).

use pgzest_query::{Cardinality, Params, QueryFile, parse_query_file};

fn codes(f: &QueryFile) -> Vec<&'static str> {
    f.diagnostics.iter().map(|d| d.code).collect()
}

fn ok(src: &str) -> QueryFile {
    let f = parse_query_file(src);
    assert!(
        f.diagnostics.is_empty(),
        "no diagnostic expected, got: {:?}",
        f.diagnostics
    );
    f
}

#[test]
fn a_file_splits_into_queries() {
    let f = ok("\
-- Queries on pages.

-- name: page_by_id :optional
-- A page by its identifier.
SELECT id, title FROM pages WHERE id = $id;

-- name: rename_page
UPDATE pages SET title = $title WHERE id = $id RETURNING id;
");
    assert_eq!(f.queries.len(), 2);

    let q = &f.queries[0];
    assert_eq!(q.name, "page_by_id");
    assert_eq!(q.cardinality, Some(Cardinality::Optional));
    assert_eq!(q.doc, vec!["A page by its identifier."]);
    assert_eq!(q.sql, "SELECT id, title FROM pages WHERE id = $id");
    assert_eq!(
        q.positional_sql(),
        "SELECT id, title FROM pages WHERE id = $1"
    );

    let q = &f.queries[1];
    assert_eq!(q.name, "rename_page");
    assert_eq!(q.cardinality, None);
    let Params::Named(params) = &q.params else {
        panic!("named parameters expected: {:?}", q.params)
    };
    let names: Vec<&str> = params.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["title", "id"], "order of first appearance");
    assert_eq!(
        q.positional_sql(),
        "UPDATE pages SET title = $1 WHERE id = $2 RETURNING id"
    );
}

#[test]
fn ranges_target_the_file() {
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
fn a_repeated_parameter_is_a_single_parameter() {
    let f = ok("-- name: q\nSELECT * FROM t WHERE a = $x OR b = $x OR c = $y;\n");
    assert_eq!(
        f.queries[0].positional_sql(),
        "SELECT * FROM t WHERE a = $1 OR b = $1 OR c = $2"
    );
}

#[test]
fn casts_and_accents_do_not_shift_the_rewrite() {
    // Non-ASCII on purpose: positions are byte offsets.
    let f = ok("-- name: q\nSELECT 'éèà' AS x, $id::bigint, $name;\n");
    assert_eq!(
        f.queries[0].positional_sql(),
        "SELECT 'éèà' AS x, $1::bigint, $2"
    );
}

#[test]
fn strings_and_dollar_bodies_are_neither_headers_nor_parameters() {
    let f = ok("\
-- name: q
SELECT '-- name: trap', '$not_a_param', $tag$ $not_either $tag$, $real;
");
    assert_eq!(f.queries.len(), 1);
    let Params::Named(p) = &f.queries[0].params else {
        panic!()
    };
    assert_eq!(p.len(), 1);
    assert_eq!(p[0].name, "real");
}

#[test]
fn positional_parameters_are_accepted_without_gaps() {
    let f = ok("-- name: q\nSELECT $2, $1;\n");
    assert_eq!(f.queries[0].params, Params::Positional { count: 2 });
    assert_eq!(f.queries[0].positional_sql(), "SELECT $2, $1");

    let f = parse_query_file("-- name: q\nSELECT $1, $3;\n");
    assert_eq!(codes(&f), ["query-parameter-gap"]);
    assert!(
        f.queries.is_empty(),
        "query with invalid parameters not returned"
    );
}

#[test]
fn mixing_named_and_positional_is_an_error() {
    let f = parse_query_file("-- name: q\nSELECT $id, $1;\n");
    assert_eq!(codes(&f), ["query-mixed-parameters"]);
    assert!(f.queries.is_empty());
}

#[test]
fn malformed_parameters() {
    let f = parse_query_file("-- name: q\nSELECT $pageId;\n");
    assert_eq!(codes(&f), ["query-parameter-invalid"]);

    let f = parse_query_file("-- name: q\nSELECT $0;\n");
    assert_eq!(codes(&f), ["query-parameter-invalid"]);
}

#[test]
fn without_parameters() {
    let f = ok("-- name: q\nSELECT 1\n");
    assert_eq!(f.queries[0].params, Params::None);
    assert_eq!(f.queries[0].sql, "SELECT 1", "the trailing `;` is optional");
}

#[test]
fn sql_outside_a_query_reported_once() {
    let f = parse_query_file("SELECT 1;\nSELECT 2;\n-- name: q\nSELECT 3;\n");
    assert_eq!(codes(&f), ["query-without-name"]);
    assert_eq!(f.queries.len(), 1, "the named query remains usable");
}

#[test]
fn a_single_statement_per_query() {
    let f = parse_query_file("-- name: q\nSELECT 1;\nSELECT 2;\nSELECT 3;\n");
    assert_eq!(codes(&f), ["query-multiple-statements"]);
}

#[test]
fn empty_query() {
    let f = parse_query_file("-- name: a\n-- name: b\nSELECT 1;\n");
    assert_eq!(codes(&f), ["query-empty"]);
    assert_eq!(f.queries.len(), 1);
    assert_eq!(f.queries[0].name, "b");
}

#[test]
fn duplicate_names() {
    let src = "-- name: q\nSELECT 1;\n-- name: q\nSELECT 2;\n";
    let f = parse_query_file(src);
    assert_eq!(codes(&f), ["query-duplicate-name"]);
    assert_eq!(f.queries.len(), 1);
    assert_eq!(f.queries[0].sql, "SELECT 1", "the first definition is kept");
    let at = f.diagnostics[0].span.as_ref().unwrap().start;
    assert_eq!(at, src.rfind("q\n").unwrap(), "the second one is reported");
}

#[test]
fn invalid_headers() {
    for (src, why) in [
        ("-- name: PageById\nSELECT 1;\n", "case"),
        ("-- name: 2pages\nSELECT 1;\n", "leading digit"),
        ("-- name:\nSELECT 1;\n", "no name"),
        ("-- name: q :first\nSELECT 1;\n", "unknown cardinality"),
        ("-- name: q :one extra\nSELECT 1;\n", "extra text"),
        ("SELECT 1; -- name: q\n", "not at line start"),
    ] {
        let f = parse_query_file(src);
        assert!(
            codes(&f).contains(&"query-header-invalid"),
            "{why}: {:?}",
            f.diagnostics
        );
    }
}

#[test]
fn all_cardinalities() {
    for c in Cardinality::ALL {
        let f = ok(&format!("-- name: q :{}\nSELECT 1;\n", c.as_str()));
        assert_eq!(f.queries[0].cardinality, Some(c));
    }
}

#[test]
fn an_unreadable_file_is_reported() {
    let f = parse_query_file("-- name: q\nSELECT 'unterminated;\n");
    assert_eq!(codes(&f), ["parse-error"]);
}

#[test]
fn header_with_row_reference() {
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
fn invalid_row_reference() {
    for (src, expected) in [
        ("-- name: a ->\nSELECT 1;\n", "without a query name"),
        ("-- name: a -> Card\nSELECT 1;\n", "invalid query name"),
        ("-- name: a -> card extra\nSELECT 1;\n", "unexpected text"),
    ] {
        let f = parse_query_file(src);
        assert!(
            f.diagnostics.iter().any(|d| d.message.contains(expected)),
            "{src:?}: {:?}",
            f.diagnostics
        );
    }
}
