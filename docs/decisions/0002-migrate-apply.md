# 0002 — Applying migrations: `pgzest migrate apply`

- **Status**: accepted (option C), 2026-11-09. Phases 1 and 3 done:
  `migrate status | apply | resolve` on the flat layout
  (`crates/pgzest-runtime/src/migrate.rs`, `crates/pgzest-cli/src/apply.rs`,
  tests `crates/pgzest-cli/tests/migrate_apply.rs`). Phases 2 and 4 open.
- **Context**: PLAN.md D10 and P7; user feedback on 0.1.0 ("no `migrate
  apply`, no registry"); Mesalia trajectory (no more Prisma migrations
  after the import, so nothing applies them any more)

## Question

pgzest generates migrations (`migrate create`), verifies them by replay
(`migrate check`) and compares a database with them (`db check`), but
something else has to apply them. Should pgzest apply them itself, and if
so with which registry, which transaction rules and which checks?

## What exists

- Three directory layouts, read and written: flat (`0007_name.sql`),
  Prisma (`<timestamp>_<name>/migration.sql`), Diesel
  (`<date>_<name>/up.sql`).
- A `-- pgzest: postgres=18 extensions=citext@1.8 log=…` header on each
  generated migration, checked against the replayed state.
- `db check` already reads the registries of the other tools
  (`_prisma_migrations`, `__diesel_schema_migrations`): applied set, rows
  started and never finished, recorded migrations the repository lacks.
- The parsed AST of every statement: pgzest knows, before anything runs,
  which statements cannot run in a transaction (`CREATE INDEX
  CONCURRENTLY`…).

## Options

### A. Delegate to an existing tool (sqlx-cli, refinery, dbmate, Prisma)

Document one tool per layout and teach `db check` its registry.

- ✅ Nothing to build or maintain; flat files (`0001_init.sql`) already
  match sqlx-cli's naming.
- ❌ The checks only pgzest can make are lost: the header's PostgreSQL
  version and extension versions against the server, statements that
  cannot run in a transaction detected before the first one runs, the
  replay of the repository before touching the database.
- ❌ Each tool has its own registry and naming rules; refinery requires
  `V1__name.sql`, Prisma requires its CLI and keeps Node in the deployment.
- ❌ For Mesalia, the end of Prisma migrations leaves a tool to pick and
  a registry to migrate.

### B. `pgzest migrate apply` with its own registry only

- ✅ Full control.
- ❌ An existing Prisma or Diesel project has to switch registries: a
  one-off copy of its history, and no way back.

### C. `pgzest migrate apply` writing the registry of the layout (recommended)

The registry follows the directory layout, as `db check` already reads it:

| Layout | Registry | Recorded |
|---|---|---|
| Prisma | `_prisma_migrations` | Prisma's columns; `checksum` = SHA-256 of the file, as Prisma computes it |
| Diesel | `__diesel_schema_migrations` | `version` (timestamp without `-`), `run_on` |
| flat | `_pgzest_migrations` (configurable) | see below |

- ✅ A Prisma or Diesel project changes tools without migrating its
  history, and can go back: each tool keeps reading a registry it wrote.
- ✅ `db check` reads the same tables it reads today.
- ❌ Two foreign formats to write faithfully: tested against the real
  tools (Prisma's `migrate status`, Diesel's `migration list`) in the
  oracle suite, as the packs are tested against PostgreSQL.

pgzest's own registry, for flat layouts:

```sql
CREATE TABLE _pgzest_migrations (
    name        text PRIMARY KEY,           -- file stem, `0007_add_loyalty`
    checksum    bytea NOT NULL,             -- SHA-256 of the file
    header      text,                       -- the `-- pgzest:` line, as applied
    started_at  timestamptz NOT NULL,
    finished_at timestamptz,                -- NULL: started, not finished
    applied_by  text NOT NULL DEFAULT current_user,
    pgzest      text NOT NULL               -- pgzest version that applied it
);
```

**No schema of its own.** Some hosts (OVH, some clients' databases)
allow no schema besides `public` or the one they provide, so applying
migrations never creates a schema. The table is created unqualified, in
the connection's `current_schema()`, as Prisma does with `?schema=`;
`[migrations] table` names another one, optionally qualified
(`app._pgzest_migrations`). The advisory lock needs no object at all.
`db check` and the introspection leave the table out, as they leave out
`_prisma_migrations` today.

## Proposed behavior (option C)

```sh
pgzest migrate status  [--url …]      # applied, pending, failed; exit 1 if pending or failed
pgzest migrate apply   [--url …]      # applies the pending ones, in order
pgzest migrate apply --dry-run        # what would run, nothing executed
pgzest migrate resolve <name> --applied | --rolled-back   # after a failed non-transactional run
```

`--url` defaults to `DATABASE_URL`, as for `db check` and `db pack`.

### Before anything runs (offline, then read-only)

Every check runs before the first statement; a failure applies nothing.

1. **The repository replays.** The same build as `migrate check`: a
   migration rejected by the replay, or a `TODO` left by `migrate create`,
   is never sent to a database.
2. **The registry agrees with the repository.**
   - A recorded migration missing from the repository is an error (the
     repository is behind the database), as in `db check`.
   - A recorded checksum that differs from the file is an error: an
     applied migration was edited. `migrate resolve <name> --checksum`
     records the new one after review.
   - A row started and never finished blocks everything until `migrate
     resolve`.
   - **A pending migration that sorts before an applied one is an
     error.** The replay verified the repository's order; the database
     would receive another one, which nobody verified. Typical cause: two
     branches merged. Fix: rename the late migration after the last
     applied one (`migrate create` numbers it that way).
3. **The server matches the headers.** A header describes the state
   *after* its migration, so a migration that updates an extension
   (`ALTER EXTENSION citext UPDATE TO '1.9'`) carries the new version.
   - The PostgreSQL major version is the project's (`[postgres]
     version`). A migration generated for PostgreSQL 17 is not applied to
     18 without being regenerated or its header updated: its analysis was
     made for 17.
   - Each extension version a pending migration's header names is
     **available** on the server (`pg_available_extension_versions`): the
     update it performs can run.
   - The extensions **installed** now (`pg_extension`) are those of the
     last applied migration's header, at the same versions: that is the
     state the pending migrations were verified from. A difference means
     the database was changed by hand (an `ALTER EXTENSION … UPDATE` run
     outside the migrations) and stops the run.

### Running

- **One transaction per migration**, its registry row written in the same
  transaction: a failed migration leaves neither changes nor a row.
- **Statements that cannot run in a transaction** — `CREATE INDEX
  CONCURRENTLY`, `DROP INDEX CONCURRENTLY`, `REINDEX CONCURRENTLY`,
  `VACUUM`, `ALTER SYSTEM`, `CREATE DATABASE`… — are found in the AST.
  A migration made only of such statements runs without a transaction,
  statement by statement: its row is written as started first, finished
  last, and a failure leaves it started (`migrate resolve`). A migration
  that **mixes** them with other statements is an error before anything
  runs, naming the statements: split it in two. `migrate create` already
  writes risk M006 on an index built without `CONCURRENTLY`; it would
  then write the concurrent form in a migration of its own.
- **One deployment at a time**: `pg_advisory_lock` on a fixed key for the
  whole run, so two replicas starting together apply each migration once.
  The second waits, then finds nothing pending.
- **Lock timeout**: `SET LOCAL lock_timeout` from `[migrations]
  lock_timeout` (default `"5s"`). A migration waiting for an `ACCESS
  EXCLUSIVE` lock queues every query behind it; it is better to fail and
  retry. `0` disables it.
- The SQL runs as written. pgzest never rewrites a migration at apply
  time and never writes data-migration code (D10). No down migrations: a
  rollback is a new migration; Diesel's `down.sql` stays unused.

### Embedded in the application

Many Rust services apply their migrations at startup. The same
implementation, in `pgzest-runtime` behind a `migrate` feature, with the
migrations compiled into the binary, as Diesel's `embed_migrations!`
does: nothing to copy next to the executable.

```rust
// The files are read, checked (replay, mixed transactions) and embedded
// at compile time: a migration that does not replay fails the build.
// Their text, name and checksum are constants of the binary; adding a
// migration file recompiles the crate.
static MIGRATIONS: pgzest_runtime::migrate::Migrations = pgzest_macros::migrations!();

pgzest_runtime::migrate::apply(&mut client, &MIGRATIONS).await?;
```

The checks against the registry and the server (steps 2 and 3) then run
at startup.

## Phasing

1. `migrate status` and `migrate apply` on the flat layout with
   `_pgzest_migrations`; transactions, advisory lock, lock timeout,
   checksums, header checks. Oracle tests on PostgreSQL 18: concurrent
   runs, a failing migration, an edited applied migration, an out-of-order
   pending one.
2. The Prisma and Diesel registries, tested against the real tools.
3. Non-transactional migrations and `migrate resolve`.
4. `migrations!()` and `pgzest_runtime::migrate`.

## Open questions

- Server version different from the headers' (an upgraded server): an
  error as proposed, or a warning with `--accept-version`?
- `db check` before applying (`--check-drift`): it needs a scratch
  database (`CREATEDB`), so opt-in, or the default in CI?
