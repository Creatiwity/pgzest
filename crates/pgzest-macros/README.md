# pgzest-macros

Typed PostgreSQL queries in Rust, checked at compile time against your
schema and migrations — without a database connection.

```rust
// src/queries.rs: one module per file of queries/
pgzest_macros::queries!("queries");

// inline, with named parameters
let page = pgzest_macros::query!(
    "SELECT id, title FROM pages WHERE id = $id",
    id = page_id,
)
.fetch(&client) // the cardinality is optional → Option<Row>
.await?;
```

The project is the one of the nearest `pgzest.toml`: its schema is
replayed on a model of the PostgreSQL catalog, and each query is typed as
PostgreSQL would type it, nullability included. An SQL error is a compile
error on the string. The generated code runs on
[`pgzest-runtime`](https://crates.io/crates/pgzest-runtime).

Guide: [typed queries in Rust](https://github.com/Creatiwity/pgzest/blob/main/site/src/content/docs/languages/rust.md).
Part of [pgzest](https://github.com/Creatiwity/pgzest).

Licensed under either of Apache-2.0 or MIT, at your option.
