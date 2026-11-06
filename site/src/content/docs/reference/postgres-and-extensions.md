---
title: "PostgreSQL versions and extensions"
description: "Which PostgreSQL version and extensions pgzest knows, how their catalogs are made, and how to add one."
sidebar:
  label: "Versions and extensions"
---

pgzest never guesses what a function, an operator or a type of PostgreSQL
is. Everything built in — PostgreSQL's own catalog and each extension's —
comes from a **pack**: a JSON file generated from a real server, versioned
in the repository (`crates/pgzest-catalog/packs/`) and shipped inside
pgzest.

## What is covered

| Contributor | Version | Pack |
|-------------|---------|------|
| PostgreSQL | 18 | `postgres-18.json` |
| `btree_gin` | 1.3 | `btree_gin-1.3.json` |
| `btree_gist` | 1.8 | `btree_gist-1.8.json` |
| `citext` | 1.8 | `citext-1.8.json` |
| `fuzzystrmatch` | 1.2 | `fuzzystrmatch-1.2.json` |
| `pg_trgm` | 1.6 | `pg_trgm-1.6.json` |
| PostGIS (`postgis`) | 3.6.4 | `postgis-3.6.4.json` |
| `unaccent` | 1.1 | `unaccent-1.1.json` |
| `uuid-ossp` | 1.1 | `uuid-ossp-1.1.json` |

A pack holds the schemas, types, functions and procedures, aggregates,
operators, casts, operator classes and access methods — what the analysis
resolves calls, operators and casts against. The PostgreSQL 18 pack is the
one the oracle checks: 19,032 out of 19,032 overload resolutions identical
to the server's.

## How it is used

**The PostgreSQL version** comes from `pgzest.toml`:

```toml
[postgres]
version = 18
```

Any other version is an explicit error, not a fallback to the nearest
one.

**Extensions are not configured.** They come from the `CREATE EXTENSION`
statements of your schema or migrations, replayed like the rest:

- `CREATE EXTENSION postgis` loads the PostGIS pack; its objects land in
  the schema the statement names (`SCHEMA extensions`) or the first
  schema of the `search_path`, as in PostgreSQL;
- `VERSION '…'` selects that version's pack; without it, the version used
  is the only one shipped;
- `ALTER EXTENSION … UPDATE [TO '…']` and `ALTER EXTENSION … SET SCHEMA`
  swap the pack accordingly;
- a type from an extension that was never created is reported as
  missing, with the extension that provides it as the hint (a function is
  reported as missing, without that hint yet).

Generated migrations start with a header recording what they were
written for, checked on every replay:

```sql
-- pgzest: postgres=18 extensions=citext@1.8
```

| Diagnostic | When |
|------------|------|
| `extension-unavailable` | No pack for this extension, or not for this version: the hint lists the versions available. |
| `type-not-found` | Includes types of an extension not created; the hint names the extension. |
| `migration-header-mismatch` | A migration announces an extension the replay does not have, or the reverse. |

## Adding an extension, or another version of one

The pack is generated from a PostgreSQL 18 server on which the extension
is installed (for PostGIS, the `postgis/postgis:18-3.6` image). The
generator creates a temporary database, records the catalog, runs
`CREATE EXTENSION`, records it again and keeps the difference:

```sh
cargo xtask catalog-gen --pg-url postgres://postgres:…@localhost:5432/postgres \
  --extension pg_partman --version 5.2.4
# writes crates/pgzest-catalog/packs/pg_partman-5.2.4.json
```

Then:

1. Register the file in `crates/pgzest-catalog/src/builtin.rs` (the
   `PACKS` list: name, version, file). Several versions of the same
   extension can coexist; `CREATE EXTENSION … VERSION` picks one.
2. Add a check of what the project needs from it — a query using its
   functions in the oracle corpus (`crates/pgzest-oracle/cases/`) compares
   pgzest's typing with the server's.
3. Rebuild: the pack is embedded in the binary.

Packs are data, not code: reviewing one means reading what the server
reported, nothing is written by hand.

## Another PostgreSQL version

Generating the pack is the same command, against a server of that
version and without `--extension`: it writes `postgres-<major>.json`.
That is not enough on its own today, and pgzest refuses any version but
18 rather than pretend:

- `Catalog::for_postgres` (`crates/pgzest-catalog/src/builtin.rs`) only
  loads 18;
- the parser and the analysis follow PostgreSQL 18: syntax that is new
  in 18 — `RETURNING old/new`, virtual generated columns — would have to
  be rejected for an older target;
- the oracle runs on PostgreSQL 18 only: each supported version needs its
  own run in CI, with every extension pack regenerated for it.
