//! pgweave demo: the queries in `queries/` become the [`queries`] module
//! (`src/queries.rs`), generated at compile time by the `queries!` macro:
//! one submodule per file (`queries::menu`, `queries::promotions::deals`).
//!
//! To see the generated code: `pgweave describe --rust queries/*.sql`, or
//! "Go to Definition" on a function of `queries` in VS Code.

pub mod inline;
/// Enums for the project's enumerated types (`dish_kind`, `order_status`…),
/// generated once: `[rust] types = "crate::db"` in pgweave.toml.
#[pgweave_macros::types]
pub mod db {}
pub mod queries;

/// A restaurant's most expensive dish: a query written inline, analyzed at
/// compile time like the others (try introducing a mistake into it).
pub async fn priciest_dish<C: pgweave_runtime::GenericClient + ?Sized>(
    client: &C,
    restaurant: i64,
) -> Result<Option<(String, i32)>, pgweave_runtime::Error> {
    let row = pgweave_macros::query!(
        "SELECT name, price FROM dishes WHERE restaurant_id = $restaurant ORDER BY price DESC LIMIT 1",
        restaurant = restaurant,
    )
    .fetch(client)
    .await?;
    Ok(row.map(|r| (r.name, r.price)))
}
