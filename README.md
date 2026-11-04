# pgweave

PostgreSQL language toolchain in Rust. Parser, type-checker, linter, migration engine, LSP server, and VS Code extension — all sharing the same AST.

> **Status: v0.1, usable but the analysis is not complete.** The components
> below compile and are tested, but the type-checker does existence checks
> and partial inference — no full type-compatibility checking yet. Expect
> false positives.

## Quick start

Prerequisites: [Rust](https://rustup.rs) (the version is pinned by
`rust-toolchain.toml`) and Node.js ≥ 18.

```bash
make bootstrap   # installs Rust + npm dependencies, checks the tooling
make demo        # opens VS Code on examples/demo with the pgweave extension loaded
make demo-run    # runs the demo's Rust program against PostgreSQL 18 (Docker)
make help        # lists all targets
```

`make demo` is the entry point if you are discovering the project: a small
application (migrations, named queries, generated Rust code) to explore
in the editor, with a guided tour in `examples/demo/README.md`.
`make demo-cli` runs `check`, `lint`, `codegen` and `emit` on
`examples/` and prints the actual diagnostics.

To try it in VS Code:

```bash
make vscode-dev      # builds the LSP + the extension, then explains what's next
code editors/vscode  # then F5 → "Run Extension (playground)"
```

F5 opens a window on `examples/playground/` — a real project with
`schema/`, `migrations/` and `pgweave.toml`, where each file
illustrates one capability (errors, directives, parser tolerance, lint).
See `examples/playground/README.md`.

`make playground` gives the same demonstration from the CLI.

## Status by component

| Component | Status | Detail |
|---|:---:|---|
| SQL + PL/pgSQL parser | ✅ | sqlparser-rs (PostgreSQL dialect, not forked) + dedicated PL/pgSQL lexer/parser |
| AST + NodeId/Span | ✅ | Spans as byte offsets, `Visit`/`VisitMut` traits |
| Logical-order normalization | ✅ | FROM → JOIN → WHERE → GROUP BY → SELECT |
| Type-checker | 🟡 | Table/column existence OK; partial inference, no full type compatibility |
| Linter (L001–L009, M001–M003) | ✅ | Style + migration-safety rules |
| Diff / migration engine | ✅ | Diff + snapshots + file generation |
| TypeScript codegen | 🟡 | Works on named SELECTs; type coverage to be extended |
| LSP server | 🟡 | Diagnostics + hover + completion + semantic tokens; little real-world testing |
| VS Code extension | ✅ | TextMate grammar + LSP client, F5 launch |
| WASM bindings | 🟡 | Compile (requires `wasm-pack`), checked in CI, no tests |
| Node bindings (napi-rs) | 🟡 | Compile; no tests, no published package |
| `pgweave` npm package | 🟡 | ~1000 lines; only `policy` and `prisma` have tests |
| PostGIS plugin | 🟡 | Spatial types + signatures, partial coverage |
| WASM plugins (wasmi) | 🟡 | Working host, one example in `examples/wasm-plugin` |
| JetBrains extension | ❌ | Skeleton (2 Kotlin files), not built in CI |

Legend: ✅ works and tested · 🟡 works, partial coverage or scope · ❌ not delivered

Tests: 71 Rust tests, 8 TypeScript tests. `make ci` reproduces what the
GitHub Actions CI checks.

## Parser tolerance and suppressions

A statement the analysis could not process is no longer silently
swallowed — it is reported, at a level you control.

Distinct categories:

| Category | Meaning | Default |
|---|---|---|
| `unparsable-statement` | sqlparser-rs could not read the statement. May be valid PostgreSQL out of scope **or** a syntax error — indistinguishable at this level. | `warn` |
| `unsupported-statement` | Valid statement, deliberately not modeled in the AST (`GRANT`, `CREATE VIEW`, …). | `warn` |
| `unparsed-function-body` | The statement is analyzed, but the **body** of a PL/pgSQL function could not be parsed. It then falls back to a nearly blind textual fallback: variables, types, `RETURN` and trigger context are no longer checked. A single unhandled construct is enough. | `warn` (follows `unparsable`) |

**Silence is treated as a bug.** None of these three situations goes
away without saying so — that is what separates an *incomplete* tool from a
*misleading* one. `unsupported` used to default to `ignore`: a dropped
`CREATE VIEW` meant the view did not exist for any other file, without a word.

The `warn` default on `unparsable` makes the problem visible without breaking
the build (`check` exits 0 on a warning), which lets you adopt the tool on an
existing codebase and then tighten it.

### Project config

```toml
# pgweave.toml — full format: docs/config.md
[postgres]
version = 18            # required

[parse]
unparsable  = "warn"    # "error" | "warn" | "ignore"
unsupported = "ignore"  # "error" | "warn" | "ignore"
```

One-off override on the command line:

```bash
pgweave check q.sql --on-unparsable error     # tighten in CI
pgweave check q.sql --on-unparsable ignore    # for the duration of a migration
```

### Comment directives

ESLint-style, in a line (`--`) or block (`/* */`) comment:

```sql
-- pgweave-disable-next-line                          -- all rules
-- pgweave-disable-next-line column-not-found         -- just one
-- pgweave-disable-next-line column-not-found, L001   -- several
-- pgweave-enable-next-line L001                      -- re-enables locally
-- pgweave-disable                                    -- start of block
-- pgweave-enable                                     -- end of block
-- pgweave-disable-file                               -- whole file
```

An unclosed `-- pgweave-disable` extends to the end of the file. A
justification may follow a second `--` and is ignored:

```sql
-- pgweave-disable-next-line unparsable-statement -- PG 18 syntax, see #214
CREATE TABLE … ;
```

Suppression is positional: it relies on the diagnostic's `span`.
Type-checker and linter both produce one, at statement granularity — the
same as the type-checker's. A diagnostic without one (none today) would only
be covered by `pgweave-disable-file`.

Note: the **type-checker targets the faulty expression** (`SELECT id, nope`
underlines `nope`, not the `SELECT`), including inside a sub-expression.
**Lint** rules target the statement — their nodes of interest are not
always expressions.

## Macros

A compact syntax that desugars into standard SQL. Nothing is guessed: all
the information is carried by the syntax.

```sql
@function user_count(target_org UUID): INTEGER {
    DECLARE total INTEGER;
    SELECT count(*) INTO total FROM users WHERE org_id = target_org;
    RETURN total;
}
```

11 lines → 5. `DECLARE`s are written close to where they are used and are
hoisted into the section before `BEGIN` at compile time.

Macros live in `.pgsql` files; `pgweave build` produces standard `.sql`,
readable by psql and pg_dump — no lock-in.

```bash
pgweave check q.pgsql                # analyzes directly, without compiling
pgweave macros                       # the available macros
pgweave macros --verbose             # holes, flags and options of each one
pgweave build q.pgsql                # prints the generated SQL
pgweave build q.pgsql --out-dir gen  # writes gen/q.sql
```

`check` and the LSP server desugar on their own: **`build` is only needed
to produce a `.sql` meant for psql or a deployment**. Diagnostics point to
the right line of the `.pgsql`, not of the generated SQL.

### Provided

| Macro | Role |
|---|---|
| `@function name(params)[: TYPE] [attributes] { … }` | PL/pgSQL or SQL function |
| `@procedure name(params) [attributes] { … }` | procedure |
| `@trigger name timing events on table(s) for each row\|statement [when (…)] { … }` | the function **and** the `CREATE TRIGGER` |

`@function` attributes: `immutable`, `stable`, `volatile`,
`security_definer`, `strict`, `leakproof`, `language <lang>`, `cost N`,
`rows N`.

`RETURN` stays explicit: an `AFTER` or a `FOR EACH STATEMENT` trigger does not
follow the same conventions as a `BEFORE … FOR EACH ROW`, and an implicit rule
varying with timing would be a trap.

### The multi-table trigger

```sql
@trigger touch before update on (users, organizations) for each row {
    NEW.updated_at = now();
    RETURN NEW;
}
```

One function is generated **per table**, giving each its own name and
`CREATE TRIGGER`. The checker verifies the column on each table and
reports the one missing on any of them — as it also does for a
hand-written shared function (`set_updated_at` attached to several
tables): the body is checked once per table that fires it.

### Isolated temp tables and snapshots

In the body of `@function`, `@procedure` or `@trigger` (PL/pgSQL):

```sql
@function close_month(m DATE): BIGINT {
    DECLARE total BIGINT;
    @temp_table work (order_id BIGINT PRIMARY KEY, amount INT NOT NULL);
    INSERT INTO work SELECT id, total FROM orders WHERE month = m;
    @snapshot filled;
    UPDATE work SET amount = amount * 2 WHERE amount < 10;
    SELECT sum(amount) INTO total FROM work;
    RETURN total;
}
```

- **`@temp_table name (columns);`** — a temp table private to each run. A
  plain temp table is shared by the whole session: a nested or recursive
  call, or a call per loop iteration, mixes its rows with the caller's
  (or collides on its keys). pgweave gives each run an id and rewrites
  every access from the analysis of the body: reads see the run's rows
  only, `UPDATE`/`DELETE` touch them only, `INSERT` tags them, keys are
  unique per run, and the run's rows are deleted on `RETURN`. The table is
  created under a name derived from its definition, so two routines
  declaring `work` differently never meet.
- **`@snapshot label;`** — with `pgweave build --log debug`, copies at
  that point the run's rows of each temp table and the variables in scope
  into `pgweave_debug.snapshot` / `pgweave_debug.snapshot_rows` (created
  by that build). In any other build — analysis, migrations, deployment —
  it emits nothing.

Nothing is guessed: an access pgweave cannot rewrite is an error
(`TRUNCATE` → write `DELETE`, `RETURNING *`, a `RETURN` reading the table,
a CTE of the same name), and dynamic SQL (`EXECUTE`) in such a body is a
warning, since it is not rewritten. "Show expansion" displays the
rewritten body. A snapshot taken in a transaction that fails is rolled
back with it.

### Run logs

Every PL/pgSQL routine of a macro (`@function`, `@procedure`, `@trigger`)
records its runs at the project level:

```toml
[log]
level = "step"     # none (default) | info | step | debug
target = "table"   # table (default) | notify | notice
```

The level is fixed when the code is generated: below it, no code is
emitted. Analysis, the editor, `pgweave build` and `pgweave migrate` all
see the routines at this level; `pgweave build --log <level>
[--log-target …]` overrides it for one build.

| Level | Recorded |
|-------|----------|
| `none` (default) | nothing |
| `info` | one entry per run: routine, start, duration, the run that caused it (calling routine, or the statement that fired a trigger) and the depth, the caller's `pgweave.traceparent` setting, and the `SQLSTATE` when the routine returns from its own exception handler |
| `step` | plus one entry per `INSERT`, `UPDATE`, `DELETE` or `EXECUTE`: kind, target table, source line, row count |
| `debug` | plus the `@snapshot` copies |

With the `table` target (default), entries go to the unlogged tables
`pgweave_log.run` and `pgweave_log.step`, joined on `run`, which also
identifies the run in `pgweave_debug.snapshot`; `pgweave_log.run.parent`
is the run that caused it. With `notify`, each entry is a JSON
`pg_notify('pgweave_log', …)` (steps, then the run). With `notice`, a run
raises `RAISE NOTICE 'pgweave_log'` with its JSON in `DETAIL` when it
starts (routine, parent, depth, the traceparent on the outermost run) and
when it ends (its run id, duration, steps, error): notices reach the client at
once, even when the transaction then fails — the target for tracing. Only
the runs of a sampled trace raise them: the caller's `pgweave.traceparent`
is a W3C `traceparent` whose `sampled` flag is set. Without one, or when
it is not sampled or malformed, the run raises nothing and never fails
for it; to trace by hand in `psql`:
`SET pgweave.traceparent = '00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01'`.

Steps are kept in a variable and written when the run ends, before each
`RETURN` and at the end of the body: the recording changes neither
`ROW_COUNT` nor `FOUND` for the body. A run whose error leaves the
routine is rolled back with its log (as notifications of a failed
transaction are never sent); an error the routine handles itself is
logged with its `SQLSTATE`, steps rolled back by it included. SQL-language
routines are not logged.

In a declarative project, `pgweave migrate create` generates the log
tables with the first logged routines, and records the level in the
migration header: `-- pgweave: postgres=18 log=step` (`log=step:notify`
for the `notify` target). The replay checks the header against the
routines of the file. Changing the level regenerates every macro routine
in the next migration, with a note saying why.

Only macro routines are instrumented: a plain `CREATE FUNCTION` stays as
written. Logging a routine is opting in, by writing it as a macro, and the
code that does it is in the migration, for review.

Cost, measured on PostgreSQL 18 (Docker on an Apple M4 Pro) with a
function that inserts a row, fires a trigger updating another table, and
returns the result of a second function — three runs per call, 11 µs per
call without logging:

| Level, target | Per call | Per run |
|---------------|----------|---------|
| `none` | +0 µs (no code generated) | 0 |
| `info`, `table` | +18 µs | ~6 µs |
| `step`, `table` | +35 µs | ~12 µs |
| `info`, `notify` | +23 µs | ~8 µs |
| `step`, `notify` | +36 µs | ~12 µs |
| `info`, `notice`, trace not sampled | +3 µs | ~1 µs |
| `step`, `notice`, trace not sampled | +4 µs | ~1.4 µs |
| `info`, `notice`, sampled trace | +98 µs | ~33 µs |
| `step`, `notice`, sampled trace | +105 µs | ~35 µs |

A few microseconds per run, to compare with the round trip of a query
(100 µs to a few ms): negligible for business routines, noticeable for a
trigger on a bulk load of millions of rows — log it at `none`, or below
the level it would cost. A sampled run with `notice` costs the most: of
its ~33 µs, building the two JSON events takes ~9 µs and sending them to
the client ~23 µs (measured with `client_min_messages = warning`, which
builds them without sending) — the cost of a message more than of its
size: two empty notices cost about as much. Sampling bounds it: only the
requests the application traces pay it; a run not sampled reads the
traceparent and keeps the depth only when `pgweave.max_depth` is set.

### Traces

With `target = "notice"`, `pgweave-runtime` reads the runs off the
connection (feature `trace`) and hands each finished tree to the
application: the routine called, the triggers it fired, the routines they
called, nested, with their timings and their steps. What to do with it is
the application's choice. With the feature `opentelemetry`,
`trace::otel::export` turns a tree into spans under the application's
span, through the `opentelemetry` API only: the tracer provider, its
exporter (OTLP or any other) and the backend are the application's.

```rust
use pgweave_runtime::trace::{self, Tracing};

let tracing = Tracing::new(move |run| trace::otel::export(&tracer, &run, &Context::current()));
let (client, connection) = config.connect(NoTls).await?;
// `drive` replaces `connection.await`; other notices and the LISTEN
// notifications go to the closure.
tokio::spawn({ let t = tracing.clone(); async move { t.drive(connection, |_| {}).await } });

trace::set_traceparent(&client, &traceparent, true).await?; // in the transaction
if let Err(e) = client.query("SELECT add_line($1, $2)", &[&order, &amount]).await {
    tracing.fail_open(&e); // runs the failure left open: error spans
}
```

A run that never ends is still traced: set `pgweave.max_depth` (e.g.
`SET pgweave.max_depth = 50`) and a logged routine called deeper raises
`54001` naming itself — an endless trigger cascade fails fast, and its
spans show the chain of calls that led there. `statement_timeout` stops a
loop that does not recurse the same way: the runs it left open are closed
as failed.

Only a sampled trace is traced: `set_traceparent` with the application's
current `traceparent`, whose `sampled` flag the routines read. Notices
reach the client when `client_min_messages` is `notice` or below (the
default). Only the routines of macros (`@function`, `@procedure`,
`@trigger`) are logged: a plain `CREATE FUNCTION` runs untraced.

### `pgweave dev`

```sh
pgweave dev --url postgres://localhost/myapp_dev   # or DATABASE_URL
```

Applies the routines of the project's schema files — functions,
procedures, triggers — to a development database, built at the `debug`
log level by default (`--log`, `--log-target` as for `pgweave build`), then
again each time a file is saved: run logs and snapshots are there without
a separate build. Tables and the rest of the schema go through
migrations; `pgweave dev` only issues `CREATE OR REPLACE` (added when the
file omits it). A routine whose return type or parameter names changed is
dropped and created again (PostgreSQL refuses if something depends on
it); a routine a file no longer declares is dropped, if this session
created it. A file with a macro error, or that the database rejects, is
not applied (one transaction per file), and the error is shown at its
place in the source. `--once` applies everything and exits.

### Viewing the expansion in VS Code

Cursor inside a macro, then **⌘⌥E** (`Ctrl+Alt+E` elsewhere) — or the
"Show expansion" link displayed above each macro, or the context menu.
The generated SQL opens in a **read-only** tab alongside: editing it would
have no effect on the `.pgsql`, and suggesting otherwise would be a trap.

Cursor outside any macro: the whole file is desugared.

Under the hood, a custom LSP request `pgweave/expandMacro` — the same mechanism
as Rust Analyzer's "Expand macro recursively".

### Declaring one

A `*.pgmacro` file in the `[macros] directory` (default: `macros/`):

```
macro audited
header {name:ident}

template
CREATE TABLE {name} (
{body},
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
end
```

Directives: `macro`, `header`, `default`, `flag`, `option`, `wrap`,
`template`…`end`. `wrap <hole> -> <text>` renders an optional hole only
when given (`wrap when -> \nWHEN ({})` in `@trigger`). Hole types: `ident`,
`word`, `words`, `parens`, `type`, `list`, `keywords` — a **closed** set,
which keeps the format from drifting into a templating language. A hole
the template never renders is a definition error: what the user wrote
would be dropped.

The provided macros are written in this same format
(`crates/pgweave-macro/src/builtins/`): an extension mechanism that the
provided macros do not use is an untested mechanism. A project macro with
the name of a provided macro replaces it.

### Fragments

A recurring piece of SQL — column list, expression, predicate — is
written once in a `.sql` file of the `[macros] directory` and inserted with
`@name`, in the schema as in queries:

```sql
-- macros/search.sql
-- fragment: search_vector(t)
-- Must stay identical to the index expression: it is written only here.
to_tsvector('french'::regconfig, coalesce(t.nom, ''))
```

```sql
CREATE INDEX etab_search ON etablissements USING gin ((@search_vector));
SELECT e.siret FROM etablissements e WHERE @search_vector(e) @@ plainto_tsquery('french', $q);
```

Parameters are qualifiers (`@f(e)`: `t.` → `e.`; `@f`: removed).
The fragment is checked in every statement that inserts it; hovering
`@name` shows its documentation and the inserted SQL. A query can also
reuse another query's row struct (`-- name: x :many -> other`),
after its columns are checked. Details:
[`docs/named-queries.md`](docs/named-queries.md#fragments).

## Named queries

Queries called from code live in `[queries] directory`, one
per `-- name:` header:

```sql
-- name: page_by_id :optional
SELECT id, title FROM pages WHERE id = $id;
```

`pgweave check` verifies their format (name, `$name` parameters, one statement
per query) and each query against the schema. They will be the source for
generating typed calls (Rust, then TypeScript). Full format:
[`docs/named-queries.md`](docs/named-queries.md).

## Project scope

`pgweave check` and the LSP server share the same model: the files in
scope are **indexed without being checked** (tables, functions, RLS), then only
the current file is analyzed against this index.

The scope is discovered by walking up to `pgweave.toml`:

```toml
[postgres]
version = 18

[migrations]
directory = "migrations"     # indexed, each file with its sequence

[schema]
directory = "schema"         # indexed recursively
include = ["db/functions"]   # extra directories
```

If `[schema] directory` is absent, **the whole project** is the scope —
consistent with the classification, which then treats every non-migration
file as Schema. `node_modules/`, `target/`, `dist/`, `.schema-snapshots/` and
hidden directories are never walked.

Resolution priority, first found wins: explicit `--schema` → DDL of the
analyzed file → project index → plugins.

```bash
pgweave check queries/q.sql              # uses the project index
pgweave check queries/q.sql --no-project # ignores the config, isolated mode
pgweave check q.sql --schema other.sql   # adds an explicit source
```

If a file in scope contains an unreadable statement, the objects it
defines are missing from the index — hence cascading `table-not-found`
false positives. The CLI reports it instead of implying a complete
index.

## Known limitations

In order of importance:

1. No type-compatibility checking between operands.

2. No introspection of a live database: the schema comes from DDL files.

---

## CLI

Build:

```bash
cargo build -p pgweave
```

### check

Type-check a SQL file. Exits 1 if any errors are found.

```bash
pgweave check query.sql
pgweave check query.sql --schema schema.sql
```

The `--schema` option can point to a `.sql` file or a directory of `.sql` files (read in lexicographic order and concatenated).

### lint

Check for style and best-practice violations.

```bash
pgweave lint query.sql
```

Rules: `L001` (SELECT *), `L002` (implicit JOIN), `L003` (missing WHERE on DELETE/UPDATE), `L004` (LIKE without index), `L005`–`L009` (RLS, coalesce, NULL comparisons, etc.).

### migrate create, migrate check, migrate diff

The declarative schema describes the target state, migrations the history
(PLAN.md, D10). In `pgweave.toml`:

```toml
[schema]
directory = "schema"
declarative = true
```

The analysis (CLI, editor, `build.rs`) then replays only the schema; the
migrations are replayed separately and compared with it. A gap is reported
(`schema-drift`, on the last migration) until the next one is
generated:

```bash
pgweave migrate create add_loyalty      # writes the migration, verified by replay
pgweave migrate check                   # CI: fails if the schema moved on alone
pgweave migrate diff old.sql new.sql    # between two schemas, no project
```

`migrate create` compares the state reached by the migrations with the schema,
object by object (schemas, extensions, enums, types, domains, sequences, tables,
columns, constraints, views, routines, indexes, triggers, policies, row-level
security, comments), and writes the migration in the directory's format
(numbered files, Prisma `<timestamp>_<name>/migration.sql`, Diesel
`<timestamp>_<name>/up.sql`):

```sql
-- pgweave: postgres=18 extensions=citext@1.8,pg_trgm@1.6
-- Migration "search", generated by `pgweave migrate create`.

CREATE EXTENSION IF NOT EXISTS pg_trgm SCHEMA public VERSION '1.6';

ALTER TABLE customers
  ADD COLUMN loyalty integer DEFAULT 0 NOT NULL;

-- !! M006: index public.customers_name_trgm built without CONCURRENTLY: writes to public.customers blocked during the build
CREATE INDEX customers_name_trgm ON customers USING gin (name gin_trgm_ops);
```

- **Order**: what would block the changes goes first
  (policies, triggers, views to rebuild, modified indexes and constraints),
  then the creations, with tables, routines and views sorted together by
  dependencies; definitive drops (columns, tables, routines,
  types, extensions, schemas) come last.
- **Verified before being written**: the migration is replayed after the
  others, and the resulting state must be exactly the schema.
- **Simplest migration, adapted by the developer**: the generated
  migration reaches the schema in the simplest way, even if it loses data
  (a rename becomes a drop and an add) or fails on a populated table (NOT
  NULL column without default). Only the developer knows the data: the
  header of the file (`!! TO ADAPT`) lists what to review, and `pgweave
  migrate check` verifies the final state once it is adapted.
- **Nothing is approximated**: a change pgweave cannot express (enum value
  removed, domain changed, identity added…) is written as a `TODO` in the
  migration; `migrate check` fails until it is written by hand.
- **Risks** reported on each affected statement: M001 `NOT NULL` column
  without default, M002 dropped column, M003 dropped table, M004
  column type changed, M005 `SET NOT NULL`, M006 index without
  `CONCURRENTLY` on an existing table, M007 constraint validated against
  all rows.

The oracle (`make oracle`) applies the generated migrations and the declarative
schema to two PostgreSQL 18 databases, and compares what PostgreSQL says about
their objects; `cargo xtask migrate-oracle <project>` does the same on a
real project.

### emit

Parse a SQL file and print the AST (useful for debugging):

```bash
pgweave emit query.sql
```

### codegen

Generate TypeScript type interfaces from SQL functions and named queries:

```bash
pgweave codegen --input queries.sql
pgweave codegen --input queries.sql --output types.ts
```

---

## VS Code extension

### Development (F5)

Open the extension folder in VS Code, then press F5:

```bash
code editors/vscode/
```

The extension auto-discovers the `pgweave-lsp` binary from `target/debug/pgweave-lsp` or `target/release/pgweave-lsp` relative to the extension root — no manual path configuration needed in development.

### Settings

| Setting | Default | Description |
|---------|---------|-------------|
| `pgweave.lspPath` | `pgweave-lsp` | Path to the LSP binary (auto-detected in dev) |
| `pgweave.schemaFile` | `""` | Path to a schema `.sql` file **or directory** for type-checking |

The `schemaFile` setting accepts:
- A path to a single `.sql` file
- A path to a directory — all `.sql` files are read in lexicographic order

### Features

- **Diagnostics** — type errors and lint violations as you type, for `.sql` and `.pgsql` files
- **Hover** — column types and table names
- **Completion** — context-aware:
  - Statement start: `SELECT`, `INSERT INTO`, `UPDATE`, …
  - After `FROM`/`JOIN`: table names from schema
  - In `SELECT`/`WHERE`: columns from referenced tables, qualified (`alias.col`) and unqualified
  - After `alias.`: only columns from that alias
  - In `ORDER BY`/`GROUP BY`: column names + `ASC`/`DESC`

Build the LSP server:

```bash
cargo build -p pgweave-lsp          # debug
cargo build --release -p pgweave-lsp # release
```

---

## Crate structure

```
crates/
  pgweave-ast/         AST types (Statement, Expr, DDL nodes, Span, NodeId)
  pgweave-diagnostics/ Diagnostic, Severity, Span, codes — dependency-free base
  pgweave-parser/      sqlparser-rs + PL/pgSQL extension → pgweave-ast
  pgweave-schema/      SchemaProvider + schema types, resolver, index, config
  pgweave-catalog/     PostgreSQL catalog (generated packs), DDL replay
  pgweave-sema/        Semantic analysis: types, overloads, queries, routines
  pgweave-checker/     Checking (facade over pgweave-sema) + plugin rules
  pgweave-lint/        Lint rules, LintConfig
  pgweave-codegen/     SQL emitter, diff engine, TypeScript codegen
  pgweave-analysis/    Orchestration facade: parse → check → lint
  pgweave-plugin/      Plugin system + PostGIS plugin
  pgweave-plugin-wasm/ WASM plugin host (wasmi)
  pgweave-lsp/         tower-lsp server (diagnostics, hover, completion)
  pgweave-cli/         pgweave binary (check, lint, migrate, emit, codegen)

bindings/
  wasm/            wasm-bindgen bindings
  node/            napi-rs bindings

editors/
  vscode/          VS Code extension (TypeScript + TextMate grammars)
```

---

## Architecture notes

The AST is reordered to **logical evaluation order** (`FROM → JOIN → WHERE → GROUP BY → HAVING → SELECT → ORDER BY`) before type-checking. This makes the type-checker a single linear pass with no backtracking.

The `SchemaProvider` trait is the single entry point for schema information. Plugins implement it to add types and functions (e.g. PostGIS spatial operators). The `CompositeSchemaProvider` merges multiple providers — first match wins.

### Layering rule

Internal dependencies form a strict DAG, from bottom to top:

```
pgweave-ast · pgweave-diagnostics        (no internal dependency)
  └── pgweave-parser
       └── pgweave-schema              (SchemaProvider lives here)
            └── pgweave-catalog        (catalog, replay)
                 └── pgweave-sema      (semantic analysis)
                      ├── pgweave-checker  ┐ independent passes
                      ├── pgweave-lint     │ that don't know
                      └── pgweave-codegen  ┘ each other
                           └── pgweave-analysis  (facade: chains the passes)
                                └── pgweave-cli · pgweave-lsp · bindings/*
```

Two invariants not to break:

1. **`pgweave-diagnostics` and `pgweave-schema` depend on nothing of substance.**
   `Diagnostic` and `SchemaProvider` are shared contracts; they used to live
   in `pgweave-checker`, which forced the linter to compile the
   type-checker and inverted `pgweave-schema`'s dependency.
2. **A pass never calls another pass.** `pgweave-lint` does not know
   the checker, nor the checker the linter. All chaining lives in
   `pgweave-analysis`, above.

See `SPECS.md` for full design decisions, `ARCHITECTURE.md` for internals, and `ROADMAP.md` for what's planned.

---

## Building everything

Everything goes through the `Makefile`:

```bash
make bootstrap   # Rust + npm dependencies (do it once)
make all         # Rust + TypeScript + VS Code extension
make test        # Rust + TypeScript tests
make ci          # build + test + clippy + typecheck (what the CI checks)
make clean       # removes build artifacts
```

Rust-only targets: `make build`, `make release`, `make test-rust`,
`make clippy`, `make fmt`. `make wasm` requires `cargo install wasm-pack`.

The code is formatted with `rustfmt` and has no clippy warnings. The CI
blocks on both (`cargo fmt --all --check` and `clippy -- -D warnings`),
so run `make ci` before pushing.
