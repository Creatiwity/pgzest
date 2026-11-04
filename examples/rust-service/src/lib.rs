//! Example service: the queries in `queries/` compiled into typed Rust
//! functions by `pgzest-build` (see `build.rs`).

pub mod queries {
    include!(concat!(env!("OUT_DIR"), "/pgzest_queries.rs"));
}

/// A page's title: inline query, analyzed at compile time.
pub async fn page_title<C: pgzest_runtime::GenericClient + ?Sized>(
    client: &C,
    id: i64,
) -> Result<Option<String>, pgzest_runtime::Error> {
    let row = pgzest_macros::query!("SELECT title FROM pages WHERE id = $id", id = id)
        .fetch(client)
        .await?;
    Ok(row.map(|r| r.title))
}

/// Renames an author's pages: inline query returning no row.
pub async fn rename_by_author<C: pgzest_runtime::GenericClient + ?Sized>(
    client: &C,
    author: uuid::Uuid,
    title: &str,
) -> Result<u64, pgzest_runtime::Error> {
    pgzest_macros::query!(
        "UPDATE pages SET title = $title WHERE author = $author",
        title = title,
        author = author,
    )
    .execute(client)
    .await
}

/// Published pages whose title matches: query read from
/// `sql/live_pages.sql`.
pub async fn live_pages<C: pgzest_runtime::GenericClient + ?Sized>(
    client: &C,
    pattern: &str,
) -> Result<Vec<String>, pgzest_runtime::Error> {
    let rows = pgzest_macros::query_file!("sql/live_pages.sql", pattern = pattern)
        .fetch(client)
        .await?;
    Ok(rows.into_iter().map(|r| r.title).collect())
}
