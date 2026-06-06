# pgsql-tools — Roadmap

> Mise à jour : juin 2026. Basée sur l'audit complet du codebase et l'analyse architecturale OXC/datafusion.

---

## État actuel (baseline)

| Composant | État | Notes |
|-----------|:----:|-------|
| Parser SQL + PL/pgSQL | ✅ | sqlparser-rs + dialecte PL/pgSQL custom, 55 tests |
| AST pgsql-ast | ✅ | DDL, DML, PL/pgSQL — sans NodeId/Span |
| Normalisation SELECT | ✅ | Ordre logique FROM→WHERE→SELECT |
| Type-checker (existence) | ✅ | E001/E003/W001/W003/W005 — pas d'inférence de type |
| Architecture plugins | ✅ | CheckRule + Checker walker + WASM host (wasmi) + Prisma skeleton |
| Codegen SQL (emit + diff + inject) | ✅ | Utilisable, à étendre |
| WASM bindings | ✅ | 4 fonctions exposées via wasm-bindgen |
| Node.js bindings | ✅ | 4 fonctions via napi-rs |
| CLI `pgsql check/lint/emit` | ✅ | Fonctionnel |
| LSP (diagnostics) | ⚠️ | Pipeline branché, hover/completion sont des stubs |
| Lint (4 règles) | ⚠️ | L001–L004, pas de règles avancées |
| pgsql-semantic | ❌ | Absent — bloque types TS, LSP avancé, type compat |
| NodeId / Span sur AST | ❌ | Absent — bloque références croisées et LSP |
| Framework npm | ❌ | Absent |
| Extension VS Code | ❌ | Absente |
| Génération types TypeScript | ❌ | Absente |

---

## Phase A — Fondation AST : NodeId + Span + Visit traits

**Durée estimée** : 1–2 semaines  
**Statut** : ✅ Livrée  
**Dépendances** : aucune  
**Débloque** : Phase B (semantic), LSP hover/completion

### Livrables

- `NodeId(u32)` sur tous les nœuds AST principaux : `Statement`, `SelectStmt`, `Expr` et ses variants, `CreateTable`, `CreateFunction`, `CreateProcedure`, `CreatePolicy`, `PlStatement`
- `Span { start: u32, end: u32 }` (byte offsets depuis la source) sur les mêmes nœuds
- Parser assigne les `NodeId` via un compteur atomique ; `Span` extrait des positions sqlparser-rs
- `pgsql_ast::visit::Visit<'a>` trait : ~40 méthodes default no-op, une par type de nœud
- `pgsql_ast::visit::VisitMut` trait : `enter_*` / `exit_*` pour les transformations
- `AstWalker` : implémentation du parcours qui appelle les méthodes du visitor
- Migration des règles checker/linter existantes vers le Visit trait

---

## Phase B — `pgsql-semantic` : symbol table + inférence de types

**Durée estimée** : 3–4 semaines  
**Statut** : ✅ Livrée  
**Dépendances** : Phase A  
**Débloque** : Phase C (LSP), Phase D (types TS), checker type-compat

### Livrables

- Nouveau crate `crates/pgsql-semantic/`
- `SymbolTable` : `name → Symbol { kind, node_id, data_type, span }`
  - Kinds : Table, Column, Function, Cte, PlVar, PlParam
- `ScopeTree` : hiérarchie de scopes, un par SELECT / sous-requête / CTE / bloc PL/pgSQL
  - `scope_at_offset(pos) → ScopeId` pour le LSP
  - `available_columns(scope_id) → Vec<Symbol>` pour la completion
- Type inference : `infer_expr_type(expr, scope, symbols) → Option<DataType>`
  - Expressions scalaires : littéraux, colonnes, opérateurs binaires, CASE, CAST, fonctions
- `OutputColumn { name, data_type, node_id }` par colonne de sortie d'un SELECT
- `SemanticResult { symbols, scopes, expr_types, select_columns }`
- `SemanticCtx<'a>` passé aux règles checker/linter
- Intégration dans `check_sql_self_contained` : parse → semantic → check

---

## Phase C — LSP complet + Extension VS Code

**Durée estimée** : 2 semaines  
**Statut** : ✅ Livrée  
**Dépendances** : Phase B  
**Débloque** : expérience IDE complète

### Livrables

- **Hover** : `node_at_offset` → `semantic.expr_types[node.id]` → affichage type
- **Completion** : `scope.available_columns()` → liste de suggestions avec types
- **Go-to-definition** : `symbols.declaration_span(symbol)` → position dans le fichier
- **Grammar TextMate** : `editors/vscode/syntaxes/pgsql.tmLanguage.json`
  - Tokens : keywords SQL, identifiants, strings, dollar-quotes, commentaires
  - PL/pgSQL : DECLARE, BEGIN/END, types de control flow
- **Extension VS Code** : `editors/vscode/`
  - `package.json` avec contribution point LSP + grammar
  - `src/extension.ts` : lance le serveur LSP local
  - Publish sur VS Code Marketplace

---

## Phase D — Génération de types TypeScript

**Durée estimée** : 1–2 semaines  
**Statut** : ✅ Livrée  
**Dépendances** : Phase B  
**Débloque** : Framework npm (Phase E)

### Livrables

- `pgsql_codegen::generate_ts_types(stmts, semantic) → String`
  - Chaque SELECT nommé → interface TypeScript `{ col: type, ... }`
  - Mapping `DataType → TypeScript type` : `Text → string`, `Integer → number`, `Boolean → boolean`, `Jsonb → unknown`, `Uuid → string`, etc.
  - Types nullable : `col: string | null`
- Hook dans bindings WASM/node : `generate_types(sql, schema_sql) → string`
- CLI : `pgsql codegen types --input schema.sql --output types/`
- Tests sur `examples/schema.sql` → vérification des interfaces générées

---

## Phase E — Framework npm `pgsql-tools`

**Durée estimée** : 3–4 semaines  
**Statut** : ✅ Livrée (TypeScript, nécessite pg pour la DB)  
**Dépendances** : Phase D  
**Débloque** : livrable principal côté TypeScript

### Livrables

- `npm/pgsql-tools/` — package npm
- `db.query<T>(sql, params)` → résultat typé avec le type généré en Phase D
- Connexion pool (wrapping `pg` ou `postgres`)
- `db.migrate({ from, to })` → diff + déploiement migrations
- DSL déclaratif pour RLS policies
- DSL pour triggers et procédures stockées
- Plugin Prisma : `pgsql-tools/plugins/prisma` — import du schéma Prisma
- Documentation + exemples complets

---

## Phase F — Extensions et polish

**Durée estimée** : continue  
**Statut** : ✅ Livrée  
**Dépendances** : Phases C + E  

### Livrables

- **Plugin PostGIS** : types GEOMETRY/GEOGRAPHY, fonctions spatiales ST_*
- **Lint rules avancées** : index manquant sur FK, unused JOINs, LIKE '%...', agrégats sans GROUP BY, NOT NULL sans DEFAULT sur migration
- **Syntaxe simplifiée** : desugar vers SQL standard (patterns upsert, soft delete déclaratif, etc.)
- **Extension JetBrains** : port de l'extension VS Code
- **Règles de migration sécurisée** (inspiré Squawk) : ADD COLUMN NOT NULL, DROP COLUMN, RENAME sans lock, etc.
- **Introspection live** : connexion à une DB PostgreSQL pour enrichir le SchemaProvider

---

## Dépendances entre phases

```
A (NodeId + Visit)
└── B (pgsql-semantic)
    ├── C (LSP + VS Code)
    └── D (Types TS)
        └── E (Framework npm)

F (Extensions) ← dépend de C + E, continu
```

---

## Historique des livraisons

| Phase | Date de livraison | Notes |
|-------|:-----------------:|-------|
| A — Fondation AST (NodeId + Span + Visit) | juin 2026 | NodeId/Span sur nœuds principaux, traits Visit/VisitMut |
| B — pgsql-semantic (symbol table + types) | juin 2026 | SymbolTable, ScopeTree, inférence d'expressions |
| C — LSP + Extension VS Code | juin 2026 | Hover, completion, go-to-definition, grammar TextMate |
| D — Génération types TypeScript | juin 2026 | codegen SELECT → interfaces TS, CLI `pgsql codegen` |
| E — Framework npm pgsql-tools | juin 2026 | db.ts, migrate.ts, policy.ts, backends/, plugin Prisma |
| F — Extensions et polish | juin 2026 | Lint L005-L009, migration safety M001-M003, PostGIS plugin, JetBrains skeleton |

---

## Métriques de succès

| Phase | Métrique |
|-------|----------|
| A | Tous les nœuds AST principaux ont `id` + `span` ; 0 tests cassés |
| B | `infer_expr_type` couvre 80% des expressions SQL courantes ; `OutputColumn` correct sur `examples/schema.sql` |
| C | Hover affiche le type dans VS Code sur `examples/schema.sql` ; completion liste les colonnes disponibles |
| D | `project_summary()` génère `{ total_tasks: number, completion_rate: number, ... }` |
| E | `db.query<ProjectSummary>(...)` typé end-to-end sans cast manuel |
| F | 0 faux positif sur un vrai schéma de production de 50+ tables |
