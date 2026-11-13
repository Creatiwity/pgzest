# pgzest — Master plan

> Written on 2026-10-31. Replaces `ROADMAP.md` as the steering reference.
> `SPECS.md`, `ARCHITECTURE.md`, `PLPGSQL_SPEC.md` and `SPECS-schema-plpgsql.md`
> remain context documents; in case of contradiction, this plan prevails.
>
> **Project name: `pgzest`** (formerly `pgweave`, `pgsql-tools`).

---

## 0. Why this plan

The repository contains a real analysis foundation (SQL + PL/pgSQL parser, project
index, macros, lint, LSP, green tests), but `ROADMAP.md` declares phases delivered
that are only delivered on the surface, and **the layer that justifies the project
does not exist**: calling typed SQL from code, with no runtime cost.

Concrete needs that frame the priorities:

- **Mesalia** — a new central gRPC component in Rust starts within 1 to 2
  weeks (from 2026-10-31) and **will replace Prisma with this project**.
  Constructs used: procedures, triggers, temporary tables, `CHECK`,
  `GENERATED ALWAYS AS … STORED`, enums (modified via `ALTER TYPE`),
  `citext` and `btree_gist` extensions. Quoted PascalCase identifiers
  (`"OrderLine"`), inherited from Prisma. History of 224
  Prisma migrations.
- **Sherpa** — a personal project used as a testing ground; schema being
  redesigned, no existing reference schema. Same scope as
  Mesalia.
- **Rust first**, on raw `tokio-postgres`. TypeScript next.
- **PostgreSQL 18** as the initial target version (Mesalia in production).
  Notable consequence: in PG 18, `GENERATED ALWAYS AS (…)` without a keyword
  produces a **virtual** column — both variants are modeled.
- This project is the successor of `kysely-procedures`: back to standard SQL,
  with the "sugar" needed to make it modern and maintainable.
- **COPY FROM** streamed from a CSV (siren case: staging table, 50+
  columns listed by hand, unchecked) — to be covered natively.
- **Extensibility** for PostgreSQL extensions (PostGIS first).
- **Deliberately postponed**: RLS and JSONB. Both deserve dedicated DX work
  before being tooled (see §6).

Cross-cutting requirement: robust, clear, maintainable. No shortcut that creates
debt to be repaid before it has even been used.

---

## 1. Principles

They settle future trade-offs; a PR that violates one must justify it.

1. **Fidelity to PostgreSQL, not approximation.** Type resolution rules
   (overloads, implicit casts, `unknown` literals, `UNION`/`CASE`)
   follow the *Type Conversion* chapter of the PG documentation. The catalog
   of built-in functions/operators/types is **generated from a real
   PostgreSQL**, never written by hand.
2. **Never guess.** An unknown type or nullability is represented
   as such (`Unknown`), propagated without cascading errors, and the root cause
   is diagnosed. A query descriptor (§4) **refuses** to be produced
   if it contains an unknown: no `any`, no `unknown`, no silent
   cast on the calling-code side.
3. **Silence is a bug** (already in place, to be kept): everything that is not
   analyzed is reported.
4. **A single source of truth per concern.** One catalog, one
   resolver, one query descriptor consumed by all targets (Rust,
   TS, LSP).
5. **Determinism.** Same input → same output, byte for byte (map
   order, node identifiers, generated code). A prerequisite for caches and
   snapshots.
6. **The extension mechanism is the one used by built-ins.** `pg_catalog` is
   loaded as one extension among others; the provided macros are
   written in the public format (already the case).
7. **Validated against reality.** Everything that claims to mimic PostgreSQL is tested
   against PostgreSQL (differential oracle, §7) and against real corpora.
8. **Faithful identifiers.** An unquoted identifier is folded to
   lowercase, a quoted identifier is kept as is — exactly like
   PostgreSQL. `"OrderLine"` and `orderline` are two distinct
   objects. (The current code lowercases everything: this is a bug for
   Mesalia, fixed in P1/P2.)
9. **Every name is qualified by its schema** once resolved. No catalog
   object exists "without a schema" (see D11).

---

## 2. Architecture decisions

Short format: decision, reason, rejected alternative.

### D1 — Parser: wrapper dialect over `sqlparser-rs`, fork as a last resort

Today `PostgreSqlDialect` is used as is, and what it does not handle
is worked around by **textual rewriting before parsing** (`PROCEDURE` →
`FUNCTION … RETURNS VOID`, removal of `RETURNS TABLE`, `SECURITY DEFINER`,
`COST`, `ROWS`). These rewrites are fragile (comments, strings, case),
shift spans and do not scale.

**Decision**: a `PgsqlDialect` that delegates to `PostgreSqlDialect` and
intercepts through the `parse_statement` / `parse_prefix` / `parse_infix` /
`get_next_precedence` hooks (pattern recommended by sqlparser-rs). Generic
gaps are contributed upstream; a fork is considered only if a blocking gap
can be handled neither by a hook nor upstream within an acceptable time,
and it is then documented (minimal patch, tracked rebase).

Several rewrites date from sqlparser 0.54; 0.62 may cover some of them
natively — to be inventoried first.

The in-house AST `pgzest-ast` is kept: it isolates the rest of the code from
sqlparser AST changes.

### D2 — Single, layered catalog, resolved on demand

Replaces `SchemaProvider`, `SchemaResolver` (eager today), `SchemaIndex`,
the checker's `TypeContext` and the `SymbolTable` of `pgzest-semantic`.

Layers, from outermost to innermost, each able to shadow the
previous one:

```
global contributors (D9): PG major version + enabled extensions
  └─ project (schema files of the repository, organized by PG schema — D11)
          └─ routine (parameters, DECLARE, temporary tables, NEW/OLD, FOUND…)
              └─ PL/pgSQL block (local variables, cursors, labels)
                  └─ statement (CTE, aliases, output columns)
```

Unqualified names are resolved according to `search_path` (project config, overridden
per file/routine via `SET search_path`).

Complete object model from the start for what is in scope:
schemas, tables (permanent/temporary, `ON COMMIT`), columns (type,
nullability, default, **identity**, **generated** `STORED` or `VIRTUAL` + expression),
named constraints (PK, UNIQUE, FK, **CHECK** + expression), types (base,
enum, domain + constraints, composite, array), functions (overloads,
IN/OUT/INOUT/VARIADIC modes, defaults, volatility, `STRICT`, `SETOF`,
`RETURNS TABLE`), procedures, triggers, operators, casts, sequences, views,
extensions. RLS policies are **modeled** (for migration diffing)
but not tooled on the DX side (§6).

### D3 — Incremental engine: `salsa`

The need for "lightweight index + lazy resolution of what is referenced + incremental
LSP" is exactly that of rust-analyzer and `ty` (Astral), which
rely on `salsa`. A home-made cache would reproduce salsa, less tested.

**Decision**: `salsa` for the project database (files, revisions, memoized
queries: parse of a file, declarations of a file, catalog object
by name, analysis of a query). Direct bonus: each analyzed query knows
**the exact set of objects it depends on**, which serves the invalidation
of the descriptor cache (§4) and Rust recompilation (§5).

Safeguard: a 2–3 day spike validates the salsa API (pinned version) on
parse + declarations + one table resolution before committing to the migration.
Abandon criterion: inability to express the "routine" scope cleanly,
or a measured overhead > 2× on Mesalia indexing.

**Result (2026-11-01): GO** — `docs/decisions/0001-salsa.md`. Index without
parsing in 4 ms, all Mesalia tables in 23 ms (versus 52 ms for
the existing code), 134/134 tables identical to the existing code, 0.07 ms after a
modification; routine scope expressed without difficulty.

### D4 — A single semantic analyzer, on a resolved representation

`pgzest-semantic` and the checker each do their own resolution today.
**Decision**: a `pgzest-sema` crate produces a resolved representation
(HIR) — each reference points to a catalog object or a
scope entry, each expression carries `(type, nullability)`. The
normalization into logical order (FROM → … → ORDER BY) remains the entry step,
as stated in `CLAUDE.md`. Checker, lint, codegen and LSP become
read-only consumers of this HIR.

### D5 — Nullability: a first-class attribute

Lattice `NonNull | Nullable | Unknown`. Sources: `NOT NULL`, PK, identity,
generated (depending on the expression), `NOT NULL` domains, outer joins (the
optional side becomes `Nullable`), aggregates (`count` NonNull, `sum`/`max`
Nullable), `STRICT` functions, `COALESCE`, `CASE` without `ELSE`, scalar
subqueries (always Nullable), `RETURNING`. Refinement through `WHERE x IS NOT
NULL` is a later improvement, not a prerequisite.

On the parameter side: a parameter is required non-null if it is compared by a
strict operator; optional if it feeds a nullable column. Explicit
override always possible.

### D6 — Named parameters: `$name`

`$1…$n` (PG native) and `$name` are accepted, without mixing them within the same
query. Why `$name` rather than `:name`:

- it naturally extends the native `$n` notation;
- `:name` already has a meaning in `psql` (client-side interpolation) and coexists poorly
  with `::cast` and `a[1:2]` slices;
- precedent: pgtyped.

Lexical constraint: `$name` immediately followed by `$` is the start of a
dollar-quote, not a parameter. The SQL sent to the server is always
rewritten to positional form.

### D7 — Query descriptor: single contract between analysis and targets

All generation (Rust macro, `.d.ts`, LSP) goes through a serialized,
stable and versioned descriptor (§4). Targets never call back
the analyzer in their own way.

### D8 — Rust runtime: `tokio-postgres`, statically known types

- Execution via `query_typed` (parameter types sent with the query:
  pipelined Parse/Bind/Execute, **no `Prepare` round trip**) or via a
  per-connection prepared-statement cache for hot queries.
- Decoding by column index, via `FromSql`, into generated structs —
  no dynamic `Row`, no lookup by name.
- Generic over `GenericClient`: pool (`deadpool-postgres`), transaction or
  bare client.
- No database connection at compile time (a structural difference
  from sqlx and cornucopia/clorinde).

### D9 — Global-context contributors: PG version and extensions, same mechanism

The PostgreSQL major version and each extension are
**contributors** to the global context, described by a single pack format:

- catalog fragment (types, functions, operators, casts, aggregates,
  index access methods and operator classes — needed for `btree_gist`);
- **generated** from a real PostgreSQL (`xtask catalog-gen --pg 18
  [--extension citext]`), never written by hand; an extension pack is the
  *delta* between the catalog before and after `CREATE EXTENSION`;
- identified by `(name, version)`: `postgres@18`, `citext@1.6`,
  `btree_gist@1.7`… The PG major version is a contributor like any other;
- per-target type mapping table (`citext` → `String`,
  `geometry` → type chosen by the project);
- optional rules. Data, not code. WASM plugins remain the channel
  for custom **rules**, not for describing a catalog.

**Where does the list of active contributors come from?**

- **Major version**: `[postgres] version` in `pgzest.toml`, the only source.
- **Extensions**: deduced from the schema state, not declared separately. The
  `CREATE EXTENSION` / `DROP EXTENSION` / `ALTER EXTENSION … UPDATE` of the
  declarative schema define the target state; replaying the migrations
  defines the deployed state. An extension lives in a schema
  (`CREATE EXTENSION citext SCHEMA extensions`): its objects are placed there
  and resolved via `search_path` (D11).
- **Both are analyzed at each point of the replay**: a migration is
  checked against the PG version and the extensions active *at that point*.

**Associated diagnostics**:

- use of an object provided by an extension that is not enabled → error with
  suggestion ("`citext` comes from the `citext` extension, which is missing; add
  `CREATE EXTENSION citext`");
- extension enabled but never used → warning (lint);
- object missing or changed in the target PG version (function removed, type
  renamed) → error contextualized by version;
- pack requested but not available for this version → explicit error,
  never a silent fallback to another version.

Each generated migration carries a machine-readable header stating the
context against which it was generated and verified:

```sql
-- pgzest: postgres=18 extensions=btree_gist@1.7,citext@1.6
```

The replay checks the consistency of this header with the reconstructed state
(extension announced but never created → error; PG version in the config
different from the migration's → information, and the migration is
analyzed with **its** version). Legacy migrations without a header are
analyzed with the config version and reported once.

### D10 — Declarative schema as source of truth, generated migrations

The repository's schema files describe the target state; migrations are
produced by diff, reviewed, then frozen. Existing migrations serve to
verify that the state can be reproduced (and for brownfield import), not as a
competing source of truth.

The diff also covers contributors (D9) and schemas (D11): it emits
`CREATE SCHEMA`, `CREATE EXTENSION [SCHEMA …]`, `ALTER EXTENSION … UPDATE`,
`DROP EXTENSION` and their counterparts, ordered before the objects that
depend on them (and after for drops). A `DROP EXTENSION` while
schema objects still depend on it is an error, not a migration.

### D11 — First-class PostgreSQL schemas

The schema (namespace) is part of every object's identity, everywhere:

- **Catalog**: key = `(schema, name)`, and `(schema, name, argument
  types)` for functions and procedures. `pg_catalog` and `pg_temp`
  are schemas like any other; temporary tables live in
  `pg_temp` and take priority in resolution, as in PG.
- **Resolution**: effective `search_path` = project config → `SET search_path`
  in a file or a migration → `SET search_path` clause of a function
  (which applies to its body). `pg_catalog` implicitly first when it
  is not listed, as in PG. Ambiguities and shadowing are reported.
- **Schema files**: a file or directory can declare its default
  schema; the recommended convention is `schema/<schema_name>/…`.
- **Migrations**: `CREATE SCHEMA` / `DROP SCHEMA` handled by the diff; a
  moved object (`ALTER … SET SCHEMA`) is a move, not a
  drop + create.
- **Lint**: `SECURITY DEFINER` without `SET search_path` → warning
  (classic vulnerability); object created without qualification in a file with no
  default schema → configurable warning.
- **Generated code**: the schema is part of the generated name (one Rust module per
  schema, except for the configured default schema) — two `users` tables
  in `public` and `auth` must never collide.
- **Descriptor** (§4): all types and dependencies in it are qualified.

---

## 3. Inventory: keep, rework, remove

| Item | Fate | Note |
|---|---|---|
| `pgzest-ast` | Keep, extend | Missing DDL (§P1), spans on all nodes, deterministic per-file IDs |
| PL/pgSQL parser (lexer + parser) | Keep, extend | Remaining cases from `SPECS-schema-plpgsql.md` §6 |
| Parser textual rewrites | **Remove** | Replaced by D1 |
| `pgzest-diagnostics`, `pgzest-disable…` directives | Keep | |
| `pgzest-expand`, source maps, `build`/expansion | Keep, extend | Expression fragments (§P6) |
| `SchemaProvider`/`SchemaResolver`/`SchemaIndex` | **Rework** → `pgzest-catalog` | D2, D3 |
| `pgzest-semantic` + the checker's `TypeContext` | **Merge** → `pgzest-sema` | D4 |
| Hand-written `builtins.rs` | **Remove** | Generated (D9, principle 1) |
| Checker/lint rules | Keep, port to the HIR | |
| `pgzest-codegen::diff`, migration safety | Keep, extend | Full D2 model |
| `pgzest-codegen::ts_types` | **Remove** | `HashMap` ordering bug; replaced by the descriptor (§P8) |
| LSP + VS Code extension | Keep | Rewired onto salsa/HIR, no new feature before P4 |
| `npm/pgzest` (`db.query<T>` = cast) | **Freeze** | Resumed in §P8 on top of the descriptor |
| WASM/Node bindings | Freeze | Resumed in §P8 |
| `ROADMAP.md` | Archive | Banner pointing here |

---

## 4. The query descriptor

Contract between the analysis and all targets. Stable JSON format, versioned
(`"version": 1`), deterministically ordered.

```jsonc
{
  "version": 1,
  "name": "page_by_id",                  // named query, or derived from the hash
  "sql": "SELECT id, title, creation_date FROM pages WHERE id = $1",
  "command": "select",                   // select | insert | update | delete | call | copy_in
  "params": [
    { "name": "id", "type": { "pg": "pg_catalog.int8", "oid": 20 }, "nullable": "unknown" }
  ],
  "columns": [
    { "name": "id",            "type": { "pg": "pg_catalog.int8", "oid": 20 },          "nullable": "non_null" },
    { "name": "title",         "type": { "pg": "pg_catalog.text", "oid": 25 },          "nullable": "non_null" },
    { "name": "creation_date", "type": { "pg": "pg_catalog.timestamptz", "oid": 1184 }, "nullable": "nullable" }
  ],
  "cardinality": "optional",             // many | one | optional | none
  "depends_on": ["public.pages"],        // catalog objects, for invalidation
  "fingerprint": "…"                     // hash(normalized sql + definitions of depends_on)
}
```

- **Cardinality** inferred conservatively: equality on a
  unique key/PK → `optional`; `INSERT … VALUES (one row) RETURNING` →
  `one`; aggregate without `GROUP BY` → `one`; statement without `RETURNING` →
  `none`. Otherwise `many`. Can be overridden explicitly, never guessed beyond that.
- Three-state nullability (D5): `non_null`, `nullable`, `unknown`. A
  parameter is `non_null` or `nullable` when it is assigned to a column
  (`INSERT … VALUES`, `SET`), `unknown` otherwise: the target decides (P5).
- Types are references to the catalog (including enums, domains,
  composites, extension types); the mapping to a language type
  is done by the target, via the mapping table (D9).
- On-disk cache (`.pgzest/queries/<fingerprint>.json`), usable offline
  in CI and by tools that do not want to embed the analyzer.

---

## 5. Target DX on the Rust side

Illustrative — final names are set in P5.

```rust
// Inline query: analyzed at compile time, SQL errors at the right Rust span.
let page = pgzest::query!(
    "SELECT id, title, creation_date FROM pages WHERE id = $id",
    id = page_id,
)
.fetch_optional(&client)        // optional cardinality → Option<Row>
.await?;
// page: Option<PageByIdRow { id: i64, title: String, creation_date: Option<OffsetDateTime> }>

// Named query, defined in queries/pages.sql, reusable and
// serving as a reference type:
let rows = queries::pages::list_recent(&client, ListRecentParams { limit: 20 }).await?;

// Typed COPY: column list checked against the catalog, CSV or binary stream.
let n = pgzest::copy_in!("etablissement_staging", columns = @etablissement_columns, format = csv(header))
    .from_reader(&client, csv_stream)
    .await?;
```

Design points:

- **Correct recompilation**: the macro emits an `include_bytes!` of the files
  the query depends on (known via `depends_on`), so that rustc recompiles
  when the schema changes — without relying on unstable APIs.
- **Compilation performance**: one salsa catalog per macro process,
  lazy loading; budget measured in P5 (see §7).
- **Named queries**: generated by `build.rs` (inspectable code, no
  macro magic) from the same descriptor as `query!`.
- **Types**: PG enums → generated Rust enums; domains → optional newtypes;
  extension types → via the pack (D9); dates via `time` or `chrono`, at
  the project's choice (feature).

---

## 6. Deliberately out of scope (later design phases)

- **RLS and privileges** (`GRANT`/`REVOKE`, roles): parsing, the catalog
  model and the diff remain covered (a migration must lose neither a
  policy nor a privilege). The DX for writing them readably will be the subject of a dedicated
  design phase.
- **JSONB**: typed as opaque `jsonb` (→ `serde_json::Value` or an explicit user
  type). Structural typing of JSON paths is a design phase.
- **Runtime instrumentation** (`SPECS-schema-plpgsql.md` §3): kept
  as an idea, not planned.
- **Hash markers in migrations** (§4 of the same spec): after P7.
- **Introspection of a live database** as a schema source: not needed for
  the target (D10); useful for brownfield import, reassessed in P7.
- **JetBrains extension**, LSP polish beyond what exists.

---

## 7. Quality strategy

Set up from P0; each phase enriches it.

- **PostgreSQL differential oracle** (CI, PG container pinned to the
  target version): for each query of the test corpus, compare the parameter
  and column types we infer with those returned by PostgreSQL's
  `Prepare`. Any gap is a bug or a documented divergence.
  Generated migrations are applied, then the real introspected catalog
  is compared with the expected catalog.
- **Real corpora**: Mesalia migrations, siren schema, Sherpa schema — read
  from a path given by an environment variable, **never copied into
  the repository**. Criterion: zero false positives, zero unjustified unanalyzed
  statements.
- **Snapshots** (`insta`) for diagnostics, HIR, descriptors, generated code.
- **Fuzzing** of the parser and the PL/pgSQL lexer: no panics.
- **Benchmarks** (`divan` or `criterion`) with budgets tracked in CI:
  Mesalia indexing, cold / warm analysis of a query, compile time
  of a crate with 200 `query!`.
- `make ci` remains the single entry point; zero clippy warnings.

---

## 8. Phases

Each phase has verifiable exit criteria. The next one is not started
on an unfinished phase, except where parallelization is explicitly noted.

### P0 — Reset (working foundations) — ✅ done (2026-11-01)

- ✅ Documentation aligned with reality (`CLAUDE.md`, `ROADMAP.md` banner,
  README).
- ✅ PG 18 differential oracle: `pgzest-oracle` crate, corpus
  `crates/pgzest-oracle/cases`, snapshot report (`insta`), `make oracle`
  (Docker) and `postgres:18` CI job. Baseline: 65 % of columns conformant, 0 %
  of parameters (no parameter inference).
- ✅ "Analysis rate" report on Mesalia: `cargo xtask corpus` (corpus
  via environment variable, aggregates without names), `docs/baselines/`.
  87 % of statements analyzed, 220/220 migrations valid on PG 18,
  27 false positives, all due to the textual fallback for PL/pgSQL bodies.
- ✅ Deterministic node identifiers: numbering per parse
  (`crates/pgzest-parser/src/ids.rs`), the (file, `NodeId`) pair
  identifies a node.
- ✅ salsa spike (D3): GO, `docs/decisions/0001-salsa.md`.
- ✅ Named-query format frozen: `site/src/content/docs/reference/named-queries.md` (header
  `-- name: x [:one|:optional|:many|:none]`, `$name`, checked by `check`
  and the LSP).
- ✅ Project config format frozen: `site/src/content/docs/reference/config.md` (`[postgres] version`
  required, single `pgzest.toml` file, strict keys). `search_path`
  and per-directory default schema are described there as P2 evolutions.
- ✅ Renamed to `pgzest` (`pgzest-*` crates, `pgzest` /
  `pgzest-lsp` binaries, `pgzest.toml` config, `pgzest-disable…` directives,
  VS Code extension, CI). The `.pgsql` / `.pgmacro` file extensions and
  the `pgsql` language identifier are kept: they name the language,
  not the tool. No compatibility alias.

**Exit**: CI with a working oracle; Mesalia baseline report published
in `docs/`; salsa decision recorded; named-query and config formats
published.

### P1 — Faithful parser — ✅ done (2026-11-01, `e1c6b1f`)

Summary: Mesalia 1740/1740 statements and 28/28 bodies analyzed, siren
294/294 and 10/10; no textual rewriting (wrapper dialect +
pgzest grammar over tokens); PL/pgSQL parser rewritten over the dialect's
tokens (ordinary SQL expressions, spans); faithful identifiers
(principle 8); `-- pgzest:` header read; deterministic fuzzing without panics
(`crates/pgzest-analysis/tests/no_panic.rs`). An audit of ignored sqlparser
fields removed a series of silent losses (windows,
`FILTER`, named arguments, `NOT VALID`, `USING`, `DEFAULT VALUES`…).
Sherpa: no usable schema corpus (sandbox), not applicable.
Left for P2: 10 Mesalia `column-not-found` false positives (checked
against the final state, resolved by the replay) and `function-not-found` on
built-in or extension functions (generated catalog, D9 packs).

- `PgsqlDialect` wrapper; removal of all textual rewrites.
- DDL coverage of the scope: `CREATE TYPE` (enum, composite), `CREATE
  DOMAIN`, `CREATE VIEW`, `CREATE SEQUENCE`, `CREATE SCHEMA`, `CREATE
  EXTENSION`, `GENERATED ALWAYS AS (…) [STORED | VIRTUAL]` columns (in
  PG 18, no keyword = `VIRTUAL`) and `IDENTITY`, `RETURNING OLD/NEW`
  (PG 18),
  named constraints, `EXCLUDE USING gist` constraints (Mesalia, via
  `btree_gist`), `CREATE TEMP TABLE … ON COMMIT`, native `CREATE PROCEDURE`,
  `CALL`, `COPY … FROM STDIN`, `CREATE/ALTER/DROP EXTENSION` (with `SCHEMA`
  and `VERSION`), `ALTER TYPE … ADD VALUE / RENAME`, `ALTER … SET SCHEMA`,
  `SET search_path`, `ALTER` variants used by the corpora, `COMMENT ON`.
- Quoted identifiers kept with their case in the AST (principle 8).
- `-- pgzest: postgres=… extensions=…` header recognized (D9).
- PL/pgSQL: remaining cases from `SPECS-schema-plpgsql.md` §6 (`RETURNING …
  INTO`, record variables, `VALUES` with variables, `FOR … IN`, `RETURN
  QUERY`, `GET DIAGNOSTICS`, `EXCEPTION`), removal of the textual fallback
  (any unparsed body = diagnostic, no degraded analysis).
- Spans on all expression and DDL nodes.

**Exit**: Mesalia/siren/Sherpa corpora 100 % parsed, or each failure
justified by a ticket; fuzzing without panics; no textual rewriting
left.

### P2 — Catalog — ✅ done (2026-11-01, `26e08b8`)

Done: `pgzest-catalog` crate (D2/D11 model, exact case), generated packs
(`cargo xtask catalog-gen`: postgres@18, citext, btree_gist, btree_gin,
pg_trgm, unaccent, fuzzystrmatch, postgis@3.6.4), ordered DDL replay
(`Replay`, `DO` blocks evaluated over virtual tables, `ddl-not-replayed`
otherwise), `ProjectCatalog` replacing `SchemaResolver`/`DdlSchemaProvider`/
`builtins.rs` (removed) for checker, analysis, CLI and LSP, exact
resolution everywhere, D9 diagnostics (missing extension suggested, migration
headers), brownfield import `pgzest schema import`, salsa database
`pgzest-project` with an incrementality test. Mesalia: reconstructed catalog
identical to the real one, 0 `check` diagnostics, import verified (exported schema →
empty database → identical catalog).

siren (`cargo xtask corpus siren --path ~/dev/creat/siren/migrations
--layout diesel --pg-url postgres://postgres:pgzest@localhost:54319/postgres
--details`, `pgzest-postgis` container, `postgis/postgis:18-3.6` image on
amd64): catalog and import identical to the real one, except the 2 triggers created
by `diesel_manage_updated_at` (dynamic SQL: `ddl-not-replayed`).
Executing this very common Diesel helper would require evaluating `EXECUTE
format(…)`: to be picked up with body interpretation (P3+).

LSP on `pgzest-project`: salsa `Workspace` (project macros as input,
editor text taking priority over disk until closed, per-file replay
diagnostics like `pgzest check`), `ProjectBuilder` shared with
the CLI. Baselines regenerated (`docs/baselines/`).

Exit criteria: met. Explicitly postponed to P3:
- view column types (24 Mesalia columns "not established"): they
  require query typing;
- routine/block scopes in the catalog, with the HIR (the checker's
  `TypeContext` carries them in the meantime);
- execution of dynamic SQL in bodies (`EXECUTE format(…)`, the
  `diesel_manage_updated_at` case), currently reported as `ddl-not-replayed`.

- `pgzest-catalog` crate: D2 model, layers, schemas and `search_path`
  (D11), case-sensitive keys (principle 8).
- Generator (`xtask catalog-gen`): extracts `pg_namespace`, `pg_type`,
  `pg_proc`, `pg_operator`, `pg_cast`, `pg_aggregate`, `pg_opclass`,
  `pg_am` from a pinned PG → `postgres@<major>` pack versioned in the repository.
  Extension packs as a before/after `CREATE EXTENSION` delta: `citext` and
  `btree_gist` first (Mesalia), then PostGIS (siren).
- Active contributors (D9): PG version from the config, extensions from
  the reconstructed state; diagnostics for missing / unused /
  unavailable extensions; migration header checks.
- salsa project database: files → declarations → objects resolved on
  demand; ordered application of DDL (`ALTER` included).
- Routine/block scopes: parameters, variables, temporary tables created
  in the body (visible after their creation), `NEW`/`OLD` depending on timing,
  events and level (row/statement), special variables (`FOUND`,
  `TG_OP`…).
- Migration of the LSP and the CLI onto this database; removal of
  `SchemaProvider`/`SchemaResolver`/`SchemaIndex`/`builtins.rs`.

- **Brownfield import** (brought forward from P7, Mesalia critical path):
  replay the Prisma migrations → declarative schema files organized
  by PG schema (`pgzest schema import`) + baseline migration with header.
  The import only emits the reconstructed catalog; it does not require the
  P7 diff.

**Exit**: Mesalia catalog reconstructed identical (per the oracle) to the
real catalog obtained by applying its migrations to an empty PG,
including extensions and schemas; Mesalia declarative schema imported and
reviewed; modifying a file → only dependent queries are
recomputed (test).

### P3 — Unified semantic analysis — ✅ done (2026-11-02)

Done (2026-11-01):
- `pgzest-sema`, type system drawn from the catalog:
  - coercions, common type, polymorphism;
  - function and operator overload resolution;
  - differential sweep: 19,032/19,032 resolutions identical to PostgreSQL.
- Expression and query typing on the normalized AST:
  - parameters typed by their first use;
  - D5 nullability;
  - column names from `FigureColname`.
- Query oracle: 243 queries, 749/751 columns (`ROWS FROM (…)`,
  unreadable for sqlparser, still to be read), 146/146 parameters; operator
  precedence checked against PostgreSQL (`make oracle`).
- View columns typed during replay (`QueryTyper`): Mesalia identical to the real one, 1,622/1,622 columns.
- (2026-11-02) `check` goes through sema (`pgzest_sema::check`): each
  statement is checked when it applies, against the catalog state at that
  point (the replay's `StatementChecker`); a PL/pgSQL body is checked against the final
  state if it is still in effect. CLI, LSP, incremental database and corpus
  share this mechanism. Mesalia: 0 diagnostics (injected fault: detected);
  siren: unchanged.

Remaining, in order:
1. Corpus: ✅ 183 hand-written queries (`make oracle`), plus
   probes generated from the real catalogs of `cargo xtask corpus`
   (`SELECT *`, `= $1`, `INSERT … VALUES ($1)`): Mesalia 3,342/3,342,
   siren 476/476; and the queries in bodies of routines in effect,
   prepared by PostgreSQL with their variables as typed parameters (temporary
   and transition tables recreated): Mesalia 58/58, siren 10/10, none
   unprobed. Remaining: the parser accepts unquoted `default` as a column
   name, which PostgreSQL rejects.
2. Resolved references: ✅ what P4 and the LSP consume — resolved catalog
   objects (`TypeSystem::depends`), column and variable references
   with position, type and nullability (`column_refs`,
   `var_refs`). A full HIR (standalone typed tree) remains to be done when a
   consumer needs it (lints, P6).
3. ✅ Named arguments (`MatchNamedCall`), data-modifying CTEs, `GROUPING SETS`,
   column definition lists, `RETURNING old/new`. Query lints
   only visit the `SELECT` bodies of CTEs.
4. ✅ Routine/block scopes in sema (`routine.rs`): named and
   positional parameters, labeled blocks, `variable_conflict = error` in PL/pgSQL,
   column before parameter in SQL, records whose structure follows
   assignments, cursors, `NEW`/`OLD`/`TG_*` per trigger table, transition
   tables, temporary tables of the body.
5. ✅ Checker ported to `pgzest-sema` (the old `TypeContext` and its rules
   are removed; `pgzest-checker` now only hosts plugin
   rules); compatibility diagnostic (`WHERE text_col = 42`).
   `pgzest-semantic` is removed: positional LSP hover (references
   resolved by sema), TypeScript codegen typed by sema (D5 nullability,
   enums, no more ordering heuristic). Remaining: lints ported to sema.
6. Execution of dynamic SQL in bodies: postponed from P2.
7. ✅ Table expressions in their context: `DEFAULT` (without columns,
   convertible to the column), generated expression (non-generated columns),
   boolean `CHECK`, `ALTER … SET DEFAULT`, `CREATE DOMAIN … CHECK (VALUE …)`
   — PostgreSQL 18 messages, verified.

Exit criteria met: 311 representative queries checked against
PostgreSQL (243 written, 68 from Mesalia and siren routine bodies), zero
false positives on the corpora, `WHERE text_col = 42` diagnosed. Postponed,
without blocking P4: lints ported to sema, full HIR, dynamic SQL in
bodies, `ROWS FROM (…)` (unreadable for sqlparser). (`DEFAULT` outside
assignment: rejected as PostgreSQL does since 2026-11-02.)

- `pgzest-sema` crate: normalization → resolution → typed HIR.
- Faithful type system: categories, implicit/assignment casts,
  operator and function overload resolution, `unknown` literals,
  `UNION`/`CASE`/`COALESCE`/`VALUES`, arrays, enums, domains.
- Nullability (D5). Inference of `$n`/`$name` parameters (D6).
- Semantics of in-scope constructs: forbidden insert/update
  of a generated column; `CHECK` and `GENERATED` expressions typed in the
  context of their table; `DEFAULT`; `ON CONFLICT`; `RETURNING`; `CALL`
  checked against the procedure signature; trigger body checked per
  table (existing, ported).
- Checker and lint ported to the HIR; removal of `pgzest-semantic` and of
  `TypeContext`.

**Exit**: differential oracle green on the query corpus (≥ 300
representative queries, including Mesalia bodies); zero false positives on
the corpora; type compatibility checked (`WHERE text_col = 42` →
diagnostic).

### P4 — Query descriptor — ✅ done (2026-11-02)

Done (2026-11-02):
- `pgzest_query::descriptor`: types (catalog references, OIDs of
  built-in types), three-state nullability of parameters (established by
  assignment to a column) and columns, cardinality inferred
  conservatively (primary/unique key fixed by equalities, aggregate without
  `GROUP BY`, `LIMIT 1`, single-row `INSERT … VALUES`, set-returning
  functions), `depends_on` (project relations, types, routines and operators
  resolved by sema), SHA-256 fingerprint of the re-emitted SQL and of the
  dependency definitions, `.pgzest/queries/<fingerprint>.json` cache.
- Parameters in the order of the positional rewrite (first
  appearance), not in analysis order.
- Declared cardinality (`-- name: x :one`): narrows inference (contract
  checked at runtime), does not contradict it (`none` against returned
  rows, or the reverse: error).
- `pgzest describe` (query files, `--sql`, `--cache`), JSON.
- The oracle now goes through the descriptor: 749/751 columns, 146/146
  parameters.
- `pgzest check` and the LSP report, on a query file, what
  prevents describing it (untyped parameter, contradicted cardinality).

- `pgzest-query` crate: §4 descriptor, cardinality, `depends_on`,
  fingerprint, disk cache.
- Named queries in files (`queries/*.sql`, `-- name: …` header or
  dedicated macro), `$name` → positional rewrite.
- CLI `pgzest describe <file|query>` and JSON output.

**Exit**: deterministic descriptors (snapshots); explicit refusal when
a type is unknown; oracle green on parameter and column types.

### P5 — Rust integration — 🚧 exit reached except the comparison with sqlx

Done (2026-11-02):
- `pgzest-runtime`: `Query` (positional SQL, parameter OIDs),
  `fetch_one`/`fetch_optional`/`fetch_all`/`execute` aligned with the
  cardinality, over `GenericClient` (client, transaction, pool);
  `query_typed` when OIDs are fixed, server-side preparation for a
  project type (enum); decoding by index; `Db`/`RowCount`/
  `Decode` errors.
- `pgzest_codegen::rust`: one function per query, parameter struct
  (borrowed) and row struct, D5 nullability (`Option` unless non-nullness is
  established; parameter of unknown nullability expected non-null), generated
  enums (labels), domains as their base type, arrays as
  `Vec<Option<T>>`, `time` or `chrono`; project mapping table;
  explicit refusal of a type without a mapping or of a column without a Rust
  name.
- `pgzest-build`: generation from `build.rs` (project from `pgzest.toml`,
  no connection), `rerun-if-changed` on the schema, the migrations, the
  queries and the macros; an undescribable query fails the
  compilation with its position.
- `examples/rust-service`: schema (enum, domain, array, uuid, dates),
  named queries, end-to-end test against PostgreSQL 18 (`make
  oracle`, CI).

- `pgzest-macros`: `query!` (inline query, `$name` parameters bound by
  `name = expr` and checked against the expected type, `fetch` aligned with the
  cardinality, `execute` without rows; SQL error = compile error on
  the string; `include_bytes!` of the project files for
  recompilation; project cached by the macro server, invalidated
  by modification time). Enums refused inline: use a named query.

- `COPY … FROM STDIN` as a named query: `copy_in` descriptor
  (fed columns, generated ones excluded or refused as PostgreSQL does),
  function generated over typed records (binary) or a byte stream
  (text, CSV), on a client or a transaction; the example reproduces the siren
  pattern (staging + binary and CSV COPY + swap) against PostgreSQL 18.

- Streamed rows (`_stream`) for queries of `many` cardinality.
- `make bench`: generated `page_by_id` against hand-written `tokio-postgres`,
  145.0 vs 145.7 µs per query (ratio 0.995, guardrail at 1.25).

- Compile budget (example, dev machine): schema modified → 1.5 s
  rebuild in debug (`build.rs` generation and `query!` included),
  code only modified → 1.05 s. At Mesalia scale (220 migrations),
  catalog + description + generation: 0.29 s in debug.
- Guide: [`site/src/content/docs/languages/rust.md`](site/src/content/docs/languages/rust.md).

- `query_file!`: query read from a file of the crate, errors
  positioned in that file.
- LSP: hover on a `-- name:` header → the query's descriptor.
- Mesalia catalog replay: 4.4 s → 76 ms in debug (filtering before
  joining in the evaluation of `DO` block conditions).

- Prisma and Diesel migrations read as is (`[migrations]
  directory`): a Prisma project like Mesalia plugs in without conversion.

Remaining: per-pack type mapping (PostGIS), checked CSV
header (`HEADER MATCH`), comparison with sqlx (crate unavailable
offline), budget measured on a Mesalia-sized project.

- `pgzest-runtime`: D8 execution, `fetch_one`/`fetch_optional`/`fetch_all`/
  `execute` aligned with the cardinality, streams (`RowStream`), transactions,
  typed `copy_in` (CSV with optional header validation, binary with
  structs).
- `pgzest-macros`: `query!`, `query_file!`, `copy_in!`; SQL diagnostics
  relocated into the Rust source; correct recompilation (§5).
- `build.rs` generation of named queries.
- Rust type mapping, extensible per pack (PostGIS).
- End-to-end example reproducing the siren load (staging + COPY +
  swap) on `tokio-postgres`.

**Exit**: an example service compiles and runs against PG in CI;
compile budget met; execution benchmark compared with
hand-written `tokio-postgres` (target gap: negligible) and with sqlx.

### P6 — Fragments and expression macros — ✅ exit reached (2026-11-03)

Done (2026-11-03):
- Fragments: `.sql` files in `[macros] directory`, `-- fragment: name`
  or `name(t, …)` headers, body in ordinary SQL documented by its comments.
  `@name` inserts them wherever code is copied (schema, migrations,
  named queries, `query!`, macro bodies), only for a declared fragment
  name (`@` otherwise remains absolute value). Parameters =
  relation qualifiers (`@f(e)`: `t.` → `e.`; `@f`: removed), nothing
  else is substituted. Non-atomic expression body parenthesized (nature
  read by the parser); nesting, cycles refused. Lexer that distinguishes
  strings, `$$…$$`, comments and quoted identifiers.
- Source map: opaque segments, inserted text is mapped back to
  the whole invocation; an error coming from a fragment is underlined on
  `@name(x)`; `$name` parameters brought by a fragment attached to the
  query, in order of appearance after insertion.
- Named queries: fragments inserted query by query in the file's
  coordinate frame (`NamedQuery::expanded`), descriptor and code generated on the
  inserted SQL; `-> other` (additive change to the frozen format): rows with the
  shape of query `other`, checked (names, order, types, nullability
  not relaxed), struct reused by the generated code; `query-row-mismatch`
  in `check` and the editor, cross-file references included.
- Editor: hover on an `@name` (doc, inserted SQL), fragment files
  checked (format) and taken into account unsaved, rechecking of the
  other documents when they change; `Fragments` role (never schema nor
  queries).
- **Exit**: `examples/rust-service` (siren pattern) — column list of the
  load written once (COPY, `INSERT … SELECT`, `ON CONFLICT … SET
  (…) = ROW(…)`), full-text expression written once for the index and the
  query, with the oracle verifying on PostgreSQL that the plan uses the index;
  `examples/demo` — "dish card" projection written once, three
  queries and a `DishCardRow` struct.

Additions (2026-11-03, after exit):
- One Rust module per query file (`queries/order/book.sql` →
  `queries::order::book`), enums at the root of the tree; qualified
  references `-> order::book::card` (relative path in the generated code);
  the promise of `site/src/content/docs/reference/named-queries.md` is kept.
- `pgzest_macros::queries!("queries")`: the tree of a directory, or the
  queries of a file, generated at the call site (a one-line
  `src/queries.rs`); same generation as `pgzest-build`
  (`pgzest_build::generate_path`).
- `[queries] include`: query directories next to the code, each the root
  of its own tree; a query file outside these directories (sandbox)
  is attached to the main root.
- `examples/demo` in declarative mode: full schema written by hand, the
  view's migration generated by `migrate create`, `src/queries.rs` by the
  macro, a `promotions::deals` submodule.

- `sql!`: one query per macro, name, cardinality, doc and visibility as
  parameters, string reduced to the SQL; `-> Path` (Rust struct of another
  query, `sql!` or file) checked by constant assertions on
  `PGZEST_COLUMNS`. Refreshed as you type in rust-analyzer (measured:
  0.1 s), whereas `queries!` only refreshes when the server restarts (a
  rust-analyzer limitation, measured: neither `rebuildProcMacros` nor `reloadWorkspace`
  rerun a macro whose input is unchanged).
- `#[pgzest_macros::types] mod db {}` and `[rust] types`: enums generated
  once, referenced by `sql!`, `queries!` and `build.rs`.
- Positions: generated tokens located on the call (no more rust-analyzer
  panics), names on the written name, link to the SQL line on hover.
- SQL highlighting of `sql!`/`query!` strings (injection grammar,
  verified with VS Code's Rust grammar), `pgzest-lsp` on `.rs` files
  (queries extracted by lexical scan, exact positions including
  escapes: diagnostics, hover, completion), semantic tokens reserved
  for SQL documents; hover on a `$parameter` (established type, not a
  homonymous column).
- Optional parameter `$name?` (nullable, `Option` in Rust); variant
  queries: `[[ … ]]` blocks (one parameter each) and `{{ name: a = … |
  b = … }}` choices (Rust enum), each combination described at compile time (≤ 256,
  same columns required), chosen at runtime by an exhaustive `match`.
  Variants obtained by equal-length erasure: file positions
  preserved.

Remaining:
- Fragments in a hand-written `CREATE FUNCTION … AS $$…$$`: not
  inserted (a dollar string is not interpreted); use `@function`.
- `pgzest build` does not yet eject a query file (the
  descriptors carry the inserted SQL, and so does the generated code).
- A real Mesalia projection: Mesalia's Rust component does not exist
  yet; the pattern is shown in the demo.

- Reusable named fragments (column list, predicates), typed in
  their context of use, with source maps (existing mechanism extended).
- A named query usable as a reference type by others
  (compatibility check, never a forced cast).
- Visible (LSP) and ejectable (`pgzest build`) expansion, like the current
  macros.

**Exit**: the siren column list and the recurring projections of
Mesalia/Sherpa expressed only once.

### P7 — Migrations — ✅ exit reached (2026-11-03)

Done (2026-11-03):
- `pgzest_catalog::diff`: two catalogs compared object by object
  (schemas, extensions, enums with `ADD VALUE` at its position, types,
  domains, sequences, tables, columns, constraints, views, routines,
  indexes, triggers, policies, row-level security, comments); ordering by
  dependencies (preliminary drops, creations sorted together by the
  export graph, definitive drops last); views
  rebuilt by transitive closure (view citing a view, a recreated routine
  or a table one of whose columns changes type or disappears),
  replaced when their typed columns allow it; routines
  replaced, or dropped then recreated if their output signature
  changes. What cannot be expressed without loss is reported as unsupported,
  never approximated.
- Safety rules M001 to M007 carried by each statement. The migration is
  always written, as simple as possible: data loss and likely failures are
  listed in its header for the developer to adapt (no `--allow-destructive`
  refusal); changes pgzest cannot express are left as `TODO`, and
  `migrate check` fails until they are written.
- Declarative mode (`[schema] declarative = true`, D10): the analysis
  replays only the schema, migrations are replayed separately (their
  diagnostics and context header checked against their own state),
  the gap is reported (`schema-drift`).
- `pgzest migrate create` (directory format: flat, Prisma, Diesel;
  `-- pgzest:` header; verified by replay before writing), `migrate
  check` (CI), `migrate diff` on the catalog. The old AST-based diff and
  `.schema-snapshots` are removed.
- Fidelity: a routine's body is kept as written (export and
  migrations); indexes: order, `NULLS`, operator class, `INCLUDE`,
  `NULLS NOT DISTINCT` modeled (`WITH (…)` refused); literal default
  keeping its type when the column changes type, as PostgreSQL does;
  `CREATE INDEX` checked (columns, expressions, predicate, operator
  class known for the method).
- Oracle (`pgzest_oracle::schema`, `make oracle`): applied migrations
  and declarative schema yield two databases identical according to PostgreSQL
  (`format_type`, `pg_get_*def`) — test evolution across four versions;
  `cargo xtask migrate-oracle <project>` on a real project; corpus
  reports enriched with the text-exact comparison.
- **Exit**: Mesalia imported (220 migrations ↔ declarative schema:
  3,864 identical lines), then an evolution (`pg_trgm` extension, GIN index
  `gin_trgm_ops`, column, enum value) produced by `migrate
  create`: 221 migrations ↔ schema, 3,868 identical lines.

Remaining:
- `ALTER DOMAIN`, `ALTER TYPE … ATTRIBUTE`, `ALTER SEQUENCE`, modified identity and
  generated expression, `SET LOGGED`: reported as unsupported.
- Privileges (`GRANT`/`REVOKE`): neither modeled nor compared.
- Names of NOT NULL constraints (PostgreSQL 18) not modeled: a
  column rename leaves the old name, the import does not reproduce it
  (no behavioral effect; the oracle compares these constraints without
  their name).
- `CREATE INDEX`: compatibility of the operator class with the type,
  default class and immutability of expressions not checked.
- View rebuilt as soon as a column of its table changes or disappears,
  even if not cited (correct, broader than PostgreSQL).
- Renames: not detected (drop + add, flagged in the header for the
  developer to turn into `RENAME`).

- Diff on the full D2 model (schemas, extensions, types, domains, enums
  including `ADD VALUE`, functions/procedures with dependency ordering,
  triggers, generated, checks, exclusions, policies, privileges).
- Automatic emission of `CREATE/ALTER/DROP EXTENSION` and `CREATE/DROP
  SCHEMA` (D9, D10, D11); context header on each migration.
- Extended safety rules (existing M001–M003).
- Oracle verification: migration applied on PG → introspected
  catalog == declarative schema.

**Exit**: a real evolution of the Mesalia schema produced by `pgzest
migrate create` and validated by the oracle, including an extension addition.

### P9 — Procedure observability — ✅ exit reached (2026-11-04)

Ports the mechanisms of kysely-pg-procedures that take the "black box"
out of a PL/pgSQL run, as directives of the PL/pgSQL macros (`@function`,
`@procedure`, `@trigger`), instrumented by pgzest from the analysis of
the body rather than by hand. Order: (1) isolated temp tables and (3)
`@snapshot`, then (2) log levels and (5) `pgzest dev` (apply on save), then
(6) tamper detection and (4) OpenTelemetry — done.

**Isolated temp tables** — `@temp_table name (columns);` at the top of a
body. A session-scoped temp table is shared by every call in the session:
a nested or recursive call, or a call per loop iteration, mixes its rows
with the caller's. Each run therefore gets an id (`_pgzest_run uuid :=
gen_random_uuid()`) and every access is rewritten from the parsed body:

- the table is created `IF NOT EXISTS` under a physical name suffixed with
  a hash of its definition (two routines declaring `work` differently, or
  a definition changed by a deployment while a pooled session keeps the old
  table, never meet), with a hidden last column `_pgzest_run`;
  `PRIMARY KEY` / `UNIQUE` include it (uniqueness per run);
- a read (`FROM`, `JOIN`, `USING`, subqueries, CTEs, expressions) becomes
  `(SELECT <declared columns> FROM <physical> WHERE _pgzest_run = <run>)`
  under the written alias, so `*` never shows the hidden column;
- `UPDATE` / `DELETE` get `AND <alias>._pgzest_run = <run>`;
- `INSERT` fills the hidden column through its default
  (`current_setting('pgzest.run')`), set by `set_config(…, true)` right
  before each statement that inserts (a nested call may have changed it);
- the run's rows are deleted before each `RETURN` and at the end of the
  body; an error rolls them back with the rest.

Nothing is guessed: an access pgzest cannot rewrite is an error (temp
table in `TRUNCATE`, DDL, `RETURNING *`, a CTE shadowing it, a `RETURN`
expression reading it, a position the parser could not give), and dynamic
SQL in an instrumented body is a warning (not isolated).

**`@snapshot label;`** — in a debug build (`pgzest build --log debug`, later
`pgzest dev`), copies at that point the run's rows of every isolated temp
table and the variables in scope (parameters and declarations) into
`pgzest_debug.snapshot` / `pgzest_debug.snapshot_rows`; in a normal
build (analysis, migrations, deployment) it emits nothing. A record not
yet assigned cannot be read: it is reported as skipped, never guessed.
Snapshots written in a transaction that fails are rolled back with it.

**Log levels** — `pgzest build --log none|info|step|debug` and
`--log-target table|notify`, fixed at build time (no code below the
level): `info` writes one entry per run (routine, start, duration, the
caller's `pgzest.traceparent`, the `SQLSTATE` of an error the routine
handles), `step` adds one per INSERT / UPDATE / DELETE / EXECUTE (kind,
target, source line, row count), `debug` adds the snapshots. Steps are
appended to a `jsonb[]` variable by `GET DIAGNOSTICS` and a simple
assignment, which leave `ROW_COUNT` and `FOUND` untouched, and written at
each exit — with the temp table cleanup, `FOUND` saved and restored around
it for a `RETURN` that reads it — to `pgzest_log.run` / `.step`
(unlogged) or as `pg_notify('pgzest_log', json)`. The project level is
`[log] level` / `target` in `pgzest.toml`: analysis, the LSP, `pgzest
build` and `pgzest migrate` all expand the routines at it, and the log
tables are part of the replayed schema (a declarative project's migrations
create them). Each generated migration records the level in its header
(`-- pgzest: postgres=18 log=step`, `log=step:notify`); the replay checks
it against the instrumentation of the routines the file creates, and that
the tables they write to exist at that point. A level change regenerates
every macro routine, with a note in the migration.

**Traces** — each logged run knows the run that caused it (calling
routine, or the statement that fired a trigger) and its depth:
`pgzest.span` / `pgzest.depth`, transaction-local settings set in the
declarations (before any statement, outside an exception block's
subtransaction) and restored at each exit; an error leaving the routine
rolls them back with its subtransaction. A `RETURN <value>` becomes
`DECLARE _pgzest_result <type> := <value>; BEGIN <exit> RETURN
_pgzest_result; END;` (result type without modifiers, `record` for a
trigger): the calls of the value are runs of the routine, and it may read
`FOUND` or a temp table; a polymorphic result keeps the exit before the
`RETURN`, with a warning when the value calls something; a variable, a
field or a constant (`RETURN NEW;`) needs no block. `pgzest.max_depth`
bounds the depth (`54001`, naming the routine). The `notice` target raises
a start and an end event per run, which reach the client even when the
transaction fails — only under a sampled `pgzest.traceparent` (W3C flag,
matched by a pattern: absent or malformed reads as not sampled, never an
error), so that unsampled requests pay ~1 µs per run instead of ~33: a run
not sampled reads the traceparent, and keeps the depth only for a
`pgzest.max_depth` set; no run id, clock, parent or span setting. The
start event names the run (traceparent on the outermost run only), the
end refers to it by id; steps carry their offset from the start, built
with `json_build_object` (no jsonb round trip). Not done: steps as
indexes into a per-routine manifest — sending costs per message (two
empty notices cost about as much), not per byte; `pgzest-runtime::trace` (feature `trace`) assembles
them into trees, closes the runs a failed statement left open, and
(feature `opentelemetry`) exports each tree as spans under the caller's
`traceparent` — the SDK generates span ids, so a tree is exported when its
outermost run ends, parents first; through the `opentelemetry` API only,
the provider and exporter being the application's. `Tracing::drive`
passes every other message (notices, `LISTEN` notifications) to the
application. Oracle: routine → trigger → routine nested, the parent
restored after a handled failure, an endless recursion seen through its
notices; spans checked in an in-memory exporter. Only macro routines are
instrumented: logging is opted in, and visible in the migration. Cost per
run (PostgreSQL 18): ~6 µs at `info`, ~12 µs at `step` (`table` or
`notify`), ~33 µs with `notice` when sampled (~23 µs of it sending), ~1 µs
when not; nothing at `none`.

**Macro slots** — every slot of a macro header is rendered by its
template, checked when the definition loads: `@trigger … when (…)` used to
capture the condition and drop it silently. `wrap <slot> -> <text>`
renders an optional slot only when given.

**`pgzest dev`** — applies the routines (functions, procedures,
triggers) of the schema files to a development database, at the `debug`
log level by default, then again on each save (files polled every
300 ms, `pgzest.toml` and macros included). One transaction per file,
`CREATE OR REPLACE` ensured; a changed return type or parameter name is
retried after `DROP ROUTINE`; routines and triggers a file no longer
declares are dropped when this session created them; database errors are
mapped back to the source position. Tables stay with migrations.

**`pgzest db check`** — tamper detection, on the database rather than on
migration files: editing a generated migration is expected (the developer
adapts it, `migrate check` verifies the final state). The migrations the
database received (`_prisma_migrations`, `__diesel_schema_migrations`,
else all or `--until`) are replayed in a scratch database on the same
server (or `--scratch-url`, same major version required), and both are
described by PostgreSQL (`pgzest_oracle::schema::introspect`, the P7
oracle's description); the migrations table is left out. Differences are
grouped by object — extra, missing, changed with a line diff of the
definitions. A migration recorded but missing from the repository, or
started and never finished, is an error. Oracle test: a hotfixed routine,
an added index and a dropped trigger found; pending migrations left out.

**Exit**: a routine using a temp table, called recursively and from a loop
on PostgreSQL 18, keeps its rows apart (oracle test), and its snapshots
show each step.

### P8 — TypeScript

- Extraction of queries in TS files (oxc parser), descriptors →
  `.d.ts`, Vite plugin + watch mode, thin runtime on `pg`/`postgres`.
- Constraint to accept: a *tagged template* loses the literal type of its
  string in TypeScript. Typing therefore goes through generated overloads
  indexed by the exact string (the graphql-codegen "client preset"
  model) or through a language service plugin — choice to be made
  when the phase opens.
- Resumption or replacement of `npm/pgzest` and the bindings.

**Exit**: the `getPage` example of the target DX typed end to end, without
cast.

**Later, with the Vite plugin (not a priority): devtools.** A panel in
the Vite DevTools (Nuxt included) showing the queries the current page ran
and, under each one, the routine and trigger runs it caused (P9 traces,
snapshots). Prerequisites, useful without any panel:

1. A span per query in the runtimes (Rust, then TypeScript): query name,
   parameters, duration, rows; behind a feature.
2. Correlation with the page: the browser sets a `traceparent` on its
   requests, the backend passes it on (`pgzest.traceparent`, already
   read by the routines).
3. A feed to read them from, locally: served by `pgzest dev`, or an
   OpenTelemetry collector (a local Jaeger already shows the trees).

Candidate UI: [Devframe](https://devfra.me) (the foundation of Vite
DevTools): one definition mounted in Vite DevTools, standalone, as a
static report or as an MCP server. Experimental as of 2026-11; to be
reassessed when this item opens.

After TypeScript, **.NET** (Npgsql) is considered, not committed: a
generator over the same descriptor. The site presents languages as such
(`site/src/content/docs/languages/`: shared SQL side, per-language
generated code and runtime).

### P10 — Publication: README and documentation site — 🚧 in progress

Done (2026-11-04): README redesigned with `beautify-github-readme`
(README mode, pure static SVG: `assets/readme/hero.svg`, the demo's
`restaurant_by_slug` threaded to its generated struct, and
`how-it-works.svg`); Starlight site in `site/` (`make site`, `make
site-dev`): getting started, how it works, six guides, reference; the
user-facing `docs/*.md` moved there, `docs/` keeps the internal
documents. `cargo xtask docs` generates the CLI reference (from `--help`)
and the diagnostics reference (from the documented code constants, every
sema code moved to `codes.rs`, dead codes removed); `--check` in CI, with
the site build and its link validation. Name and repository settled
(2026-11-04): `pgzest`, `github.com/Creatiwity/pgzest`. `pgweave` was
already taken (the `pg_weave` extension, `weavepg.dev`); the zest says the
project's stance: the SQL stays raw, it is seasoned, not cooked. Visual
identity to match (citrus palette, zest mark). Remaining: the packages (publication), the TypeScript guide
with P8.

Makes the project presentable before it is published: what it does is
there, scattered between the README, `docs/` and this plan.

- **README** redesigned with the
  [`beautify-github-readme`](https://github.com/oil-oil/beautify-github-readme)
  skill (Claude Code): reading order built around the key features and
  their proof — typed queries (Rust, then TypeScript), analysis as
  PostgreSQL would do it (oracle), fragments and macros, declarative
  schema and migrations, routine observability (run logs, traces,
  `max_depth`, `db check`), editor. SVG hero and diagrams, Markdown for
  commands. Local preview first; every claim checked against the code
  (measured costs, oracle results), nothing promised that is not done.
- **Documentation site**: getting started, guides (Rust queries,
  TypeScript queries, macros and fragments, migrations, observability),
  reference (CLI, `pgzest.toml`, diagnostic codes, macro format),
  examples (the demo as a guided tour). `docs/` is its source; the
  generator is chosen when the phase opens (mdBook, Rust-native, or a
  JS one such as Starlight/VitePress if the TypeScript audience
  prevails). Built and link-checked in CI, the CLI and config reference
  generated from the code (`--help`, the config structs) so it cannot
  drift.
- Before publishing: package names (crates.io, npm), license,
  versioning and changelog (name and repository settled above).
  Settled 2026-11-06: license MIT OR Apache-2.0 (© Creatiwity SAS);
  crates sorted, merged and renamed (§9), 17 published crates, version
  0.1.0 for all, `cargo publish --workspace --dry-run` in CI; changelog
  generated by git-cliff from Conventional Commits after 0.1.0; the LSP
  is `pgzest lsp`, one binary per platform (macOS arm64, Linux x64/arm64,
  Windows x64), shipped in platform-specific VS Code packages; release
  workflow on a version tag, publishing behind the `release` environment
  (RELEASING.md); site on GitHub Pages with its legal notice (publisher
  Creatiwity SAS). Remaining, the owner's: accounts and tokens
  (crates.io, Marketplace, Open VSX), the `release` environment, Pages,
  then the tag. The npm package stays frozen until P8; the JetBrains
  plugin is not published.
- Before publishing, the history: no private corpus structure may be
  public. Audit of 2026-11-04 (every blob of every commit, every commit
  message, against the identifiers of the Mesalia migrations): no
  migration, schema or column list ever committed; the baselines hold
  aggregates only. Two isolated names: a table name used as a quoted
  identifier example (in the tree until 2026-11-04, replaced by
  `"OrderLine"`; still in older commits) and an enum type name in the
  message of `e0e4420`. The owner judged them harmless (2026-11-04): the
  history is published as is. Publishing itself — site,
  packages, pushes — only with explicit approval.

**Exit**: the README and the site, previewed locally, describe every
shipped feature with a runnable example, and no feature that is not
shipped; the site builds in CI without broken links.

Found while checking the claims (to fix, outside P10):
- A file classified as queries (outside `schema/` and `migrations/`) is
  checked on the final schema without its own DDL being applied: a trigger
  function there never meets the `CREATE TRIGGER` of the same file
  (`trigger-no-context`, body unchecked) — `examples/playground/queries/
  06-macros.pgsql`. CLI and LSP alike.
- Parenthesized joins (`t JOIN (u JOIN v ON …) ON …`, `FROM (t JOIN u
  ON …) AS x`) and, since sqlparser 0.63, right-deep joins (`t JOIN u
  JOIN v ON … ON …`): parsed, but `TableFactor::NestedJoin` is not
  converted (`unsupported-statement`). Found 2026-11-06.
- `ORDER BY … USING <op>`: parsed since sqlparser 0.63, not modeled.
- ~~The `[plugins] builtin postgis` plugin is a hand-written list of
  PostGIS signatures~~: removed with the `prisma` plugin (2026-11-06,
  before publication); plugins contribute rules only, never schema.

### Possible parallelization

- P1 (parser) and the P2 catalog generator are independent.
- P6 can start as soon as P4 is done.
- P7 can start as soon as P3 is done (the diff does not depend on the descriptor).
- P10 can start before P8 ends: the Rust side is documented first, the
  TypeScript guide is added with P8.

### Mesalia trajectory (transition without shortcuts)

The gRPC component starts before P5 is delivered. Rather than rushing the
phases at the cost of their robustness, the transition is organized so that the
code written in the meantime **migrates mechanically** to the generated code:

1. **From the component's start**: queries written in
   `queries/*.sql` files in the named-query format (P4) and with the
   `$name` syntax (D6); execution via `tokio-postgres` and a thin hand-written
   layer that follows the shape of the future runtime (one function per query,
   parameter struct, row struct). `pgzest check` (existing) already
   checks table/column existence in CI.
2. **The named-query format is therefore frozen in P0**, not in P4 — this is
   the only phase advance requested by Mesalia.
3. **Prisma remains the owner of migrations** until the end of P2 (import
   reviewed). After that, no more Prisma migrations: new migrations
   are written in SQL in the repository, checked by `pgzest check`, then
   generated by `pgzest migrate create` from P7 on.
4. **When P5 is delivered**: the manual layer is replaced query by
   query with generated code; compilation reports every type or
   nullability gap the manual layer was hiding.

Status as of 2026-11-02: the component can start directly on generated
code (`pgzest-build`, `pgzest-runtime`, `site/src/content/docs/languages/rust.md` guide), its
`pgzest.toml` pointing to the existing Prisma migrations; the manual
layer from step 1 is no longer needed.

---

## 9. Crate organization

Settled before the first publication (2026-11-06): the legacy code is
removed (`ts_types`, the hand-written plugins), the small crates are
merged, and `pgzest-macro` is renamed so it is not confused with the
`pgzest-macros` proc-macros.

```
crates/
  pgzest-diagnostics/  diagnostics, codes, directives (no internal dependency)
  pgzest-ast/          syntactic AST, spans, SQL emission
  pgzest-parser/       PgzestDialect + PL/pgSQL
  pgzest-expand/       macros, fragments, source maps, instrumentation
  pgzest-schema/       pgzest.toml, file roles, sources, migration headers
  pgzest-catalog/      object model, generated packs (D9), DDL replay
  pgzest-sema/         normalization, resolution, types, nullability → HIR
  pgzest-query/        named queries, query descriptor
  pgzest-codegen/      Rust generation, SQL injection
  pgzest-lint/         style / perf / migration-safety rules
  pgzest-analysis/     checking + plugin rules, the salsa project database,
                       the pipeline shared by the CLI and the LSP
  pgzest-plugin/       builtin and WASM plugins
  pgzest-oracle/       differential oracle; live-schema introspection (db check)
  pgzest-cli/          the `pgzest` binary, language server included (`pgzest lsp`)
  pgzest-runtime/      tokio-postgres execution of generated queries
  pgzest-build/        named queries compiled from build.rs
  pgzest-macros/       `sql!` / `query!` proc-macros
packs: crates/pgzest-catalog/packs/ (generated, D9)
xtask/                 catalog-gen, corpus, docs, migrate-oracle
```

Users depend on `pgzest-runtime`, `pgzest-build` and `pgzest-macros`, and
install `pgzest`; the other crates are published as their dependencies,
without a stability guarantee of their own.

---

## 10. Open questions

To be settled at the latest when the indicated phase opens.

| Question | Phase |
|---|---|
| Which PG major versions to generate and maintain as packs | P2 |
| Cut-over date for "no more Prisma migrations" for Mesalia | P2 |
| `time` or `chrono` by default | P5 |
| Generated overloads vs language service plugin for TS | P8 |
