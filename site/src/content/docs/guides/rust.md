---
title: "Typed queries in Rust"
description: "Named queries and inline SQL compiled into typed Rust over tokio-postgres."
---

Full example:
[`examples/rust-service`](https://github.com/Creatiwity/pgweave/tree/main/examples/rust-service),
run against PostgreSQL 18 in CI.

A project's SQL queries become typed Rust functions, at compile time and
**without a database connection**. Types come from the catalog that
pgweave rebuilds by replaying the project's schema and migrations
(`pgweave.toml`). Execution goes through `tokio-postgres`.

Three ways to write a query:

- **named query** in `queries/**/*.sql`, compiled by the `queries!` macro
  or by `build.rs`: this is the main form (generated enums, `COPY`,
  fragments, shared rows), one Rust module per file;
- **`sql!`**, a query defined in Rust code: same generation as a named
  query, the string containing only the SQL;
- **`query!`**, inline in an expression, for a one-off query.

## Setup

```toml
# pgweave.toml
[postgres]
version = 18

[schema]
directory = "schema"        # or [migrations] directory = "migrations"

[queries]
directory = "queries"
```

```toml
# Cargo.toml
[dependencies]
pgweave-runtime = { path = "…/crates/pgweave-runtime" }
pgweave-macros  = { path = "…/crates/pgweave-macros" }   # queries!, query!
tokio-postgres  = { version = "0.7", features = ["with-time-0_3", "with-uuid-1"] }
time = "0.3"      # dates (or chrono, see below)
uuid = "1"        # if the schema has uuids
```

```rust
// src/queries.rs — all content is generated at compile time
pgweave_macros::queries!("queries");
```

```rust
// src/lib.rs
pub mod queries;
// queries/order/book.sql, query `get` → crate::queries::order::book::get
```

`queries!` takes a directory (one module per file and per subdirectory,
PostgreSQL enums at the root) or a file (its queries at the call site).
Queries can live close to the code that uses them: declare their directory
in `[queries] include`, then
`pgweave_macros::queries!("src/order/queries");` in `src/order/mod.rs`.
Each file read is tracked; a file **added** to a directory is only seen
when the module calling the macro is recompiled.

### Queries in code: `sql!`

```rust
// src/inline.rs
pgweave_macros::sql! {
    /// A restaurant's dishes, from cheapest to most expensive.
    pub dishes_by_price: many -> crate::queries::menu::DishCardRow,
    r#"SELECT @dish_card(d) FROM dishes d
       WHERE d.restaurant_id = $restaurant ORDER BY d.price"#
}
// crate::inline::dishes_by_price(&client, DishesByPriceParams { restaurant })
```

- One query per macro; name, cardinality (optional, inferred otherwise),
  documentation and visibility are parameters, the string contains only
  the SQL (`@name` fragments included).
- `-> Path` reuses the row struct of another query, `sql!` or file, by its
  Rust path. Compatibility (names, order, types, nullability never
  loosened) is checked at compile time, through constant assertions:
  `` `price` can be null here, but not in
  crate::queries::menu::DishCardRow ``.
- Editing the string reruns the analysis immediately, in the editor too
  (the macro's input changes); "go to definition" leads to the name
  written in the macro.

In VS Code, the pgweave extension treats the string like a query file: SQL
highlighting (`$param`, `@fragment` included), diagnostics positioned in
the string, hover (descriptor on the name, a column's type, fragment),
completion. It disables rust-analyzer's semantic tokens on strings
(`rust-analyzer.semanticHighlighting.strings.enable`), which would cover
the highlighting. Outside a pgweave project (`pgweave.toml`), a `query!`
with the same name (sqlx…) is not analyzed.

### Enums: `#[types]`

A `sql!` query that uses a PostgreSQL enum cannot define it itself (two
queries in the same module would define it twice). The project's enums are
generated once, in a declared module:

```toml
# pgweave.toml
[rust]
types = "crate::db"
```

```rust
// src/lib.rs
#[pgweave_macros::types]
pub mod db {}
// crate::db::DishKind, crate::db::OrderStatus…
```

With `[rust] types`, `queries!` and `build.rs` also refer to it instead of
generating their own enums.

### Refreshing in the editor

rust-analyzer only reruns a macro if its input changes. For `sql!` and
`query!`, the input is the SQL: every change is picked up as you type. For
`queries!("directory")`, the input is the path: a change to a `.sql` is
only seen by rust-analyzer when the server restarts (`cargo check` always
sees it). A schema change is in the same situation for all forms.

Macro-free variant, from `build.rs` (the whole `[queries] directory`,
including files added to it, tracked by Cargo):

```rust
// build.rs  ([build-dependencies] pgweave-build)
fn main() {
    pgweave_build::Builder::new()
        .generate()
        .unwrap_or_else(|e| panic!("pgweave: {e}"));
}

// src/lib.rs
pub mod queries {
    include!(concat!(env!("OUT_DIR"), "/pgweave_queries.rs"));
}
```

The schema comes from declarative files (`[schema]`) or from migrations
(`[migrations] directory`), replayed in order. Recognized formats:
numbered files (`0001_init.sql`), Prisma
(`<timestamp>_<name>/migration.sql`), Diesel (`<date>_<name>/up.sql`,
`down.sql` files are never applied). An existing Prisma project therefore
plugs in without conversion:

```toml
[migrations]
directory = "packages/models/prisma/migrations"
```

A query the analysis cannot describe **fails the build**, with its
position in the SQL file. Changing the schema, a migration or a query
reruns the generation: rust-analyzer follows, with no watcher to start.
`pgweave describe --rust queries/x.sql` prints the generated code without
compiling; `pgweave describe` alone prints the JSON descriptors.

## Named queries

File format: [named queries](../../reference/named-queries/).

```sql
-- name: page_by_id
SELECT id, slug, title, status, published_on FROM pages WHERE id = $id;
```

becomes:

```rust
pub struct PageByIdParams { pub id: i64 }
pub struct PageByIdRow {
    pub id: i64,
    pub slug: String,
    pub title: String,
    pub status: PageStatus,               // PostgreSQL enum → Rust enum
    pub published_on: Option<time::Date>, // nullable column → Option
}
pub async fn page_by_id<C: GenericClient + ?Sized>(client: &C, params: PageByIdParams)
    -> Result<Option<PageByIdRow>, pgweave_runtime::Error>;
```

`client` is anything implementing `GenericClient`: client, transaction,
pool connection.

### Cardinality

The return type follows the number of rows, **inferred** conservatively:

| Query | Cardinality | Return |
|---|---|---|
| equalities on all columns of a primary key, a unique constraint or a unique index (without predicate) | `optional` | `Option<Row>` |
| `INSERT … VALUES (one row) RETURNING` | `one` | `Row` |
| aggregate without `GROUP BY` (`count(*)`…) | `one` | `Row` |
| `LIMIT 1` | `optional` | `Option<Row>` |
| without `RETURNING` | `none` | `u64` (rows affected) |
| otherwise | `many` | `Vec<Row>`, and `x_stream` as a stream |

A declaration in the header (`-- name: x :one`) **narrows** the inference:
`:one` on an `optional` query asserts that the row exists, and this is
checked at run time (`Error::RowCount`). It cannot contradict it (`:none`
on a query that returns rows is an error).

### Nullability

Never assumed:

- **column**: `T` only if its non-nullability is established (`NOT NULL`
  preserved through joins, `count(*)`, literal…), otherwise `Option<T>`;
- **parameter**: assigned to a column (`VALUES`, `SET`), it takes that
  column's nullability (`Option<T>` for a nullable column); elsewhere
  (`WHERE id = $id`), it is expected non-null.

### Types

| PostgreSQL | Rust |
|---|---|
| `bool` | `bool` |
| `int2`, `int4`, `int8` | `i16`, `i32`, `i64` |
| `float4`, `float8` | `f32`, `f64` |
| `numeric` | `rust_decimal::Decimal` |
| `text`, `varchar`, `char(n)`, `name`, `citext` | `String` (parameter: `&str`) |
| `bytea` | `Vec<u8>` (parameter: `&[u8]`) |
| `uuid` | `uuid::Uuid` |
| `json`, `jsonb` | `serde_json::Value` |
| `date`, `time`, `timestamp`, `timestamptz` | `time::Date`, `time::Time`, `time::PrimitiveDateTime`, `time::OffsetDateTime` (or `chrono`) |
| array `T[]` | `Vec<Option<T>>` (parameter: `&[T]`) |
| project enum | generated Rust enum (PascalCase variants) |
| domain | type of its base type |

A type missing from the table is a **refusal**, never a fallback: declare
it in `build.rs`.

```rust
pgweave_build::Builder::new()
    .temporal(pgweave_codegen::rust::Temporal::Chrono)
    .type_override("public.geometry", "geo_types::Geometry<f64>")
    .generate()
```

Column names become snake_case fields (`"createdAt"` → `created_at`). A
column without a Rust name (`?column?`) or two columns with the same name
are refused: give them an alias.

### Execution

- All parameter types built in: `query_typed`, in a single round trip.
- Project type as a parameter (enum): the query is prepared by the server,
  which fixes its types (one more round trip).
- Measured cost (`make bench`): identical to hand-written `tokio-postgres`
  (ratio 0.995 on `page_by_id`).

## `COPY … FROM STDIN`

A named query can be a `COPY`:

```sql
-- name: load_staging
COPY etablissements_staging (siret, nom, effectif, ouvert) FROM STDIN (FORMAT binary);

-- name: load_staging_csv
COPY etablissements_staging FROM STDIN (FORMAT csv, HEADER true);
```

- binary format: `load_staging(&tx, records)` where `records` iterates over
  `LoadStagingRecord { siret: &str, nom: &str, effectif: Option<i32>, … }`;
- text or CSV: `load_staging_csv(&tx, reader)` where `reader` is a
  `tokio::io::AsyncRead`, passed through as-is.

`COPY` requires a `Client` or a `Transaction` (`CopyClient` trait).
Generated columns are not fed (and refused if listed). The example chains
staging, `COPY` and an `INSERT … ON CONFLICT` switchover in a transaction.

## `query!`

```rust
let title = pgweave_macros::query!("SELECT title FROM pages WHERE id = $id", id = id)
    .fetch(&client)          // optional → Option<Row>
    .await?
    .map(|r| r.title);

let n = pgweave_macros::query!(
    "UPDATE pages SET title = $title WHERE author = $author",
    title = "Hello",
    author = author,
)
.execute(&client)            // no row → u64
.await?;
```

- named parameters (`$name`), bound with `name = value`; each value is
  checked against the expected type;
- `fetch` returns what the cardinality promises; `fetch_one` /
  `fetch_optional` narrow it (checked at run time);
- a SQL error is a compile error on the string;
- no enum or `COPY` inline: they require unique types in the crate, hence
  a named query.

`query_file!("sql/live_pages.sql", pattern = p)` does the same with a query
read from a file of the crate (path relative to its `Cargo.toml`): the file
is included for recompilation, and an error is positioned inside it
(`sql/live_pages.sql:5:7: [column-not-found] …`).

## Errors

`pgweave_runtime::Error`: `Db` (PostgreSQL or connection error),
`RowCount` (number of rows contrary to the cardinality), `Decode`
(undecodable value), `Io` (reading `COPY` data). Each one names the query.
