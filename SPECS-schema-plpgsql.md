# pgsql-tools — Spec : Universalisation du schéma & Analyse PL/pgSQL

> Document de design — itérer avant d'implémenter.

---

## 1. Universalisation du schéma (`SchemaSource`)

### Motivation

Un projet réel peut avoir son schéma réparti entre :
- un seul fichier DDL (`schema.sql`)
- un dossier de migrations numérotées (`migrations/000001_init.sql`, `000002_add_users.sql`, …)
- un mix des deux, plus des fonctions dans un dossier séparé
- un dump pg_dump

Le type-checker et le LSP ne doivent pas avoir besoin de savoir d'où vient le schéma. La résolution doit être uniforme.

---

### 1.1 `FileRole` — classification des fichiers SQL

```rust
enum FileRole {
    /// Fichier de migration numéroté. Seul endroit où les DDL structurants sont autorisés.
    Migration { sequence: u64 },
    /// Fichier DDL de référence (schéma statique, dump pg, fichier de types).
    Schema,
    /// Requêtes applicatives (.sql embarqués dans du code, scripts de seed, etc.)
    Query,
}
```

**Détection automatique** (heuristique sur le nom de fichier) :
- `^[0-9]+_.*\.sql$` → `Migration`
- `schema.sql`, `*.schema.sql`, fichiers dans `schema/` → `Schema`
- Tout le reste → `Query`

Surchargeable par config projet (`pgsql-tools.toml`).

**Règles DDL par rôle** :

| DDL | Migration | Schema | Query |
|---|---|---|---|
| `CREATE TABLE/TYPE/FUNCTION` | ✅ | ✅ | ❌ E-DDL-001 |
| `ALTER TABLE`, `ADD COLUMN` | ✅ | ⚠️ W-DDL-002 | ❌ E-DDL-001 |
| `DROP TABLE/COLUMN` | ✅ | ⚠️ W-DDL-003 | ❌ E-DDL-001 |
| `INSERT/UPDATE/DELETE` | ✅ (seed) | ⚠️ W-DDL-004 | ✅ |

---

### 1.2 `SchemaSource` — abstraction source

```rust
pub trait SchemaSource: Send + Sync {
    /// Chunks SQL à indexer, avec leur origine et leur rôle.
    fn chunks(&self) -> Vec<SqlChunk>;
}

pub struct SqlChunk {
    pub origin: SourceOrigin,  // (path: PathBuf, byte_range: Range<usize>)
    pub sql: String,
    pub role: FileRole,
}
```

**Implémentations concrètes** :

```rust
/// Un fichier unique — rôle détecté ou explicite.
struct FileSource { path: PathBuf, role: Option<FileRole> }

/// Tous les .sql d'un dossier, triés (important pour les migrations).
struct DirSource  { root: PathBuf, pattern: &'static str }

/// Composition arbitraire de sources.
struct MultiSource { sources: Vec<Box<dyn SchemaSource>> }
```

**Ordre de résolution** : les `Migration` sont traitées dans l'ordre de leur `sequence`. La dernière définition d'un objet (même nom, même schéma) gagne — comportement identique à un `CREATE OR REPLACE`.

---

### 1.3 `SchemaIndex` — première passe rapide (noms seulement)

Scan léger (regex/token scan, pas d'AST complet) sur tous les chunks. Objectif : < 50ms sur 500 fichiers de migration.

```rust
pub struct SchemaIndex {
    pub tables:    HashMap<String, IndexEntry>,    // "public.users" → entry
    pub functions: HashMap<String, IndexEntry>,    // "public.get_user" → entry
    pub procedures:HashMap<String, IndexEntry>,
    pub types:     HashMap<String, IndexEntry>,
    pub schemas:   HashSet<String>,
    pub sequences: HashMap<String, IndexEntry>,
}

pub struct IndexEntry {
    pub origin: SourceOrigin,
    pub role:   FileRole,
    /// Position dans le fichier (pour go-to-definition).
    pub def_range: Range<usize>,
}
```

Le `SchemaIndex` est reconstruit incrémentalement sur les changements de fichiers (le LSP invalide uniquement les entrées du fichier modifié).

---

### 1.4 `SchemaResolver` — deuxième passe lazy

Parse complet d'un objet uniquement quand on en a besoin.

```rust
pub struct SchemaResolver {
    index:      Arc<SchemaIndex>,
    source_map: HashMap<PathBuf, String>,           // path → contenu SQL brut
    cache:      DashMap<String, Arc<TableSchema>>,  // "public.users" → colonnes
}

impl SchemaProvider for SchemaResolver {
    fn get_table(&self, name: &str) -> Option<Arc<TableSchema>> {
        // 1. cache hit → retour immédiat
        // 2. lookup dans index → trouve le fichier source
        // 3. parse uniquement le statement CREATE TABLE correspondant
        // 4. stocke en cache, retourne
    }
}
```

**Règle de nommage** : les noms dans l'index sont toujours `schema.name` (ex: `public.users`). La résolution accepte `users` (→ cherche `public.users` en fallback) et `myschema.users`.

---

### 1.5 Config projet (`pgsql-tools.toml`)

```toml
[schema]
sources = [
    { path = "migrations/",  role = "migration" },
    { path = "schema/types.sql", role = "schema" },
]

[schema.search_path]
default = ["public", "extensions"]
```

---

## 2. Analyse des corps de fonctions PL/pgSQL

### 2.1 Problème actuel

`check_plpgsql_body` fait une extraction ligne par ligne des fragments SQL. Cette approche est fondamentalement cassée parce qu'elle extrait des *fragments*, pas des *statements complets*. Résultat : faux positifs sur tout ce qui dépend du contexte (variables, paramètres, CTEs).

**Note** : les CTEs (`WITH … AS (…)`) ne sont pas un cas spécial dans les corps de fonctions — ils fonctionnent comme dans les requêtes standard. Le problème est uniquement que l'extracteur actuel les découpe. Avec une extraction correcte des statements complets, le checker existant les gère naturellement.

---

### 2.2 `PlpgsqlExtractor` — extraction structurée

Remplace le scanner ligne-par-ligne. Parse la structure PL/pgSQL pour produire :

```rust
pub struct PlpgsqlBody {
    pub context:    PlpgsqlContext,
    pub statements: Vec<PlpgsqlStmt>,
}

pub enum PlpgsqlStmt {
    /// Un statement SQL complet (SELECT, INSERT, UPDATE, DELETE, WITH …)
    Sql { sql: String, span: Span },
    /// EXECUTE format(…) — SQL dynamique, non analysable
    DynamicSql { hint: &'static str, span: Span },
    /// CREATE TEMP TABLE — définit une table temporaire dans le contexte
    CreateTempTable { def: TempTableDef, span: Span },
    /// Affectation de variable (`:=`)
    Assign { var: String, span: Span },
    /// Bloc de contrôle (IF/LOOP/FOR/…) — contient d'autres statements
    ControlBlock { children: Vec<PlpgsqlStmt>, span: Span },
}
```

L'extracteur s'appuie sur le parseur PL/pgSQL existant (`crates/pgsql-parser/src/plpgsql.rs`) et étend son analyse structurelle.

---

### 2.3 `PlpgsqlContext` — couche de résolution virtuelle

Construite à partir de la section DECLARE et des paramètres de la fonction.

```rust
pub struct PlpgsqlContext {
    /// Paramètres de la fonction (name, type)
    pub params: Vec<(String, DataType)>,
    /// Variables déclarées dans DECLARE
    pub declared: Vec<(String, PlVar)>,
    /// Tables temporaires créées dans le corps
    pub temp_tables: Vec<TempTableDef>,
    /// Tables système injectées (observabilité — voir §3)
    pub system_tables: Vec<TempTableDef>,
    /// Variables système injectées (voir §3)
    pub system_vars: Vec<(String, DataType)>,
}

pub enum PlVar {
    Scalar(DataType),
    /// `RECORD` — type inconnu statiquement
    Record,
    /// `tablename%ROWTYPE`
    RowOf(String),
    /// Variable de curseur
    Cursor,
}

pub struct TempTableDef {
    pub name: String,
    pub columns: Vec<ColumnDef>,
    /// true si c'est une table injectée par le runtime (§3)
    pub injected: bool,
}
```

Le `PlpgsqlContext` est passé au checker comme **couche de résolution supplémentaire**, au-dessus du `SchemaResolver`. La hiérarchie de résolution pour un statement SQL extrait d'un corps de fonction :

```
1. CTEs du statement (scope local, déjà géré par le checker)
2. Tables temporaires (PlpgsqlContext.temp_tables + system_tables)
3. Variables/paramètres (PlpgsqlContext.params + declared)
4. Schéma global (SchemaResolver)
```

---

### 2.4 Classification des erreurs (remplace la suppression)

```rust
pub enum BodyDiagKind {
    /// Référence résolue via le contexte PL/pgSQL — pas une erreur.
    ResolvedAsVariable { name: String, var_type: PlVar },
    /// Référence résolue via CTE local — pas une erreur.
    ResolvedAsCte { name: String },
    /// Référence résolue via table temporaire — pas une erreur.
    ResolvedAsTempTable { name: String },

    /// Nom inconnu : pas dans le schéma, pas dans le contexte.
    /// → Erreur affichée (E002 avec contexte [in function_name]).
    UnresolvedRef { name: String },

    /// Nom qui ressemble à une variable RECORD utilisée comme alias de table.
    /// → Warning W-PLV-001 avec hint "declare explicit type".
    PossiblyVariable { name: String, declared_as: PlVar },

    /// SQL dynamique (EXECUTE / EXECUTE format(…)).
    /// → Info I-DYN-001 : "cannot type-check dynamic SQL".
    DynamicSql,
}
```

**Ce qui était supprimé avant** (`E002 [in function_name]`) est maintenant classifié :
- Si le nom est dans `PlpgsqlContext` → `ResolvedAs*` (silencieux)
- Si inconnu → `UnresolvedRef` (erreur réelle, affichée)
- Si ambigu (RECORD) → `PossiblyVariable` (warning)

---

## 3. Schéma virtuel — Runtime d'observabilité

Inspiré de `kysely-pg-procedures`. Quand une fonction utilise des tables temporaires **ou** que le mode observabilité est activé, le runtime injecte des éléments supplémentaires dans le schéma virtuel de la fonction. Ces éléments doivent être connus du type-checker pour éviter les faux positifs, et sont émis dans la migration générée.

### 3.1 Injection automatique sur les tables temporaires

Toute table temporaire définie dans une fonction reçoit automatiquement une colonne supplémentaire :

```sql
"_proc_instance_id" UUID NOT NULL
```

Cette colonne :
- isole les lignes par exécution de la procédure (évite les collisions en cas d'appels récursifs ou parallèles)
- est injectée automatiquement dans tous les `INSERT` générés
- est utilisée comme filtre dans tous les `SELECT/UPDATE/DELETE` sur la table temporaire
- déclenche un `DELETE FROM temp_table WHERE _proc_instance_id = _proc_instance_id` automatique avant le `RETURN`

### 3.2 Variables système injectées (DECLARE)

Quand l'observabilité est activée (ou dès qu'il y a des tables temporaires) :

```sql
DECLARE
    _proc_instance_id  UUID       := gen_random_uuid();
    _proc_started_at   TIMESTAMPTZ := clock_timestamp();
    _traceparent       TEXT       := current_setting('app.traceparent', true);
    _span_id           TEXT       := encode(gen_random_bytes(8), 'hex');
    -- mode 'step' ou 'debug' uniquement :
    _proc_row_count    INTEGER;
    _proc_steps        JSONB[]    := ARRAY[]::JSONB[];
```

Ces variables sont ajoutées au `PlpgsqlContext.system_vars` → le type-checker les voit comme des variables scalaires valides.

### 3.3 Tables système injectées (schéma virtuel)

Disponibles dans toutes les fonctions instrumentées :

```
_proc_log (
    execution_id    UUID        NOT NULL,
    function_name   TEXT        NOT NULL,
    traceparent     TEXT,
    span_id         TEXT,
    started_at      TIMESTAMPTZ NOT NULL,
    duration_ms     INTEGER     NOT NULL,
    steps           JSONB                   -- mode 'step'/'debug' uniquement
)

_proc_snapshot (
    execution_id    UUID        NOT NULL,
    function_name   TEXT        NOT NULL,
    snapshot_name   TEXT        NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL
)

_proc_snapshot_rows (
    execution_id    UUID        NOT NULL,
    snapshot_name   TEXT        NOT NULL,
    table_name      TEXT        NOT NULL,
    row_data        JSONB       NOT NULL
)

_proc_snapshot_vars (
    execution_id    UUID        NOT NULL,
    snapshot_name   TEXT        NOT NULL,
    var_name        TEXT        NOT NULL,
    var_value       TEXT
)
```

Ces tables sont ajoutées au `PlpgsqlContext.system_tables` (`injected: true`) → visibles par le type-checker sans appartenir au `SchemaResolver` global.

### 3.4 NOTIFY et traceparent

En fin de fonction (avant `RETURN`), le runtime émet :

```sql
PERFORM pg_notify('proc_log', json_build_object(
    'execution_id',  _proc_instance_id,
    'function_name', '<nom_de_la_fonction>',
    'traceparent',   _traceparent,
    'span_id',       _span_id,
    'started_at',    _proc_started_at,
    'duration_ms',   EXTRACT(MILLISECONDS FROM clock_timestamp() - _proc_started_at)::int,
    'steps',         _proc_steps   -- si mode step/debug
)::text);
```

Le `traceparent` est lu depuis `current_setting('app.traceparent', true)`. Le middleware applicatif le positionne via `SET LOCAL "app.traceparent" = '...'` avant l'appel de la procédure.

### 3.5 Snapshot nommé

API interne (utilisable dans le corps de la fonction via la syntaxe SQL pgsql-tools) :

```sql
-- Capture l'état des tables temporaires et des variables à ce moment
PERFORM pgsql_tools_snapshot('<snapshot_name>');
```

Dé-sugaré vers :
```sql
INSERT INTO _proc_snapshot VALUES (_proc_instance_id, '<fn>', '<snapshot_name>', clock_timestamp());
INSERT INTO _proc_snapshot_rows
    SELECT _proc_instance_id, '<snapshot_name>', 'MyTempTable', row_to_json(t)
    FROM "MyTempTable" t WHERE t._proc_instance_id = _proc_instance_id;
-- + INSERT INTO _proc_snapshot_vars pour chaque variable déclarée pertinente
```

### 3.6 Appels récursifs (directs et indirects)

`_proc_instance_id` est une variable PostgreSQL locale à chaque appel de fonction — en cas de récursion, chaque appel a son propre UUID. Les tables temporaires étant filtrées par `_proc_instance_id`, il n'y a pas de collision entre niveaux de récursion.

Le codegen n'a rien de spécial à faire pour gérer la récursion : l'isolation est structurelle.

### 3.6b Activation de l'instrumentation

**Par défaut** : activée automatiquement sur toutes les fonctions/procédures PL/pgSQL.

**Opt-out par fonction** (commentaire immédiatement avant le `CREATE`) :
```sql
-- @pgsql-tools: no-instrumentation
CREATE OR REPLACE FUNCTION my_simple_helper() …
```

**Opt-in par fonction** (si désactivé globalement dans la config) :
```sql
-- @pgsql-tools: instrumentation
CREATE OR REPLACE FUNCTION my_critical_fn() …
```

**Config projet** (voir §5 — `pgsql-tools.toml`) :
```toml
[instrumentation]
enabled = true          # true par défaut ; false pour opt-out global
log_level = "step"      # "none" | "step" | "debug"
log_target = "notify"   # "notify" | "table" | "both"
```

---

### 3.7 Génération dans la migration

Les éléments du schéma virtuel sont **émis dans la migration générée** dans un bloc séparé :

```sql
-- [PGSQL-TOOLS:RUNTIME:BEGIN]
-- Tables d'observabilité (créées une seule fois dans le schéma)
CREATE UNLOGGED TABLE IF NOT EXISTS _proc_log ( … );
CREATE UNLOGGED TABLE IF NOT EXISTS _proc_snapshot ( … );
CREATE UNLOGGED TABLE IF NOT EXISTS _proc_snapshot_rows ( … );
CREATE UNLOGGED TABLE IF NOT EXISTS _proc_snapshot_vars ( … );
-- [PGSQL-TOOLS:RUNTIME:END]
```

Ce bloc est idempotent (`IF NOT EXISTS`) et placé avant les fonctions qui en dépendent.

---

## 4. Hash de migration (Roadmap — ne pas implémenter maintenant)

### Motivation

Permettre au LSP de type-checker un fichier de migration *en cours d'édition* en comprenant l'état du schéma à chaque point du fichier, et de détecter toute divergence entre l'état attendu et l'état réel.

### 4.1 Format des marqueurs

Chaque changement est un scope `BEFORE`/`AFTER` identifié par un `id` court (hash tronqué ou UUID court) partagé entre les deux tags. Le `BEFORE` porte le hash de l'état **avant**, le `AFTER` porte le hash de l'état **après** — le `id` permet de les associer sans ambiguïté même dans un fichier long.

```sql
-- [PGSQL-TOOLS:MIGRATION:BEGIN id="mig_a3f7c" schema_hash="a3f7c12"]

-- [PGSQL-TOOLS:CHANGE:BEFORE id="chg_b91d4" kind="table" name="public.users" hash="b91d4e8"]
ALTER TABLE users ADD COLUMN email_verified boolean DEFAULT false NOT NULL;
-- [PGSQL-TOOLS:CHANGE:AFTER id="chg_b91d4" hash="c42e1a9"]

-- [PGSQL-TOOLS:CHANGE:BEFORE id="chg_d03c1" kind="function" name="public.get_user" hash="d03c1f2"]
CREATE OR REPLACE FUNCTION get_user(p_id uuid) RETURNS users …;
-- [PGSQL-TOOLS:CHANGE:AFTER id="chg_d03c1" hash="e71a9b3"]

-- [PGSQL-TOOLS:MIGRATION:END id="mig_a3f7c" schema_hash="d57f3b1"]
```

**Règles du format** :
- Le `id` du `CHANGE:BEFORE` et de son `CHANGE:AFTER` est identique.
- Le `id` du `MIGRATION:BEGIN` et de son `MIGRATION:END` est identique.
- `kind` sur le `BEFORE` : `"table"` | `"function"` | `"procedure"` | `"type"` | `"sequence"` | `"index"` | `"policy"` | `"trigger"`.
- Un `CHANGE:BEFORE` sans `CHANGE:AFTER` correspondant → erreur W-MIG-002 "unclosed change scope".

### 4.2 Sémantique des hash

| Marqueur | Contenu hashé |
|---|---|
| `MIGRATION:BEGIN schema_hash` | Hash de `SchemaIndex` complet (tous objets, noms + signatures) |
| `CHANGE:BEFORE hash` | Hash de l'objet (`kind`) **avant** le DDL qui suit |
| `CHANGE:AFTER hash` | Hash de l'objet **après** le DDL |
| `MIGRATION:END schema_hash` | Hash de `SchemaIndex` complet **après** tous les DDL du fichier |

**Hash par type d'objet** :

| kind | Contenu du hash |
|---|---|
| `table` | colonnes (nom, ordre, type, nullabilité), contraintes nommées |
| `function` | signature (nom, paramètres + types, type de retour), langage |
| `procedure` | signature (nom, paramètres + types) |
| `type` | définition (enum values, ou colonnes pour les composite types) |
| `sequence` | start, increment, min, max |
| `index` | table cible, colonnes, méthode (btree, gin, …), condition WHERE |
| `policy` | table cible, commande, USING, WITH CHECK |
| `trigger` | table cible, timing, events, fonction appelée |

Les valeurs par défaut d'expression ne sont pas incluses dans le hash (trop volatiles).

### 4.3 Comportement du LSP dans un fichier de migration

Le LSP lit le fichier linéairement :

1. Lit `MIGRATION:BEGIN id=… schema_hash=…` → charge le `SchemaIndex` de cet état (depuis le journal ou le recalcul)
2. Pour chaque `CHANGE:BEFORE id="chg_X" kind=… name=… hash=…` :
   - Valide que l'objet courant correspond au hash → sinon W-MIG-001 "schema state mismatch"
   - Ouvre un scope identifié par `"chg_X"`
3. Le DDL entre BEFORE et AFTER est analysé et appliqué au `SchemaIndex` local (mutation non persistée)
4. Pour `CHANGE:AFTER id="chg_X" hash=…` :
   - Ferme le scope `"chg_X"` (BEFORE orphelin → W-MIG-002)
   - Valide que l'état post-DDL correspond au hash → sinon W-MIG-003 "unexpected post-change state"
5. Tout SQL après un `CHANGE:AFTER` est type-checked contre le schéma **incluant** les mutations précédentes
6. Lit `MIGRATION:END` → valide l'état final global

Résultat : une colonne ajoutée par `ALTER TABLE` dans un `CHANGE` scope est visible dans les `INSERT` qui suivent dans le même fichier.

### 4.4 Génération automatique (`migrate generate`)

La commande `pgsql-tools migrate generate` :
1. Diff du `SchemaIndex` avant/après chaque DDL → calcule les hash
2. Génère les `id` courts (8 chars hex, tronqués du hash du couple before+after)
3. Insère les marqueurs dans le fichier de migration généré
4. Maintient `pgsql-tools-hashes.json` (hash → `SchemaIndex` sérialisé pour rechargement rapide)

### 4.5 Initialisation brownfield (`migrate init`)

Pour les projets existants qui n'ont pas encore de migrations gérées par pgsql-tools.

**Commande** :
```bash
pgsql-tools migrate init [options]
  --from-sql <path>       Utilise les fichiers DDL existants comme source de vérité
  --from-db  <connstr>    Introspecte une base de données live (via pg_catalog)
  --name     <slug>       Nom de la migration baseline (défaut : "baseline")
  --sequence <n>          Numéro de séquence (défaut : 0 → "000000_baseline.sql")
```

Si ni `--from-sql` ni `--from-db` n'est fourni, utilise les `sources` définis dans `pgsql-tools.toml`.

**Ce que la commande produit** :

1. **`migrations/000000_baseline.sql`** — migration idempotente avec `IF NOT EXISTS` sur tous les objets :

```sql
-- [PGSQL-TOOLS:MIGRATION:BEGIN id="mig_init00" schema_hash="initial"]

-- [PGSQL-TOOLS:CHANGE:AFTER id="chg_a1b2c3" kind="table" name="public.users" hash="b91d4e8"]
CREATE TABLE IF NOT EXISTS public.users (
    id          uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    email       text        NOT NULL UNIQUE,
    created_at  timestamptz NOT NULL DEFAULT now()
);

-- [PGSQL-TOOLS:CHANGE:AFTER id="chg_d4e5f6" kind="function" name="public.get_user" hash="c03d1f2"]
CREATE OR REPLACE FUNCTION public.get_user(p_id uuid) RETURNS public.users …;

-- [PGSQL-TOOLS:MIGRATION:END id="mig_init00" schema_hash="a3f7c12"]
```

Différences par rapport aux migrations normales :
- `MIGRATION:BEGIN` a `schema_hash="initial"` (pas de hash before — point de départ)
- Pas de `CHANGE:BEFORE` (les objets n'existaient pas dans le journal)
- Uniquement `CHANGE:AFTER` pour enregistrer l'état cible de chaque objet

2. **`pgsql-tools-hashes.json`** — initialisé avec le hash final

3. **`pgsql-tools.toml`** — créé si absent, avec les sources détectées

**Réécriture safe des DDL** :

| DDL original | Version safe générée |
|---|---|
| `CREATE TABLE t` | `CREATE TABLE IF NOT EXISTS t` |
| `CREATE FUNCTION f` | `CREATE OR REPLACE FUNCTION f` |
| `CREATE PROCEDURE p` | `CREATE OR REPLACE PROCEDURE p` |
| `CREATE TYPE t AS ENUM` | `CREATE TYPE IF NOT EXISTS t AS ENUM` (PG14+) |
| `CREATE SEQUENCE s` | `CREATE SEQUENCE IF NOT EXISTS s` |
| `CREATE INDEX i ON t` | `CREATE INDEX IF NOT EXISTS i ON t` |
| `CREATE UNIQUE INDEX i ON t` | `CREATE UNIQUE INDEX IF NOT EXISTS i ON t` |
| `CREATE TRIGGER tr` | `CREATE OR REPLACE TRIGGER tr` (PG14+) |
| `CREATE POLICY p ON t` | `DO $$ BEGIN IF NOT EXISTS … END $$` |
| `ALTER TABLE t ADD COLUMN c` | `DO $$ BEGIN ALTER TABLE t ADD COLUMN c; EXCEPTION WHEN duplicate_column THEN NULL; END $$` |

**Comportement si une migration 000000 existe déjà** : erreur avec message explicite "baseline already exists — use `migrate init --sequence N` to choose a different number".

---

## 5. Config projet (`pgsql-tools.toml`)

Fichier à la racine du projet. Toutes les valeurs ont des defaults raisonnables — un projet minimal peut fonctionner sans config.

```toml
# ── Sources de schéma ──────────────────────────────────────────────────────────
[schema]
# Ordre = ordre d'application. Les migrations sont triées par sequence en interne.
sources = [
    { path = "migrations/",        role = "migration" },
    { path = "schema/types.sql",   role = "schema"    },
    { path = "schema/functions/",  role = "schema"    },
]
# search_path PostgreSQL par défaut pour la résolution des noms non qualifiés
search_path = ["public", "extensions"]

# ── Migrations ────────────────────────────────────────────────────────────────
[migrations]
directory = "migrations/"
# {sequence} = numéro, {name} = description slug, {ts} = timestamp ISO
format    = "{sequence:06}_{name}.sql"
# Journal des hash (chemin relatif)
hash_journal = "pgsql-tools-hashes.json"

# ── Instrumentation PL/pgSQL ──────────────────────────────────────────────────
[instrumentation]
enabled    = true          # false = opt-out global, réactivable par commentaire
log_level  = "step"        # "none" | "step" | "debug"
log_target = "notify"      # "notify" | "table" | "both"

# ── Lint ──────────────────────────────────────────────────────────────────────
[lint]
# Règles désactivées globalement
disabled = []
# Exemples : ["W-DDL-002", "W-DDL-003"]

# Surcharge de sévérité : transformer un warning en erreur, ou silencier une erreur
# [lint.override]
# "W-DDL-002" = "error"
# "E002"      = "warn"

# ── Plugins ───────────────────────────────────────────────────────────────────
[plugins]
enabled = []
# Exemples : ["postgis", "timescaledb"]
```

### Commentaires de contrôle inline

Les commentaires `-- @pgsql-tools: <directive>` s'appliquent à l'objet qui les suit immédiatement (même comportement que `eslint-disable-next-line`).

| Directive | Effet |
|---|---|
| `no-instrumentation` | Désactive l'injection runtime sur cette fonction |
| `instrumentation` | Force l'injection runtime (si disabled globalement) |
| `no-lint` | Désactive tout lint sur ce statement |
| `no-lint: W-DDL-002` | Désactive une règle spécifique sur ce statement |
| `lint: error` | Élève tous les warnings en erreurs pour ce statement |

---

## 6. Workarounds parser — roadmap des cas hors scope

Plusieurs constructions PL/pgSQL ont été temporairement simplifiées pour ne pas bloquer le checker.
Chaque cas est documenté ici avec l'approche correcte à implémenter.

### Cas 1 — `RETURNING … INTO var` (strip actuel → AST dédié)

**Situation** : `INSERT … RETURNING col INTO v_result` n'est pas compris par sqlparser-rs.
**Workaround actuel** : `strip_returning_into()` supprime la clause ` INTO var` avant de passer le fragment à sqlparser, perdant l'information sur les variables cibles.
**Fix correct** :
1. Ajouter `PlStatement::DmlReturningInto { stmt: Box<Statement>, targets: Vec<Ident> }` dans `pgsql-ast`.
2. Dans `parse_dml_statement` (parser PL/pgSQL), détecter la présence de `INTO` après `RETURNING`, extraire les cibles, et construire ce variant au lieu de stripper.
3. Dans le checker, vérifier les colonnes RETURNING contre le schéma et marquer les `targets` comme variables assignées dans le contexte.

### Cas 2 — Variables record (`v_rec.col`) dans le SQL embarqué

**Situation** : Une variable déclarée `v_rec RECORD` ou `v_row my_table%ROWTYPE` peut être utilisée comme `v_rec.col` dans des expressions SQL. Le checker voit `v_rec` comme un qualificateur de table et émet E002.
**Workaround actuel** : le checker texte (`check_plpgsql_body`) supprime tous les E002, au prix de faux négatifs.
**Fix correct** :
1. Étendre `TypeContext` avec `record_vars: HashMap<String, Option<&TableSchema>>` — `None` pour `RECORD`, `Some(ts)` pour `%ROWTYPE`.
2. Dans `BodySchemaProvider::get_table`, consulter `record_vars` en priorité (avant `temp_tables`).
3. Propager les assignations `SELECT … INTO v_rec FROM t` pour inférer le type de `v_rec` de manière flow-sensitive.

### Cas 3 — `NEW`/`OLD` dans le SQL embarqué des triggers ✅ (implémenté)

**Situation** : Les triggers utilisent `NEW.col` et `OLD.col` dans leurs corps SQL. Sans contexte, le checker émet E002 pour `NEW` et `OLD`.
**Fix implémenté** (deux niveaux) :
1. **Trigger table connue** : quand un `CREATE TRIGGER ... ON table_name EXECUTE FUNCTION fn()` est trouvé dans le même ensemble de fichiers analysés, le checker construit un `TriggerCtx` avec la table, et enregistre `new`/`old` dans `temp_tables` avec le schéma de cette table. `BodySchemaProvider.get_table("new")` retourne alors le bon schéma → vérification de colonnes complète.
2. **Trigger table inconnue** : quand la fonction a `RETURNS TRIGGER` mais aucun `CREATE TRIGGER` n'est trouvé dans les fichiers analysés (`trigger_table == None`), `is_trigger = true` dans `TypeContext`. Tous les `check_select_*` de ce contexte passent `phantom_qualifiers = {"new", "old"}` : le qualifier est reconnu comme valide, E002 supprimé, mais vérification de colonnes skippée (pas de schéma disponible).
3. **Multi-fichiers** : le CLI `check` construit maintenant le trigger_map depuis l'ensemble context + fichiers vérifiés via `Checker::run_with_context`. Passer `examples/procs.sql --schema examples/schema.sql` → les `CREATE TRIGGER` dans le schéma sont visibles même si les fonctions sont dans `procs.sql`.

**Points restants pour le roadmap** :
- Clause `REFERENCING NEW TABLE AS name` (nommage custom des tables de transition) → nécessite extension AST + parser
- Détection de la disponibilité flow-sensitive : `INSERT` only → OLD indéfini, `IF TG_OP = 'DELETE'` → OLD défini dans le scope du IF
- Statement-level triggers (NEW/OLD sont des tables, pas des lignes) → sémantique différente à gérer

### Cas 4 — Plvars dans `INSERT … VALUES (…)`

**Situation** : `INSERT INTO t (col) VALUES (p_param)` — `p_param` est une variable PL/pgSQL mais `check_sql_in_body` ne parcourt pas les expressions VALUES avec `plvars`, émettant potentiellement E001.
**Workaround actuel** : seul le SELECT source d'un INSERT est vérifié avec `check_select_with_plvars` ; les VALUES bruts ne sont pas traversés.
**Fix correct** :
1. Dans `check_sql_in_body`, pour `Statement::Insert` avec `InsertSource::Values`, parcourir chaque expression avec `check_expr` en passant `plvars`.
2. Alternativement, traiter les plvars comme des littéraux inconnus (`Expr::Literal`) en aval dans le type-checker.

### Cas 5 — Fallback texte (`check_plpgsql_body`) trop permissif

**Situation** : Quand le parser PL/pgSQL échoue sur un corps complexe (CTEs, FOR IN unnest, etc.), on bascule sur l'analyse ligne-par-ligne. Ce fallback supprime les E002 entiers et rate beaucoup de constructions.
**Fix correct** :
1. Étendre le parser PL/pgSQL pour couvrir les cas qui échouent (FOR IN, RETURN QUERY, CTEs imbriquées).
2. À défaut, que le fallback texte n'émette que des avertissements structurels (W002, W003) sans diagnostics de colonnes/tables — mieux vaut rien que des faux positifs.
3. Suivre le taux de fallback via un compteur de métriques dans le LSP (ratio corps parsés / corps texte) pour prioriser les cas à traiter.

---

## 7. Roadmap — ce qui n'est pas dans cette spec

À ajouter ultérieurement :
- **Go-to-definition** : naviguer vers la définition d'une table/colonne/fonction depuis n'importe quel fichier SQL
- **Rename refactoring** : renommer une table ou colonne avec mise à jour de toutes les références
- **Introspection live** : `IntrospectionJsonProvider` depuis une connexion DB (complément de `SchemaResolver`)
- **Plugin PostGIS** : types géométriques dans le type-checker
- **RLS policies** : type-check des expressions `USING`/`WITH CHECK`
