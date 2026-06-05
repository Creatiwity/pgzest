# pgsql-tools — Specs & Decisions

PostgreSQL language toolchain écrit en Rust, distribuable en WASM ou napi-rs, conçu pour s'intégrer dans des projets TypeScript ou tout autre langage.

## Motivation

Le SQL pur est plus lisible que les query builders typés (cf. kysely-pg-procedures). L'objectif est d'avoir le meilleur des deux mondes : écrire du SQL lisible tout en bénéficiant de la sécurité de type, de l'outillage IDE, et de la génération de code automatique (observabilité, snapshots, migrations).

---

## Livrables

### 1. Core Rust (agnostique)

Ensemble de crates Rust purs, sans dépendance à un runtime ou un écosystème particulier. Fondation de tous les autres livrables.

### 2. Framework npm

Package npm (via WASM ou napi-rs) orienté DX pour les projets TypeScript/JavaScript. Couvre :
- Gestion de la base de données (connexion, migrations, diff)
- Exécution et typage des requêtes SQL (`db.query<MyQuery>()` avec types inférés à la consommation)
- Gestion des **policies RLS** (authoring déclaratif, type-check des expressions `USING`/`WITH CHECK`, déploiement via diff)
- Gestion des **triggers et procédures stockées** (authoring, type-check, déploiement)
- Type-checking à 100% — toute requête ou procédure invalide est une erreur de compilation/build
- Plugins : Prisma (génération de schéma depuis l'AST annoté), et autres via interface commune

### 3. Lint SQL

Crate `pgsql-lint` séparé avec ses propres règles configurables. Distinct du type-checker (qui valide la correction) : le linter enforce des conventions de style et des patterns à éviter (nommage, anti-patterns de performance, règles projet). Consommable en CLI et via le framework npm.

### 4. Génération de typages à la consommation

Quand une requête SQL est exécutée ou buildée, génération automatique des types TypeScript correspondant aux colonnes du résultat. S'appuie sur l'AST annoté produit par le type-checker.

### 5. Extensions IDE

Extensions pour les éditeurs (VS Code en priorité, JetBrains ensuite) fournissant :
- Coloration syntaxique pour le SQL PostgreSQL et PL/pgSQL
- Diagnostics en temps réel (erreurs de type, colonnes inexistantes, policies invalides, etc.) via le serveur LSP
- Autocomplétion contextuelle
- Go-to-definition sur tables, colonnes, fonctions

---

## Architecture générale — Pipeline

```
Source SQL (ordre libre, syntaxe standard ou simplifiée)
    ↓ parse
AST syntaxique
    ↓ normalize / reorder
AST logique (FROM → JOIN → WHERE → GROUP BY → HAVING → SELECT → ORDER BY → LIMIT)
    ↓ type-check (passe linéaire unique)
AST annoté avec types
    ↓ codegen / lint / diagnostics LSP / génération de typages TS
```

### Pourquoi reordonner l'AST

SQL s'écrit dans un ordre syntaxique (`SELECT … FROM … JOIN …`) mais s'évalue dans un ordre logique différent. En normalisant l'AST vers l'ordre d'évaluation avant le type-checker, on obtient une passe de type-checking **linéaire et sans backtracking** : à chaque clause, le contexte de types des clauses précédentes est complet et finalisé.

- Aliases définis dans `SELECT` et utilisés dans `ORDER BY` → résolus sans ambiguïté
- Expansion de `*` → triviale une fois le contexte FROM + JOINs résolu
- CTEs → chaque CTE est résolu et devient un type nommé avant d'être référencé
- La syntaxe simplifiée peut avoir l'ordre qu'elle veut : la normalisation absorbe tout

---

## Parser

**Choix : `sqlparser-rs` forké avec dialecte PL/pgSQL custom**

| Critère | `libpg_query` | `sqlparser-rs` |
|---|---|---|
| Fidélité PG | 100% (même parser que PG) | ~90-95% |
| AST | Protobuf (pas Rust natif) | `enum`/`struct` Rust idiomatiques |
| Extensions custom | Impossible (parser opaque) | Forkable/extensible |
| WASM | Difficile (FFI C) | Natif |
| PL/pgSQL body `$$…$$` | Oui | Partiel (string opaque) → à étendre |

Le gap principal avec `sqlparser-rs` est le parsing des corps de fonctions PL/pgSQL (`$$…$$`). C'est un gap comblable par extension du parser — ce qu'on devra faire de toute façon pour la syntaxe simplifiée.

### Plugin system

Plugins syntaxiques enregistrés sur l'AST pour les extensions PostgreSQL :
- PostGIS : types géométriques, fonctions spatiales
- Autres extensions : même mécanique

---

## Type system

### SchemaProvider trait

```rust
trait SchemaProvider {
    fn get_table(&self, name: &str) -> Option<TableSchema>;
    fn get_type(&self, name: &str) -> Option<TypeDef>;
}
```

Deux implémentations, sélectionnées par la config projet :

1. **`DdlFileProvider`** : analyse des fichiers `.sql` DDL via le même parser
2. **`IntrospectionJsonProvider`** : consomme un fichier JSON d'introspection (output d'une connexion live à une DB)

### Logique inversée — plugins de génération de schéma

Des plugins peuvent **générer** un schéma à partir de l'analyse (logique inverse de Prisma) :
- Input : AST annoté avec types inférés
- Output : schéma au format voulu (Prisma schema, JSON Schema, TypeScript types, etc.)

L'interface `SchemaProvider` est la même dans les deux sens.

---

## Fonctionnalités de type-checking

- Expansion automatique de `*` (en listant les colonnes réelles)
- Vérification d'existence des colonnes référencées
- Vérification de cohérence de type dans le contexte d'utilisation
- Résolution des alias de table et de colonne
- Support des CTEs (chaque CTE devient un type nommé dans le contexte)
- JOINs : extension du contexte de types à chaque JOIN
- **RLS policies** : type-check des expressions `USING` et `WITH CHECK` dans le contexte de la table cible

---

## RLS Policies

Parsing et type-checking des `CREATE POLICY` / `ALTER POLICY`. Le framework npm expose un DSL déclaratif pour les authorer ; le checker valide que les expressions référencent des colonnes et des types corrects. Les policies sont déployées comme le reste du schéma, via le diff de migration.

---

## Syntaxe simplifiée

Une syntaxe sugar pour les cas standards qui **desugar vers du SQL standard** lors du parsing. Le desugar se fait sur l'AST — le type-checker ne voit jamais que du SQL normalisé.

Exemples à définir : insertions typées, upserts déclaratifs, patterns récurrents de procédures, etc.

---

## Génération de code à la build

À la génération du SQL final (migrations, procédures) :
- Injection des **champs cachés** (audit, soft delete, etc.)
- Injection des **snapshots** (historique de valeurs)
- Injection des **NOTIFY** pour OpenTelemetry / observabilité
- Génération du **diff de migration** (similaire à ce qui existe dans kysely-pg-procedures)

---

## Intégration

### Distribution

- **WASM** : intégration universelle (Node.js, Bun, Deno, browser, tout runtime WASM)
- **napi-rs** : addon natif Node.js pour perf maximale

### LSP

Serveur LSP (`tower-lsp`) pour intégration IDE :
- Coloration syntaxique sémantique
- Erreurs et warnings précis (colonne inexistante, type incompatible, policy invalide, etc.)
- Autocomplétion contextuelle (colonnes disponibles selon le contexte FROM/JOIN)
- Go-to-definition sur les tables/colonnes/fonctions

---

## Structure du monorepo (cible)

```
pgsql-tools/
├── crates/
│   ├── pgsql-parser/       # parser sqlparser-rs forké + dialecte PL/pgSQL
│   ├── pgsql-ast/          # AST normalisé + types
│   ├── pgsql-checker/      # type-checker (passe linéaire sur AST logique)
│   ├── pgsql-lint/         # règles de lint configurables (style, anti-patterns)
│   ├── pgsql-codegen/      # génération SQL + injections build-time
│   └── pgsql-lsp/          # serveur LSP (tower-lsp)
├── bindings/
│   ├── wasm/               # build WASM
│   └── node/               # napi-rs addon
├── npm/
│   └── pgsql-tools/        # framework npm (DX layer : requêtes, RLS, triggers, migrations)
├── plugins/
│   ├── postgis/            # plugin PostGIS
│   └── prisma/             # plugin génération schéma Prisma
├── editors/
│   ├── vscode/             # extension VS Code
│   └── jetbrains/          # extension JetBrains
└── SPECS.md
```

---

## Références

- [sqlparser-rs](https://github.com/apache/datafusion-sqlparser-rs)
- [libpg_query](https://github.com/pganalyze/libpg_query)
- [tower-lsp](https://github.com/ebkalderon/tower-lsp)
- [napi-rs](https://github.com/napi-rs/napi-rs)
- [kysely-pg-procedures](../kysely-pg-procedures) — projet d'origine, même domaine, approche query builder typé
