//! Les `NodeId` sont déterministes : même texte, mêmes identifiants, quoi
//! qu'on ait parsé avant (PLAN.md, principe 5).

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

/// Tous les `id` de l'AST, dans l'ordre du parcours sérialisé.
fn ids(sql: &str) -> Vec<u64> {
    let stmts = parse(sql).expect("parse");
    let json = serde_json::to_value(&stmts).expect("sérialisation");
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
fn meme_texte_memes_identifiants_quel_que_soit_l_historique() {
    let first = ids(SQL);
    // Parses intermédiaires : avec l'ancien compteur global, ils décalaient
    // tous les identifiants suivants.
    ids("SELECT 1; SELECT 2; SELECT 3;");
    ids(SQL);
    assert_eq!(ids(SQL), first);
    assert!(!first.is_empty());
}

#[test]
fn la_numerotation_recommence_a_chaque_parse() {
    let ids = ids(SQL);
    assert_eq!(ids.iter().min(), Some(&1));
}

#[test]
fn les_identifiants_sont_uniques_dans_un_parse_corps_plpgsql_compris() {
    let ids = ids(SQL);
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        ids.len(),
        "un corps PL/pgSQL parsé pendant le fichier continue sa numérotation"
    );
}

#[test]
fn deterministe_entre_threads() {
    let expected = ids(SQL);
    let handles: Vec<_> = (0..4).map(|_| std::thread::spawn(|| ids(SQL))).collect();
    for h in handles {
        assert_eq!(h.join().unwrap(), expected);
    }
}
