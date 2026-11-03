# pgsql-tools — Spec: Full support for the PostgreSQL / PL/pgSQL dialect

## Current situation

The parser layer is built on **sqlparser-rs v0.54** with `PostgreSqlDialect`. This dialect covers
almost all of standard PostgreSQL SQL (DDL, DML, policies, triggers) but leaves two gaps:

1. **PL/pgSQL function/procedure bodies** (`$$...$$`) — the body is tokenized as an opaque
   `DollarQuotedString` and stored as-is in `FunctionBody::Raw(String)`. No structured AST
   is produced.

2. **Missing function options** in `parse_postgres_create_function`:
   - `SECURITY DEFINER` / `SECURITY INVOKER`
   - `COST n` / `ROWS n`
   - `SET config_param TO value` (configuration options)
   - `RETURNS SETOF type` (partially handled, but not always)

---

## Solution architecture

### What sqlparser-rs provides as extension points

The official documentation recommends the **"wrapper dialect"** pattern:

> *"Write a new parser that delegates to the ANSI parser so that as much as possible of the core
> functionality can be re-used."*

The `Dialect` trait exposes four key hooks:

```rust
// Called first on every `parse_statement()`. Returning Some(...) short-circuits the default.
fn parse_statement(&self, parser: &mut Parser) -> Option<Result<Statement, ParserError>>

// Called first on every prefix expression parse.
fn parse_prefix(&self, parser: &mut Parser) -> Option<Result<Expr, ParserError>>

// Called first on every infix expression.
fn parse_infix(&self, parser: &mut Parser, expr: &Expr, prec: u8)
    -> Option<Result<Expr, ParserError>>

// Called for each column option in CREATE TABLE.
fn parse_column_option(&self, parser: &mut Parser)
    -> Result<Option<Result<Option<ColumnOption>, ParserError>>, ParserError>
```

**What sqlparser-rs already does for PostgreSQL:**
- `Token::DollarQuotedString` — the tokenizer already extracts the `$$...$$` body as a string
- `parse_create_function_body_string()` — produces `Expr::Value(Value::DollarQuotedString(s))`
- `CreatePolicy`, `CreateTrigger`, `AlterTable { EnableRowLevelSecurity }` — all parsed

**What is missing:**
- `SECURITY DEFINER` / `SECURITY INVOKER` in the options loop of `parse_postgres_create_function`
- A structural parser for the PL/pgSQL body itself

---

## Implementation plan

### Part A — Dialect wrapper (no fork)

Create in `crates/pgsql-parser/src/dialect.rs` a `PgsqlToolsDialect` that wraps
`PostgreSqlDialect` and overrides `parse_statement()` to intercept `CREATE FUNCTION` /
`CREATE PROCEDURE` and hand them to our own logic for extracting the missing options.

```rust
pub struct PgsqlToolsDialect;

impl Dialect for PgsqlToolsDialect {
    fn parse_statement(&self, parser: &mut Parser)
        -> Option<Result<Statement, ParserError>>
    {
        // Let sqlparser parse the statement normally...
        None
        // ...then post-process to inject SECURITY DEFINER etc.
        // (see Part A2: post-processing)
    }

    // Full delegation for everything else
    fn is_identifier_start(&self, ch: char) -> bool { ... }
    fn is_identifier_part(&self, ch: char) -> bool { ... }
    // + all the supports_*() methods of PostgreSqlDialect
}
```

**Why not fork?**
- sqlparser-rs is actively maintained (frequent releases). A fork creates a permanent merge
  debt for hundreds of `supports_*` methods.
- The `parse_statement()` hook is enough to intercept what we need.
- `DollarQuotedString`s are already extracted — we don't need to touch the tokenizer.

#### A1 — Missing options in CREATE FUNCTION

The problem: after sqlparser has parsed the `Statement::CreateFunction`, the
`security_definer` field is absent from the sqlparser struct (it does not exist in the v0.54 codebase).

**Solution:** post-processing in `convert.rs`. When converting `CreateFunction`,
re-scan the source SQL (or the leftover tokens) to detect `SECURITY DEFINER`.

More cleanly: in the `parse_statement` hook, intercept `CREATE [OR REPLACE] FUNCTION`,
delegate to sqlparser for everything except the unhandled keywords, then consume
`SECURITY DEFINER | SECURITY INVOKER | COST n | ROWS n | SET param TO value` ourselves.

```
CREATE OR REPLACE FUNCTION foo() RETURNS TRIGGER
  LANGUAGE plpgsql
  SECURITY DEFINER          ← sqlparser ignores this token → panic or error
  AS $$ ... $$;
```

Currently sqlparser stops before `SECURITY DEFINER` and leaves unconsumed tokens
→ parse error. Our hook must consume these tokens.

#### A2 — `RETURNS SETOF type`

`RETURNS SETOF uuid` is partially handled via `DataType::Custom("SETOF")` but without the inner
type. Our `convert_return_type()` must detect this case and produce `ReturnType::SetOf(inner)`.

---

### Part B — PL/pgSQL parser

This is the central piece. The `$$...$$` body is a string — we need a **Rust mini-parser**
to break it down into a structured AST.

#### B1 — PL/pgSQL AST (new file `crates/pgsql-ast/src/plpgsql.rs`)

```rust
/// Full body of a PL/pgSQL function or procedure
pub struct PlpgsqlBody {
    pub declare: Vec<VarDecl>,
    pub block: Block,
}

/// Variable declaration in the DECLARE section
pub struct VarDecl {
    pub name: Ident,
    pub data_type: DataType,
    pub not_null: bool,
    pub default: Option<PlExpr>,
    pub constant: bool,
}

/// BEGIN ... END block (possibly labelled)
pub struct Block {
    pub label: Option<String>,
    pub statements: Vec<PlStatement>,
    pub exception: Option<ExceptionBlock>,
}

pub struct ExceptionBlock {
    pub handlers: Vec<ExceptionHandler>,
}

pub struct ExceptionHandler {
    pub conditions: Vec<String>,   // SQLSTATE or condition name
    pub block: Block,
}

/// PL/pgSQL statement
pub enum PlStatement {
    // Assignment: var := expr
    Assign { target: AssignTarget, value: PlExpr },
    // SELECT INTO: SELECT ... INTO [STRICT] var FROM ...
    SelectInto { strict: bool, target: Vec<Ident>, query: SelectStmt },
    // PERFORM: runs a query and discards the result
    Perform(PlExpr),
    // Dynamic EXECUTE
    Execute { sql: PlExpr, into: Option<Vec<Ident>>, using: Vec<PlExpr> },
    // RETURN / RETURN NEXT / RETURN QUERY
    Return(Option<PlExpr>),
    ReturnNext(PlExpr),
    ReturnQuery(SelectStmt),
    // RAISE
    Raise { level: RaiseLevel, message: RaiseMessage, options: Vec<RaiseOption> },
    // IF / ELSIF / ELSE
    If { condition: PlExpr, then: Block, elsif: Vec<(PlExpr, Block)>, else_: Option<Block> },
    // Loops
    Loop(Block),
    While { condition: PlExpr, body: Block },
    ForIn { var: Ident, from: PlExpr, to: PlExpr, by: Option<PlExpr>, body: Block, reverse: bool },
    ForSelect { var: Ident, query: SelectStmt, body: Block },
    ForExecute { var: Ident, sql: PlExpr, body: Block },
    // Control flow
    Exit { label: Option<String>, when: Option<PlExpr> },
    Continue { label: Option<String>, when: Option<PlExpr> },
    // Embedded DML
    Sql(Statement),
    // CALL procedure
    Call { name: ObjectName, args: Vec<PlExpr> },
    // Nested block
    SubBlock(Block),
    // GET DIAGNOSTICS
    GetDiagnostics(Vec<(Ident, DiagnosticsItem)>),
    // NULL (no-op)
    Null,
}

pub enum AssignTarget {
    Simple(Ident),
    RowField(Ident, Ident),   // row.field
}

pub enum RaiseLevel { Debug, Log, Info, Notice, Warning, Exception }

pub enum RaiseMessage {
    Format { format: String, args: Vec<PlExpr> },
    Using,   // RAISE USING option = ...
}

pub struct RaiseOption { pub key: RaiseOptionKey, pub value: PlExpr }
pub enum RaiseOptionKey { Message, Detail, Hint, Errcode, Column, Constraint, Datatype, Table, Schema }

pub enum DiagnosticsItem {
    RowCount, PgContext, PgExceptionContext, PgExceptionDetail, PgExceptionHint,
    ReturnedSqlstate, ColumnName, ConstraintName, PgDatatype, MessageText, TableName, SchemaName,
}

/// PL/pgSQL expression (superset of SQL Expr)
pub enum PlExpr {
    Sql(Expr),                 // standard SQL expression
    Var(Ident),                // local variable
    RowField(Ident, Ident),    // row.field
    Found,                     // special FOUND variable
    NullExpr,                  // NULL
}
```

#### B2 — PL/pgSQL parser (`crates/pgsql-parser/src/plpgsql/mod.rs`)

Write a recursive-descent parser that operates on a string (the `$$...$$` body).

**Approach**: a minimal tokenizer dedicated to PL/pgSQL + a hand-written parser.
Do not reuse the sqlparser tokenizer for the body — PL/pgSQL keywords such as
`ELSIF`, `LOOP`, `RAISE`, `PERFORM` are not standard SQL keywords, and handling
`DECLARE` / local variables is orthogonal to SQL.

Structure:

```
crates/pgsql-parser/src/plpgsql/
  mod.rs         → pub fn parse_plpgsql_body(body: &str) -> Result<PlpgsqlBody, PlParseError>
  lexer.rs       → Minimal PL/pgSQL tokenizer (identifiers, keywords, strings, numbers, ops)
  parser.rs      → Recursive-descent parser
  keywords.rs    → Enum of PL/pgSQL keywords
  error.rs       → PlParseError with span
```

**Tokenizer**: reuse `sqlparser::tokenizer::Tokenizer` for SQL expressions (SELECT,
INSERT...) inside the body — the custom tokenizer is only needed for the PL/pgSQL
control structure itself.

**Integration in convert.rs**:

```rust
// Currently:
FunctionBody::Raw(body_string)

// After:
match pgsql_parser::plpgsql::parse_plpgsql_body(&body_string) {
    Ok(parsed) => FunctionBody::Parsed(Box::new(parsed)),
    Err(e) => {
        // Graceful degradation: keep the Raw body with a diagnostic
        FunctionBody::Raw(body_string)
    }
}
```

`FunctionBody` becomes:
```rust
pub enum FunctionBody {
    Raw(String),                         // not parsed yet or parse failed
    SqlQuery(Box<SelectStmt>),           // simple LANGUAGE sql
    Parsed(Box<plpgsql::PlpgsqlBody>),   // parsed LANGUAGE plpgsql
}
```

---

### Part C — Structural PL/pgSQL checker

Once the PL/pgSQL AST is available, `pgsql-checker` can do real type-checking:

```rust
pub fn check_plpgsql_body(
    body: &PlpgsqlBody,
    function_sig: &CreateFunction,
    schema: &dyn SchemaProvider,
) -> Vec<Diagnostic>
```

**What the checker can validate:**

| Construct | Validation |
|---|---|
| `DECLARE v type` | Type known in SchemaProvider or a built-in PG type |
| `v := expr` | Type of `expr` compatible with the declared type of `v` |
| `SELECT INTO v FROM t WHERE ...` | Columns exist on `t`, type of `v` compatible |
| `OLD.col` / `NEW.col` | Column exists on the trigger's table |
| `RAISE EXCEPTION msg` | Always valid |
| `EXECUTE sql_string` | Warn W004 (dynamic SQL cannot be analyzed) |
| Function call `f(args)` | Signature known via SchemaProvider.get_function() |
| `RETURN expr` | Type of `expr` compatible with the declared `RETURNS` |
| Undeclared variable | E002 "variable 'x' not declared" |
| Variable used before assignment | W005 (best-effort) |

**Type context**: a `TypeContext` is maintained while walking the body:

```rust
struct PlTypeContext<'a> {
    variables: HashMap<String, DataType>,   // DECLARE vars
    params: HashMap<String, DataType>,      // function parameters
    return_type: &'a ReturnType,
    trigger_table: Option<&'a TableSchema>, // for OLD/NEW
    schema: &'a dyn SchemaProvider,
}
```

---

## Implementation roadmap

### Step 1 — Dialect wrapper + missing options *(~1 day)*

**Files to create/modify:**
- `crates/pgsql-parser/src/dialect.rs` — `PgsqlToolsDialect`
- `crates/pgsql-parser/src/lib.rs` — use `PgsqlToolsDialect` instead of `PostgreSqlDialect`
- `crates/pgsql-parser/src/convert.rs` — post-processing `SECURITY DEFINER`, `COST`, `ROWS`, `RETURNS SETOF`
- `crates/pgsql-ast/src/ddl.rs` — add `security_definer`, `cost`, `rows` to `CreateFunction`

**Validation test:**
```sql
CREATE OR REPLACE FUNCTION current_user_org_ids()
RETURNS SETOF UUID
LANGUAGE sql STABLE SECURITY DEFINER AS $$ ... $$;
```
→ Parses without error, `security_definer = true`, `return_type = ReturnType::SetOf(DataType::Uuid)`

---

### Step 2 — PL/pgSQL AST *(~0.5 day)*

**Files:**
- `crates/pgsql-ast/src/plpgsql.rs` — all the PL/pgSQL AST types above
- `crates/pgsql-ast/src/lib.rs` — `pub mod plpgsql`

---

### Step 3 — PL/pgSQL parser *(~3-4 days — the big piece)*

**Files:**
- `crates/pgsql-parser/src/plpgsql/mod.rs`
- `crates/pgsql-parser/src/plpgsql/lexer.rs`
- `crates/pgsql-parser/src/plpgsql/parser.rs`
- `crates/pgsql-parser/src/plpgsql/keywords.rs`
- `crates/pgsql-parser/src/plpgsql/error.rs`

**Recommended implementation order** (simple to complex):
1. Lexer + keywords + lexer unit tests
2. `BEGIN...END` block parser with `NULL` and `RETURN` statements
3. `RAISE`
4. `IF/ELSIF/ELSE/END IF`
5. `ASSIGN (:=)` + `DECLARE`
6. `SELECT INTO`, `PERFORM`
7. Loops (`LOOP`, `WHILE`, `FOR`)
8. Dynamic `EXECUTE`
9. `EXCEPTION` handlers
10. Nested blocks, labels, `EXIT`/`CONTINUE`

**Main validation test:**
Among the functions in `examples/schema.sql`:
- `set_updated_at()` — trivial (BEGIN, NEW.updated_at := now(), RETURN NEW)
- `enforce_task_status_transition()` — IF/ELSIF, NEW.col, RAISE EXCEPTION
- `assign_task()` — DECLARE, SELECT INTO, IF NOT FOUND, UPDATE

---

### Step 4 — Structural PL/pgSQL checker *(~2 days)*

**Files:**
- `crates/pgsql-checker/src/check/plpgsql.rs` — replaces the current scanner
- `crates/pgsql-checker/src/check/type_context.rs` — `PlTypeContext`

**Validation test:**
```sql
-- Must produce E001 "column 'nonexistent' does not exist on table 'tasks'"
CREATE OR REPLACE FUNCTION bad_trigger() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
  NEW.nonexistent := now();
  RETURN NEW;
END;
$$;
```

---

### Step 5 — End-to-end integration *(~1 day)*

- `examples/schema.sql` 100% parsed with no residual `FunctionBody::Raw`
- All checks on the example's PL/pgSQL bodies pass
- Update the `parse_full_schema_counts_statements` test:
  all statements → `ok`, 0 `syntax` or `unsupported`

---

## Summary of gaps and their solution

| Gap | Solution | Complexity |
|---|---|---|
| `SECURITY DEFINER` not parsed | Dialect wrapper + post-processing | Low |
| `RETURNS SETOF type` incomplete | Post-processing in convert.rs | Low |
| `COST n` / `ROWS n` ignored | Same | Low |
| Opaque PL/pgSQL body | Dedicated mini-parser (Step 3) | **High** |
| Type-checking PL/pgSQL bodies | Structural checker (Step 4) | High |
| Undeclared variables | Covered by Step 4 | — |
| `EXECUTE` (dynamic SQL) | Warning W004, no analysis | Low |
| Custom extensions (PostGIS…) | Existing `dialect.rs` plugin | Low |

---

## What we will not parse (justified out of scope)

| Construct | Reason |
|---|---|
| `EXECUTE format(...)` with dynamic SQL | Statically impossible by nature |
| `COPY` (streaming) | No use case in the toolchain |
| `LISTEN` / `NOTIFY` DDL | Parsed by sqlparser, no type-check needed |
| C / internal bodies | Opaque binaries, nothing to analyze |
| PL/Python, PL/Perl, etc. | External languages, out of scope |

---

## Dependencies to add

No additional external dependency is needed — the PL/pgSQL mini-parser is
written from scratch in pure Rust, which guarantees WASM portability and no
system dependencies.
