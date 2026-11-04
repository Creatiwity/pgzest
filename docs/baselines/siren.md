# Corpus `siren` — analysis rate

> Generated on 2026-11-03 by `cargo xtask corpus siren` (pgzest `d49e938`).
> Files selected: Diesel: `*/up.sql`, sorted by timestamped directory.
> Aggregates only: no identifier from the corpus appears here.

## Reading

| Measure | Value |
|---|--:|
| Files | 21 |
| Unreadable files | 0 |
| Statements | 294 |
| Analyzed (in the AST) | 294 (100 %) |
| Skipped: unreadable by sqlparser | 0 |
| Skipped: not modeled | 0 |
| Functions and procedures | 10 |
| PL/pgSQL bodies analyzed | 5 |
| `LANGUAGE sql` bodies analyzed | 5 |
| Bodies not analyzed | 0 |

## `check` diagnostics

Each migration is checked against the catalog replayed up to it (inclusive), including the `postgres@18` catalog and extensions. Since the migrations have been applied in production, an error is presumably a false positive.

Files with at least one error: 0/21. Statements missing from the index: 0.

| Severity | Code | Count |
|---|---|--:|
| warning | `trigger-no-context` | 1 |

## Replay on PostgreSQL

PostgreSQL 18: 21/21 migrations applied on a fresh database.

## Catalog rebuilt by pgzest

Replay of the migrations in order on the `postgres@18` catalog (extension packs included).

| Replay diagnostic | Count |
|---|--:|
| `ddl-not-replayed` | 4 |

Comparison with the real catalog (project objects, excluding extension members; expressions compared by presence):

| Objects | Real | Identical | Not established (P3) | Different | Missing | Extra |
|---|--:|--:|--:|--:|--:|--:|
| columns | 232 | 232 | 0 | 0 | 0 | 0 |
| constraints | 12 | 12 | 0 | 0 | 0 | 0 |
| extensions | 5 | 5 | 0 | 0 | 0 | 0 |
| indexes | 45 | 45 | 0 | 0 | 0 | 0 |
| policies | 0 | 0 | 0 | 0 | 0 | 0 |
| relations | 12 | 12 | 0 | 0 | 0 | 0 |
| routines | 8 | 8 | 0 | 0 | 0 | 0 |
| schemas | 1 | 1 | 0 | 0 | 0 | 0 |
| sequences | 3 | 3 | 0 | 0 | 0 | 0 |
| triggers | 2 | 0 | 0 | 0 | 2 | 0 |
| types | 12 | 12 | 0 | 0 | 0 | 0 |

## Import (declarative schema)

The rebuilt catalog, exported as SQL (`pgzest_catalog::dump`) and applied to a fresh database, compared with the real catalog of the migrations:

| Objects | Real | Identical | Different | Missing | Extra |
|---|--:|--:|--:|--:|--:|
| columns | 232 | 232 | 0 | 0 | 0 |
| constraints | 12 | 12 | 0 | 0 | 0 |
| extensions | 5 | 5 | 0 | 0 | 0 |
| indexes | 45 | 45 | 0 | 0 | 0 |
| policies | 0 | 0 | 0 | 0 | 0 |
| relations | 12 | 12 | 0 | 0 | 0 |
| routines | 8 | 8 | 0 | 0 | 0 |
| schemas | 1 | 1 | 0 | 0 | 0 |
| sequences | 3 | 3 | 0 | 0 | 0 |
| triggers | 2 | 0 | 0 | 2 | 0 |
| types | 12 | 12 | 0 | 0 | 0 |

Text-exact, according to PostgreSQL (`format_type`, `pg_get_*def`: defaults, indexes, views, routine bodies; P7 migration oracle, NOT NULL constraint names excepted), 386 description line(s) compared: 2 only in the migrations, 0 only in the import.

## Generated queries (oracle)

Probes drawn from the rebuilt catalog, prepared by PostgreSQL on the migrations' database and described by `pgzest-sema` (parameter types, column names and types; a refusal on both sides counts as agreement). Queries from the bodies of current routines are prepared with their variables as typed parameters (`PREPARE`), the way PL/pgSQL executes them; only column types are compared:

| Probe | Total | Identical |
|---|--:|--:|
| INSERT … VALUES ($1) | 232 | 232 |
| SELECT * | 12 | 12 |
| WHERE column = $1 | 232 | 232 |
| routine body query | 10 | 10 |

Body queries not probed (not analyzed by pgzest, or variable without a position in the AST): 0.

## Time (indicative, machine-dependent)

| Step | Duration |
|---|--:|
| Parse all files | 29 ms |
| Check all files | 100 ms |
| Replay on the catalog (`pgzest-catalog`) | 40 ms |
