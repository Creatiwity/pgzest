//! Operator precedence: each expression is read back by pgzest then emitted
//! fully parenthesized; PostgreSQL evaluates both forms on a set of values
//! and must find the same result. A parser precedence gap changes the
//! meaning of a query without reporting anything.

use pgzest_oracle::URL_ENV;

/// Expressions over `a b c d` (booleans), `x y z w` (integers), `s t u v`
/// (texts).
const EXPRESSIONS: &[&str] = &[
    "a IS DISTINCT FROM b AND c",
    "a IS NOT DISTINCT FROM b OR c",
    "NOT a = b AND c",
    "NOT a AND b",
    "a = b IS NULL",
    "x BETWEEN y AND z AND c",
    "x NOT BETWEEN y AND z OR c",
    "x IN (y, z) AND c",
    "s LIKE t AND c",
    "s LIKE t || 'x'",
    "s || t LIKE 'x%'",
    "x + y * z = w",
    "a AND b OR c AND NOT d",
    "x < y IS TRUE",
    "- x ^ y",
    "- x * y",
    "x ^ y ^ z",
    "x - y - z",
    "x IS NULL = c",
    "NOT x IS NULL",
    "x::text || y",
    "-x::int",
    "x % y * z",
    "x << y + z",
    "x & y | z",
    "s || t = u || v",
    "x = ANY(ARRAY[y, z]) AND c",
    "x > y AND y > z OR NOT c",
];

/// Value sets: each must give the same result for both forms.
const ROWS: &str = "(VALUES
    (true, false, true, false, 1, 2, 3, 4, 'a', 'b', 'ab', 'x'),
    (false, true, NULL, true, -2, 3, 2, 7, 'xa', 'x%', NULL, 'ab'),
    (NULL, NULL, false, NULL, 0, 5, 1, 0, '', 'x', 'x', '')
) AS q(a, b, c, d, x, y, z, w, s, t, u, v)";

#[tokio::test]
#[ignore = "requires PostgreSQL 18: `make oracle` (PGZEST_ORACLE_URL variable)"]
async fn operator_precedence_matches_postgresql() {
    let url = std::env::var(URL_ENV)
        .unwrap_or_else(|_| panic!("{URL_ENV} must be set: run `make oracle`"));
    let (client, conn) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
        .await
        .expect("connection");
    tokio::spawn(conn);

    let mut divergent = Vec::new();
    for e in EXPRESSIONS {
        let stmts = pgzest_parser::parse(&format!("SELECT {e}")).expect("parse");
        let emitted = pgzest_ast::emit::emit_statement(&stmts[0]);
        let ours = emitted
            .trim_end_matches(';')
            .trim_start_matches("SELECT ")
            .to_string();
        let sql = format!(
            "SELECT bool_and(({e}) IS NOT DISTINCT FROM ({ours})) FROM (SELECT * FROM {ROWS}) r"
        );
        match client.query_one(&sql, &[]).await {
            Ok(row) if row.get::<_, Option<bool>>(0) == Some(true) => {}
            Ok(_) => divergent.push(format!("{e}  →  {ours}")),
            Err(err) => divergent.push(format!(
                "{e}  →  {ours} : {}",
                err.as_db_error()
                    .map_or(err.to_string(), |d| d.message().to_string())
            )),
        }
    }
    assert!(
        divergent.is_empty(),
        "readings diverging from PostgreSQL:\n{}",
        divergent.join("\n")
    );
}
