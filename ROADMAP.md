# pgsql-tools — Roadmap

> **Archived on 2026-10-31 — superseded by [PLAN.md](PLAN.md).**
> The "✅ Delivered" statuses below overstate the actual state (e.g. no
> nullability, no type compatibility, heuristic TS generation,
> `db.query<T>` reduced to a cast). Kept for the record.

> Updated: June 2026. Based on the full codebase audit and the OXC/datafusion architectural analysis.

---

## Current state (baseline)

| Component | State | Notes |
|-----------|:----:|-------|
| SQL + PL/pgSQL parser | ✅ | sqlparser-rs + custom PL/pgSQL dialect, 55 tests |
| pgsql-ast AST | ✅ | DDL, DML, PL/pgSQL — no NodeId/Span |
| SELECT normalization | ✅ | Logical order FROM→WHERE→SELECT |
| Type-checker (existence) | ✅ | E001/E003/W001/W003/W005 — no type inference |
| Plugin architecture | ✅ | CheckRule + Checker walker + WASM host (wasmi) + Prisma skeleton |
| SQL codegen (emit + diff + inject) | ✅ | Usable, to be extended |
| WASM bindings | ✅ | 4 functions exposed via wasm-bindgen |
| Node.js bindings | ✅ | 4 functions via napi-rs |
| CLI `pgsql check/lint/emit` | ✅ | Working |
| LSP (diagnostics) | ⚠️ | Pipeline wired, hover/completion are stubs |
| Lint (4 rules) | ⚠️ | L001–L004, no advanced rules |
| pgsql-semantic | ❌ | Missing — blocks TS types, advanced LSP, type compat |
| NodeId / Span on AST | ❌ | Missing — blocks cross-references and LSP |
| npm framework | ❌ | Missing |
| VS Code extension | ❌ | Missing |
| TypeScript type generation | ❌ | Missing |

---

## Phase A — AST foundation: NodeId + Span + Visit traits

**Estimated duration**: 1–2 weeks  
**Status**: ✅ Delivered  
**Dependencies**: none  
**Unlocks**: Phase B (semantic), LSP hover/completion

### Deliverables

- `NodeId(u32)` on all main AST nodes: `Statement`, `SelectStmt`, `Expr` and its variants, `CreateTable`, `CreateFunction`, `CreateProcedure`, `CreatePolicy`, `PlStatement`
- `Span { start: u32, end: u32 }` (byte offsets from the source) on the same nodes
- The parser assigns `NodeId`s via an atomic counter; `Span` extracted from sqlparser-rs positions
- `pgsql_ast::visit::Visit<'a>` trait: ~40 default no-op methods, one per node type
- `pgsql_ast::visit::VisitMut` trait: `enter_*` / `exit_*` for transformations
- `AstWalker`: traversal implementation that calls the visitor methods
- Migration of the existing checker/linter rules to the Visit trait

---

## Phase B — `pgsql-semantic`: symbol table + type inference

**Estimated duration**: 3–4 weeks  
**Status**: ✅ Delivered  
**Dependencies**: Phase A  
**Unlocks**: Phase C (LSP), Phase D (TS types), checker type-compat

### Deliverables

- New crate `crates/pgsql-semantic/`
- `SymbolTable`: `name → Symbol { kind, node_id, data_type, span }`
  - Kinds: Table, Column, Function, Cte, PlVar, PlParam
- `ScopeTree`: scope hierarchy, one per SELECT / subquery / CTE / PL/pgSQL block
  - `scope_at_offset(pos) → ScopeId` for the LSP
  - `available_columns(scope_id) → Vec<Symbol>` for completion
- Type inference: `infer_expr_type(expr, scope, symbols) → Option<DataType>`
  - Scalar expressions: literals, columns, binary operators, CASE, CAST, functions
- `OutputColumn { name, data_type, node_id }` per output column of a SELECT
- `SemanticResult { symbols, scopes, expr_types, select_columns }`
- `SemanticCtx<'a>` passed to checker/linter rules
- Integration into `check_sql_self_contained`: parse → semantic → check

---

## Phase C — Full LSP + VS Code extension

**Estimated duration**: 2 weeks  
**Status**: ✅ Delivered  
**Dependencies**: Phase B  
**Unlocks**: full IDE experience

### Deliverables

- **Hover**: `node_at_offset` → `semantic.expr_types[node.id]` → type display
- **Completion**: `scope.available_columns()` → list of suggestions with types
- **Go-to-definition**: `symbols.declaration_span(symbol)` → position in the file
- **TextMate grammar**: `editors/vscode/syntaxes/pgsql.tmLanguage.json`
  - Tokens: SQL keywords, identifiers, strings, dollar-quotes, comments
  - PL/pgSQL: DECLARE, BEGIN/END, control flow types
- **VS Code extension**: `editors/vscode/`
  - `package.json` with LSP + grammar contribution points
  - `src/extension.ts`: launches the local LSP server
  - Publish on the VS Code Marketplace

---

## Phase D — TypeScript type generation

**Estimated duration**: 1–2 weeks  
**Status**: ✅ Delivered  
**Dependencies**: Phase B  
**Unlocks**: npm framework (Phase E)

### Deliverables

- `pgsql_codegen::generate_ts_types(stmts, semantic) → String`
  - Each named SELECT → TypeScript interface `{ col: type, ... }`
  - `DataType → TypeScript type` mapping: `Text → string`, `Integer → number`, `Boolean → boolean`, `Jsonb → unknown`, `Uuid → string`, etc.
  - Nullable types: `col: string | null`
- Hook in WASM/node bindings: `generate_types(sql, schema_sql) → string`
- CLI: `pgsql codegen types --input schema.sql --output types/`
- Tests on `examples/schema.sql` → verification of the generated interfaces

---

## Phase E — `pgsql-tools` npm framework

**Estimated duration**: 3–4 weeks  
**Status**: ✅ Delivered (TypeScript, requires pg for the DB)  
**Dependencies**: Phase D  
**Unlocks**: main TypeScript-side deliverable

### Deliverables

- `npm/pgsql-tools/` — npm package
- `db.query<T>(sql, params)` → result typed with the type generated in Phase D
- Connection pool (wrapping `pg` or `postgres`)
- `db.migrate({ from, to })` → diff + migration deployment
- Declarative DSL for RLS policies
- DSL for triggers and stored procedures
- Prisma plugin: `pgsql-tools/plugins/prisma` — Prisma schema import
- Documentation + complete examples

---

## Phase F — Extensions and polish

**Estimated duration**: ongoing  
**Status**: 🟡 Partially delivered  
**Dependencies**: Phases C + E  

### Deliverables

- **PostGIS plugin**: GEOMETRY/GEOGRAPHY types, ST_* spatial functions
- **Advanced lint rules**: missing index on FK, unused JOINs, LIKE '%...', aggregates without GROUP BY, NOT NULL without DEFAULT in a migration
- **Simplified syntax**: ❌ not started — desugar to standard SQL (upsert patterns, declarative soft delete, etc.)
- **JetBrains extension**: ❌ skeleton only (2 Kotlin files, not compiled in CI)
- **Safe migration rules** (inspired by Squawk): ADD COLUMN NOT NULL, DROP COLUMN, RENAME without lock, etc.
- **Live introspection**: ❌ not started — connection to a PostgreSQL DB to enrich the SchemaProvider

---

## Dependencies between phases

```
A (NodeId + Visit)
└── B (pgsql-semantic)
    ├── C (LSP + VS Code)
    └── D (TS types)
        └── E (npm framework)

F (Extensions) ← depends on C + E, ongoing
```

---

## Delivery history

| Phase | Delivery date | Notes |
|-------|:-----------------:|-------|
| A — AST foundation (NodeId + Span + Visit) | June 2026 | NodeId/Span on main nodes, Visit/VisitMut traits |
| B — pgsql-semantic (symbol table + types) | June 2026 | SymbolTable, ScopeTree, expression inference |
| C — LSP + VS Code extension | June 2026 | Hover, completion, go-to-definition, TextMate grammar |
| D — TypeScript type generation | June 2026 | codegen SELECT → TS interfaces, CLI `pgsql codegen` |
| E — pgsql-tools npm framework | June 2026 | db.ts, migrate.ts, policy.ts, backends/, Prisma plugin |
| F — Extensions and polish | June 2026 (partial) | Lint L005-L009, migration safety M001-M003, PostGIS plugin. Remaining: JetBrains, live introspection, simplified syntax |

---

## Success metrics

| Phase | Metric |
|-------|----------|
| A | All main AST nodes have `id` + `span`; 0 broken tests |
| B | `infer_expr_type` covers 80% of common SQL expressions; correct `OutputColumn` on `examples/schema.sql` |
| C | Hover shows the type in VS Code on `examples/schema.sql`; completion lists available columns |
| D | `project_summary()` generates `{ total_tasks: number, completion_rate: number, ... }` |
| E | `db.query<ProjectSummary>(...)` typed end-to-end without a manual cast |
| F | 0 false positives on a real 50+ table production schema |
