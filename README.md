# pgweave

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
make demo        # ouvre VS Code sur examples/demo, extension pgweave chargée
make demo-run    # exécute le programme Rust de la démo sur PostgreSQL 18 (Docker)
make help        # liste toutes les cibles
```

`make demo` est le point d'entrée si tu découvres le projet : une petite
application (migrations, requêtes nommées, code Rust généré) à explorer
dans l'éditeur, avec une visite guidée dans `examples/demo/README.md`.
`make demo-cli` exécute `check`, `lint`, `codegen` et `emit` sur
`examples/` et affiche les diagnostics réels.

Pour tester dans VS Code :

```bash
make vscode-dev      # compile le LSP + l'extension, puis explique la suite
code editors/vscode  # puis F5 → « Run Extension (playground) »
```

F5 ouvre une fenêtre sur `examples/playground/` — un vrai projet avec
`schema/`, `migrations/` et `pgweave.toml`, dont chaque fichier
illustre une capacité (erreurs, directives, tolérance du parser, lint).
Voir `examples/playground/README.md`.

`make playground` fait la même démonstration en CLI.

## État par composant

| Composant | État | Détail |
|---|:---:|---|
| Parser SQL + PL/pgSQL | ✅ | sqlparser-rs (dialecte PostgreSQL, non forké) + lexer/parser PL/pgSQL dédié |
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
| Paquet npm `pgweave` | 🟡 | ~1000 lignes ; seuls `policy` et `prisma` ont des tests |
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
| `unsupported-statement` | Statement valide, volontairement pas modélisé dans l'AST (`GRANT`, `CREATE VIEW`, …). | `warn` |
| `unparsed-function-body` | Le statement est analysé, mais le **corps** d'une fonction PL/pgSQL n'a pas pu être parsé. Il retombe alors sur un repli textuel presque aveugle : variables, types, `RETURN` et contexte trigger ne sont plus vérifiés. Une seule construction non gérée suffit. | `warn` (suit `unparsable`) |

**Le silence est traité comme un bug.** Aucune de ces trois situations ne
disparaît sans le dire — c'est ce qui distingue un outil *incomplet* d'un outil
*trompeur*. `unsupported` valait `ignore` par défaut : un `CREATE VIEW` jeté
signifiait que la vue n'existait pour aucun autre fichier, sans un mot.

Le défaut `warn` sur `unparsable` rend le problème visible sans casser le
build (`check` sort 0 sur un warning), ce qui permet d'adopter l'outil sur une
base existante puis de durcir.

### Config projet

```toml
# pgweave.toml — format complet : docs/config.md
[postgres]
version = 18            # obligatoire

[parse]
unparsable  = "warn"    # "error" | "warn" | "ignore"
unsupported = "ignore"  # "error" | "warn" | "ignore"
```

Surcharge ponctuelle en ligne de commande :

```bash
pgweave check q.sql --on-unparsable error     # durcir en CI
pgweave check q.sql --on-unparsable ignore    # le temps d'une migration
```

### Directives en commentaire

À la ESLint, en commentaire ligne (`--`) ou bloc (`/* */`) :

```sql
-- pgweave-disable-next-line                          -- toutes les règles
-- pgweave-disable-next-line column-not-found         -- une seule
-- pgweave-disable-next-line column-not-found, L001   -- plusieurs
-- pgweave-enable-next-line L001                      -- réactive ponctuellement
-- pgweave-disable                                    -- début de bloc
-- pgweave-enable                                     -- fin de bloc
-- pgweave-disable-file                               -- tout le fichier
```

Un `-- pgweave-disable` non refermé porte jusqu'à la fin du fichier. Une
justification peut suivre un second `--` et est ignorée :

```sql
-- pgweave-disable-next-line unparsable-statement -- syntaxe PG 18, cf. #214
CREATE TABLE … ;
```

La suppression est positionnelle : elle s'appuie sur le `span` du diagnostic.
Type-checker et linter en produisent tous les deux, à la granularité du
statement — la même que celle du type-checker. Un diagnostic qui n'en aurait
pas (aucun aujourd'hui) ne serait couvert que par `pgweave-disable-file`.

Précision : le **type-checker vise l'expression fautive** (`SELECT id, nope`
souligne `nope`, pas le `SELECT`), y compris dans une sous-expression. Les
règles de **lint** visent le statement — leurs nœuds d'intérêt ne sont pas
toujours des expressions.

## Macros

Une syntaxe compacte qui se désucre en SQL standard. Rien n'est deviné : toute
l'information est portée par la syntaxe.

```sql
@function user_count(target_org UUID): INTEGER {
    DECLARE total INTEGER;
    SELECT count(*) INTO total FROM users WHERE org_id = target_org;
    RETURN total;
}
```

11 lignes → 5. Les `DECLARE` s'écrivent près de leur usage et sont remontés en
section avant `BEGIN` à la compilation.

Les macros vivent dans des `.pgsql` ; `pgweave build` produit du `.sql` standard,
lisible par psql et pg_dump — aucun verrouillage.

```bash
pgweave check q.pgsql                # analyse directement, sans compiler
pgweave macros                       # les macros disponibles
pgweave macros --verbose             # trous, drapeaux et options de chacune
pgweave build q.pgsql                # affiche le SQL généré
pgweave build q.pgsql --out-dir gen  # écrit gen/q.sql
```

`check` et le serveur LSP désucrent d'eux-mêmes : **`build` n'est nécessaire
que pour produire un `.sql` destiné à psql ou à un déploiement**. Les
diagnostics pointent la bonne ligne du `.pgsql`, pas du SQL engendré.

### Fournies

| Macro | Rôle |
|---|---|
| `@function name(params)[: TYPE] [attributs] { … }` | fonction PL/pgSQL ou SQL |
| `@procedure name(params) [attributs] { … }` | procédure |
| `@trigger name timing events on table(s) for each row\|statement [when (…)] { … }` | la fonction **et** le `CREATE TRIGGER` |

Attributs de `@function` : `immutable`, `stable`, `volatile`,
`security_definer`, `strict`, `leakproof`, `language <lang>`, `cost N`,
`rows N`.

Le `RETURN` reste explicite : un `AFTER` ou un `FOR EACH STATEMENT` n'a pas les
mêmes conventions qu'un `BEFORE … FOR EACH ROW`, et une règle implicite
variable selon le timing serait un piège.

### Le trigger multi-tables

```sql
@trigger touch before update on (users, organizations) for each row {
    NEW.updated_at = now();
    RETURN NEW;
}
```

Une fonction est générée **par table**, ce qui donne à chacune un nom et un
`CREATE TRIGGER` propres. Le checker vérifie la colonne sur chaque table et
signale celle qui manque sur l'une d'elles — comme il le fait aussi pour une
fonction partagée écrite à la main (`set_updated_at` attaché à plusieurs
tables) : le corps est vérifié une fois par table qui la déclenche.

### Voir l'expansion dans VS Code

Le curseur dans une macro, puis **⌘⌥E** (`Ctrl+Alt+E` ailleurs) — ou le lien
« Voir l'expansion » affiché au-dessus de chaque macro, ou le menu contextuel.
Le SQL engendré s'ouvre dans un onglet **en lecture seule** à côté : le
modifier n'aurait aucun effet sur le `.pgsql`, laisser croire le contraire
serait un piège.

Curseur hors macro : tout le fichier est désucré.

Sous le capot, une requête LSP custom `pgweave/expandMacro` — même mécanique que
« Expand macro recursively » de Rust Analyzer.

### En déclarer une

Un `*.pgmacro` dans le répertoire `[macros] directory` (défaut : `macros/`) :

```
macro audited
header {name:ident}

template
CREATE TABLE {name} (
{body},
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
end
```

Directives : `macro`, `header`, `default`, `flag`, `option`, `template`…`end`.
Types de trou : `ident`, `word`, `words`, `parens`, `type`, `list`, `keywords`
— un jeu **fermé**, ce qui empêche le format de dériver vers un langage de
gabarits.

Les macros fournies sont écrites dans ce même format
(`crates/pgweave-macro/src/builtins/`) : un mécanisme d'extension dont les
macros fournies ne se servent pas est un mécanisme non testé. Une macro du
projet portant le nom d'une macro fournie la remplace.

## Requêtes nommées

Les requêtes appelées depuis le code vivent dans `[queries] directory`, une
par en-tête `-- name:` :

```sql
-- name: page_by_id :optional
SELECT id, title FROM pages WHERE id = $id;
```

`pgweave check` vérifie leur format (nom, paramètres `$name`, un statement
par requête) et chaque requête contre le schéma. Elles serviront de source à
la génération d'appels typés (Rust, puis TypeScript). Format complet :
[`docs/named-queries.md`](docs/named-queries.md).

## Scope du projet

`pgweave check` et le serveur LSP partagent le même modèle : les fichiers du
scope sont **indexés sans être checkés** (tables, fonctions, RLS), puis seul
le fichier courant est analysé contre cet index.

Le scope est découvert en remontant jusqu'à `pgweave.toml` :

```toml
[postgres]
version = 18

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
pgweave check queries/q.sql              # utilise l'index du projet
pgweave check queries/q.sql --no-project # ignore la config, mode isolé
pgweave check q.sql --schema other.sql   # ajoute une source explicite
```

Si un fichier du scope contient un statement illisible, les objets qu'il
définit sont absents de l'index — donc des `table-not-found` en cascade qui
sont des faux positifs. La CLI le signale au lieu de laisser croire à un index
complet.

## Limites connues

Par ordre d'importance :

1. Pas de vérification de compatibilité de types entre opérandes.

2. Pas d'introspection d'une base live : le schéma vient de fichiers DDL.

---

## CLI

Build:

```bash
cargo build -p pgweave
```

### check

Type-check a SQL file. Exits 1 if any errors are found.

```bash
pgweave check query.sql
pgweave check query.sql --schema schema.sql
```

The `--schema` option can point to a `.sql` file or a directory of `.sql` files (read in lexicographic order and concatenated).

### lint

Check for style and best-practice violations.

```bash
pgweave lint query.sql
```

Rules: `L001` (SELECT *), `L002` (implicit JOIN), `L003` (missing WHERE on DELETE/UPDATE), `L004` (LIKE without index), `L005`–`L009` (RLS, coalesce, NULL comparisons, etc.).

### migrate create

Compare the canonical schema against the last snapshot and generate a migration file.

```bash
pgweave migrate create <name> [options]
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

.schema-snapshots/   ← managed by pgweave, do not edit
  users.sql
  posts.sql

migrations/
  20260607-143022_add_email.sql   ← generated
```

1. Edit a file in `schema/`
2. Run `pgweave migrate create add_email`
3. Review the generated migration — dangerous ops (`DROP COLUMN`, `DROP TABLE`) are commented out by default
4. Uncomment and adjust as needed, add data migration steps

Each migration file is self-documenting:

```sql
-- Migration: add_email (20260607-143022)
-- Generated by: pgweave migrate create
--
-- pgweave-schema-hash: users=4f3a0cc242b0477c

ALTER TABLE users ADD COLUMN email TEXT;
```

The `pgweave-schema-hash` is a stable FNV-1a hash of the canonical CREATE TABLE at migration time — useful for audit and for linking a migration to the exact schema state it was written against.

If no changes are detected, no file is created.

### migrate diff

Compare two schema files directly (lower-level utility):

```bash
pgweave migrate diff old.sql new.sql
```

### emit

Parse a SQL file and print the AST (useful for debugging):

```bash
pgweave emit query.sql
```

### codegen

Generate TypeScript type interfaces from SQL functions and named queries:

```bash
pgweave codegen --input queries.sql
pgweave codegen --input queries.sql --output types.ts
```

---

## VS Code extension

### Development (F5)

Open the extension folder in VS Code, then press F5:

```bash
code editors/vscode/
```

The extension auto-discovers the `pgweave-lsp` binary from `target/debug/pgweave-lsp` or `target/release/pgweave-lsp` relative to the extension root — no manual path configuration needed in development.

### Settings

| Setting | Default | Description |
|---------|---------|-------------|
| `pgweave.lspPath` | `pgweave-lsp` | Path to the LSP binary (auto-detected in dev) |
| `pgweave.schemaFile` | `""` | Path to a schema `.sql` file **or directory** for type-checking |

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
cargo build -p pgweave-lsp          # debug
cargo build --release -p pgweave-lsp # release
```

---

## Crate structure

```
crates/
  pgweave-ast/         Types AST (Statement, Expr, nœuds DDL, Span, NodeId)
  pgweave-diagnostics/ Diagnostic, Severity, Span, codes — socle sans dépendance
  pgweave-parser/      sqlparser-rs + extension PL/pgSQL → pgweave-ast
  pgweave-schema/      SchemaProvider + types de schéma, resolver, index, config
  pgweave-catalog/     Catalogue PostgreSQL (packs générés), rejeu des DDL
  pgweave-sema/        Analyse sémantique : types, surcharges, requêtes, routines
  pgweave-checker/     Vérification (façade de pgweave-sema) + règles de plugins
  pgweave-lint/        Règles de lint, LintConfig
  pgweave-codegen/     Émetteur SQL, moteur de diff, codegen TypeScript
  pgweave-analysis/    Façade d'orchestration : parse → check → lint
  pgweave-plugin/      Système de plugins + plugin PostGIS
  pgweave-plugin-wasm/ Hôte de plugins WASM (wasmi)
  pgweave-lsp/         Serveur tower-lsp (diagnostics, hover, complétion)
  pgweave-cli/         binaire pgweave (check, lint, migrate, emit, codegen)

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
pgweave-ast · pgweave-diagnostics        (aucune dépendance interne)
  └── pgweave-parser
       └── pgweave-schema              (SchemaProvider vit ici)
            └── pgweave-catalog        (catalogue, rejeu)
                 └── pgweave-sema      (analyse sémantique)
                      ├── pgweave-checker  ┐ passes indépendantes,
                      ├── pgweave-lint     │ qui ne se connaissent pas
                      └── pgweave-codegen  ┘
                           └── pgweave-analysis  (façade : enchaîne les passes)
                                └── pgweave-cli · pgweave-lsp · bindings/*
```

Deux invariants à ne pas casser :

1. **`pgweave-diagnostics` et `pgweave-schema` ne dépendent de personne d'utile.**
   `Diagnostic` et `SchemaProvider` sont des contrats partagés ; ils vivaient
   dans `pgweave-checker`, ce qui obligeait le linter à compiler le
   type-checker et inversait la dépendance de `pgweave-schema`.
2. **Une passe n'appelle jamais une autre passe.** `pgweave-lint` ne connaît
   pas le checker, ni le checker le linter. Tout enchaînement vit dans
   `pgweave-analysis`, au-dessus.

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
