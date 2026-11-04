# pgzest (formerly pgweave, pgsql-tools)

PostgreSQL language toolchain in Rust. **PLAN.md is the steering document** (principles, decisions D1–D11, phases P0–P8); SPECS.md and ARCHITECTURE.md are historical context.

**Language rule: everything in the repo is in English** — code, comments, docs, messages, tests and commit messages.

## Stack

- **Language**: Rust (workspace with several crates)
- **Parser base**: `sqlparser-rs` (crates.io, `PostgreSqlDialect`, not forked) + in-house PL/pgSQL parser. Textual rewrites before parsing are debt to remove (PLAN.md D1: wrapper dialect)
- **LSP**: `tower-lsp`
- **Priority target**: Rust (`tokio-postgres`), then TypeScript. WASM / napi-rs bindings frozen until P8

## Key decisions

- The AST is normalized into logical evaluation order (FROM → JOIN → WHERE → SELECT …) before the type checker — never bypass this step
- The type checker only sees the normalized AST, never the raw syntactic AST
- A single entry point for schema info: today `SchemaProvider`, eventually the `pgzest-catalog` catalog (PLAN.md D2) — no direct access to DDL files from the checker
- Never guess a type or nullability: unknown = `Unknown` + diagnostic (PLAN.md §1)
- Silence is a bug: everything not analyzed is reported
- Plugins generate schema from the analysis (the reverse of Prisma's logic)

## Structure

Current: see `Cargo.toml` (workspace). Target: PLAN.md §9.
