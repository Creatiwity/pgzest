//! Example service: the queries in `queries/` compiled into typed Rust
//! functions by `pgweave-build` (see `build.rs`).

pub mod queries {
    include!(concat!(env!("OUT_DIR"), "/pgweave_queries.rs"));
}

/// A page's title: inline query, analyzed at compile time.
pub async fn page_title<C: pgweave_runtime::GenericClient + ?Sized>(
    client: &C,
    id: i64,
) -> Result<Option<String>, pgweave_runtime::Error> {
    let row = pgweave_macros::query!("SELECT title FROM pages WHERE id = $id", id = id)
        .fetch(client)
        .await?;
    Ok(row.map(|r| r.title))
}

/// Renames an author's pages: inline query returning no row.
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

/// Published pages whose title matches: query read from
/// `sql/live_pages.sql`.
pub async fn live_pages<C: pgweave_runtime::GenericClient + ?Sized>(
    client: &C,
    pattern: &str,
) -> Result<Vec<String>, pgweave_runtime::Error> {
    let rows = pgweave_macros::query_file!("sql/live_pages.sql", pattern = pattern)
        .fetch(client)
        .await?;
    Ok(rows.into_iter().map(|r| r.title).collect())
}
