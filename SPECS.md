# pgsql-tools — Specs & Decisions

PostgreSQL language toolchain written in Rust, distributable as WASM or napi-rs, designed to integrate into TypeScript projects or any other language.

## Motivation

Plain SQL is more readable than typed query builders (see kysely-pg-procedures). The goal is the best of both worlds: write readable SQL while getting type safety, IDE tooling, and automatic code generation (observability, snapshots, migrations).

---

## Deliverables

### 1. Rust core (agnostic)

A set of pure Rust crates, with no dependency on a particular runtime or ecosystem. Foundation for all other deliverables.

### 2. npm framework

A DX-oriented npm package (via WASM or napi-rs) for TypeScript/JavaScript projects. Covers:
- Database management (connection, migrations, diff)
- Execution and typing of SQL queries (`db.query<MyQuery>()` with types inferred at the point of use)
- Management of **RLS policies** (declarative authoring, type-checking of `USING`/`WITH CHECK` expressions, deployment via diff)
- Management of **triggers and stored procedures** (authoring, type-checking, deployment)
- 100% type-checking — any invalid query or procedure is a compile/build error
- Plugins: Prisma (schema generation from the annotated AST), and others through a common interface

### 3. SQL lint

A separate `pgsql-lint` crate with its own configurable rules. Distinct from the type-checker (which validates correctness): the linter enforces style conventions and patterns to avoid (naming, performance anti-patterns, project rules). Usable from the CLI and through the npm framework.

### 4. Type generation at the point of use

When a SQL query is executed or built, the TypeScript types matching the result columns are generated automatically. Relies on the annotated AST produced by the type-checker.

### 5. IDE extensions

Editor extensions (VS Code first, JetBrains next) providing:
- Syntax highlighting for PostgreSQL SQL and PL/pgSQL
- Real-time diagnostics (type errors, missing columns, invalid policies, etc.) via the LSP server
- Context-aware completion
- Go-to-definition on tables, columns, functions

---

## Overall architecture — Pipeline

```
SQL source (free order, standard or simplified syntax)
    ↓ parse
Syntactic AST
    ↓ normalize / reorder
Logical AST (FROM → JOIN → WHERE → GROUP BY → HAVING → SELECT → ORDER BY → LIMIT)
    ↓ type-check (single linear pass)
Type-annotated AST
    ↓ codegen / lint / LSP diagnostics / TS type generation
```

### Why reorder the AST

SQL is written in a syntactic order (`SELECT … FROM … JOIN …`) but evaluated in a different logical order. By normalizing the AST to evaluation order before the type-checker, type-checking becomes a **linear pass with no backtracking**: at each clause, the type context of the preceding clauses is complete and final.

- Aliases defined in `SELECT` and used in `ORDER BY` → resolved unambiguously
- Expansion of `*` → trivial once the FROM + JOINs context is resolved
- CTEs → each CTE is resolved and becomes a named type before being referenced
- The simplified syntax can use any order it likes: normalization absorbs it all

---

## Parser

**Choice: forked `sqlparser-rs` with a custom PL/pgSQL dialect**

| Criterion | `libpg_query` | `sqlparser-rs` |
|---|---|---|
| PG fidelity | 100% (same parser as PG) | ~90-95% |
| AST | Protobuf (not native Rust) | Idiomatic Rust `enum`/`struct` |
| Custom extensions | Impossible (opaque parser) | Forkable/extensible |
| WASM | Hard (C FFI) | Native |
| PL/pgSQL body `$$…$$` | Yes | Partial (opaque string) → to be extended |

The main gap with `sqlparser-rs` is parsing PL/pgSQL function bodies (`$$…$$`). This gap can be closed by extending the parser — which we will have to do anyway for the simplified syntax.

### Plugin system

Syntactic plugins registered on the AST for PostgreSQL extensions:
- PostGIS: geometric types, spatial functions
- Other extensions: same mechanism

---

## Type system

### SchemaProvider trait

```rust
trait SchemaProvider {
    fn get_table(&self, name: &str) -> Option<TableSchema>;
    fn get_type(&self, name: &str) -> Option<TypeDef>;
}
```

Two implementations, selected by the project config:

1. **`DdlFileProvider`**: analyzes DDL `.sql` files with the same parser
2. **`IntrospectionJsonProvider`**: consumes an introspection JSON file (output of a live connection to a DB)

### Inverted logic — schema generation plugins

Plugins can **generate** a schema from the analysis (the inverse of Prisma's approach):
- Input: AST annotated with inferred types
- Output: schema in the desired format (Prisma schema, JSON Schema, TypeScript types, etc.)

The `SchemaProvider` interface is the same in both directions.

---

## Type-checking features

- Automatic expansion of `*` (listing the actual columns)
- Existence check for referenced columns
- Type consistency check in the usage context
- Resolution of table and column aliases
- CTE support (each CTE becomes a named type in the context)
- JOINs: the type context is extended at each JOIN
- **RLS policies**: type-checking of `USING` and `WITH CHECK` expressions in the context of the target table

---

## RLS Policies

Parsing and type-checking of `CREATE POLICY` / `ALTER POLICY`. The npm framework exposes a declarative DSL to author them; the checker validates that expressions reference correct columns and types. Policies are deployed like the rest of the schema, via the migration diff.

---

## Simplified syntax

A sugar syntax for standard cases that **desugars to standard SQL** during parsing. Desugaring happens on the AST — the type-checker only ever sees normalized SQL.

Examples to be defined: typed inserts, declarative upserts, recurring procedure patterns, etc.

---

## Build-time code generation

When generating the final SQL (migrations, procedures):
- Injection of **hidden fields** (audit, soft delete, etc.)
- Injection of **snapshots** (value history)
- Injection of **NOTIFY** for OpenTelemetry / observability
- Generation of the **migration diff** (similar to what exists in kysely-pg-procedures)

---

## Integration

### Distribution

- **WASM**: universal integration (Node.js, Bun, Deno, browser, any WASM runtime)
- **napi-rs**: native Node.js addon for maximum performance

### LSP

LSP server (`tower-lsp`) for IDE integration:
- Semantic syntax highlighting
- Precise errors and warnings (missing column, incompatible type, invalid policy, etc.)
- Context-aware completion (columns available given the FROM/JOIN context)
- Go-to-definition on tables/columns/functions

---

## Monorepo structure (target)

```
pgsql-tools/
├── crates/
│   ├── pgsql-parser/       # forked sqlparser-rs parser + PL/pgSQL dialect
│   ├── pgsql-ast/          # normalized AST + types
│   ├── pgsql-checker/      # type-checker (linear pass over the logical AST)
│   ├── pgsql-lint/         # configurable lint rules (style, anti-patterns)
│   ├── pgsql-codegen/      # SQL generation + build-time injections
│   └── pgsql-lsp/          # LSP server (tower-lsp)
├── bindings/
│   ├── wasm/               # WASM build
│   └── node/               # napi-rs addon
├── npm/
│   └── pgsql-tools/        # npm framework (DX layer: queries, RLS, triggers, migrations)
├── plugins/
│   ├── postgis/            # PostGIS plugin
│   └── prisma/             # Prisma schema generation plugin
├── editors/
│   ├── vscode/             # VS Code extension
│   └── jetbrains/          # JetBrains extension
└── SPECS.md
```

---

## References

- [sqlparser-rs](https://github.com/apache/datafusion-sqlparser-rs)
- [libpg_query](https://github.com/pganalyze/libpg_query)
- [tower-lsp](https://github.com/ebkalderon/tower-lsp)
- [napi-rs](https://github.com/napi-rs/napi-rs)
- [kysely-pg-procedures](../kysely-pg-procedures) — original project, same domain, typed query builder approach
