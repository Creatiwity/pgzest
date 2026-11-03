//! Demo scenario, on a fresh database:
//!
//! ```text
//! make oracle-up                 # PostgreSQL 18 in Docker (port 54318)
//! cargo run -p pgweave-demo
//! ```
//!
//! `DATABASE_URL` points to another server (the `pgweave_demo` database is
//! recreated there).

use pgweave_demo::db::{DishKind, OrderStatus};
use pgweave_demo::{inline, queries};
use pgweave_runtime::futures_util::StreamExt;

const DEFAULT_URL: &str = "postgres://postgres:pgweave@localhost:54318/postgres";
const DB: &str = "pgweave_demo";

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn kind(k: DishKind) -> &'static str {
    match k {
        DishKind::Starter => "starter",
        DishKind::Main => "main",
        DishKind::Dessert => "dessert",
        DishKind::Drink => "drink",
    }
}

fn euros(cents: i64) -> String {
    format!("€{}.{:02}", cents / 100, cents % 100)
}

#[tokio::main]
async fn main() -> Result<()> {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| DEFAULT_URL.to_string());
    let mut client = fresh_database(&url).await?;

    println!("── Restaurants");
    let bistro = queries::restaurants::create_restaurant(
        &client,
        queries::restaurants::CreateRestaurantParams {
            slug: "le-bistrot",
            name: "Le Bistrot",
            opened_on: time::Date::from_calendar_date(2021, time::Month::March, 15).ok(),
        },
    )
    .await?;
    let ramen = queries::restaurants::create_restaurant(
        &client,
        queries::restaurants::CreateRestaurantParams {
            slug: "ramen-ya",
            name: "Ramen-ya",
            opened_on: None,
        },
    )
    .await?;
    println!("   le-bistrot = {}, ramen-ya = {}", bistro.id, ramen.id);
    let found = queries::restaurants::restaurant_by_slug(
        &client,
        queries::restaurants::RestaurantBySlugParams { slug: "le-bistrot" },
    )
    .await?;
    println!(
        "   restaurant_by_slug(le-bistrot) → {:?}",
        found.map(|r| r.name)
    );

    let since = time::Date::from_calendar_date(2020, time::Month::January, 1)?;
    let recent = inline::opened_since(&client, inline::OpenedSinceParams { since }).await?;
    println!(
        "   open since 2020 (sql!): {}",
        recent
            .iter()
            .map(|r| r.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );

    println!("── Dishes: CSV COPY, then a typed insert");
    let csv =
        tokio::fs::File::open(concat!(env!("CARGO_MANIFEST_DIR"), "/data/dishes.csv")).await?;
    let copied = queries::menu::import_dishes(&client, csv).await?;
    println!("   {copied} dishes imported by COPY");
    let tags = ["homemade".to_string(), "seasonal".to_string()];
    let dish = queries::menu::add_dish(
        &client,
        queries::menu::AddDishParams {
            restaurant: bistro.id,
            name: "Floating island",
            kind: DishKind::Dessert,
            price: 700,
            vegetarian: true,
            tags: &tags,
        },
    )
    .await?;
    println!("   \"Floating island\" added (id {})", dish.id);

    println!("── Le Bistrot's menu (read as a stream, from the `menu` view)");
    let mut menu = std::pin::pin!(
        queries::menu::menu_of_stream(
            &client,
            queries::menu::MenuOfParams {
                restaurant: "le-bistrot"
            }
        )
        .await?
    );
    while let Some(item) = menu.next().await {
        let item = item?;
        // Columns of a view: PostgreSQL does not keep NOT NULL there → Option.
        println!(
            "   {:<24} {:<8} {:>9}{}",
            item.name.unwrap_or_default(),
            item.kind.map_or("?", kind),
            item.price.map_or_else(String::new, |p| euros(p.into())),
            if item.vegetarian == Some(true) {
                "  (veggie)"
            } else {
                ""
            }
        );
    }
    // Two queries, one shared card (`-> dish_card`): a single Rust type.
    let veggie = queries::menu::vegetarian_dishes(
        &client,
        queries::menu::VegetarianDishesParams {
            restaurant: "le-bistrot",
        },
    )
    .await?;
    let affordable = queries::promotions::deals::affordable_dishes(
        &client,
        queries::promotions::deals::AffordableDishesParams {
            restaurant: bistro.id,
        },
    )
    .await?;
    let names = |cards: &[queries::menu::DishCardRow]| {
        cards
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    println!("   vegetarian: {}", names(&veggie));
    println!("   under €10: {}", names(&affordable));
    // Query written in the code (`sql!`), same row struct.
    let by_price = inline::dishes_by_price(
        &client,
        inline::DishesByPriceParams {
            restaurant: bistro.id,
        },
    )
    .await?;
    println!("   by price (sql!): {}", names(&by_price));
    let counts = queries::menu::dish_counts(
        &client,
        queries::menu::DishCountsParams {
            restaurant: bistro.id,
        },
    )
    .await?;
    println!(
        "   {} dishes, {} of them vegetarian, cheapest at {}",
        counts.total,
        counts.vegetarian,
        counts
            .cheapest
            .map_or_else(|| "—".into(), |c| euros(c.into()))
    );
    if let Some((name, price)) = pgweave_demo::priciest_dish(&client, bistro.id).await? {
        println!(
            "   most expensive (query!): {name}, {}",
            euros(price.into())
        );
    }

    println!("── Customer (citext email: case does not matter)");
    let alice = queries::customers::upsert_customer(
        &client,
        queries::customers::UpsertCustomerParams {
            email: "alice@example.org",
            full_name: "Alice Martin",
            phone: None,
        },
    )
    .await?;
    let again = queries::customers::upsert_customer(
        &client,
        queries::customers::UpsertCustomerParams {
            email: "ALICE@example.org",
            full_name: "Alice Martin-Durand",
            phone: Some("+33 6 12 34 56 78"),
        },
    )
    .await?;
    println!("   same customer: {}", alice.id == again.id);

    println!("── Order, in a transaction");
    let tx = client.transaction().await?;
    let order = queries::orders::open_order(
        &tx,
        queries::orders::OpenOrderParams {
            restaurant: bistro.id,
            customer: Some(alice.id),
        },
    )
    .await?;
    for (dish, quantity) in [(2, 2), (4, 1), (dish.id, 2)] {
        queries::orders::add_item(
            &tx,
            queries::orders::AddItemParams {
                order: order.id,
                quantity,
                dish,
            },
        )
        .await?;
    }
    tx.commit().await?;
    println!("   order {} ({:?})", order.id, order.status);

    for status in [OrderStatus::Placed, OrderStatus::Served] {
        let updated = queries::orders::set_status(
            &client,
            queries::orders::SetStatusParams {
                status,
                id: order.id,
            },
        )
        .await?
        .ok_or("order not found")?;
        println!("   → {:?} at {}", updated.status, updated.updated_at.time());
    }

    let summary = queries::orders::order_summary(
        &client,
        queries::orders::OrderSummaryParams { id: order.id },
    )
    .await?
    .ok_or("order not found")?;
    println!(
        "   summary: {} line(s), total {}, customer {}",
        summary.lines,
        summary.total_cents.map_or_else(|| "—".into(), euros),
        summary.customer.as_deref().unwrap_or("anonymous")
    );
    let alice = queries::customers::customer_by_email(
        &client,
        queries::customers::CustomerByEmailParams {
            email: "alice@EXAMPLE.org",
        },
    )
    .await?
    .ok_or("customer not found")?;
    println!(
        "   {} has {} loyalty point(s) (credited by the trigger)",
        alice.full_name, alice.loyalty_points
    );

    // Optional filter (`$status?`): `None` does not filter.
    for status in [
        None,
        Some(OrderStatus::Served),
        Some(OrderStatus::Cancelled),
    ] {
        let orders = queries::orders::orders_of_restaurant(
            &client,
            queries::orders::OrdersOfRestaurantParams {
                restaurant: bistro.id,
                status,
            },
        )
        .await?;
        println!(
            "   Le Bistrot's orders, status {status:?}: {}",
            orders.len()
        );
    }

    // Pagination with variants: `[[ … ]]` filters and `{{ … }}` sort order,
    // each combination checked at compile time.
    use queries::orders::{OrdersPageParams, OrdersPageSort};
    for (status, since, sort) in [
        (None, None, OrdersPageSort::Recent),
        (Some(OrderStatus::Served), None, OrdersPageSort::Oldest),
        (
            None,
            Some(time::OffsetDateTime::now_utc() + time::Duration::days(1)),
            OrdersPageSort::Recent,
        ),
    ] {
        let page = queries::orders::orders_page(
            &client,
            OrdersPageParams {
                restaurant: bistro.id,
                status,
                since,
                limit: 20,
                sort,
            },
        )
        .await?;
        println!(
            "   page (status {status:?}, since {}, sort {sort:?}): {} order(s)",
            if since.is_some() {
                "tomorrow"
            } else {
                "forever"
            },
            page.len()
        );
    }

    println!("── Served revenue, by dish kind");
    for r in queries::orders::revenue_by_kind(&client).await? {
        println!(
            "   {:<8} {:>10}",
            kind(r.kind),
            r.revenue_cents.map_or_else(|| "—".into(), euros)
        );
    }

    let purged = queries::orders::purge_drafts(
        &client,
        queries::orders::PurgeDraftsParams {
            before: time::OffsetDateTime::now_utc(),
        },
    )
    .await?;
    println!("── {purged} draft(s) deleted");
    Ok(())
}

/// Recreates the `pgweave_demo` database and applies the migrations to it,
/// like a deployment: the declarative schema is their target, not a script.
async fn fresh_database(url: &str) -> Result<tokio_postgres::Client> {
    let connect = |config: tokio_postgres::Config| async move {
        let (client, conn) = config.connect(tokio_postgres::NoTls).await?;
        tokio::spawn(async move {
            if let Err(e) = conn.await {
                eprintln!("connection: {e}");
            }
        });
        Ok::<_, tokio_postgres::Error>(client)
    };
    let config: tokio_postgres::Config = url.parse()?;
    let admin = connect(config.clone())
        .await
        .map_err(|e| format!("{e} — is PostgreSQL running? (`make oracle-up`, or DATABASE_URL)"))?;
    admin
        .batch_execute(&format!("DROP DATABASE IF EXISTS {DB} WITH (FORCE)"))
        .await?;
    admin
        .batch_execute(&format!("CREATE DATABASE {DB}"))
        .await?;
    let mut config = config;
    config.dbname(DB);
    let client = connect(config).await?;
    for dir in ["migrations"] {
        let mut files: Vec<_> = std::fs::read_dir(format!("{}/{dir}", env!("CARGO_MANIFEST_DIR")))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|e| e == "sql"))
            .collect();
        files.sort();
        for f in files {
            client.batch_execute(&std::fs::read_to_string(&f)?).await?;
        }
    }
    println!("Database {DB} recreated, migrations applied.\n");
    Ok(client)
}
