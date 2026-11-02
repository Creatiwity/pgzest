//! Précédence des opérateurs : chaque expression est relue par pgweave puis
//! émise entièrement parenthésée ; PostgreSQL évalue les deux formes sur un
//! jeu de valeurs et doit trouver le même résultat. Un écart de précédence
//! du parser change le sens d'une requête sans rien signaler.

use pgweave_oracle::URL_ENV;

/// Expressions sur `a b c d` (booléens), `x y z w` (entiers), `s t u v`
/// (textes).
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

/// Jeux de valeurs : chacun doit donner le même résultat aux deux formes.
const ROWS: &str = "(VALUES
    (true, false, true, false, 1, 2, 3, 4, 'a', 'b', 'ab', 'x'),
    (false, true, NULL, true, -2, 3, 2, 7, 'xa', 'x%', NULL, 'ab'),
    (NULL, NULL, false, NULL, 0, 5, 1, 0, '', 'x', 'x', '')
) AS q(a, b, c, d, x, y, z, w, s, t, u, v)";

#[tokio::test]
#[ignore = "nécessite PostgreSQL 18 : `make oracle` (variable PGWEAVE_ORACLE_URL)"]
async fn la_precedence_des_operateurs_est_celle_de_postgresql() {
    let url = std::env::var(URL_ENV)
        .unwrap_or_else(|_| panic!("{URL_ENV} doit être défini : lancer `make oracle`"));
    let (client, conn) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
        .await
        .expect("connexion");
    tokio::spawn(conn);

    let mut divergent = Vec::new();
    for e in EXPRESSIONS {
        let stmts = pgweave_parser::parse(&format!("SELECT {e}")).expect("parse");
        let emitted = pgweave_ast::emit::emit_statement(&stmts[0]);
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
        "lectures divergentes de PostgreSQL :\n{}",
        divergent.join("\n")
    );
}
