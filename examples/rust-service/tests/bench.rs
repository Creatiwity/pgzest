//! Run-time cost of the generated code versus hand-written `tokio-postgres`
//! (PLAN.md P5: target gap negligible). Same query, same connection,
//! alternating rounds to smooth out noise; the report is shown with
//! `--nocapture`.

use std::time::{Duration, Instant};

use pgweave_example_rust_service::queries;
use tokio_postgres::types::Type;

const ITERATIONS: usize = 2_000;
const ROUNDS: usize = 5;

#[tokio::test]
#[ignore = "requires PostgreSQL 18: `make bench` (PGWEAVE_ORACLE_URL variable)"]
async fn generated_code_vs_tokio_postgres() {
    let url = std::env::var("PGWEAVE_ORACLE_URL").expect("PGWEAVE_ORACLE_URL");
    let (admin, conn) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
        .await
        .expect("connection");
    tokio::spawn(conn);
    let db = format!("pgweave_bench_{}", std::process::id());
    for sql in [
        format!("DROP DATABASE IF EXISTS {db}"),
        format!("CREATE DATABASE {db}"),
    ] {
        admin.batch_execute(&sql).await.expect("database");
    }
    let mut config: tokio_postgres::Config = url.parse().expect("url");
    config.dbname(&db);
    let (client, conn) = config
        .connect(tokio_postgres::NoTls)
        .await
        .expect("connection");
    tokio::spawn(conn);
    // Desugared schema (fragments), as pgweave analyzes it.
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let project = pgweave_build::load_project(root).expect("project");
    let schema = std::fs::read_to_string(root.join("schema/schema.sql")).expect("schema");
    let schema = pgweave_macro::expand_source(&schema, &project.macros).sql;
    client.batch_execute(&schema).await.expect("schema applied");
    client
        .batch_execute(
            "INSERT INTO pages (slug, title, tags) \
             SELECT 'p-' || i, 'Page ' || i, ARRAY['a', 'b'] FROM generate_series(1, 100) i",
        )
        .await
        .expect("data");

    let generated = || async {
        let start = Instant::now();
        for i in 0..ITERATIONS {
            let id = (i % 100) as i64 + 1;
            let page = queries::pages::page_by_id(&client, queries::pages::PageByIdParams { id })
                .await
                .expect("page_by_id");
            std::hint::black_box(page);
        }
        start.elapsed()
    };
    let by_hand = || async {
        let start = Instant::now();
        for i in 0..ITERATIONS {
            let id = (i % 100) as i64 + 1;
            let row = client
                .query_typed_opt(
                    "SELECT id, slug, title, status, tags, author, published_on FROM pages WHERE id = $1",
                    &[(&id, Type::INT8)],
                )
                .await
                .expect("query");
            let page = row.map(|r| {
                (
                    r.get::<_, i64>(0),
                    r.get::<_, String>(1),
                    r.get::<_, String>(2),
                    r.get::<_, queries::PageStatus>(3),
                    r.get::<_, Vec<Option<String>>>(4),
                    r.get::<_, Option<uuid::Uuid>>(5),
                    r.get::<_, Option<time::Date>>(6),
                )
            });
            std::hint::black_box(page);
        }
        start.elapsed()
    };

    // Warm-up, then alternating rounds; keep the best of each.
    generated().await;
    by_hand().await;
    let (mut best_gen, mut best_hand) = (Duration::MAX, Duration::MAX);
    for _ in 0..ROUNDS {
        best_gen = best_gen.min(generated().await);
        best_hand = best_hand.min(by_hand().await);
    }
    let per = |d: Duration| d.as_secs_f64() * 1e6 / ITERATIONS as f64;
    let ratio = best_gen.as_secs_f64() / best_hand.as_secs_f64();
    println!(
        "page_by_id, {ITERATIONS} executions, best of {ROUNDS} rounds:\n  \
         generated code  {:.1} µs/query\n  tokio-postgres  {:.1} µs/query\n  ratio           {ratio:.3}",
        per(best_gen),
        per(best_hand)
    );
    // Loose safeguard: a gross regression (preparing on every call,
    // needless copying) would clearly exceed this threshold.
    assert!(ratio < 1.25, "generated code {ratio:.2}× slower");

    drop(client);
    admin
        .batch_execute(&format!("DROP DATABASE {db} WITH (FORCE)"))
        .await
        .expect("cleanup");
}
