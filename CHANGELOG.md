# Changelog

All notable changes to pgzest. From 0.1.0 on, the entries are generated
from the commit messages ([Conventional Commits](https://www.conventionalcommits.org),
[git-cliff](https://git-cliff.org), see [RELEASING.md](./RELEASING.md));
versions follow [Semantic Versioning](https://semver.org) — before 1.0, a
minor version may break.

## [0.2.1] - 2026-11-12

### Features

- **cli**: TLS to the database, sslmode and sslrootcert read as libpq (06d6b04)

### Fixes

- **sema**: calls that return NULL by themselves are nullable (1b9225f)
- **check**: no syntax-not-modeled for routine defaults and quoted values (3f47691)
- **config**: say what to write when [postgres] version is missing (fc4bbea)
- **codegen**: no dead_code warnings on generated items (1f1a6cc)
- **check**: routine bodies resolve names with the routine's search_path (8513c95)

## [0.2.0] - 2026-11-09

### Features

- **lsp**: hover inside routine bodies (058d944)
- extension packs provided by the project (709f5a1)
- **log**: configurable schemas for the log and snapshot tables (189c767)
- **migrate**: apply migrations with a registry (e393d70)
- report syntax read but not modeled (syntax-not-modeled) (70447d5)

### Fixes

- **lsp**: hover from the analysis, never a guess by name (a90f314)
- **catalog**: FORCE ROW LEVEL SECURITY, foreign keys without columns (2c0c500)
- **parser**: ON DELETE SET NULL | SET DEFAULT (columns) (5b1f287)

### Documentation

- the site is at pgzest.creatiwity.net (b0583f6)

### Maintenance

- **release**: resumable crates.io publication (4a9233f)

## [0.1.0] - 2026-11-06

First release.

- **Analysis as PostgreSQL would do it**, without a database: the
  catalog of PostgreSQL 18 and of `btree_gin`, `btree_gist`, `citext`,
  `fuzzystrmatch`, `pg_trgm`, PostGIS, `unaccent` and `uuid-ossp` is
  generated from a real server; schema, migrations and queries are
  replayed on it, every statement checked against the state at that
  point, every query typed with its nullability. A differential oracle
  checks the typing against a real PostgreSQL 18 in CI.
- **Typed Rust**: named queries in `.sql` files, `sql!` and `query!`
  compiled at build time (`pgzest-build`, `pgzest-macros`), run on
  `tokio-postgres` (`pgzest-runtime`); `pgzest describe` prints the query
  descriptors as JSON for other languages.
- **Fragments and macros** that desugar into standard SQL.
- **Declarative schema and migrations**: `pgzest migrate create` writes
  the migration from the schema diff, `pgzest migrate check` replays it;
  Prisma and Diesel layouts read as they are; `pgzest db check` compares
  a live database with the migrations.
- **Observable routines**: run logs and OpenTelemetry traces of the
  instrumented functions and triggers.
- **Editor**: the language server (`pgzest lsp`) and the VS Code
  extension — diagnostics as you type, hover, completion, macro
  expansion, in SQL files and inside Rust strings.
