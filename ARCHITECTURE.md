# pgsql-tools — Architecture globale

> Document de référence. Sources : analyse d'OXC, datafusion-sqlparser-rs, audit du codebase (juin 2026).

---

## Principe fondateur

Inspiré d'OXC : **parse → normalize → semantic → tools**, dans cet ordre strict, sans aller-retour.
Chaque passe reçoit le résultat de la précédente en lecture seule.

```
Source SQL
  ↓ parse          → AST syntaxique + NodeId + Span (positions exactes)
  ↓ normalize      → AST logique (FROM→JOIN→WHERE→SELECT, desugar syntaxique)
  ↓ semantic       → SemanticResult (symbol table, scope tree, types résolus)
  ↓ [tools — lecture seule sur SemanticResult]
      ├── checker  → diagnostics de correction  (E001, E003, E005…)
      ├── linter   → diagnostics de style/perf  (L001, L003, L004…)
      ├── codegen  → SQL formaté, migrations diff, types TypeScript
      └── LSP      → hover, completion, go-to-definition
```

**Règle absolue** : aucun outil ne modifie l'AST. Seul `normalize` (VisitMut) est autorisé à
transformer l'arbre. Checker, linter, LSP : lecture seule + émission de diagnostics.

---

## Les 3 pièces fondamentales manquantes

### 1. `NodeId` + `Span` sur chaque nœud AST

Chaque nœud AST reçoit un `NodeId(u32)` stable assigné pendant le parse, et un `Span { start: u32, end: u32 }` en bytes depuis la source.

**Pourquoi** : sans `NodeId`, la symbol table ne peut pas faire de références croisées (col → table d'origine, call → définition). Sans `Span`, le LSP ne peut pas répondre à "quel nœud est sous le curseur ?".

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span { pub start: u32, pub end: u32 }

// Exemple sur SelectStmt
pub struct SelectStmt {
    pub id: NodeId,
    pub span: Span,
    pub columns: Vec<SelectItem>,
    pub from: Vec<TableRef>,
    // ...
}
```

### 2. La passe `pgsql-semantic` (crate nouvelle)

Le cœur manquant. Une seule passe sur l'AST logique produit :

```rust
pub struct SemanticResult {
    /// name (lower) → Symbol (table / colonne / fonction / CTE / var PL/pgSQL)
    pub symbols: SymbolTable,
    /// hiérarchie des scopes (SELECT, sous-requête, CTE, bloc PL/pgSQL)
    pub scopes: ScopeTree,
    /// type inféré de chaque expression  (NodeId → DataType)
    pub expr_types: HashMap<NodeId, DataType>,
    /// colonnes de sortie de chaque SELECT  (NodeId → Vec<OutputColumn>)
    pub select_columns: HashMap<NodeId, Vec<OutputColumn>>,
}

pub struct OutputColumn {
    pub name: Option<String>,   // alias si présent
    pub data_type: DataType,
    pub node_id: NodeId,        // nœud Expr source
}
```

Ce `SemanticResult` est passé en lecture seule à checker, linter, LSP, codegen.

### 3. `Visit` / `VisitMut` traits + walker

Remplace les pattern-match ad hoc dispersés dans les règles. Toutes les règles implémentent
`Visit` (lecture seule) ; `normalize` implémente `VisitMut`.

```rust
pub trait Visit<'a> {
    fn visit_select(&mut self, node: &'a SelectStmt, ctx: &SemanticCtx<'a>) {}
    fn visit_expr(&mut self, node: &'a Expr, ctx: &SemanticCtx<'a>) {}
    fn visit_function_call(&mut self, node: &'a FunctionCall, ctx: &SemanticCtx<'a>) {}
    fn visit_create_table(&mut self, node: &'a CreateTable, ctx: &SemanticCtx<'a>) {}
    // ~40 méthodes, toutes default no-op
}

pub trait VisitMut {
    fn enter_select(&mut self, node: &mut SelectStmt) {}
    fn exit_select(&mut self, node: &mut SelectStmt) {}
    // enter_* / exit_* pour chaque type de nœud
}
```

---

## Ce que `pgsql-semantic` débloque

| Feature | Sans semantic | Avec semantic |
|---------|:---:|:---:|
| E001 colonne inconnue | ✅ existence | ✅ + type du contexte |
| Type de retour d'une SELECT | ❌ | ✅ `Vec<OutputColumn>` |
| Génération types TypeScript | ❌ | ✅ `{id: string, name: string}` |
| Hover LSP | ❌ | ✅ type de la colonne survolée |
| Completion LSP | partiel | ✅ colonnes disponibles dans le scope |
| Go-to-definition | ❌ | ✅ NodeId → position déclaration |
| Expansion SELECT * | ❌ | ✅ liste réelle des colonnes |
| Résolution CTE | ❌ | ✅ CTE → type nommé dans scope |
| Compat. types WHERE | ❌ | ✅ `WHERE text_col = 42` → E005 |
| Overload résolution fonctions | ❌ | ✅ |

---

## Structure cible du monorepo

```
pgsql-tools/
├── crates/
│   ├── pgsql-ast/          # Types AST + NodeId + Span + Visit traits       [à étendre]
│   ├── pgsql-parser/       # sqlparser-rs + dialecte PL/pgSQL               [existe]
│   ├── pgsql-semantic/     # SymbolTable + ScopeTree + type inference        [NEW — Phase B]
│   ├── pgsql-checker/      # Règles de correction (sur SemanticResult)       [existe, à migrer]
│   ├── pgsql-lint/         # Règles de style/perf (sur SemanticResult)       [existe, à étendre]
│   ├── pgsql-codegen/      # emit SQL + diff migrations + génération TS      [existe, à étendre]
│   ├── pgsql-lsp/          # Serveur LSP (tower-lsp)                        [squelette]
│   ├── pgsql-plugin/       # Plugin trait + PluginRegistry                  [existe]
│   ├── pgsql-plugin-wasm/  # Host WASM plugins (wasmi)                      [existe]
│   └── pgsql-cli/          # CLI pgsql                                      [existe]
│
├── bindings/
│   ├── wasm/               # wasm-bindgen : check + semantic + lint + migrate
│   └── node/               # napi-rs : idem, API sync + async
│
├── npm/
│   └── pgsql-tools/        # Framework DX TypeScript                        [NEW — Phase E]
│       ├── src/
│       │   ├── db.ts        # db.query<T>(), connexion, transactions
│       │   ├── migrate.ts   # diff + deploy migrations
│       │   ├── policy.ts    # DSL RLS policies
│       │   └── codegen.ts   # appelle WASM pour générer les types
│       └── types/           # types générés à la build
│
├── plugins/
│   ├── prisma/              # PrismaPlugin (dans pgsql-plugin aujourd'hui)
│   └── postgis/             # PostGIS types + fonctions spatiales            [NEW — Phase F]
│
├── editors/
│   ├── vscode/              # Extension VS Code                              [NEW — Phase C]
│   │   ├── syntaxes/        # Grammar TextMate SQL + PL/pgSQL
│   │   └── src/extension.ts # Client LSP
│   └── jetbrains/           # Plugin JetBrains                              [futur]
│
├── examples/
│   ├── schema.sql           # Schéma d'exemple multi-tenant
│   └── wasm-plugin/         # Plugin WASM exemple
│
├── ARCHITECTURE.md          # Ce fichier
├── ROADMAP.md               # Roadmap phases A–F
├── SPECS.md                 # Spécifications fonctionnelles
└── pgsql.toml               # Config exemple
```

---

## Pipeline de données détaillé

### Parse

`pgsql_parser::parse(sql) → Result<Vec<Statement>>`

- sqlparser-rs 0.54 avec PostgreSqlDialect custom
- Préprocessing : neutralise SECURITY DEFINER, COST n, ROWS n
- Postprocessing : réinjecte les flags extraits sur les nœuds
- Corps PL/pgSQL `$$...$$` : lexer + parser custom → `FunctionBody::Parsed(PlpgsqlBody)`
- **À ajouter (Phase A)** : assignation `NodeId` + extraction `Span` depuis token positions

### Normalize

`pgsql_parser::normalize(stmts) → Vec<Statement>` *(partiellement implémenté)*

- Réordonne SELECT en ordre logique : FROM → JOIN → WHERE → GROUP BY → HAVING → SELECT → ORDER BY
- Résout les alias de colonnes dans ORDER BY (déjà fait dans `NormalizedSelect`)
- Desugar : syntaxe simplifiée → SQL standard
- **Implémentation** : `VisitMut` sur l'AST, en place

### Semantic

`pgsql_semantic::analyze(stmts, schema) → SemanticResult` *(Phase B)*

- Collecte toutes les déclarations : tables (depuis `SchemaProvider`), colonnes, fonctions, CTEs, variables PL/pgSQL
- Construit le `ScopeTree` : un scope par SELECT, sous-requête, CTE, bloc PL/pgSQL
- Résout chaque référence de colonne vers son `NodeId` de déclaration
- Infère le type de chaque `Expr` : `NodeId → DataType`
- Calcule `OutputColumn` pour chaque `SelectStmt`

### Checker / Linter (après semantic)

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
// SQL formaté
let sql = pgsql_codegen::emit_statement(&stmt);

// Migration diff
let diff = pgsql_codegen::diff_schemas(&old_stmts, &new_stmts);

// Types TypeScript (nécessite semantic)
let ts = pgsql_codegen::generate_ts_types(&stmts, &semantic);
```

### LSP

```rust
// hover — type de l'expression sous le curseur
let node = ast.node_at_offset(cursor_offset)?;
let ty = semantic.expr_types[&node.id];
hover_text = format!("{}: {}", node.name, ty);

// completion — colonnes disponibles dans le scope courant
let scope = semantic.scopes.scope_at_offset(cursor_offset);
completions = scope.available_columns();
```

---

## Gestion des extensions PostgreSQL

Via le `CheckRule` trait + `PluginRegistry` :

- **Built-in** : règles dans `pgsql-checker` et `pgsql-lint`
- **Rust plugins** : crate qui implémente `Plugin` + enregistrement dans le registry
- **WASM plugins** : `.wasm` via `pgsql-plugin-wasm`, ABI JSON-over-linear-memory
- **PostGIS** : types géométriques (GEOMETRY, GEOGRAPHY), fonctions spatiales → plugin dédié

---

## Références

- [OXC Architecture](https://github.com/oxc-project/oxc) — inspiration visitor pattern, semantic pass, arena alloc
- [datafusion-sqlparser-rs](https://github.com/apache/datafusion-sqlparser-rs) — parser base + Dialect trait
- [DataFusion SQL Planner](https://github.com/apache/datafusion/tree/main/datafusion/sql) — ContextProvider pattern, SqlToRel
- [tower-lsp](https://github.com/ebkalderon/tower-lsp) — LSP server framework
- [Squawk](https://github.com/sbdchd/squawk) — linter SQL PostgreSQL, architecture de référence pour les règles
