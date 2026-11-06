//! Windows and aggregate modifiers: their expressions are checked like
//! the rest of the query. They used to be dropped during conversion — a
//! nonexistent column in a `PARTITION BY` passed.

use pgzest_analysis::check_sql_self_contained;
use pgzest_diagnostics::{Diagnostic, codes};

const SCHEMA: &str = "CREATE TABLE t (a INT, b TEXT, c INT);\n";

fn columns_not_found(sql: &str) -> Vec<Diagnostic> {
    check_sql_self_contained(&format!("{SCHEMA}{sql}"))
        .into_iter()
        .filter(|d| d.code == codes::COLUMN_NOT_FOUND)
        .collect()
}

#[test]
fn window_expressions_are_checked() {
    for sql in [
        "SELECT row_number() OVER (PARTITION BY nope) FROM t;",
        "SELECT row_number() OVER (ORDER BY nope) FROM t;",
        "SELECT sum(a) OVER w FROM t WINDOW w AS (PARTITION BY nope);",
        "SELECT string_agg(b, ',' ORDER BY nope) FROM t;",
        "SELECT count(*) FILTER (WHERE nope > 0) FROM t;",
        "SELECT percentile_cont(0.5) WITHIN GROUP (ORDER BY nope) FROM t;",
    ] {
        assert_eq!(columns_not_found(sql).len(), 1, "{sql}");
    }
}

#[test]
fn correct_windows_produce_nothing() {
    let d = columns_not_found(
        "SELECT a, sum(c) OVER (PARTITION BY b ORDER BY a ROWS BETWEEN 1 PRECEDING AND CURRENT ROW),\n\
         string_agg(b, ',' ORDER BY a) OVER w\n\
         FROM t WINDOW w AS (PARTITION BY c);",
    );
    assert!(d.is_empty(), "{d:?}");
}
