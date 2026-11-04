---
title: "Getting started"
description: "Try the demo, then set up a project of your own."
---

pgzest is not published to crates.io or npm yet: you build it from the
repository.

## Try the demo

Prerequisites: [Rust](https://rustup.rs) (the version is pinned by
`rust-toolchain.toml`), Node.js 18 or later for the VS Code extension, and
Docker for PostgreSQL 18.

```sh
git clone https://github.com/Creatiwity/pgzest
cd pgzest
make bootstrap   # Rust and npm dependencies, checks the tooling
make demo        # VS Code on examples/demo, with the pgzest extension
make demo-run    # the demo's Rust program against PostgreSQL 18 (Docker)
```

`examples/demo` is a small ordering application written like a real
project — a declarative schema, generated migrations, named queries, Rust
code using them, a traced trigger. Its `README.md` is a guided tour: hover
a query, break a column name, generate a migration, read a trace.

`make demo` opens VS Code with the extension loaded in development mode:
nothing is installed in your VS Code.

## Set up a project

A project is a directory with a `pgzest.toml`; the CLI and the editor
find it by walking up from the file they work on.

```toml
# pgzest.toml
[postgres]
version = 18

[schema]
directory = "schema"     # the CREATE TABLEs, functions, triggers…

[queries]
directory = "queries"    # the named queries
```

The schema can also come from existing migrations — numbered files,
Prisma or Diesel layouts, read as they are:

```toml
[migrations]
directory = "prisma/migrations"
```

Check the project:

```sh
cargo build --release -p pgzest        # the CLI, in target/release/pgzest
pgzest check schema/*.sql queries/*.sql
```

Every option: [`pgzest.toml`](../reference/config/).

## Next

- Generate Rust from the queries: [Typed queries in Rust](../guides/rust/).
- Move the schema to declarative files and generate migrations:
  [Schema and migrations](../guides/migrations/).
- Install the editor support: [Editor](../guides/editor/).
