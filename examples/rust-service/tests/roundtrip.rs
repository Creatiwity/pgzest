//! Le service d'exemple contre un vrai PostgreSQL : chaque fonction générée
//! s'exécute, types et cardinalités tiennent à l'exécution.

use pgweave_example_rust_service::queries::{self, PageStatus};

#[tokio::test]
#[ignore = "nécessite PostgreSQL 18 : `make oracle` (variable PGWEAVE_ORACLE_URL)"]
async fn les_requetes_generees_s_executent() {
    let url = std::env::var("PGWEAVE_ORACLE_URL").expect("PGWEAVE_ORACLE_URL");
    let (admin, conn) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
        .await
        .expect("connexion");
    tokio::spawn(conn);
    let db = format!("pgweave_example_{}", std::process::id());
    for sql in [
        format!("DROP DATABASE IF EXISTS {db}"),
        format!("CREATE DATABASE {db}"),
    ] {
        admin.batch_execute(&sql).await.expect("base");
    }

    let mut config: tokio_postgres::Config = url.parse().expect("url");
    config.dbname(&db);
    let (mut client, conn) = config
        .connect(tokio_postgres::NoTls)
        .await
        .expect("connexion");
    tokio::spawn(conn);
    let schema = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/schema/schema.sql"))
        .expect("schéma");
    client
        .batch_execute(&schema)
        .await
        .expect("schéma appliqué");

    let author = uuid::Uuid::from_u128(42);
    let tags = vec!["rust".to_string(), "sql".to_string()];
    let inserted = queries::insert_page(
        &client,
        queries::InsertPageParams {
            slug: "hello-world",
            title: "Hello",
            tags: &tags,
            author: Some(author),
        },
    )
    .await
    .expect("insert_page");
    assert_eq!(inserted.status, PageStatus::Draft);

    let page = queries::page_by_id(&client, queries::PageByIdParams { id: inserted.id })
        .await
        .expect("page_by_id")
        .expect("la page existe");
    assert_eq!(page.slug, "hello-world");
    assert_eq!(
        page.tags,
        [Some("rust".to_string()), Some("sql".to_string())]
    );
    assert_eq!(page.author, Some(author));
    assert_eq!(page.published_on, None);

    let missing = queries::page_by_id(&client, queries::PageByIdParams { id: -1 })
        .await
        .expect("page_by_id");
    assert_eq!(missing, None);

    // Enum en paramètre : type du projet, préparé par le serveur.
    let drafts = queries::pages_by_status(
        &client,
        queries::PagesByStatusParams {
            status: PageStatus::Draft,
        },
    )
    .await
    .expect("pages_by_status");
    assert_eq!(drafts.len(), 1);

    // Transaction : même API, client ou transaction.
    let tx = client.transaction().await.expect("transaction");
    let day = time::Date::from_calendar_date(2026, time::Month::November, 2).expect("date");
    let published = queries::publish(
        &tx,
        queries::PublishParams {
            on: Some(day),
            slug: "hello-world",
        },
    )
    .await
    .expect("publish")
    .expect("page publiée");
    assert_eq!(published.published_on, Some(day));
    tx.commit().await.expect("commit");

    let n = queries::count_pages(&client).await.expect("count_pages");
    // `count(*)` n'est jamais NULL : champ non optionnel.
    assert_eq!(n.n, 1);
    let deleted = queries::delete_drafts(&client)
        .await
        .expect("delete_drafts");
    assert_eq!(deleted, 0);

    // Le domaine refuse un slug invalide : l'erreur de PostgreSQL remonte.
    let err = queries::insert_page(
        &client,
        queries::InsertPageParams {
            slug: "Pas Valide",
            title: "x",
            tags: &[],
            author: None,
        },
    )
    .await
    .expect_err("CHECK du domaine");
    assert!(matches!(err, pgweave_runtime::Error::Db { .. }), "{err}");

    drop(client);
    admin
        .batch_execute(&format!("DROP DATABASE {db} WITH (FORCE)"))
        .await
        .expect("nettoyage");
}
