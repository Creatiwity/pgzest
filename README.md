# pgsql-tools

PostgreSQL language toolchain in Rust. Parser, type-checker, linter, migration engine, LSP server, and VS Code extension — all sharing the same AST.

> **État : v0.1, utilisable mais l'analyse n'est pas complète.** Les briques
> ci-dessous compilent et sont testées, mais le type-checker fait de la
> vérification d'existence et de l'inférence partielle — pas encore de
> vérification de compatibilité de types complète. Faux positifs attendus.

## Démarrage rapide

Prérequis : [Rust](https://rustup.rs) (la version est épinglée par
`rust-toolchain.toml`) et Node.js ≥ 18.

```bash
make bootstrap   # installe les dépendances Rust + npm, vérifie l'outillage
make demo        # compile et lance la CLI sur examples/ — voir l'outil en action
make help        # liste toutes les cibles
```

`make demo` est le point d'entrée si tu découvres le projet : il exécute
`check`, `lint`, `codegen` et `emit` sur `examples/` et affiche les
diagnostics réels.

Pour tester dans VS Code :

```bash
make vscode-dev      # compile le LSP + l'extension, puis explique la suite
code editors/vscode  # puis F5 → « Run Extension (playground) »
```

F5 ouvre une fenêtre sur `examples/playground/` — un vrai projet avec
`schema/`, `migrations/` et `pgsql-tools.config.toml`, dont chaque fichier
illustre une capacité (erreurs, directives, tolérance du parser, lint).
Voir `examples/playground/README.md`.

`make playground` fait la même démonstration en CLI.

## État par composant

| Composant | État | Détail |
|---|:---:|---|
| Parser SQL + PL/pgSQL | ✅ | sqlparser-rs forké + lexer/parser PL/pgSQL dédié |
| AST + NodeId/Span | ✅ | Spans en byte offsets, traits `Visit`/`VisitMut` |
| Normalisation ordre logique | ✅ | FROM → JOIN → WHERE → GROUP BY → SELECT |
| Type-checker | 🟡 | Existence des tables/colonnes OK ; inférence partielle, pas de compat de types complète |
| Linter (L001–L009, M001–M003) | ✅ | Règles de style + sûreté de migration |
| Moteur de diff / migrations | ✅ | Diff + snapshots + génération de fichiers |
| Codegen TypeScript | 🟡 | Fonctionne sur les SELECT nommés ; couverture des types à étendre |
| Serveur LSP | 🟡 | Diagnostics + hover + complétion + semantic tokens ; peu testé en usage réel |
| Extension VS Code | ✅ | Grammaire TextMate + client LSP, lancement F5 |
| Bindings WASM | 🟡 | Compilent (nécessite `wasm-pack`), vérifiés en CI, pas de tests |
| Bindings Node (napi-rs) | 🟡 | Compilent ; pas de tests, pas de paquet publié |
| Paquet npm `pgsql-tools` | 🟡 | ~1000 lignes ; seuls `policy` et `prisma` ont des tests |
| Plugin PostGIS | 🟡 | Types + signatures spatiales, couverture partielle |
| Plugins WASM (wasmi) | 🟡 | Hôte fonctionnel, un exemple dans `examples/wasm-plugin` |
| Extension JetBrains | ❌ | Squelette (2 fichiers Kotlin), non compilé en CI |

Légende : ✅ fonctionne et testé · 🟡 fonctionne, couverture ou périmètre partiel · ❌ non livré

Tests : 71 tests Rust, 8 tests TypeScript. `make ci` reproduit ce que vérifie
la CI GitHub Actions.

## Tolérance du parser et suppressions

Un statement que l'analyse n'a pas pu traiter n'est plus avalé
silencieusement — il est signalé, à un niveau que tu contrôles.

Deux catégories, distinguées :

| Catégorie | Signification | Défaut |
|---|---|---|
| `unparsable-statement` | sqlparser-rs n'a pas su lire le statement. Peut être du PostgreSQL valide hors périmètre **ou** une faute de syntaxe — indiscernables à ce niveau. | `warn` |
| `unsupported-statement` | Statement valide, volontairement pas modélisé dans l'AST (`GRANT`, …). Aucune ambiguïté. | `ignore` |

Le défaut `warn` sur `unparsable` rend le problème visible sans casser le
build (`check` sort 0 sur un warning), ce qui permet d'adopter l'outil sur une
base existante puis de durcir.

### Config projet

```toml
# pgsql-tools.config.toml
[parse]
unparsable  = "warn"    # "error" | "warn" | "ignore"
unsupported = "ignore"  # "error" | "warn" | "ignore"
```

Surcharge ponctuelle en ligne de commande :

```bash
pgsql check q.sql --on-unparsable error     # durcir en CI
pgsql check q.sql --on-unparsable ignore    # le temps d'une migration
```

### Directives en commentaire

À la ESLint, en commentaire ligne (`--`) ou bloc (`/* */`) :

```sql
-- pgsql-disable-next-line                          -- toutes les règles
-- pgsql-disable-next-line column-not-found         -- une seule
-- pgsql-disable-next-line column-not-found, L001   -- plusieurs
-- pgsql-enable-next-line L001                      -- réactive ponctuellement
-- pgsql-disable                                    -- début de bloc
-- pgsql-enable                                     -- fin de bloc
-- pgsql-disable-file                               -- tout le fichier
```

Un `-- pgsql-disable` non refermé porte jusqu'à la fin du fichier. Une
justification peut suivre un second `--` et est ignorée :

```sql
-- pgsql-disable-next-line unparsable-statement -- syntaxe PG 18, cf. #214
CREATE TABLE … ;
```

La suppression est positionnelle : elle s'appuie sur le `span` du diagnostic.
Type-checker et linter en produisent tous les deux, à la granularité du
statement — la même que celle du type-checker. Un diagnostic qui n'en aurait
pas (aucun aujourd'hui) ne serait couvert que par `pgsql-disable-file`.

Précision : le **type-checker vise l'expression fautive** (`SELECT id, nope`
souligne `nope`, pas le `SELECT`), y compris dans une sous-expression. Les
règles de **lint** visent le statement — leurs nœuds d'intérêt ne sont pas
toujours des expressions.

## Scope du projet

`pgsql check` et le serveur LSP partagent le même modèle : les fichiers du
scope sont **indexés sans être checkés** (tables, fonctions, RLS), puis seul
le fichier courant est analysé contre cet index.

Le scope est découvert en remontant jusqu'à `pgsql-tools.config.toml` :

```toml
[migrations]
directory = "migrations"     # indexé, chaque fichier avec sa séquence

[schema]
directory = "schema"         # indexé récursivement
include = ["db/functions"]   # répertoires supplémentaires
```

Si `[schema] directory` est absent, **tout le projet** est le scope — cohérent
avec la classification, qui traite alors tout fichier non-migration comme
Schema. `node_modules/`, `target/`, `dist/`, `.schema-snapshots/` et les
répertoires cachés ne sont jamais parcourus.

Priorité de résolution, premier trouvé gagne : `--schema` explicite → DDL du
fichier analysé → index du projet → plugins.

```bash
pgsql check queries/q.sql              # utilise l'index du projet
pgsql check queries/q.sql --no-project # ignore la config, mode isolé
pgsql check q.sql --schema other.sql   # ajoute une source explicite
```

Si un fichier du scope contient un statement illisible, les objets qu'il
définit sont absents de l'index — donc des `table-not-found` en cascade qui
sont des faux positifs. La CLI le signale au lieu de laisser croire à un index
complet.

## Limites connues

Par ordre d'importance :

1. **Faux positif `trigger-ambiguous-table`.** Le checker signale une erreur
   quand une même fonction trigger (ex. `set_updated_at`) sert plusieurs
   tables — c'est un motif PostgreSQL idiomatique. Visible sur
   `examples/schema.sql`, qui ne passe donc pas `check`.

2. Pas de vérification de compatibilité de types entre opérandes.

3. Pas d'introspection d'une base live : le schéma vient de fichiers DDL.

---

## CLI

Build:

```bash
cargo build -p pgsql
```

### check

Type-check a SQL file. Exits 1 if any errors are found.

```bash
pgsql check query.sql
pgsql check query.sql --schema schema.sql
```

The `--schema` option can point to a `.sql` file or a directory of `.sql` files (read in lexicographic order and concatenated).

### lint

Check for style and best-practice violations.

```bash
pgsql lint query.sql
```

Rules: `L001` (SELECT *), `L002` (implicit JOIN), `L003` (missing WHERE on DELETE/UPDATE), `L004` (LIKE without index), `L005`–`L009` (RLS, coalesce, NULL comparisons, etc.).

### migrate create

Compare the canonical schema against the last snapshot and generate a migration file.

```bash
pgsql migrate create <name> [options]
```

**Options:**

| Flag | Default | Description |
|------|---------|-------------|
| `--schema-dir` | `schema/` | Directory of canonical `CREATE TABLE` files |
| `--migrations-dir` | `migrations/` | Where migration files are written |
| `--snapshots-dir` | `.schema-snapshots/` | Where snapshots are stored |

**Workflow:**

```
schema/
  users.sql     ← edit this (your canonical CREATE TABLE)
  posts.sql

.schema-snapshots/   ← managed by pgsql, do not edit
  users.sql
  posts.sql

migrations/
  20260607-143022_add_email.sql   ← generated
```

1. Edit a file in `schema/`
2. Run `pgsql migrate create add_email`
3. Review the generated migration — dangerous ops (`DROP COLUMN`, `DROP TABLE`) are commented out by default
4. Uncomment and adjust as needed, add data migration steps

Each migration file is self-documenting:

```sql
-- Migration: add_email (20260607-143022)
-- Generated by: pgsql migrate create
--
-- pgsql-schema-hash: users=4f3a0cc242b0477c

ALTER TABLE users ADD COLUMN email TEXT;
```

The `pgsql-schema-hash` is a stable FNV-1a hash of the canonical CREATE TABLE at migration time — useful for audit and for linking a migration to the exact schema state it was written against.

If no changes are detected, no file is created.

### migrate diff

Compare two schema files directly (lower-level utility):

```bash
pgsql migrate diff old.sql new.sql
```

### emit

Parse a SQL file and print the AST (useful for debugging):

```bash
pgsql emit query.sql
```

### codegen

Generate TypeScript type interfaces from SQL functions and named queries:

```bash
pgsql codegen --input queries.sql
pgsql codegen --input queries.sql --output types.ts
```

---

## VS Code extension

### Development (F5)

Open the extension folder in VS Code, then press F5:

```bash
code editors/vscode/
```

The extension auto-discovers the `pgsql-lsp` binary from `target/debug/pgsql-lsp` or `target/release/pgsql-lsp` relative to the extension root — no manual path configuration needed in development.

### Settings

| Setting | Default | Description |
|---------|---------|-------------|
| `pgsql-tools.lspPath` | `pgsql-lsp` | Path to the LSP binary (auto-detected in dev) |
| `pgsql-tools.schemaFile` | `""` | Path to a schema `.sql` file **or directory** for type-checking |

The `schemaFile` setting accepts:
- A path to a single `.sql` file
- A path to a directory — all `.sql` files are read in lexicographic order

### Features

- **Diagnostics** — type errors and lint violations as you type, for `.sql` and `.pgsql` files
- **Hover** — column types and table names
- **Completion** — context-aware:
  - Statement start: `SELECT`, `INSERT INTO`, `UPDATE`, …
  - After `FROM`/`JOIN`: table names from schema
  - In `SELECT`/`WHERE`: columns from referenced tables, qualified (`alias.col`) and unqualified
  - After `alias.`: only columns from that alias
  - In `ORDER BY`/`GROUP BY`: column names + `ASC`/`DESC`

Build the LSP server:

```bash
cargo build -p pgsql-lsp          # debug
cargo build --release -p pgsql-lsp # release
```

---

## Crate structure

```
crates/
  pgsql-ast/         Types AST (Statement, Expr, nœuds DDL, Span, NodeId)
  pgsql-diagnostics/ Diagnostic, Severity, Span, codes — socle sans dépendance
  pgsql-parser/      sqlparser-rs + extension PL/pgSQL → pgsql-ast
  pgsql-schema/      SchemaProvider + types de schéma, resolver, index, config
  pgsql-semantic/    Table de symboles, arbre de scopes, inférence de types
  pgsql-checker/     Type-checker (passe linéaire), CheckRule, règles
  pgsql-lint/        Règles de lint, LintConfig
  pgsql-codegen/     Émetteur SQL, moteur de diff, codegen TypeScript
  pgsql-analysis/    Façade d'orchestration : parse → semantic → check → lint
  pgsql-plugin/      Système de plugins + plugin PostGIS
  pgsql-plugin-wasm/ Hôte de plugins WASM (wasmi)
  pgsql-lsp/         Serveur tower-lsp (diagnostics, hover, complétion)
  pgsql-cli/         Binaire pgsql (check, lint, migrate, emit, codegen)

bindings/
  wasm/            wasm-bindgen bindings
  node/            napi-rs bindings

editors/
  vscode/          VS Code extension (TypeScript + TextMate grammars)
```

---

## Architecture notes

The AST is reordered to **logical evaluation order** (`FROM → JOIN → WHERE → GROUP BY → HAVING → SELECT → ORDER BY`) before type-checking. This makes the type-checker a single linear pass with no backtracking.

The `SchemaProvider` trait is the single entry point for schema information. Plugins implement it to add types and functions (e.g. PostGIS spatial operators). The `CompositeSchemaProvider` merges multiple providers — first match wins.

### Règle de couches

Les dépendances internes forment un DAG strict, du bas vers le haut :

```
pgsql-ast · pgsql-diagnostics        (aucune dépendance interne)
  └── pgsql-parser
       └── pgsql-schema              (SchemaProvider vit ici)
            ├── pgsql-semantic       ┐
            ├── pgsql-checker        │ passes indépendantes,
            ├── pgsql-lint           │ qui ne se connaissent pas
            └── pgsql-codegen        ┘
                 └── pgsql-analysis  (façade : enchaîne les passes)
                      └── pgsql-cli · pgsql-lsp · bindings/*
```

Deux invariants à ne pas casser :

1. **`pgsql-diagnostics` et `pgsql-schema` ne dépendent de personne d'utile.**
   `Diagnostic` et `SchemaProvider` sont des contrats partagés ; ils vivaient
   dans `pgsql-checker`, ce qui obligeait le linter à compiler le
   type-checker et inversait la dépendance de `pgsql-schema`.
2. **Une passe n'appelle jamais une autre passe.** `pgsql-semantic` n'appelle
   pas le checker, `pgsql-lint` ne connaît pas le checker. Tout enchaînement
   vit dans `pgsql-analysis`, au-dessus.

See `SPECS.md` for full design decisions, `ARCHITECTURE.md` for internals, and `ROADMAP.md` for what's planned.

---

## Building everything

Tout passe par le `Makefile` :

```bash
make bootstrap   # dépendances Rust + npm (à faire une fois)
make all         # Rust + TypeScript + extension VS Code
make test        # tests Rust + TypeScript
make ci          # build + test + clippy + typecheck (ce que vérifie la CI)
make clean       # supprime les artefacts
```

Cibles Rust seules : `make build`, `make release`, `make test-rust`,
`make clippy`, `make fmt`. `make wasm` nécessite `cargo install wasm-pack`.

Le code est formaté avec `rustfmt` et sans aucun warning clippy. La CI
bloque sur les deux (`cargo fmt --all --check` et `clippy -- -D warnings`),
donc `make ci` avant de pousser.
