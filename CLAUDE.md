# pgweave (ex-pgsql-tools)

PostgreSQL language toolchain in Rust. **PLAN.md est le document de pilotage** (principes, décisions D1–D11, phases P0–P8) ; SPECS.md et ARCHITECTURE.md sont du contexte historique.

## Stack

- **Language**: Rust (workspace avec plusieurs crates)
- **Parser base**: `sqlparser-rs` (crates.io, `PostgreSqlDialect` non forké) + parser PL/pgSQL maison. Les réécritures textuelles avant parse sont une dette à supprimer (PLAN.md D1 : dialecte wrapper)
- **LSP**: `tower-lsp`
- **Cible prioritaire**: Rust (`tokio-postgres`), TypeScript ensuite. Bindings WASM / napi-rs gelés jusqu'à P8

## Décisions clés

- L'AST est normalisé en ordre d'évaluation logique (FROM → JOIN → WHERE → SELECT …) avant le type-checker — ne jamais bypasser cette étape
- Le type-checker ne voit que l'AST normalisé, jamais l'AST syntaxique brut
- Un seul point d'entrée pour les infos de schéma : aujourd'hui `SchemaProvider`, à terme le catalogue `pgsql-catalog` (PLAN.md D2) — pas d'accès direct à des fichiers DDL depuis le checker
- Ne jamais deviner un type ou une nullabilité : inconnu = `Unknown` + diagnostic (PLAN.md §1)
- Le silence est un bug : tout ce qui n'est pas analysé est signalé
- Les plugins génèrent du schéma à partir de l'analyse (logique inverse de Prisma)

## Structure

Actuelle : voir `Cargo.toml` (workspace). Cible : PLAN.md §9.
