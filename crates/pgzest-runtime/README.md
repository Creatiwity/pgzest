# pgzest-runtime

Runs the queries [pgzest](https://github.com/Creatiwity/pgzest) generates,
on [`tokio-postgres`](https://crates.io/crates/tokio-postgres).

The generated code describes each query — positional SQL, parameter
types, row decoding, number of rows — and this crate executes it:
`fetch` returns what the analysis promises (`T`, `Option<T>`, `Vec<T>`),
`execute` the number of affected rows. Features:

- `trace`: the run logs of the instrumented routines, as trees of runs;
- `opentelemetry`: the same runs as OpenTelemetry spans.

The code itself comes from [`pgzest-macros`](https://crates.io/crates/pgzest-macros)
(`queries!`, `query!`) or [`pgzest-build`](https://crates.io/crates/pgzest-build)
(`build.rs`). Guide: [typed queries in Rust](https://github.com/Creatiwity/pgzest/blob/main/site/src/content/docs/languages/rust.md).

Licensed under either of Apache-2.0 or MIT, at your option.
