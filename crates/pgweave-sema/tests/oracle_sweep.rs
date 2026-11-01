//! Balayage différentiel de la résolution : des combinaisons d'opérateurs et
//! de fonctions courants sur un échantillon de types, préparées par
//! PostgreSQL 18 (sans exécution) et résolues par pgweave. Le type du
//! résultat, ou le refus (introuvable, ambigu), doit être le même.
//!
//! `make oracle` (variable `PGWEAVE_ORACLE_URL`).

use pgweave_catalog::{Catalog, SearchPath};
use pgweave_sema::{ResolveError, Ty, TypeSystem, pg};

/// Types échantillonnés ; `unknown` est le littéral `NULL`.
const SAMPLES: &[&str] = &[
    "unknown",
    "int2",
    "int4",
    "int8",
    "numeric",
    "float4",
    "float8",
    "text",
    "varchar",
    "bpchar",
    "name",
    "bool",
    "date",
    "time",
    "timestamp",
    "timestamptz",
    "interval",
    "json",
    "jsonb",
    "uuid",
    "bytea",
    "_int4",
    "_text",
    "int4range",
];

const BINARY_OPS: &[&str] = &[
    "+", "-", "*", "/", "%", "=", "<>", "<", ">=", "||", "~~", "->", "->>", "@>", "&&", "?",
];
const PREFIX_OPS: &[&str] = &["-", "@", "~"];
const UNARY_FUNCS: &[&str] = &[
    "abs",
    "round",
    "trunc",
    "floor",
    "length",
    "lower",
    "upper",
    "md5",
    "cardinality",
    "jsonb_typeof",
    "to_jsonb",
    "to_json",
    "age",
    "isfinite",
    "sum",
    "avg",
    "max",
    "count",
    "array_agg",
    "bool_and",
    "concat",
    "json_build_array",
];
const BINARY_FUNCS: &[&str] = &[
    "round",
    "to_char",
    "date_trunc",
    "date_part",
    "age",
    "array_length",
    "array_append",
    "array_prepend",
    "array_cat",
    "array_position",
    "string_agg",
    "generate_series",
    "concat",
    "jsonb_build_object",
    "strpos",
    "repeat",
];

fn literal(sample: &str) -> String {
    if sample == "unknown" {
        "NULL".into()
    } else {
        format!("NULL::pg_catalog.{sample}")
    }
}

/// Ce que dit PostgreSQL : type du résultat, ou SQLSTATE du refus.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Type(String),
    NotFound,
    Ambiguous,
    Other(String),
}

impl Outcome {
    fn ours(r: Result<Option<Ty>, ResolveError>) -> Self {
        match r {
            Ok(Some(t)) => Outcome::Type(t.name),
            Ok(None) => Outcome::Other("sans résultat".into()),
            Err(ResolveError::NotFound) => Outcome::NotFound,
            Err(ResolveError::Ambiguous) => Outcome::Ambiguous,
            Err(ResolveError::Polymorphic(m)) => Outcome::Other(m),
        }
    }
}

async fn prepare(client: &tokio_postgres::Client, sql: &str) -> Outcome {
    match client.prepare(sql).await {
        Ok(stmt) => Outcome::Type(stmt.columns()[0].type_().name().to_string()),
        Err(e) => match e.as_db_error() {
            Some(db) if db.code().code() == "42883" => Outcome::NotFound,
            Some(db) if db.code().code() == "42725" => Outcome::Ambiguous,
            Some(db) => Outcome::Other(db.message().to_string()),
            None => Outcome::Other(e.to_string()),
        },
    }
}

#[tokio::test]
#[ignore = "nécessite PostgreSQL 18 : `make oracle` (variable PGWEAVE_ORACLE_URL)"]
async fn resolution_confrontee_a_postgresql() {
    let url = std::env::var("PGWEAVE_ORACLE_URL").expect("PGWEAVE_ORACLE_URL");
    let (client, conn) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
        .await
        .expect("connexion");
    tokio::spawn(conn);

    let catalog = Catalog::for_postgres(18).expect("pack");
    let ts = TypeSystem::new(&catalog);
    let path = SearchPath::default();
    let ty = |s: &str| pg(s);

    let mut cases: Vec<(String, Outcome)> = Vec::new();
    for l in SAMPLES {
        for r in SAMPLES {
            for op in BINARY_OPS {
                let sql = format!("SELECT {} {op} {}", literal(l), literal(r));
                let ours = ts
                    .resolve_operator(None, op, Some(&ty(l)), &ty(r), &path)
                    .map(|c| c.returns);
                cases.push((sql, Outcome::ours(ours)));
            }
            for f in BINARY_FUNCS {
                let sql = format!("SELECT {f}({}, {})", literal(l), literal(r));
                let ours = ts
                    .resolve_function(None, f, &[ty(l), ty(r)], &path)
                    .map(|c| c.returns);
                cases.push((sql, Outcome::ours(ours)));
            }
        }
        for op in PREFIX_OPS {
            let sql = format!("SELECT {op} {}", literal(l));
            let ours = ts
                .resolve_operator(None, op, None, &ty(l), &path)
                .map(|c| c.returns);
            cases.push((sql, Outcome::ours(ours)));
        }
        for f in UNARY_FUNCS {
            let sql = format!("SELECT {f}({})", literal(l));
            let ours = ts
                .resolve_function(None, f, &[ty(l)], &path)
                .map(|c| c.returns);
            cases.push((sql, Outcome::ours(ours)));
        }
    }

    let mut agree = 0;
    let mut diffs = Vec::new();
    for (sql, ours) in cases {
        let theirs = prepare(&client, &sql).await;
        let same = match (&theirs, &ours) {
            // Un refus pour une autre raison (incohérence polymorphe…) :
            // les deux refusent, le message n'est pas comparé.
            (Outcome::Other(_), Outcome::Other(_)) => true,
            (a, b) => a == b,
        };
        if same {
            agree += 1;
        } else {
            diffs.push(format!(
                "{sql}\n    PostgreSQL : {theirs:?}\n    pgweave    : {ours:?}"
            ));
        }
    }
    let total = agree + diffs.len();
    assert!(
        diffs.is_empty(),
        "{} écart(s) sur {total} :\n{}",
        diffs.len(),
        diffs
            .iter()
            .take(60)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
    eprintln!("{agree}/{total} résolutions identiques à PostgreSQL");
}
