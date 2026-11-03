//! `NodeId`s are deterministic: same text, same identifiers, whatever
//! was parsed before (PLAN.md, principle 5).

use pgweave_parser::parse;

const SQL: &str = "
CREATE TABLE pages (id BIGINT PRIMARY KEY, title TEXT);
SELECT id, title FROM pages WHERE id = 1;
CREATE FUNCTION f(p BIGINT) RETURNS TEXT LANGUAGE plpgsql AS $$
DECLARE t TEXT;
BEGIN
    SELECT title INTO t FROM pages WHERE id = p;
    UPDATE pages SET title = t WHERE id = p;
    RETURN t;
END;
$$;
";

/// All `id`s of the AST, in serialized traversal order.
fn ids(sql: &str) -> Vec<u64> {
    let stmts = parse(sql).expect("parse");
    let json = serde_json::to_value(&stmts).expect("serialization");
    let mut out = Vec::new();
    collect(&json, &mut out);
    out
}

fn collect(v: &serde_json::Value, out: &mut Vec<u64>) {
    match v {
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                if k == "id"
                    && let Some(n) = v.as_u64()
                {
                    out.push(n);
                }
                collect(v, out);
            }
        }
        serde_json::Value::Array(items) => items.iter().for_each(|i| collect(i, out)),
        _ => {}
    }
}

#[test]
fn same_text_same_identifiers_whatever_the_history() {
    let first = ids(SQL);
    // Intermediate parses: with the former global counter, they shifted
    // all following identifiers.
    ids("SELECT 1; SELECT 2; SELECT 3;");
    ids(SQL);
    assert_eq!(ids(SQL), first);
    assert!(!first.is_empty());
}

#[test]
fn numbering_restarts_at_each_parse() {
    let ids = ids(SQL);
    assert_eq!(ids.iter().min(), Some(&1));
}

#[test]
fn identifiers_are_unique_within_a_parse_plpgsql_body_included() {
    let ids = ids(SQL);
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        ids.len(),
        "a PL/pgSQL body parsed during the file continues its numbering"
    );
}

#[test]
fn deterministic_across_threads() {
    let expected = ids(SQL);
    let handles: Vec<_> = (0..4).map(|_| std::thread::spawn(|| ids(SQL))).collect();
    for h in handles {
        assert_eq!(h.join().unwrap(), expected);
    }
}
