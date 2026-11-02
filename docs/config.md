# `pgweave.toml` — format de configuration, v1

> Statut : **figé** (P0, 2026-11-01). Toute évolution est additive : une clé
> peut être ajoutée, aucune ne change de sens ni ne disparaît sans période de
> transition documentée ici.

Le fichier se place à la racine du projet. `pgweave` (CLI) et `pgweave-lsp`
le trouvent en remontant depuis le répertoire courant ou le dossier ouvert.
C'est **la seule** source de configuration : la CLI et l'éditeur lisent le
même fichier, avec les mêmes règles.

## Principes

- **Strict.** Une clé inconnue est une erreur. Une faute de frappe dans une
  clé ne doit jamais se traduire par un réglage ignoré en silence.
- **Rien d'accepté sans effet.** Une clé n'existe dans le format que si
  l'outil l'applique. Les évolutions prévues (en bas de page) sont refusées
  tant qu'elles ne sont pas implémentées.
- **Une config invalide arrête l'analyse.** La CLI sort avec le code `2` ; le
  LSP affiche l'erreur et n'analyse pas avec une config inventée.
- **Un seul vocabulaire de niveaux** : `"error"`, `"warn"`, `"info"`,
  `"off"` / `"ignore"` selon la section.

## Exemple complet

```toml
[postgres]
version = 18                       # obligatoire

[schema]
directory = "schema"               # fichiers de schéma déclaratifs
include = ["db/functions"]         # répertoires supplémentaires

[migrations]
directory = "migrations"

[queries]
directory = "queries"              # requêtes nommées (docs/named-queries.md)

[macros]
directory = "macros"               # *.pgmacro du projet

[parse]
unparsable = "warn"
unsupported = "warn"

[rules]
"L001" = "off"
"column-not-found" = "error"

[plugins]
builtin = { postgis = {} }
wasm = ["plugins/my_rules.wasm"]
```

La config minimale valide :

```toml
[postgres]
version = 18
```

## Sections

### `[postgres]` — obligatoire

| Clé | Type | Défaut | Rôle |
|---|---|---|---|
| `version` | entier | — (obligatoire) | Version majeure de PostgreSQL ciblée. Valeurs acceptées : `18`. |

Pourquoi obligatoire : analyser avec les règles d'une autre version serait
deviner. Une version non prise en charge est refusée avec la liste des
versions disponibles. À partir de P2, cette version sélectionne le pack de
catalogue `postgres@<version>` (PLAN.md, D9).

### `[schema]`

| Clé | Type | Défaut | Rôle |
|---|---|---|---|
| `directory` | chemin | absent | Fichiers de schéma, parcourus récursivement. |
| `include` | liste de chemins | `[]` | Répertoires de schéma supplémentaires. |
| `declarative` | booléen | `false` | Le schéma décrit seul l'état cible (D10) ; les migrations sont générées par `pgweave migrate create`. Ajouté le 2026-11-03 (P7). |

- `directory` **absent** : tout fichier `.sql` / `.pgsql` du projet, hors
  migrations et requêtes nommées, est un fichier de schéma.
- `directory` **présent** : seuls `directory` et `include` sont des fichiers
  de schéma ; les autres fichiers sont des requêtes.
- `declarative = false` : les fichiers de schéma complètent l'état atteint
  par les migrations (rejoués après elles).
- `declarative = true` : l'analyse ne rejoue que le schéma ; les migrations
  sont rejouées à part, et l'écart entre les deux est signalé
  (`schema-drift`, niveau `info`, réglable dans `[rules]` — `"error"` pour
  la CI, ou `pgweave migrate check`).

Ne sont jamais parcourus : `node_modules/`, `target/`, `dist/`,
`.schema-snapshots/` et les répertoires cachés.

### `[migrations]`

| Clé | Type | Défaut | Rôle |
|---|---|---|---|
| `directory` | chemin | `"migrations"` | Migrations, indexées dans l'ordre de leur séquence (préfixe numérique du nom). |

### `[queries]`

| Clé | Type | Défaut | Rôle |
|---|---|---|---|
| `directory` | chemin | absent | Fichiers de requêtes nommées, parcourus récursivement. Format : [`docs/named-queries.md`](named-queries.md). |

Les fichiers de ce répertoire sont vérifiés comme des requêtes (pas de DDL)
**et** selon le format des requêtes nommées. Ils ne sont jamais indexés comme
schéma.

### `[macros]`

| Clé | Type | Défaut | Rôle |
|---|---|---|---|
| `directory` | chemin | `"macros"` | Macros du projet (`*.pgmacro`). Une macro du projet portant le nom d'une macro fournie la remplace. |

### `[parse]`

| Clé | Valeurs | Défaut | Rôle |
|---|---|---|---|
| `unparsable` | `"error"` \| `"warn"` \| `"ignore"` | `"warn"` | Statement que le parser n'a pas su lire, et corps de fonction non analysé. |
| `unsupported` | `"error"` \| `"warn"` \| `"ignore"` | `"warn"` | Statement valide mais non modélisé par l'analyse. |

Surcharge ponctuelle : `pgweave check --on-unparsable error`.

### `[rules]`

Table `code = niveau`, appliquée à **tous** les diagnostics (checker, lint,
requêtes nommées), dans la CLI comme dans l'éditeur.

| Niveau | Effet |
|---|---|
| `"error"` | Fait échouer `check` (code de sortie 1). |
| `"warn"` | Signalé sans faire échouer. |
| `"info"` | Information. |
| `"off"` | Retiré. |

Les directives en commentaire (`-- pgweave-disable-next-line <code>`)
s'appliquent après `[rules]` et ont le dernier mot.

### `[plugins]`

| Clé | Type | Défaut | Rôle |
|---|---|---|---|
| `builtin` | table `nom = { options }` | `{}` | Plugins fournis à activer. Disponibles : `postgis`, `prisma`, `rls-enforcer`. Un nom inconnu est une erreur. |
| `wasm` | liste de chemins | `[]` | Plugins WASM, relatifs au fichier de config. Un plugin qui ne se charge pas est une erreur. |

## Changements par rapport à l'ancienne configuration

Avant P0, deux fichiers coexistaient : `pgsql-tools.config.toml` (projet,
lu par la CLI et le LSP) et `pgsql.toml` (règles et plugins, lu par la CLI
seule, dans le répertoire courant). La CLI et l'éditeur ne voyaient donc pas
les mêmes surcharges. Ils sont fusionnés dans `pgweave.toml` :

| Avant | Maintenant |
|---|---|
| `pgsql-tools.config.toml` | `pgweave.toml` |
| `pgsql.toml` `[rules]` / `[lint.rules]` | `[rules]` |
| `pgsql.toml` `[plugins]` | `[plugins] builtin` |
| `pgsql.toml` `wasm_plugins` | `[plugins] wasm` |
| `[schema] search_path` | retiré (n'avait aucun effet) — voir évolutions prévues |
| `[schema] file` (`pgsql.toml`) | retiré — utiliser `include` ou `--schema` |
| `[instrumentation]`, `[migrations] format`, `[migrations] hash_journal`, `[lint] disabled` | retirés (n'avaient aucun effet) |
| options de règle (`{ level, … }`) | retirées (jamais transmises aux règles) |

## Évolutions prévues (refusées tant qu'elles ne sont pas implémentées)

Décrites ici pour que les projets sachent où elles arriveront ; leur forme
exacte sera figée à l'implémentation.

- `[postgres] search_path` — `search_path` par défaut du projet (P2, D11).
- `[schema] namespaces` — schéma PostgreSQL par défaut d'un répertoire de
  schéma, ex. `{ "schema/auth" = "auth" }` (P2, D11).
- Les extensions **ne sont pas** configurées ici : elles se déduisent des
  `CREATE EXTENSION` du schéma et des migrations (D9).
