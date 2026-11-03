# pgsql-tools — Spec: Schema universalization & PL/pgSQL analysis

> Design document — iterate before implementing.

---

## 1. Schema universalization (`SchemaSource`)

### Motivation

A real project can have its schema spread across:
- a single DDL file (`schema.sql`)
- a folder of numbered migrations (`migrations/000001_init.sql`, `000002_add_users.sql`, …)
- a mix of both, plus functions in a separate folder
- a pg_dump dump

The type-checker and the LSP must not need to know where the schema comes from. Resolution must be uniform.

---

### 1.1 `FileRole` — classifying SQL files

```rust
enum FileRole {
    /// Numbered migration file. The only place where structural DDL is allowed.
    Migration { sequence: u64 },
    /// Reference DDL file (static schema, pg dump, types file).
    Schema,
    /// Application queries (.sql embedded in code, seed scripts, etc.)
    Query,
}
```

**Automatic detection** (heuristic on the file name):
- `^[0-9]+_.*\.sql$` → `Migration`
- `schema.sql`, `*.schema.sql`, files in `schema/` → `Schema`
- Everything else → `Query`

Overridable through project config (`pgsql-tools.toml`).

**DDL rules per role**:

| DDL | Migration | Schema | Query |
|---|---|---|---|
| `CREATE TABLE/TYPE/FUNCTION` | ✅ | ✅ | ❌ E-DDL-001 |
| `ALTER TABLE`, `ADD COLUMN` | ✅ | ⚠️ W-DDL-002 | ❌ E-DDL-001 |
| `DROP TABLE/COLUMN` | ✅ | ⚠️ W-DDL-003 | ❌ E-DDL-001 |
| `INSERT/UPDATE/DELETE` | ✅ (seed) | ⚠️ W-DDL-004 | ✅ |

---

### 1.2 `SchemaSource` — source abstraction

```rust
pub trait SchemaSource: Send + Sync {
    /// SQL chunks to index, with their origin and role.
    fn chunks(&self) -> Vec<SqlChunk>;
}

pub struct SqlChunk {
    pub origin: SourceOrigin,  // (path: PathBuf, byte_range: Range<usize>)
    pub sql: String,
    pub role: FileRole,
}
```

**Concrete implementations**:

```rust
/// A single file — role detected or explicit.
struct FileSource { path: PathBuf, role: Option<FileRole> }

/// All the .sql files of a folder, sorted (important for migrations).
struct DirSource  { root: PathBuf, pattern: &'static str }

/// Arbitrary composition of sources.
struct MultiSource { sources: Vec<Box<dyn SchemaSource>> }
```

**Resolution order**: `Migration`s are processed in `sequence` order. The last definition of an object (same name, same schema) wins — the same behavior as a `CREATE OR REPLACE`.

---

### 1.3 `SchemaIndex` — fast first pass (names only)

Lightweight scan (regex/token scan, no full AST) over all chunks. Goal: < 50ms on 500 migration files.

```rust
pub struct SchemaIndex {
    pub tables:    HashMap<String, IndexEntry>,    // "public.users" → entry
    pub functions: HashMap<String, IndexEntry>,    // "public.get_user" → entry
    pub procedures:HashMap<String, IndexEntry>,
    pub types:     HashMap<String, IndexEntry>,
    pub schemas:   HashSet<String>,
    pub sequences: HashMap<String, IndexEntry>,
}

pub struct IndexEntry {
    pub origin: SourceOrigin,
    pub role:   FileRole,
    /// Position in the file (for go-to-definition).
    pub def_range: Range<usize>,
}
```

The `SchemaIndex` is rebuilt incrementally on file changes (the LSP only invalidates the entries of the modified file).

---

### 1.4 `SchemaResolver` — lazy second pass

Full parse of an object only when it is needed.

```rust
pub struct SchemaResolver {
    index:      Arc<SchemaIndex>,
    source_map: HashMap<PathBuf, String>,           // path → raw SQL content
    cache:      DashMap<String, Arc<TableSchema>>,  // "public.users" → columns
}

impl SchemaProvider for SchemaResolver {
    fn get_table(&self, name: &str) -> Option<Arc<TableSchema>> {
        // 1. cache hit → return immediately
        // 2. lookup in the index → finds the source file
        // 3. parse only the matching CREATE TABLE statement
        // 4. store in cache, return
    }
}
```

**Naming rule**: names in the index are always `schema.name` (e.g. `public.users`). Resolution accepts `users` (→ falls back to `public.users`) and `myschema.users`.

---

### 1.5 Project config (`pgsql-tools.toml`)

```toml
[schema]
sources = [
    { path = "migrations/",  role = "migration" },
    { path = "schema/types.sql", role = "schema" },
]

[schema.search_path]
default = ["public", "extensions"]
```

---

## 2. Analysis of PL/pgSQL function bodies

### 2.1 Current problem

`check_plpgsql_body` extracts SQL fragments line by line. This approach is fundamentally broken because it extracts *fragments*, not *complete statements*. Result: false positives on everything that depends on context (variables, parameters, CTEs).

**Note**: CTEs (`WITH … AS (…)`) are not a special case in function bodies — they work as in standard queries. The only problem is that the current extractor splits them. With correct extraction of complete statements, the existing checker handles them naturally.

---

### 2.2 `PlpgsqlExtractor` — structured extraction

Replaces the line-by-line scanner. Parses the PL/pgSQL structure to produce:

```rust
pub struct PlpgsqlBody {
    pub context:    PlpgsqlContext,
    pub statements: Vec<PlpgsqlStmt>,
}

pub enum PlpgsqlStmt {
    /// A complete SQL statement (SELECT, INSERT, UPDATE, DELETE, WITH …)
    Sql { sql: String, span: Span },
    /// EXECUTE format(…) — dynamic SQL, cannot be analyzed
    DynamicSql { hint: &'static str, span: Span },
    /// CREATE TEMP TABLE — defines a temporary table in the context
    CreateTempTable { def: TempTableDef, span: Span },
    /// Variable assignment (`:=`)
    Assign { var: String, span: Span },
    /// Control block (IF/LOOP/FOR/…) — contains other statements
    ControlBlock { children: Vec<PlpgsqlStmt>, span: Span },
}
```

The extractor builds on the existing PL/pgSQL parser (`crates/pgsql-parser/src/plpgsql.rs`) and extends its structural analysis.

---

### 2.3 `PlpgsqlContext` — virtual resolution layer

Built from the DECLARE section and the function parameters.

```rust
pub struct PlpgsqlContext {
    /// Function parameters (name, type)
    pub params: Vec<(String, DataType)>,
    /// Variables declared in DECLARE
    pub declared: Vec<(String, PlVar)>,
    /// Temporary tables created in the body
    pub temp_tables: Vec<TempTableDef>,
    /// Injected system tables (observability — see §3)
    pub system_tables: Vec<TempTableDef>,
    /// Injected system variables (see §3)
    pub system_vars: Vec<(String, DataType)>,
}

pub enum PlVar {
    Scalar(DataType),
    /// `RECORD` — type statically unknown
    Record,
    /// `tablename%ROWTYPE`
    RowOf(String),
    /// Cursor variable
    Cursor,
}

pub struct TempTableDef {
    pub name: String,
    pub columns: Vec<ColumnDef>,
    /// true if this table is injected by the runtime (§3)
    pub injected: bool,
}
```

The `PlpgsqlContext` is passed to the checker as an **additional resolution layer**, on top of the `SchemaResolver`. The resolution hierarchy for a SQL statement extracted from a function body:

```
1. Statement CTEs (local scope, already handled by the checker)
2. Temporary tables (PlpgsqlContext.temp_tables + system_tables)
3. Variables/parameters (PlpgsqlContext.params + declared)
4. Global schema (SchemaResolver)
```

---

### 2.4 Error classification (replaces suppression)

```rust
pub enum BodyDiagKind {
    /// Reference resolved through the PL/pgSQL context — not an error.
    ResolvedAsVariable { name: String, var_type: PlVar },
    /// Reference resolved through a local CTE — not an error.
    ResolvedAsCte { name: String },
    /// Reference resolved through a temporary table — not an error.
    ResolvedAsTempTable { name: String },

    /// Unknown name: not in the schema, not in the context.
    /// → Error shown (E002 with context [in function_name]).
    UnresolvedRef { name: String },

    /// Name that looks like a RECORD variable used as a table alias.
    /// → Warning W-PLV-001 with hint "declare explicit type".
    PossiblyVariable { name: String, declared_as: PlVar },

    /// Dynamic SQL (EXECUTE / EXECUTE format(…)).
    /// → Info I-DYN-001: "cannot type-check dynamic SQL".
    DynamicSql,
}
```

**What used to be suppressed** (`E002 [in function_name]`) is now classified:
- If the name is in `PlpgsqlContext` → `ResolvedAs*` (silent)
- If unknown → `UnresolvedRef` (real error, shown)
- If ambiguous (RECORD) → `PossiblyVariable` (warning)

---

## 3. Virtual schema — Observability runtime

Inspired by `kysely-pg-procedures`. When a function uses temporary tables **or** observability mode is enabled, the runtime injects extra elements into the function's virtual schema. The type-checker must know these elements to avoid false positives, and they are emitted in the generated migration.

### 3.1 Automatic injection on temporary tables

Every temporary table defined in a function automatically gets an extra column:

```sql
"_proc_instance_id" UUID NOT NULL
```

This column:
- isolates rows per procedure execution (avoids collisions on recursive or parallel calls)
- is automatically injected into every generated `INSERT`
- is used as a filter in every `SELECT/UPDATE/DELETE` on the temporary table
- triggers an automatic `DELETE FROM temp_table WHERE _proc_instance_id = _proc_instance_id` before `RETURN`

### 3.2 Injected system variables (DECLARE)

When observability is enabled (or as soon as there are temporary tables):

```sql
DECLARE
    _proc_instance_id  UUID       := gen_random_uuid();
    _proc_started_at   TIMESTAMPTZ := clock_timestamp();
    _traceparent       TEXT       := current_setting('app.traceparent', true);
    _span_id           TEXT       := encode(gen_random_bytes(8), 'hex');
    -- 'step' or 'debug' mode only:
    _proc_row_count    INTEGER;
    _proc_steps        JSONB[]    := ARRAY[]::JSONB[];
```

These variables are added to `PlpgsqlContext.system_vars` → the type-checker sees them as valid scalar variables.

### 3.3 Injected system tables (virtual schema)

Available in every instrumented function:

```
_proc_log (
    execution_id    UUID        NOT NULL,
    function_name   TEXT        NOT NULL,
    traceparent     TEXT,
    span_id         TEXT,
    started_at      TIMESTAMPTZ NOT NULL,
    duration_ms     INTEGER     NOT NULL,
    steps           JSONB                   -- 'step'/'debug' mode only
)

_proc_snapshot (
    execution_id    UUID        NOT NULL,
    function_name   TEXT        NOT NULL,
    snapshot_name   TEXT        NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL
)

_proc_snapshot_rows (
    execution_id    UUID        NOT NULL,
    snapshot_name   TEXT        NOT NULL,
    table_name      TEXT        NOT NULL,
    row_data        JSONB       NOT NULL
)

_proc_snapshot_vars (
    execution_id    UUID        NOT NULL,
    snapshot_name   TEXT        NOT NULL,
    var_name        TEXT        NOT NULL,
    var_value       TEXT
)
```

These tables are added to `PlpgsqlContext.system_tables` (`injected: true`) → visible to the type-checker without belonging to the global `SchemaResolver`.

### 3.4 NOTIFY and traceparent

At the end of the function (before `RETURN`), the runtime emits:

```sql
PERFORM pg_notify('proc_log', json_build_object(
    'execution_id',  _proc_instance_id,
    'function_name', '<function_name>',
    'traceparent',   _traceparent,
    'span_id',       _span_id,
    'started_at',    _proc_started_at,
    'duration_ms',   EXTRACT(MILLISECONDS FROM clock_timestamp() - _proc_started_at)::int,
    'steps',         _proc_steps   -- if step/debug mode
)::text);
```

The `traceparent` is read from `current_setting('app.traceparent', true)`. The application middleware sets it with `SET LOCAL "app.traceparent" = '...'` before calling the procedure.

### 3.5 Named snapshot

Internal API (usable in the function body through the pgsql-tools SQL syntax):

```sql
-- Captures the state of temporary tables and variables at this point
PERFORM pgsql_tools_snapshot('<snapshot_name>');
```

Desugared to:
```sql
INSERT INTO _proc_snapshot VALUES (_proc_instance_id, '<fn>', '<snapshot_name>', clock_timestamp());
INSERT INTO _proc_snapshot_rows
    SELECT _proc_instance_id, '<snapshot_name>', 'MyTempTable', row_to_json(t)
    FROM "MyTempTable" t WHERE t._proc_instance_id = _proc_instance_id;
-- + INSERT INTO _proc_snapshot_vars for each relevant declared variable
```

### 3.6 Recursive calls (direct and indirect)

`_proc_instance_id` is a PostgreSQL variable local to each function call — under recursion, each call has its own UUID. Since temporary tables are filtered by `_proc_instance_id`, there is no collision between recursion levels.

The codegen has nothing special to do to handle recursion: isolation is structural.

### 3.6b Enabling instrumentation

**By default**: automatically enabled on all PL/pgSQL functions/procedures.

**Per-function opt-out** (comment immediately before the `CREATE`):
```sql
-- @pgsql-tools: no-instrumentation
CREATE OR REPLACE FUNCTION my_simple_helper() …
```

**Per-function opt-in** (if disabled globally in the config):
```sql
-- @pgsql-tools: instrumentation
CREATE OR REPLACE FUNCTION my_critical_fn() …
```

**Project config** (see §5 — `pgsql-tools.toml`):
```toml
[instrumentation]
enabled = true          # true by default; false for a global opt-out
log_level = "step"      # "none" | "step" | "debug"
log_target = "notify"   # "notify" | "table" | "both"
```

---

### 3.7 Generation in the migration

The virtual schema elements are **emitted in the generated migration** in a separate block:

```sql
-- [PGSQL-TOOLS:RUNTIME:BEGIN]
-- Observability tables (created only once in the schema)
CREATE UNLOGGED TABLE IF NOT EXISTS _proc_log ( … );
CREATE UNLOGGED TABLE IF NOT EXISTS _proc_snapshot ( … );
CREATE UNLOGGED TABLE IF NOT EXISTS _proc_snapshot_rows ( … );
CREATE UNLOGGED TABLE IF NOT EXISTS _proc_snapshot_vars ( … );
-- [PGSQL-TOOLS:RUNTIME:END]
```

This block is idempotent (`IF NOT EXISTS`) and placed before the functions that depend on it.

---

## 4. Migration hash (Roadmap — do not implement now)

### Motivation

Let the LSP type-check a migration file *while it is being edited* by understanding the schema state at every point of the file, and detect any divergence between the expected state and the actual state.

### 4.1 Marker format

Each change is a `BEFORE`/`AFTER` scope identified by a short `id` (truncated hash or short UUID) shared between both tags. The `BEFORE` carries the hash of the state **before**, the `AFTER` carries the hash of the state **after** — the `id` pairs them unambiguously even in a long file.

```sql
-- [PGSQL-TOOLS:MIGRATION:BEGIN id="mig_a3f7c" schema_hash="a3f7c12"]

-- [PGSQL-TOOLS:CHANGE:BEFORE id="chg_b91d4" kind="table" name="public.users" hash="b91d4e8"]
ALTER TABLE users ADD COLUMN email_verified boolean DEFAULT false NOT NULL;
-- [PGSQL-TOOLS:CHANGE:AFTER id="chg_b91d4" hash="c42e1a9"]

-- [PGSQL-TOOLS:CHANGE:BEFORE id="chg_d03c1" kind="function" name="public.get_user" hash="d03c1f2"]
CREATE OR REPLACE FUNCTION get_user(p_id uuid) RETURNS users …;
-- [PGSQL-TOOLS:CHANGE:AFTER id="chg_d03c1" hash="e71a9b3"]

-- [PGSQL-TOOLS:MIGRATION:END id="mig_a3f7c" schema_hash="d57f3b1"]
```

**Format rules**:
- The `id` of a `CHANGE:BEFORE` and of its `CHANGE:AFTER` are identical.
- The `id` of a `MIGRATION:BEGIN` and of its `MIGRATION:END` are identical.
- `kind` on the `BEFORE`: `"table"` | `"function"` | `"procedure"` | `"type"` | `"sequence"` | `"index"` | `"policy"` | `"trigger"`.
- A `CHANGE:BEFORE` with no matching `CHANGE:AFTER` → error W-MIG-002 "unclosed change scope".

### 4.2 Hash semantics

| Marker | Hashed content |
|---|---|
| `MIGRATION:BEGIN schema_hash` | Hash of the full `SchemaIndex` (all objects, names + signatures) |
| `CHANGE:BEFORE hash` | Hash of the object (`kind`) **before** the following DDL |
| `CHANGE:AFTER hash` | Hash of the object **after** the DDL |
| `MIGRATION:END schema_hash` | Hash of the full `SchemaIndex` **after** all the DDL in the file |

**Hash per object type**:

| kind | Hash content |
|---|---|
| `table` | columns (name, order, type, nullability), named constraints |
| `function` | signature (name, parameters + types, return type), language |
| `procedure` | signature (name, parameters + types) |
| `type` | definition (enum values, or columns for composite types) |
| `sequence` | start, increment, min, max |
| `index` | target table, columns, method (btree, gin, …), WHERE condition |
| `policy` | target table, command, USING, WITH CHECK |
| `trigger` | target table, timing, events, called function |

Expression default values are not included in the hash (too volatile).

### 4.3 LSP behavior in a migration file

The LSP reads the file linearly:

1. Reads `MIGRATION:BEGIN id=… schema_hash=…` → loads the `SchemaIndex` for that state (from the journal or by recomputing)
2. For each `CHANGE:BEFORE id="chg_X" kind=… name=… hash=…`:
   - Checks that the current object matches the hash → otherwise W-MIG-001 "schema state mismatch"
   - Opens a scope identified by `"chg_X"`
3. The DDL between BEFORE and AFTER is analyzed and applied to the local `SchemaIndex` (non-persisted mutation)
4. For `CHANGE:AFTER id="chg_X" hash=…`:
   - Closes the `"chg_X"` scope (orphan BEFORE → W-MIG-002)
   - Checks that the post-DDL state matches the hash → otherwise W-MIG-003 "unexpected post-change state"
5. Any SQL after a `CHANGE:AFTER` is type-checked against the schema **including** the previous mutations
6. Reads `MIGRATION:END` → validates the global final state

Result: a column added by `ALTER TABLE` in a `CHANGE` scope is visible to the `INSERT`s that follow in the same file.

### 4.4 Automatic generation (`migrate generate`)

The `pgsql-tools migrate generate` command:
1. Diffs the `SchemaIndex` before/after each DDL → computes the hashes
2. Generates the short `id`s (8 hex chars, truncated from the hash of the before+after pair)
3. Inserts the markers into the generated migration file
4. Maintains `pgsql-tools-hashes.json` (hash → serialized `SchemaIndex` for fast reloading)

### 4.5 Brownfield initialization (`migrate init`)

For existing projects that do not yet have migrations managed by pgsql-tools.

**Command**:
```bash
pgsql-tools migrate init [options]
  --from-sql <path>       Use the existing DDL files as the source of truth
  --from-db  <connstr>    Introspect a live database (via pg_catalog)
  --name     <slug>       Name of the baseline migration (default: "baseline")
  --sequence <n>          Sequence number (default: 0 → "000000_baseline.sql")
```

If neither `--from-sql` nor `--from-db` is given, uses the `sources` defined in `pgsql-tools.toml`.

**What the command produces**:

1. **`migrations/000000_baseline.sql`** — idempotent migration with `IF NOT EXISTS` on every object:

```sql
-- [PGSQL-TOOLS:MIGRATION:BEGIN id="mig_init00" schema_hash="initial"]

-- [PGSQL-TOOLS:CHANGE:AFTER id="chg_a1b2c3" kind="table" name="public.users" hash="b91d4e8"]
CREATE TABLE IF NOT EXISTS public.users (
    id          uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    email       text        NOT NULL UNIQUE,
    created_at  timestamptz NOT NULL DEFAULT now()
);

-- [PGSQL-TOOLS:CHANGE:AFTER id="chg_d4e5f6" kind="function" name="public.get_user" hash="c03d1f2"]
CREATE OR REPLACE FUNCTION public.get_user(p_id uuid) RETURNS public.users …;

-- [PGSQL-TOOLS:MIGRATION:END id="mig_init00" schema_hash="a3f7c12"]
```

Differences from normal migrations:
- `MIGRATION:BEGIN` has `schema_hash="initial"` (no before hash — starting point)
- No `CHANGE:BEFORE` (the objects did not exist in the journal)
- Only `CHANGE:AFTER`, to record the target state of each object

2. **`pgsql-tools-hashes.json`** — initialized with the final hash

3. **`pgsql-tools.toml`** — created if missing, with the detected sources

**Safe DDL rewriting**:

| Original DDL | Generated safe version |
|---|---|
| `CREATE TABLE t` | `CREATE TABLE IF NOT EXISTS t` |
| `CREATE FUNCTION f` | `CREATE OR REPLACE FUNCTION f` |
| `CREATE PROCEDURE p` | `CREATE OR REPLACE PROCEDURE p` |
| `CREATE TYPE t AS ENUM` | `CREATE TYPE IF NOT EXISTS t AS ENUM` (PG14+) |
| `CREATE SEQUENCE s` | `CREATE SEQUENCE IF NOT EXISTS s` |
| `CREATE INDEX i ON t` | `CREATE INDEX IF NOT EXISTS i ON t` |
| `CREATE UNIQUE INDEX i ON t` | `CREATE UNIQUE INDEX IF NOT EXISTS i ON t` |
| `CREATE TRIGGER tr` | `CREATE OR REPLACE TRIGGER tr` (PG14+) |
| `CREATE POLICY p ON t` | `DO $$ BEGIN IF NOT EXISTS … END $$` |
| `ALTER TABLE t ADD COLUMN c` | `DO $$ BEGIN ALTER TABLE t ADD COLUMN c; EXCEPTION WHEN duplicate_column THEN NULL; END $$` |

**Behavior if a 000000 migration already exists**: error with the explicit message "baseline already exists — use `migrate init --sequence N` to choose a different number".

---

## 5. Project config (`pgsql-tools.toml`)

File at the project root. Every value has a reasonable default — a minimal project can work without config.

```toml
# ── Schema sources ────────────────────────────────────────────────────────────
[schema]
# Order = application order. Migrations are sorted by sequence internally.
sources = [
    { path = "migrations/",        role = "migration" },
    { path = "schema/types.sql",   role = "schema"    },
    { path = "schema/functions/",  role = "schema"    },
]
# Default PostgreSQL search_path for resolving unqualified names
search_path = ["public", "extensions"]

# ── Migrations ────────────────────────────────────────────────────────────────
[migrations]
directory = "migrations/"
# {sequence} = number, {name} = slug description, {ts} = ISO timestamp
format    = "{sequence:06}_{name}.sql"
# Hash journal (relative path)
hash_journal = "pgsql-tools-hashes.json"

# ── PL/pgSQL instrumentation ──────────────────────────────────────────────────
[instrumentation]
enabled    = true          # false = global opt-out, can be re-enabled by comment
log_level  = "step"        # "none" | "step" | "debug"
log_target = "notify"      # "notify" | "table" | "both"

# ── Lint ──────────────────────────────────────────────────────────────────────
[lint]
# Globally disabled rules
disabled = []
# Examples: ["W-DDL-002", "W-DDL-003"]

# Severity override: turn a warning into an error, or silence an error
# [lint.override]
# "W-DDL-002" = "error"
# "E002"      = "warn"

# ── Plugins ───────────────────────────────────────────────────────────────────
[plugins]
enabled = []
# Examples: ["postgis", "timescaledb"]
```

### Inline control comments

`-- @pgsql-tools: <directive>` comments apply to the object immediately following them (same behavior as `eslint-disable-next-line`).

| Directive | Effect |
|---|---|
| `no-instrumentation` | Disables runtime injection on this function |
| `instrumentation` | Forces runtime injection (if disabled globally) |
| `no-lint` | Disables all lint on this statement |
| `no-lint: W-DDL-002` | Disables a specific rule on this statement |
| `lint: error` | Raises all warnings to errors for this statement |

---

## 6. Parser workarounds — roadmap for out-of-scope cases

Several PL/pgSQL constructs were temporarily simplified so as not to block the checker.
Each case is documented here with the correct approach to implement.

### Case 1 — `RETURNING … INTO var` (current strip → dedicated AST)

**Situation**: `INSERT … RETURNING col INTO v_result` is not understood by sqlparser-rs.
**Current workaround**: `strip_returning_into()` removes the ` INTO var` clause before passing the fragment to sqlparser, losing the information about the target variables.
**Correct fix**:
1. Add `PlStatement::DmlReturningInto { stmt: Box<Statement>, targets: Vec<Ident> }` to `pgsql-ast`.
2. In `parse_dml_statement` (PL/pgSQL parser), detect `INTO` after `RETURNING`, extract the targets, and build this variant instead of stripping.
3. In the checker, check the RETURNING columns against the schema and mark the `targets` as assigned variables in the context.

### Case 2 — Record variables (`v_rec.col`) in embedded SQL

**Situation**: A variable declared `v_rec RECORD` or `v_row my_table%ROWTYPE` can be used as `v_rec.col` in SQL expressions. The checker sees `v_rec` as a table qualifier and emits E002.
**Current workaround**: the text checker (`check_plpgsql_body`) suppresses all E002s, at the cost of false negatives.
**Correct fix**:
1. Extend `TypeContext` with `record_vars: HashMap<String, Option<&TableSchema>>` — `None` for `RECORD`, `Some(ts)` for `%ROWTYPE`.
2. In `BodySchemaProvider::get_table`, consult `record_vars` first (before `temp_tables`).
3. Propagate `SELECT … INTO v_rec FROM t` assignments to infer the type of `v_rec` in a flow-sensitive way.

### Case 3 — `NEW`/`OLD` in trigger embedded SQL ✅ (implemented)

**Situation**: Triggers use `NEW.col` and `OLD.col` in their SQL bodies. Without context, the checker emits E002 for `NEW` and `OLD`.
**Implemented fix** (two levels):
1. **Known trigger table**: when a `CREATE TRIGGER ... ON table_name EXECUTE FUNCTION fn()` is found in the same set of analyzed files, the checker builds a `TriggerCtx` with the table, and registers `new`/`old` in `temp_tables` with that table's schema. `BodySchemaProvider.get_table("new")` then returns the right schema → full column checking.
2. **Unknown trigger table**: when the function has `RETURNS TRIGGER` but no `CREATE TRIGGER` is found in the analyzed files (`trigger_table == None`), `is_trigger = true` in `TypeContext`. Every `check_select_*` in this context passes `phantom_qualifiers = {"new", "old"}`: the qualifier is recognized as valid, E002 is suppressed, but column checking is skipped (no schema available).
3. **Multi-file**: the `check` CLI now builds the trigger_map from the context + checked files via `Checker::run_with_context`. Passing `examples/procs.sql --schema examples/schema.sql` → the `CREATE TRIGGER`s in the schema are visible even if the functions are in `procs.sql`.

**Remaining roadmap items**:
- `REFERENCING NEW TABLE AS name` clause (custom naming of transition tables) → requires AST + parser extension
- Flow-sensitive availability detection: `INSERT` only → OLD undefined, `IF TG_OP = 'DELETE'` → OLD defined within the IF scope
- Statement-level triggers (NEW/OLD are tables, not rows) → different semantics to handle

### Case 4 — Plvars in `INSERT … VALUES (…)`

**Situation**: `INSERT INTO t (col) VALUES (p_param)` — `p_param` is a PL/pgSQL variable but `check_sql_in_body` does not walk VALUES expressions with `plvars`, potentially emitting E001.
**Current workaround**: only the source SELECT of an INSERT is checked with `check_select_with_plvars`; raw VALUES are not traversed.
**Correct fix**:
1. In `check_sql_in_body`, for `Statement::Insert` with `InsertSource::Values`, walk each expression with `check_expr`, passing `plvars`.
2. Alternatively, treat plvars as unknown literals (`Expr::Literal`) downstream in the type-checker.

### Case 5 — Text fallback (`check_plpgsql_body`) too permissive

**Situation**: When the PL/pgSQL parser fails on a complex body (CTEs, FOR IN unnest, etc.), we fall back to line-by-line analysis. This fallback suppresses E002s entirely and misses many constructs.
**Correct fix**:
1. Extend the PL/pgSQL parser to cover the failing cases (FOR IN, RETURN QUERY, nested CTEs).
2. Failing that, have the text fallback emit only structural warnings (W002, W003) without column/table diagnostics — nothing is better than false positives.
3. Track the fallback rate with a metrics counter in the LSP (ratio of parsed bodies / text bodies) to prioritize the cases to handle.

---

## 7. Roadmap — what is not in this spec

To add later:
- **Go-to-definition**: navigate to the definition of a table/column/function from any SQL file
- **Rename refactoring**: rename a table or column, updating all references
- **Live introspection**: `IntrospectionJsonProvider` from a DB connection (complement to `SchemaResolver`)
- **PostGIS plugin**: geometric types in the type-checker
- **RLS policies**: type-checking of `USING`/`WITH CHECK` expressions
