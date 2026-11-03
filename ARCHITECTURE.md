# pgsql-tools — Overall architecture

> Reference document. Sources: analysis of OXC, datafusion-sqlparser-rs, codebase audit (June 2026).

---

## Founding principle

Inspired by OXC: **parse → normalize → semantic → tools**, in this strict order, with no back-and-forth.
Each pass receives the result of the previous one as read-only.

```
SQL source
  ↓ parse          → syntactic AST + NodeId + Span (exact positions)
  ↓ normalize      → logical AST (FROM→JOIN→WHERE→SELECT, syntactic desugar)
  ↓ semantic       → SemanticResult (symbol table, scope tree, resolved types)
  ↓ [tools — read-only over SemanticResult]
      ├── checker  → correctness diagnostics  (E001, E003, E005…)
      ├── linter   → style/perf diagnostics   (L001, L003, L004…)
      ├── codegen  → formatted SQL, migration diffs, TypeScript types
      └── LSP      → hover, completion, go-to-definition
```

**Absolute rule**: no tool modifies the AST. Only `normalize` (VisitMut) may
transform the tree. Checker, linter, LSP: read-only + diagnostic emission.

---

## The 3 missing foundational pieces

### 1. `NodeId` + `Span` on every AST node

Each AST node gets a stable `NodeId(u32)` assigned during parsing, and a `Span { start: u32, end: u32 }` in bytes from the source.

**Why**: without `NodeId`, the symbol table cannot cross-reference (col → originating table, call → definition). Without `Span`, the LSP cannot answer "which node is under the cursor?".

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span { pub start: u32, pub end: u32 }

// Example on SelectStmt
pub struct SelectStmt {
    pub id: NodeId,
    pub span: Span,
    pub columns: Vec<SelectItem>,
    pub from: Vec<TableRef>,
    // ...
}
```

### 2. The `pgsql-semantic` pass (new crate)

The missing core. A single pass over the logical AST produces:

```rust
pub struct SemanticResult {
    /// name (lower) → Symbol (table / column / function / CTE / PL/pgSQL var)
    pub symbols: SymbolTable,
    /// scope hierarchy (SELECT, subquery, CTE, PL/pgSQL block)
    pub scopes: ScopeTree,
    /// inferred type of each expression  (NodeId → DataType)
    pub expr_types: HashMap<NodeId, DataType>,
    /// output columns of each SELECT  (NodeId → Vec<OutputColumn>)
    pub select_columns: HashMap<NodeId, Vec<OutputColumn>>,
}

pub struct OutputColumn {
    pub name: Option<String>,   // alias if present
    pub data_type: DataType,
    pub node_id: NodeId,        // source Expr node
}
```

This `SemanticResult` is passed read-only to the checker, linter, LSP, codegen.

### 3. `Visit` / `VisitMut` traits + walker

Replaces the ad hoc pattern matches scattered across rules. All rules implement
`Visit` (read-only); `normalize` implements `VisitMut`.

```rust
pub trait Visit<'a> {
    fn visit_select(&mut self, node: &'a SelectStmt, ctx: &SemanticCtx<'a>) {}
    fn visit_expr(&mut self, node: &'a Expr, ctx: &SemanticCtx<'a>) {}
    fn visit_function_call(&mut self, node: &'a FunctionCall, ctx: &SemanticCtx<'a>) {}
    fn visit_create_table(&mut self, node: &'a CreateTable, ctx: &SemanticCtx<'a>) {}
    // ~40 methods, all default no-op
}

pub trait VisitMut {
    fn enter_select(&mut self, node: &mut SelectStmt) {}
    fn exit_select(&mut self, node: &mut SelectStmt) {}
    // enter_* / exit_* for each node type
}
```

---

## What `pgsql-semantic` unlocks

| Feature | Without semantic | With semantic |
|---------|:---:|:---:|
| E001 unknown column | ✅ existence | ✅ + context type |
| Return type of a SELECT | ❌ | ✅ `Vec<OutputColumn>` |
| TypeScript type generation | ❌ | ✅ `{id: string, name: string}` |
| LSP hover | ❌ | ✅ type of the hovered column |
| LSP completion | partial | ✅ columns available in scope |
| Go-to-definition | ❌ | ✅ NodeId → declaration position |
| SELECT * expansion | ❌ | ✅ actual column list |
| CTE resolution | ❌ | ✅ CTE → named type in scope |
| WHERE type compat. | ❌ | ✅ `WHERE text_col = 42` → E005 |
| Function overload resolution | ❌ | ✅ |

---

## Target monorepo structure

```
pgsql-tools/
├── crates/
│   ├── pgsql-ast/          # AST types + NodeId + Span + Visit traits       [to extend]
│   ├── pgsql-parser/       # sqlparser-rs + PL/pgSQL dialect                [exists]
│   ├── pgsql-semantic/     # SymbolTable + ScopeTree + type inference        [NEW — Phase B]
│   ├── pgsql-checker/      # Correctness rules (over SemanticResult)         [exists, to migrate]
│   ├── pgsql-lint/         # Style/perf rules (over SemanticResult)          [exists, to extend]
│   ├── pgsql-codegen/      # emit SQL + migration diffs + TS generation      [exists, to extend]
│   ├── pgsql-lsp/          # LSP server (tower-lsp)                         [skeleton]
│   ├── pgsql-plugin/       # Plugin trait + PluginRegistry                  [exists]
│   ├── pgsql-plugin-wasm/  # WASM plugin host (wasmi)                       [exists]
│   └── pgsql-cli/          # pgsql CLI                                      [exists]
│
├── bindings/
│   ├── wasm/               # wasm-bindgen: check + semantic + lint + migrate
│   └── node/               # napi-rs: same, sync + async API
│
├── npm/
│   └── pgsql-tools/        # TypeScript DX framework                        [NEW — Phase E]
│       ├── src/
│       │   ├── db.ts        # db.query<T>(), connection, transactions
│       │   ├── migrate.ts   # diff + deploy migrations
│       │   ├── policy.ts    # RLS policies DSL
│       │   └── codegen.ts   # calls WASM to generate types
│       └── types/           # types generated at build time
│
├── plugins/
│   ├── prisma/              # PrismaPlugin (in pgsql-plugin today)
│   └── postgis/             # PostGIS types + spatial functions             [NEW — Phase F]
│
├── editors/
│   ├── vscode/              # VS Code extension                              [NEW — Phase C]
│   │   ├── syntaxes/        # TextMate grammar SQL + PL/pgSQL
│   │   └── src/extension.ts # LSP client
│   └── jetbrains/           # JetBrains plugin                              [future]
│
├── examples/
│   ├── schema.sql           # Multi-tenant example schema
│   └── wasm-plugin/         # Example WASM plugin
│
├── ARCHITECTURE.md          # This file
├── ROADMAP.md               # Roadmap phases A–F
├── SPECS.md                 # Functional specifications
└── pgsql.toml               # Example config
```

---

## Detailed data pipeline

### Parse

`pgsql_parser::parse(sql) → Result<Vec<Statement>>`

- sqlparser-rs 0.54 with a custom PostgreSqlDialect
- Preprocessing: neutralizes SECURITY DEFINER, COST n, ROWS n
- Postprocessing: re-injects the extracted flags onto the nodes
- PL/pgSQL bodies `$$...$$`: custom lexer + parser → `FunctionBody::Parsed(PlpgsqlBody)`
- **To add (Phase A)**: `NodeId` assignment + `Span` extraction from token positions

### Normalize

`pgsql_parser::normalize(stmts) → Vec<Statement>` *(partially implemented)*

- Reorders SELECT into logical order: FROM → JOIN → WHERE → GROUP BY → HAVING → SELECT → ORDER BY
- Resolves column aliases in ORDER BY (already done in `NormalizedSelect`)
- Desugar: simplified syntax → standard SQL
- **Implementation**: `VisitMut` over the AST, in place

### Semantic

`pgsql_semantic::analyze(stmts, schema) → SemanticResult` *(Phase B)*

- Collects all declarations: tables (from `SchemaProvider`), columns, functions, CTEs, PL/pgSQL variables
- Builds the `ScopeTree`: one scope per SELECT, subquery, CTE, PL/pgSQL block
- Resolves each column reference to its declaration `NodeId`
- Infers the type of each `Expr`: `NodeId → DataType`
- Computes `OutputColumn` for each `SelectStmt`

### Checker / Linter (after semantic)

```rust
// Checker
let semantic = pgsql_semantic::analyze(&stmts, &schema)?;
let ctx = SemanticCtx { semantic: &semantic, schema: &schema };
let diags = Checker::with_builtin_rules().run_with_ctx(&stmts, &ctx);

// Linter
let lint_diags = Linter::with_config(&config).run_with_ctx(&stmts, &ctx);
```

### Codegen

```rust
// Formatted SQL
let sql = pgsql_codegen::emit_statement(&stmt);

// Migration diff
let diff = pgsql_codegen::diff_schemas(&old_stmts, &new_stmts);

// TypeScript types (requires semantic)
let ts = pgsql_codegen::generate_ts_types(&stmts, &semantic);
```

### LSP

```rust
// hover — type of the expression under the cursor
let node = ast.node_at_offset(cursor_offset)?;
let ty = semantic.expr_types[&node.id];
hover_text = format!("{}: {}", node.name, ty);

// completion — columns available in the current scope
let scope = semantic.scopes.scope_at_offset(cursor_offset);
completions = scope.available_columns();
```

---

## Handling PostgreSQL extensions

Via the `CheckRule` trait + `PluginRegistry`:

- **Built-in**: rules in `pgsql-checker` and `pgsql-lint`
- **Rust plugins**: a crate that implements `Plugin` + registration in the registry
- **WASM plugins**: `.wasm` via `pgsql-plugin-wasm`, JSON-over-linear-memory ABI
- **PostGIS**: geometric types (GEOMETRY, GEOGRAPHY), spatial functions → dedicated plugin

---

## References

- [OXC Architecture](https://github.com/oxc-project/oxc) — inspiration for the visitor pattern, semantic pass, arena alloc
- [datafusion-sqlparser-rs](https://github.com/apache/datafusion-sqlparser-rs) — base parser + Dialect trait
- [DataFusion SQL Planner](https://github.com/apache/datafusion/tree/main/datafusion/sql) — ContextProvider pattern, SqlToRel
- [tower-lsp](https://github.com/ebkalderon/tower-lsp) — LSP server framework
- [Squawk](https://github.com/sbdchd/squawk) — PostgreSQL SQL linter, reference architecture for rules
