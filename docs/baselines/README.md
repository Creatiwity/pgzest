# Baseline reports (P0)

Two starting measurements, to be rerun after each phase to track progress.
The figures below date from 2026-11-01 (commit `62f1e61`).

| Report | What it measures | How to regenerate it |
|---|---|---|
| [`crates/pgweave-oracle/tests/snapshots/oracle__report.snap`](../../crates/pgweave-oracle/tests/snapshots/oracle__report.snap) | Parameter and column types, checked against PostgreSQL 18's `Prepare`, on the public corpus `crates/pgweave-oracle/cases` | `make oracle` (check), `make oracle-accept` (update) |
| [`mesalia.md`](mesalia.md) | Analysis rate and false positives on Mesalia's 220 Prisma migrations (private corpus, aggregates without names) | `PGWEAVE_CORPUS_MESALIA=… cargo xtask corpus mesalia --layout prisma --out docs/baselines/mesalia.md` (with `make oracle-up` for the replay on PG 18) |

## Oracle: what pgweave claims vs PostgreSQL 18

- **Columns: 73/112 match (65 %). Parameters: 0/29 (0 %)** — no parameter
  type inference exists.
- **Silent fallback to `text`**: when a column's type is not inferred,
  `pgweave-semantic` claims `text` (numeric `coalesce`, `max` of a
  `timestamptz`, `array_length`, scalar subquery, aggregate in a CTE…).
  This is exactly what the "never guess" principle forbids: a generator
  would produce a wrong type without knowing it. To be replaced by an
  explicit unknown (P3).
- **Literals and arithmetic**: `1.5` is `numeric` for PostgreSQL (not
  `float8`), `int * numeric` is `numeric`, `sum(int4)` is `int8` —
  PostgreSQL's resolution rules are not followed (P3, D1 of the type
  system).
- **PG 18 syntax unknown to sqlparser**: `GENERATED ALWAYS AS (…)
  VIRTUAL` causes the **whole** table to be dropped, and all its columns
  then fall back to `text` (P1).
- **Not modeled**: `CREATE TYPE … AS ENUM`, `CREATE DOMAIN`,
  `CREATE EXTENSION`, `UNION`, `CALL`, `RETURNING` columns (P1–P3).
- **Textual rewriting**: the `CREATE DOMAIN … CHECK (VALUE > 0)` is quoted
  as `CHECK (true)` — a rewrite before parsing altered the statement
  (debt D1).
- **PostgreSQL 18 rule discovered**: a `VIRTUAL` generated column
  expression cannot use a user-defined type (domain, enum…); to be ported
  to the checker (P3).
- **Fixed during P0**: `SELECT *` expansion followed the order of a
  `HashMap` (a different result on each run, duplicated columns) and
  `t.*` produced nothing.

## State at the end of P1 (`e1c6b1f`)

| Measure | P0 | P1 |
|---|--:|--:|
| Oracle: matching columns | 73/112 | 84/112 |
| Mesalia: statements analyzed | 1519/1740 (87 %) | 1740/1740 (100 %) |
| Mesalia: routine bodies not analyzed | 17/28 | 0/28 |
| Mesalia: errors (false positives) | 27 | 10 |
| siren: statements analyzed | — | 294/294 (100 %) |

- The **10 Mesalia `column-not-found`** come from checking each migration
  against the **final** state of the schema (columns renamed or dropped
  later): migration-by-migration replay (P2) removes them.
- The **`function-not-found`** (Mesalia 53, siren 7) all concern built-in
  functions (`pg_trigger_depth`, `normalize`) or extension functions
  (`unaccent`, `levenshtein_less_equal`): generated catalog and packs (D9,
  P2). Their increase since P0 comes from the bodies now being analyzed.
- siren replay on PG 18: stopped at migration 15 (`postgis` extension
  missing from the oracle image) — a limitation of the test environment.

[`siren.md`](siren.md): Diesel corpus (`--layout diesel`, local path).

## State in P3 (2026-11-02)

| Measure | P1 | P3 |
|---|--:|--:|
| Oracle: matching columns | 84/112 | 559/559 |
| Oracle: matching parameters | 0/29 | 115/115 |
| Mesalia: `check` errors (false positives) | 10 | 0 |
| Mesalia: catalog columns identical to the real one | — | 1622/1622 |
| Mesalia: generated queries identical to PG | — | 3342/3342 |
| Mesalia: routine body queries identical to PG | — | 58/58 |
| siren: generated / body queries identical to PG | — | 476/476 · 10/10 |

- `check` goes through `pgweave-sema`: each migration is checked against
  the catalog replayed up to it, a PL/pgSQL body against the final state.
- The old analyzer (`pgweave-semantic`, and its silent fallback to
  `text`) has been removed.

---

# History: P0 measurements

## Mesalia: analysis rate and false positives

- **Valid corpus**: all 220 migrations apply on a fresh PostgreSQL 18.
  Any `check` error diagnostic is therefore a false positive.
- **87 % of statements analyzed** (1519/1740). The gaps are concentrated
  in a few constructs:
  - **enums**: 116 statements (`CREATE TYPE … AS ENUM` ×75, `ALTER TYPE …
    ADD VALUE` ×24, `DROP TYPE` ×9, `ALTER TYPE … RENAME` ×8) — central for
    Mesalia, priority P1/P2;
  - **unmodeled drops** (`DROP INDEX/TABLE/TRIGGER/FUNCTION/
    VIEW`, ~64): the index still believes in dropped objects;
  - **views** (11 creations), **`DO` blocks** (10), **`EXCLUDE
    USING gist` constraints** (4, `btree_gist` extension), `BEGIN`/`COMMIT`/`SET`.
- **Functions: 17 bodies out of 28 not analyzed.** Causes: `DROP TABLE` in
  a body (8), `WITH …` statement (CTE) in a body (7), `CASE … WHEN` in an
  expression (2).
- **False positives: 27 `table-not-found` errors, all in 2 migrations**
  whose trigger functions fall back to textual handling. The "unknown
  tables" are:
  - **temporary tables** created in the body
    (`CREATE TEMP TABLE IF NOT EXISTS …`);
  - **transition tables** of statement-level triggers
    (`REFERENCING OLD TABLE AS … NEW TABLE AS …`).
  This is exactly the "temporary tables and procedure-local variables"
  context of the scoped catalog (D2): to be covered in P1 (parse) and P2
  (routine scope).
- **27 `function-not-found` warnings**: only PostgreSQL built-in functions
  missing from the hand-written catalog (`pg_trigger_depth` ×26,
  `normalize` ×1). Confirms D9: the built-in catalog must be generated from
  a real PostgreSQL.
- **Time**: parse 176 ms, index construction 307 ms, check 22 ms for 220
  files. Reference for the salsa spike criterion (indexing overhead < 2×).

## Resulting priorities for P1

By impact on Mesalia, at comparable cost:

1. Enums (`CREATE/ALTER/DROP TYPE`) and `DROP …` modeled.
2. PL/pgSQL bodies: `DROP TABLE`, `WITH` statements, `CASE … WHEN` — then
   removal of the textual fallback.
3. Temporary tables and transition tables (`REFERENCING`) in the routine
   scope.
4. `GENERATED … VIRTUAL`, `EXCLUDE USING`, views, `DO`.
