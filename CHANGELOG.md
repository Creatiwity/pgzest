# Changelog

All notable changes to pgzest. From 0.1.0 on, the entries are generated
from the commit messages ([Conventional Commits](https://www.conventionalcommits.org),
[git-cliff](https://git-cliff.org), see [RELEASING.md](./RELEASING.md));
versions follow [Semantic Versioning](https://semver.org) — before 1.0, a
minor version may break.

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
