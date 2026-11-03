# Corpus `mesalia` — analysis rate

> Generated on 2026-11-03 by `cargo xtask corpus mesalia` (pgweave `d49e938`).
> Files selected: Prisma: `*/migration.sql`, sorted by timestamped directory.
> Aggregates only: no identifier from the corpus appears here.

## Reading

| Measure | Value |
|---|--:|
| Files | 220 |
| Unreadable files | 0 |
| Statements | 1740 |
| Analyzed (in the AST) | 1740 (100 %) |
| Skipped: unreadable by sqlparser | 0 |
| Skipped: not modeled | 0 |
| Functions and procedures | 28 |
| PL/pgSQL bodies analyzed | 27 |
| `LANGUAGE sql` bodies analyzed | 1 |
| Bodies not analyzed | 0 |

## `check` diagnostics

Each migration is checked against the catalog replayed up to it (inclusive), including the `postgres@18` catalog and extensions. Since the migrations have been applied in production, an error is presumably a false positive.

Files with at least one error: 0/220. Statements missing from the index: 0.

No diagnostics.

## Replay on PostgreSQL

PostgreSQL 18: 220/220 migrations applied on a fresh database.

## Catalog rebuilt by pgweave

Replay of the migrations in order on the `postgres@18` catalog (extension packs included).

No replay diagnostics.

Comparison with the real catalog (project objects, excluding extension members; expressions compared by presence):

| Objects | Real | Identical | Not established (P3) | Different | Missing | Extra |
|---|--:|--:|--:|--:|--:|--:|
| columns | 1622 | 1622 | 0 | 0 | 0 | 0 |
| constraints | 488 | 488 | 0 | 0 | 0 | 0 |
| extensions | 2 | 2 | 0 | 0 | 0 | 0 |
| indexes | 405 | 405 | 0 | 0 | 0 | 0 |
| policies | 0 | 0 | 0 | 0 | 0 | 0 |
| relations | 122 | 122 | 0 | 0 | 0 | 0 |
| routines | 9 | 9 | 0 | 0 | 0 | 0 |
| schemas | 1 | 1 | 0 | 0 | 0 | 0 |
| sequences | 2 | 2 | 0 | 0 | 0 | 0 |
| triggers | 14 | 14 | 0 | 0 | 0 | 0 |
| types | 254 | 254 | 0 | 0 | 0 | 0 |

## Import (declarative schema)

The rebuilt catalog, exported as SQL (`pgweave_catalog::dump`) and applied to a fresh database, compared with the real catalog of the migrations:

| Objects | Real | Identical | Different | Missing | Extra |
|---|--:|--:|--:|--:|--:|
| columns | 1622 | 1622 | 0 | 0 | 0 |
| constraints | 488 | 488 | 0 | 0 | 0 |
| extensions | 2 | 2 | 0 | 0 | 0 |
| indexes | 405 | 405 | 0 | 0 | 0 |
| policies | 0 | 0 | 0 | 0 | 0 |
| relations | 122 | 122 | 0 | 0 | 0 |
| routines | 9 | 9 | 0 | 0 | 0 |
| schemas | 1 | 1 | 0 | 0 | 0 |
| sequences | 2 | 2 | 0 | 0 | 0 |
| triggers | 14 | 14 | 0 | 0 | 0 |
| types | 254 | 254 | 0 | 0 | 0 |

Text-exact, according to PostgreSQL (`format_type`, `pg_get_*def`: defaults, indexes, views, routine bodies; P7 migration oracle, NOT NULL constraint names excepted), 3864 description line(s) compared: databases identical.

## Generated queries (oracle)

Probes drawn from the rebuilt catalog, prepared by PostgreSQL on the migrations' database and described by `pgweave-sema` (parameter types, column names and types; a refusal on both sides counts as agreement). Queries from the bodies of current routines are prepared with their variables as typed parameters (`PREPARE`), the way PL/pgSQL executes them; only column types are compared:

| Probe | Total | Identical |
|---|--:|--:|
| INSERT … VALUES ($1) | 1598 | 1598 |
| SELECT * | 122 | 122 |
| WHERE column = $1 | 1622 | 1622 |
| routine body query | 58 | 58 |

Body queries not probed (not analyzed by pgweave, or variable without a position in the AST): 0.

## Time (indicative, machine-dependent)

| Step | Duration |
|---|--:|
| Parse all files | 143 ms |
| Check all files | 618 ms |
| Replay on the catalog (`pgweave-catalog`) | 78 ms |
