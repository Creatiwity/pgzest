# pgsql-tools

PostgreSQL language toolchain in Rust. See SPECS.md for full architecture decisions.

## Stack

- **Language**: Rust (workspace avec plusieurs crates)
- **Parser base**: `sqlparser-rs` forké, étendu pour PL/pgSQL
- **LSP**: `tower-lsp`
- **Bindings**: WASM (primary) + napi-rs (Node.js)

## Décisions clés

- L'AST est normalisé en ordre d'évaluation logique (FROM → JOIN → WHERE → SELECT …) avant le type-checker — ne jamais bypasser cette étape
- Le type-checker ne voit que l'AST normalisé, jamais l'AST syntaxique brut
- `SchemaProvider` est le seul point d'entrée pour les infos de schéma — pas d'accès direct à des fichiers DDL depuis le checker
- Les plugins génèrent du schéma à partir de l'analyse (logique inverse de Prisma)

## Structure cible

```
crates/pgsql-parser/   → parse + normalize
crates/pgsql-ast/      → types AST partagés
crates/pgsql-checker/  → type-check linéaire
crates/pgsql-codegen/  → génération SQL + injections
crates/pgsql-lsp/      → serveur LSP
bindings/wasm/
bindings/node/
plugins/
```
