//! Scénario de démonstration, sur une base neuve :
//!
//! ```text
//! make oracle-up                 # PostgreSQL 18 dans Docker (port 54318)
//! cargo run -p pgweave-demo
//! ```
//!
//! `DATABASE_URL` désigne un autre serveur (la base `pgweave_demo` y est
//! recréée).

use pgweave_demo::queries::{self, DishKind, OrderStatus};
use pgweave_runtime::futures_util::StreamExt;

const DEFAULT_URL: &str = "postgres://postgres:pgweave@localhost:54318/postgres";
const DB: &str = "pgweave_demo";

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn kind(k: DishKind) -> &'static str {
    match k {
        DishKind::Starter => "entrée",
        DishKind::Main => "plat",
        DishKind::Dessert => "dessert",
        DishKind::Drink => "boisson",
    }
}

fn euros(cents: i64) -> String {
    format!("{},{:02} €", cents / 100, cents % 100)
}

#[tokio::main]
async fn main() -> Result<()> {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| DEFAULT_URL.to_string());
    let mut client = fresh_database(&url).await?;

    println!("── Restaurants");
    let bistro = queries::create_restaurant(
        &client,
        queries::CreateRestaurantParams {
            slug: "le-bistrot",
            name: "Le Bistrot",
            opened_on: time::Date::from_calendar_date(2021, time::Month::March, 15).ok(),
        },
    )
    .await?;
    let ramen = queries::create_restaurant(
        &client,
        queries::CreateRestaurantParams {
            slug: "ramen-ya",
            name: "Ramen-ya",
            opened_on: None,
        },
    )
    .await?;
    println!("   le-bistrot = {}, ramen-ya = {}", bistro.id, ramen.id);
    let found = queries::restaurant_by_slug(
        &client,
        queries::RestaurantBySlugParams { slug: "le-bistrot" },
    )
    .await?;
    println!(
        "   restaurant_by_slug(le-bistrot) → {:?}",
        found.map(|r| r.name)
    );

    println!("── Plats : COPY CSV, puis un ajout typé");
    let csv =
        tokio::fs::File::open(concat!(env!("CARGO_MANIFEST_DIR"), "/data/dishes.csv")).await?;
    let copied = queries::import_dishes(&client, csv).await?;
    println!("   {copied} plats importés par COPY");
    let tags = ["maison".to_string(), "saison".to_string()];
    let dish = queries::add_dish(
        &client,
        queries::AddDishParams {
            restaurant: bistro.id,
            name: "Île flottante",
            kind: DishKind::Dessert,
            price: 700,
            vegetarian: true,
            tags: &tags,
        },
    )
    .await?;
    println!("   « Île flottante » ajouté (id {})", dish.id);

    println!("── Menu du Bistrot (lu en flux, depuis la vue `menu`)");
    let mut menu = std::pin::pin!(
        queries::menu_of_stream(
            &client,
            queries::MenuOfParams {
                restaurant: "le-bistrot"
            }
        )
        .await?
    );
    while let Some(item) = menu.next().await {
        let item = item?;
        // Colonnes d'une vue : PostgreSQL n'y garde pas NOT NULL → Option.
        println!(
            "   {:<24} {:<8} {:>9}{}",
            item.name.unwrap_or_default(),
            item.kind.map_or("?", kind),
            item.price.map_or_else(String::new, |p| euros(p.into())),
            if item.vegetarian == Some(true) {
                "  (végé)"
            } else {
                ""
            }
        );
    }
    // Deux requêtes, une même fiche (`-> dish_card`) : un seul type Rust.
    let veggie = queries::vegetarian_dishes(
        &client,
        queries::VegetarianDishesParams {
            restaurant: "le-bistrot",
        },
    )
    .await?;
    let affordable = queries::affordable_dishes(
        &client,
        queries::AffordableDishesParams {
            restaurant: bistro.id,
        },
    )
    .await?;
    let names = |cards: &[queries::DishCardRow]| {
        cards
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    println!("   végétariens : {}", names(&veggie));
    println!("   à moins de 10 € : {}", names(&affordable));
    let counts = queries::dish_counts(
        &client,
        queries::DishCountsParams {
            restaurant: bistro.id,
        },
    )
    .await?;
    println!(
        "   {} plats, dont {} végétariens, le moins cher à {}",
        counts.total,
        counts.vegetarian,
        counts
            .cheapest
            .map_or_else(|| "—".into(), |c| euros(c.into()))
    );
    if let Some((name, price)) = pgweave_demo::priciest_dish(&client, bistro.id).await? {
        println!("   le plus cher (query!) : {name}, {}", euros(price.into()));
    }

    println!("── Client (email citext : la casse ne compte pas)");
    let alice = queries::upsert_customer(
        &client,
        queries::UpsertCustomerParams {
            email: "alice@example.org",
            full_name: "Alice Martin",
            phone: None,
        },
    )
    .await?;
    let again = queries::upsert_customer(
        &client,
        queries::UpsertCustomerParams {
            email: "ALICE@example.org",
            full_name: "Alice Martin-Durand",
            phone: Some("+33 6 12 34 56 78"),
        },
    )
    .await?;
    println!("   même client : {}", alice.id == again.id);

    println!("── Commande, dans une transaction");
    let tx = client.transaction().await?;
    let order = queries::open_order(
        &tx,
        queries::OpenOrderParams {
            restaurant: bistro.id,
            customer: Some(alice.id),
        },
    )
    .await?;
    for (dish, quantity) in [(2, 2), (4, 1), (dish.id, 2)] {
        queries::add_item(
            &tx,
            queries::AddItemParams {
                order: order.id,
                quantity,
                dish,
            },
        )
        .await?;
    }
    tx.commit().await?;
    println!("   commande {} ({:?})", order.id, order.status);

    for status in [OrderStatus::Placed, OrderStatus::Served] {
        let updated = queries::set_status(
            &client,
            queries::SetStatusParams {
                status,
                id: order.id,
            },
        )
        .await?
        .ok_or("commande introuvable")?;
        println!("   → {:?} à {}", updated.status, updated.updated_at.time());
    }

    let summary = queries::order_summary(&client, queries::OrderSummaryParams { id: order.id })
        .await?
        .ok_or("commande introuvable")?;
    println!(
        "   résumé : {} ligne(s), total {}, client {}",
        summary.lines,
        summary.total_cents.map_or_else(|| "—".into(), euros),
        summary.customer.as_deref().unwrap_or("anonyme")
    );
    let alice = queries::customer_by_email(
        &client,
        queries::CustomerByEmailParams {
            email: "alice@EXAMPLE.org",
        },
    )
    .await?
    .ok_or("cliente introuvable")?;
    println!(
        "   {} a {} point(s) de fidélité (crédités par le trigger)",
        alice.full_name, alice.loyalty_points
    );

    println!("── Chiffre d'affaires servi, par type de plat");
    for r in queries::revenue_by_kind(&client).await? {
        println!(
            "   {:<8} {:>10}",
            kind(r.kind),
            r.revenue_cents.map_or_else(|| "—".into(), euros)
        );
    }

    let purged = queries::purge_drafts(
        &client,
        queries::PurgeDraftsParams {
            before: time::OffsetDateTime::now_utc(),
        },
    )
    .await?;
    println!("── {purged} brouillon(s) supprimé(s)");
    Ok(())
}

/// Recrée la base `pgweave_demo` et y applique migrations puis schéma.
async fn fresh_database(url: &str) -> Result<tokio_postgres::Client> {
    let connect = |config: tokio_postgres::Config| async move {
        let (client, conn) = config.connect(tokio_postgres::NoTls).await?;
        tokio::spawn(async move {
            if let Err(e) = conn.await {
                eprintln!("connexion : {e}");
            }
        });
        Ok::<_, tokio_postgres::Error>(client)
    };
    let config: tokio_postgres::Config = url.parse()?;
    let admin = connect(config.clone()).await.map_err(|e| {
        format!("{e} — PostgreSQL est-il lancé ? (`make oracle-up`, ou DATABASE_URL)")
    })?;
    admin
        .batch_execute(&format!("DROP DATABASE IF EXISTS {DB} WITH (FORCE)"))
        .await?;
    admin
        .batch_execute(&format!("CREATE DATABASE {DB}"))
        .await?;
    let mut config = config;
    config.dbname(DB);
    let client = connect(config).await?;
    for dir in ["migrations", "schema"] {
        let mut files: Vec<_> = std::fs::read_dir(format!("{}/{dir}", env!("CARGO_MANIFEST_DIR")))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|e| e == "sql"))
            .collect();
        files.sort();
        for f in files {
            client.batch_execute(&std::fs::read_to_string(&f)?).await?;
        }
    }
    println!("Base {DB} recréée, migrations et schéma appliqués.\n");
    Ok(client)
}
