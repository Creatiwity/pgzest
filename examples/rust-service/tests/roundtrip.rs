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
    // Même requête en flux.
    {
        use pgweave_runtime::futures_util::StreamExt;
        let mut stream = std::pin::pin!(
            queries::pages_by_status_stream(
                &client,
                queries::PagesByStatusParams {
                    status: PageStatus::Draft,
                },
            )
            .await
            .expect("pages_by_status_stream")
        );
        let first = stream.next().await.expect("une ligne").expect("décodée");
        assert_eq!(first, drafts[0]);
        assert!(stream.next().await.is_none());
    }

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

    // Requêtes inline (`query!`).
    let title = pgweave_example_rust_service::page_title(&client, inserted.id)
        .await
        .expect("page_title");
    assert_eq!(title.as_deref(), Some("Hello"));
    let renamed = pgweave_example_rust_service::rename_by_author(&client, author, "Bonjour")
        .await
        .expect("rename_by_author");
    assert_eq!(renamed, 1);

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

    // Chargement par lots : COPY binaire et CSV dans la staging, bascule,
    // le tout dans une transaction.
    let tx = client.transaction().await.expect("transaction");
    let records = [
        ("12345678900011", "Alpha", Some(3)),
        ("12345678900029", "Beta", None),
    ];
    let copied = queries::load_staging(
        &tx,
        records
            .iter()
            .map(|(siret, nom, effectif)| queries::LoadStagingRecord {
                siret,
                nom,
                effectif: *effectif,
                ouvert: Some(day),
            }),
    )
    .await
    .expect("load_staging");
    assert_eq!(copied, 2);
    let csv: &[u8] = b"siret,nom,effectif,ouvert\n12345678900037,Gamma,,2026-01-02\n";
    let copied = queries::load_staging_csv(&tx, csv)
        .await
        .expect("load_staging_csv");
    assert_eq!(copied, 1);
    let swapped = queries::swap_from_staging(&tx)
        .await
        .expect("swap_from_staging");
    assert_eq!(swapped, 3);
    queries::clear_staging(&tx).await.expect("clear_staging");
    tx.commit().await.expect("commit");
    let n = queries::count_etablissements(&client).await.expect("count");
    assert_eq!(n.n, 3);

    drop(client);
    admin
        .batch_execute(&format!("DROP DATABASE {db} WITH (FORCE)"))
        .await
        .expect("nettoyage");
}
