//! The example service against a real PostgreSQL: every generated function
//! runs, and types and cardinalities hold at run time.

use pgweave_example_rust_service::queries::{self, PageStatus};

#[tokio::test]
#[ignore = "requires PostgreSQL 18: `make oracle` (PGWEAVE_ORACLE_URL variable)"]
async fn generated_queries_run() {
    let url = std::env::var("PGWEAVE_ORACLE_URL").expect("PGWEAVE_ORACLE_URL");
    let (admin, conn) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
        .await
        .expect("connection");
    tokio::spawn(conn);
    let db = format!("pgweave_example_{}", std::process::id());
    for sql in [
        format!("DROP DATABASE IF EXISTS {db}"),
        format!("CREATE DATABASE {db}"),
    ] {
        admin.batch_execute(&sql).await.expect("database");
    }

    let mut config: tokio_postgres::Config = url.parse().expect("url");
    config.dbname(&db);
    let (mut client, conn) = config
        .connect(tokio_postgres::NoTls)
        .await
        .expect("connection");
    tokio::spawn(conn);
    // The schema uses fragments: it is applied desugared, as pgweave
    // analyzes it (what `pgweave build` would write).
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let project = pgweave_build::load_project(root).expect("project");
    let schema = std::fs::read_to_string(root.join("schema/schema.sql")).expect("schema");
    let schema = pgweave_macro::expand_source(&schema, &project.macros).sql;
    client.batch_execute(&schema).await.expect("schema applied");

    let author = uuid::Uuid::from_u128(42);
    let tags = vec!["rust".to_string(), "sql".to_string()];
    let inserted = queries::pages::insert_page(
        &client,
        queries::pages::InsertPageParams {
            slug: "hello-world",
            title: "Hello",
            tags: &tags,
            author: Some(author),
        },
    )
    .await
    .expect("insert_page");
    assert_eq!(inserted.status, PageStatus::Draft);

    let page =
        queries::pages::page_by_id(&client, queries::pages::PageByIdParams { id: inserted.id })
            .await
            .expect("page_by_id")
            .expect("the page exists");
    assert_eq!(page.slug, "hello-world");
    assert_eq!(
        page.tags,
        [Some("rust".to_string()), Some("sql".to_string())]
    );
    assert_eq!(page.author, Some(author));
    assert_eq!(page.published_on, None);

    let missing = queries::pages::page_by_id(&client, queries::pages::PageByIdParams { id: -1 })
        .await
        .expect("page_by_id");
    assert_eq!(missing, None);

    // Enum as a parameter: project type, prepared by the server.
    let drafts = queries::pages::pages_by_status(
        &client,
        queries::pages::PagesByStatusParams {
            status: PageStatus::Draft,
        },
    )
    .await
    .expect("pages_by_status");
    assert_eq!(drafts.len(), 1);
    // Same query as a stream.
    {
        use pgweave_runtime::futures_util::StreamExt;
        let mut stream = std::pin::pin!(
            queries::pages::pages_by_status_stream(
                &client,
                queries::pages::PagesByStatusParams {
                    status: PageStatus::Draft,
                },
            )
            .await
            .expect("pages_by_status_stream")
        );
        let first = stream.next().await.expect("one row").expect("decoded");
        assert_eq!(first, drafts[0]);
        assert!(stream.next().await.is_none());
    }

    // Transaction: same API, client or transaction.
    let tx = client.transaction().await.expect("transaction");
    let day = time::Date::from_calendar_date(2026, time::Month::November, 2).expect("date");
    let published = queries::pages::publish(
        &tx,
        queries::pages::PublishParams {
            on: Some(day),
            slug: "hello-world",
        },
    )
    .await
    .expect("publish")
    .expect("page published");
    assert_eq!(published.published_on, Some(day));
    tx.commit().await.expect("commit");

    // Inline queries (`query!`).
    let title = pgweave_example_rust_service::page_title(&client, inserted.id)
        .await
        .expect("page_title");
    assert_eq!(title.as_deref(), Some("Hello"));
    let renamed = pgweave_example_rust_service::rename_by_author(&client, author, "Howdy")
        .await
        .expect("rename_by_author");
    assert_eq!(renamed, 1);

    let live = pgweave_example_rust_service::live_pages(&client, "How%")
        .await
        .expect("live_pages");
    assert_eq!(live, ["Howdy"]);

    let n = queries::pages::count_pages(&client)
        .await
        .expect("count_pages");
    // `count(*)` is never NULL: non-optional field.
    assert_eq!(n.n, 1);
    let deleted = queries::pages::delete_drafts(&client)
        .await
        .expect("delete_drafts");
    assert_eq!(deleted, 0);

    // The domain rejects an invalid slug: the PostgreSQL error is surfaced.
    let err = queries::pages::insert_page(
        &client,
        queries::pages::InsertPageParams {
            slug: "Not Valid",
            title: "x",
            tags: &[],
            author: None,
        },
    )
    .await
    .expect_err("domain CHECK");
    assert!(matches!(err, pgweave_runtime::Error::Db { .. }), "{err}");

    // Batch loading: binary and CSV COPY into staging, swap, all inside a
    // transaction.
    let tx = client.transaction().await.expect("transaction");
    let records = [
        ("12345678900011", "Alpha", Some(3)),
        ("12345678900029", "Beta", None),
    ];
    let copied = queries::etablissements::load_staging(
        &tx,
        records.iter().map(
            |(siret, nom, effectif)| queries::etablissements::LoadStagingRecord {
                siret,
                nom,
                effectif: *effectif,
                ouvert: Some(day),
            },
        ),
    )
    .await
    .expect("load_staging");
    assert_eq!(copied, 2);
    let csv: &[u8] = b"siret,nom,effectif,ouvert\n12345678900037,Gamma,,2026-01-02\n";
    let copied = queries::etablissements::load_staging_csv(&tx, csv)
        .await
        .expect("load_staging_csv");
    assert_eq!(copied, 1);
    let swapped = queries::etablissements::swap_from_staging(&tx)
        .await
        .expect("swap_from_staging");
    assert_eq!(swapped, 3);
    queries::etablissements::clear_staging(&tx)
        .await
        .expect("clear_staging");
    tx.commit().await.expect("commit");
    let n = queries::etablissements::count_etablissements(&client)
        .await
        .expect("count");
    assert_eq!(n.n, 3);

    // Full-text search: the query's expression and the index's come from
    // the same fragment, so the planner uses the index.
    let found = queries::etablissements::search_etablissements(
        &client,
        queries::etablissements::SearchEtablissementsParams { q: "beta" },
    )
    .await
    .expect("search");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].nom, "Beta");
    client
        .batch_execute("SET enable_seqscan = off")
        .await
        .expect("enable_seqscan");
    // Simple protocol: the generic plan keeps `$1` without a value.
    let plan: Vec<String> = client
        .simple_query(&format!(
            "EXPLAIN (GENERIC_PLAN, COSTS OFF) {}",
            queries::etablissements::SEARCH_ETABLISSEMENTS.sql
        ))
        .await
        .expect("explain")
        .iter()
        .filter_map(|m| match m {
            tokio_postgres::SimpleQueryMessage::Row(r) => r.get(0).map(str::to_string),
            _ => None,
        })
        .collect();
    assert!(
        plan.iter().any(|l| l.contains("etablissements_search")),
        "the index is not used:\n{}",
        plan.join("\n")
    );

    drop(client);
    admin
        .batch_execute(&format!("DROP DATABASE {db} WITH (FORCE)"))
        .await
        .expect("cleanup");
}
