//! Démo pgweave : les requêtes de `queries/` deviennent le module
//! [`queries`], généré à la compilation par `build.rs`.
//!
//! Pour voir le code généré : `pgweave describe --rust queries/*.sql`, ou
//! « Go to Definition » sur une fonction de `queries` dans VS Code.

pub mod queries {
    include!(concat!(env!("OUT_DIR"), "/pgweave_queries.rs"));
}

/// Plat le plus cher d'un restaurant : requête écrite en ligne, analysée à
/// la compilation comme les autres (essayez d'y introduire une faute).
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
