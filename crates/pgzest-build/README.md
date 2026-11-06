# pgzest-build

The named queries of a [pgzest](https://github.com/Creatiwity/pgzest)
project, compiled into typed Rust from `build.rs` — without a database
connection.

```rust
// build.rs
fn main() {
    pgzest_build::Builder::new()
        .generate()
        .unwrap_or_else(|e| panic!("pgzest: {e}"));
}

// src/lib.rs
pub mod queries {
    include!(concat!(env!("OUT_DIR"), "/pgzest_queries.rs"));
}
```

The project is the one of the nearest `pgzest.toml`: its schema is
replayed, and each query of `[queries] directory` becomes a function with
its parameter and row types. A query the analysis cannot describe fails
the build, with its position; changing the schema, a migration or a query
reruns the generation. The generated code runs on
[`pgzest-runtime`](https://crates.io/crates/pgzest-runtime). Without
`build.rs`: `queries!` in [`pgzest-macros`](https://crates.io/crates/pgzest-macros).

Guide: [typed queries in Rust](https://github.com/Creatiwity/pgzest/blob/main/site/src/content/docs/languages/rust.md).

Licensed under either of Apache-2.0 or MIT, at your option.
