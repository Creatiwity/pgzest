//! Fenêtres et modificateurs d'agrégat : leurs expressions sont vérifiées
//! comme le reste de la requête. Elles étaient auparavant jetées à la
//! conversion — une colonne inexistante dans un `PARTITION BY` passait.

use pgweave_checker::check_sql_self_contained;
use pgweave_diagnostics::{Diagnostic, codes};

const SCHEMA: &str = "CREATE TABLE t (a INT, b TEXT, c INT);\n";

fn columns_not_found(sql: &str) -> Vec<Diagnostic> {
    check_sql_self_contained(&format!("{SCHEMA}{sql}"))
        .into_iter()
        .filter(|d| d.code == codes::COLUMN_NOT_FOUND)
        .collect()
}

#[test]
fn les_expressions_de_fenetre_sont_verifiees() {
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
fn des_fenetres_correctes_ne_produisent_rien() {
    let d = columns_not_found(
        "SELECT a, sum(c) OVER (PARTITION BY b ORDER BY a ROWS BETWEEN 1 PRECEDING AND CURRENT ROW),\n\
         string_agg(b, ',' ORDER BY a) OVER w\n\
         FROM t WINDOW w AS (PARTITION BY c);",
    );
    assert!(d.is_empty(), "{d:?}");
}
