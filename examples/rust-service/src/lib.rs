//! Service d'exemple : les requêtes de `queries/` compilées en fonctions
//! Rust typées par `pgweave-build` (voir `build.rs`).

pub mod queries {
    include!(concat!(env!("OUT_DIR"), "/pgweave_queries.rs"));
}

/// Titre d'une page : requête inline, analysée à la compilation.
pub async fn page_title<C: pgweave_runtime::GenericClient + ?Sized>(
    client: &C,
    id: i64,
) -> Result<Option<String>, pgweave_runtime::Error> {
    let row = pgweave_macros::query!("SELECT title FROM pages WHERE id = $id", id = id)
        .fetch(client)
        .await?;
    Ok(row.map(|r| r.title))
}

/// Renomme les pages d'un auteur : requête inline sans ligne renvoyée.
pub async fn rename_by_author<C: pgweave_runtime::GenericClient + ?Sized>(
    client: &C,
    author: uuid::Uuid,
    title: &str,
) -> Result<u64, pgweave_runtime::Error> {
    pgweave_macros::query!(
        "UPDATE pages SET title = $title WHERE author = $author",
        title = title,
        author = author,
    )
    .execute(client)
    .await
}
