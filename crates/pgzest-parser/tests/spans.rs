//! Every expression and every PL/pgSQL statement carries its position in the
//! document: a diagnostic must be able to designate exactly what it targets.
//!
//! The traversal goes through AST serialization: a `{kind, span}` node
//! is an expression or a PL/pgSQL statement, wherever
//! it is (DDL, DML, routine body…).

use serde_json::Value;

/// `{kind, span}` nodes without a position, with their kind.
fn unplaced(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Object(map) => {
            if map.len() == 2
                && let (Some(kind), Some(span)) = (map.get("kind"), map.get("span"))
                && span["start"] == 0
                && span["end"] == 0
            {
                let head = match kind {
                    Value::Object(k) => k.keys().next().cloned().unwrap_or_default(),
                    other => other.to_string(),
                };
                out.push(head);
            }
            for child in map.values() {
                unplaced(child, out);
            }
        }
        Value::Array(items) => items.iter().for_each(|i| unplaced(i, out)),
        _ => {}
    }
}

fn check(sql: &str) {
    let stmts = pgzest_parser::parse(sql).expect("parse");
    let json = serde_json::to_value(&stmts).unwrap();
    let mut out = Vec::new();
    unplaced(&json, &mut out);
    assert!(out.is_empty(), "nodes without a position: {out:?}\n{sql}");
}

#[test]
fn example_schema() {
    check(include_str!("../../../examples/schema.sql"));
}

#[test]
fn queries_and_modifications() {
    check(
        "CREATE TABLE t (a INT DEFAULT 1 CHECK (a > 0), b TEXT GENERATED ALWAYS AS (upper(c)) STORED, c TEXT);\n\
         SELECT a, count(*) FILTER (WHERE b IS NOT NULL), CASE WHEN a > 1 THEN 'x' END, a::text, ARRAY[1, 2][1]\n\
           FROM t WHERE a IN (SELECT 1) AND b LIKE 'x%' AND a = ANY ('{1}') GROUP BY a HAVING sum(a) > 0 ORDER BY 1 LIMIT 3;\n\
         WITH x AS (SELECT 1 AS a) INSERT INTO t (a) SELECT a FROM x ON CONFLICT (a) DO UPDATE SET a = excluded.a + 1 RETURNING a;\n\
         UPDATE t SET (a, c) = (1, 'z') WHERE a IS DISTINCT FROM 2;\n\
         DELETE FROM t WHERE EXISTS (SELECT 1 WHERE t.a = 1);\n\
         CREATE INDEX ON t (lower(c)) WHERE a > 0;\n\
         CALL p(1 + 2);",
    );
}

#[test]
fn plpgsql_body() {
    check(
        "CREATE FUNCTION f(n INT) RETURNS INT LANGUAGE plpgsql AS $$\n\
         DECLARE v INT := n * 2; r RECORD;\n\
         BEGIN\n\
           IF v > 1 THEN v := v - 1; ELSIF v = 0 THEN RETURN 0; END IF;\n\
           FOR i IN 1..n BY 2 LOOP v := v + i; END LOOP;\n\
           FOR r IN SELECT 1 AS x LOOP v := v + r.x; END LOOP;\n\
           SELECT count(*) INTO v FROM pg_class WHERE relname = 'x';\n\
           RAISE NOTICE 'v = %', v + 1;\n\
           ASSERT v >= 0, 'negative';\n\
           RETURN v;\n\
         EXCEPTION WHEN OTHERS THEN RETURN -1;\n\
         END $$;",
    );
}

#[test]
fn aggregate_star() {
    check("SELECT count(*) FROM t;");
    check("SELECT NULLIF(count(*), 0) FROM t;");
    check("SELECT count(*) FILTER (WHERE true) FROM t;");
    check("SELECT count(*) FILTER (WHERE true)::numeric FROM t;");
    check(
        "SELECT ROUND(count(*) FILTER (WHERE true)::NUMERIC / NULLIF(count(*), 0) * 100, 1) FROM t;",
    );
}
