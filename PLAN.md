# pgweave — Plan directeur

> Rédigé le 2026-10-31. Remplace `ROADMAP.md` comme référence de pilotage.
> `SPECS.md`, `ARCHITECTURE.md`, `PLPGSQL_SPEC.md` et `SPECS-schema-plpgsql.md`
> restent des documents de contexte ; en cas de contradiction, ce plan fait foi.
>
> **Nom du projet : `pgweave`** (anciennement `pgsql-tools`).

---

## 0. Pourquoi ce plan

Le dépôt contient un socle d'analyse réel (parser SQL + PL/pgSQL, index de
projet, macros, lint, LSP, tests verts), mais la `ROADMAP.md` déclare livrées
des phases qui ne le sont qu'en surface, et **la couche qui justifie le projet
n'existe pas** : appeler du SQL typé depuis le code, sans coût à l'exécution.

Besoins concrets qui cadrent les priorités :

- **Mesalia** — une nouvelle brique centrale gRPC en Rust démarre sous 1 à 2
  semaines (à partir du 2026-10-31) et **remplacera Prisma par ce projet**.
  Constructions utilisées : procédures, triggers, tables temporaires, `CHECK`,
  `GENERATED ALWAYS AS … STORED`, enums (retouchés par `ALTER TYPE`),
  extensions `citext` et `btree_gist`. Identifiants entre guillemets en
  PascalCase (`"MenuComponent"`), hérités de Prisma. Historique de 224
  migrations Prisma.
- **Sherpa** — projet personnel servant de terrain d'essai ; schéma en cours
  de reconception, pas de schéma de référence existant. Même périmètre que
  Mesalia.
- **Rust d'abord**, sur `tokio-postgres` brut. TypeScript ensuite.
- **PostgreSQL 18** comme version cible initiale (Mesalia en production).
  Conséquence notable : en PG 18, `GENERATED ALWAYS AS (…)` sans mot-clé
  produit une colonne **virtuelle** — les deux variantes sont modélisées.
- Ce projet est le successeur de `kysely-procedures` : retour au SQL standard,
  avec le « sucre » nécessaire pour le rendre moderne et maintenable.
- **COPY FROM** en flux depuis un CSV (cas siren : table de staging, 50+
  colonnes listées à la main, sans vérification) — à couvrir nativement.
- **Extensibilité** pour les extensions PostgreSQL (PostGIS d'abord).
- **Reporté volontairement** : RLS et JSONB. Les deux méritent un travail de DX
  dédié avant d'être outillés (cf. §6).

Exigence transverse : robuste, clair, maintenable. Pas de raccourci qui crée
de la dette à rembourser avant même d'avoir servi.

---

## 1. Principes

Ils tranchent les arbitrages futurs ; une PR qui en viole un doit le justifier.

1. **Fidélité à PostgreSQL, pas approximation.** Les règles de résolution de
   types (surcharges, casts implicites, littéraux `unknown`, `UNION`/`CASE`)
   suivent le chapitre *Type Conversion* de la documentation PG. Le catalogue
   des fonctions/opérateurs/types intégrés est **généré depuis un vrai
   PostgreSQL**, jamais écrit à la main.
2. **Ne jamais deviner.** Un type ou une nullabilité inconnus sont représentés
   comme tels (`Unknown`), propagés sans cascade d'erreurs, et la cause racine
   est diagnostiquée. Un descripteur de requête (§4) **refuse** d'être produit
   s'il contient un inconnu : pas de `any`, pas de `unknown`, pas de cast
   silencieux côté code appelant.
3. **Le silence est un bug** (déjà en place, à conserver) : tout ce qui n'est
   pas analysé est signalé.
4. **Une seule source de vérité par préoccupation.** Un catalogue, un
   résolveur, un descripteur de requête consommé par toutes les cibles (Rust,
   TS, LSP).
5. **Déterminisme.** Même entrée → même sortie, octet pour octet (ordre des
   maps, identifiants de nœuds, code généré). Condition des caches et des
   snapshots.
6. **Le mécanisme d'extension est celui des intégrés.** `pg_catalog` est
   chargé comme une extension parmi d'autres ; les macros fournies sont
   écrites dans le format public (déjà le cas).
7. **Validé contre le réel.** Tout ce qui prétend imiter PostgreSQL est testé
   contre PostgreSQL (oracle différentiel, §7) et contre des corpus réels.
8. **Identifiants fidèles.** Un identifiant non quoté est replié en
   minuscules, un identifiant quoté est conservé tel quel — exactement comme
   PostgreSQL. `"MenuComponent"` et `menucomponent` sont deux objets
   distincts. (Le code actuel met tout en minuscules : c'est un bug pour
   Mesalia, corrigé en P1/P2.)
9. **Tout nom est qualifié par son schéma** une fois résolu. Aucun objet du
   catalogue n'existe « sans schéma » (cf. D11).

---

## 2. Décisions d'architecture

Format court : décision, raison, alternative écartée.

### D1 — Parser : dialecte wrapper sur `sqlparser-rs`, fork en dernier recours

Aujourd'hui `PostgreSqlDialect` est utilisé tel quel et ce qu'il ne gère pas
est contourné par **réécriture textuelle avant parse** (`PROCEDURE` →
`FUNCTION … RETURNS VOID`, suppression de `RETURNS TABLE`, `SECURITY DEFINER`,
`COST`, `ROWS`). Ces réécritures sont fragiles (commentaires, chaînes, casse),
décalent les spans et ne passent pas à l'échelle.

**Décision** : un `PgsqlDialect` qui délègue à `PostgreSqlDialect` et
intercepte via les hooks `parse_statement` / `parse_prefix` / `parse_infix` /
`get_next_precedence` (pattern recommandé par sqlparser-rs). Les manques
génériques sont contribués upstream ; un fork n'est envisagé que si un manque
bloquant ne peut être traité ni par hook ni upstream dans un délai acceptable,
et il est alors documenté (patch minimal, rebase suivi).

Plusieurs réécritures datent de sqlparser 0.54 ; la 0.62 en couvre peut-être
nativement une partie — à inventorier en premier.

L'AST maison `pgweave-ast` est conservé : il isole le reste du code des
changements d'AST de sqlparser.

### D2 — Catalogue unique, en couches, résolu à la demande

Remplace `SchemaProvider`, `SchemaResolver` (aujourd'hui eager), `SchemaIndex`,
le `TypeContext` du checker et la `SymbolTable` de `pgweave-semantic`.

Couches, de la plus externe à la plus interne, chacune pouvant masquer la
précédente :

```
contributeurs globaux (D9) : version majeure PG + extensions activées
  └─ projet (fichiers de schéma du dépôt, organisés par schéma PG — D11)
          └─ routine (paramètres, DECLARE, tables temporaires, NEW/OLD, FOUND…)
              └─ bloc PL/pgSQL (variables locales, curseurs, labels)
                  └─ statement (CTE, alias, colonnes de sortie)
```

Résolution des noms non qualifiés selon `search_path` (config projet, surcharge
par fichier/routine via `SET search_path`).

Modèle d'objet complet dès le départ pour ce qui est dans le périmètre :
schémas, tables (permanentes/temporaires, `ON COMMIT`), colonnes (type,
nullabilité, défaut, **identity**, **generated** `STORED` ou `VIRTUAL` + expression),
contraintes nommées (PK, UNIQUE, FK, **CHECK** + expression), types (base,
enum, domaine + contraintes, composite, tableau), fonctions (surcharges,
modes IN/OUT/INOUT/VARIADIC, défauts, volatilité, `STRICT`, `SETOF`,
`RETURNS TABLE`), procédures, triggers, opérateurs, casts, séquences, vues,
extensions. Les policies RLS sont **modélisées** (pour le diff de migration)
mais pas outillées côté DX (§6).

### D3 — Moteur incrémental : `salsa`

Le besoin « index léger + résolution lazy de ce qui est référencé + LSP
incrémental » est exactement celui de rust-analyzer et de `ty` (Astral), qui
s'appuient sur `salsa`. Un cache maison reproduirait salsa, en moins testé.

**Décision** : `salsa` pour la base de projet (fichiers, révisions, requêtes
mémoïsées : parse d'un fichier, déclarations d'un fichier, objet de catalogue
par nom, analyse d'une requête). Bonus direct : chaque requête analysée connaît
**l'ensemble exact des objets dont elle dépend**, ce qui sert l'invalidation
du cache de descripteurs (§4) et la recompilation Rust (§5).

Garde-fou : un spike de 2–3 jours valide l'API salsa (version épinglée) sur
parse + déclarations + une résolution de table avant d'engager la migration.
Critère d'abandon : impossibilité d'exprimer la portée « routine » proprement
ou surcoût mesuré > 2× sur l'indexation de Mesalia.

**Résultat (2026-11-01) : GO** — `docs/decisions/0001-salsa.md`. Index sans
parse en 4 ms, toutes les tables de Mesalia en 23 ms (contre 52 ms pour
l'existant), 134/134 tables identiques à l'existant, 0,07 ms après une
modification ; portée de routine exprimée sans difficulté.

### D4 — Un seul analyseur sémantique, sur une représentation résolue

`pgweave-semantic` et le checker font aujourd'hui de la résolution chacun de
leur côté. **Décision** : un crate `pgweave-sema` produit une représentation
résolue (HIR) — chaque référence pointe vers un objet du catalogue ou une
entrée de portée, chaque expression porte `(type, nullabilité)`. La
normalisation en ordre logique (FROM → … → ORDER BY) reste l'étape d'entrée,
conformément à `CLAUDE.md`. Checker, lint, codegen et LSP deviennent des
consommateurs en lecture seule de ce HIR.

### D5 — Nullabilité : un attribut de premier ordre

Treillis `NonNull | Nullable | Unknown`. Sources : `NOT NULL`, PK, identity,
generated (selon l'expression), domaines `NOT NULL`, jointures externes (côté
optionnel devient `Nullable`), agrégats (`count` NonNull, `sum`/`max`
Nullable), fonctions `STRICT`, `COALESCE`, `CASE` sans `ELSE`, sous-requêtes
scalaires (toujours Nullable), `RETURNING`. Le raffinement par `WHERE x IS NOT
NULL` est une amélioration ultérieure, pas un prérequis.

Côté paramètres : un paramètre est requis non-null s'il est comparé par un
opérateur strict ; optionnel s'il alimente une colonne nullable. Surcharge
explicite toujours possible.

### D6 — Paramètres nommés : `$name`

`$1…$n` (natif PG) et `$name` sont acceptés, sans mélange dans une même
requête. Choix de `$name` plutôt que `:name` :

- prolonge naturellement la notation native `$n` ;
- `:name` a déjà un sens en `psql` (interpolation côté client) et cohabite mal
  avec `::cast` et les slices `a[1:2]` ;
- précédent : pgtyped.

Contrainte lexicale : `$name` immédiatement suivi de `$` est un début de
dollar-quote, pas un paramètre. Le SQL envoyé au serveur est toujours
réécrit en positionnel.

### D7 — Descripteur de requête : contrat unique entre analyse et cibles

Toute génération (macro Rust, `.d.ts`, LSP) passe par un descripteur
sérialisé, stable et versionné (§4). Les cibles ne rappellent jamais
l'analyseur à leur façon.

### D8 — Runtime Rust : `tokio-postgres`, types connus statiquement

- Exécution via `query_typed` (types de paramètres envoyés avec la requête :
  Parse/Bind/Execute pipelinés, **pas d'aller-retour de `Prepare`**) ou via
  cache de statements préparés par connexion pour les requêtes chaudes.
- Décodage par index de colonne, via `FromSql`, dans des structs générées —
  pas de `Row` dynamique, pas de lookup par nom.
- Générique sur `GenericClient` : pool (`deadpool-postgres`), transaction ou
  client nu.
- Aucune connexion à une base au moment de la compilation (différence
  structurante avec sqlx et cornucopia/clorinde).

### D9 — Contributeurs du contexte global : version PG et extensions, même mécanisme

La version majeure de PostgreSQL et chaque extension sont des
**contributeurs** du contexte global, décrits par un même format de pack :

- fragment de catalogue (types, fonctions, opérateurs, casts, agrégats,
  méthodes d'index et classes d'opérateurs — nécessaires pour `btree_gist`) ;
- **généré** depuis un PostgreSQL réel (`xtask catalog-gen --pg 18
  [--extension citext]`), jamais écrit à la main ; un pack d'extension est le
  *delta* entre le catalogue avant et après `CREATE EXTENSION` ;
- identifié par `(nom, version)` : `postgres@18`, `citext@1.6`,
  `btree_gist@1.7`… La version majeure PG est un contributeur comme un autre ;
- table de correspondance de types par cible (`citext` → `String`,
  `geometry` → type choisi par le projet) ;
- règles optionnelles. Données, pas code. Les plugins WASM restent le canal
  pour des **règles** personnalisées, pas pour décrire un catalogue.

**D'où vient la liste des contributeurs actifs ?**

- **Version majeure** : `[postgres] version` de `pgweave.toml`, seule source.
- **Extensions** : déduites de l'état du schéma, pas déclarées à part. Les
  `CREATE EXTENSION` / `DROP EXTENSION` / `ALTER EXTENSION … UPDATE` du
  schéma déclaratif définissent l'état cible ; le rejeu des migrations
  définit l'état déployé. Une extension vit dans un schéma
  (`CREATE EXTENSION citext SCHEMA extensions`) : ses objets y sont rangés
  et résolus via `search_path` (D11).
- **Les deux sont analysés à chaque point du rejeu** : une migration est
  vérifiée contre la version PG et les extensions actives *à ce moment-là*.

**Diagnostics associés** :

- utilisation d'un objet fourni par une extension non activée → erreur avec
  suggestion (« `citext` provient de l'extension `citext`, absente ; ajouter
  `CREATE EXTENSION citext` ») ;
- extension activée mais jamais utilisée → avertissement (lint) ;
- objet absent ou modifié dans la version PG cible (fonction retirée, type
  renommé) → erreur contextualisée par version ;
- pack demandé mais non disponible pour cette version → erreur explicite,
  jamais de repli silencieux sur une autre version.

Chaque migration générée porte un en-tête machine-lisible indiquant le
contexte contre lequel elle a été générée et vérifiée :

```sql
-- pgweave: postgres=18 extensions=btree_gist@1.7,citext@1.6
```

Le rejeu vérifie la cohérence de cet en-tête avec l'état reconstruit
(extension annoncée mais jamais créée → erreur ; version PG de la config
différente de celle de la migration → information, et la migration est
analysée avec **sa** version). Les migrations héritées sans en-tête sont
analysées avec la version de la config et signalées une fois.

### D10 — Schéma déclaratif comme source de vérité, migrations générées

Les fichiers de schéma du dépôt décrivent l'état cible ; les migrations sont
produites par diff, revues, puis figées. Les migrations existantes servent à
vérifier qu'on sait reproduire l'état (et à l'import brownfield), pas de
source de vérité concurrente.

Le diff couvre aussi les contributeurs (D9) et les schémas (D11) : il émet
`CREATE SCHEMA`, `CREATE EXTENSION [SCHEMA …]`, `ALTER EXTENSION … UPDATE`,
`DROP EXTENSION` et leurs symétriques, ordonnés avant les objets qui en
dépendent (et après pour les suppressions). Un `DROP EXTENSION` alors que des
objets du schéma en dépendent encore est une erreur, pas une migration.

### D11 — Schémas PostgreSQL de premier ordre

Le schéma (namespace) fait partie de l'identité de chaque objet, partout :

- **Catalogue** : clé = `(schéma, nom)`, et `(schéma, nom, types
  d'arguments)` pour les fonctions et procédures. `pg_catalog` et `pg_temp`
  sont des schémas comme les autres ; les tables temporaires vivent dans
  `pg_temp` et sont prioritaires dans la résolution, comme dans PG.
- **Résolution** : `search_path` effectif = config projet → `SET search_path`
  dans un fichier ou une migration → clause `SET search_path` d'une fonction
  (qui s'applique à son corps). `pg_catalog` implicite en tête quand il
  n'est pas listé, comme dans PG. Ambiguïtés et masquages signalés.
- **Fichiers de schéma** : un fichier ou répertoire peut déclarer son schéma
  par défaut ; la convention recommandée est `schema/<nom_du_schéma>/…`.
- **Migrations** : `CREATE SCHEMA` / `DROP SCHEMA` gérés par le diff ; un
  objet déplacé (`ALTER … SET SCHEMA`) est un déplacement, pas un
  drop + create.
- **Lint** : `SECURITY DEFINER` sans `SET search_path` → avertissement
  (faille classique) ; objet créé sans qualification dans un fichier sans
  schéma par défaut → avertissement configurable.
- **Code généré** : le schéma fait partie du nom généré (module Rust par
  schéma, sauf pour le schéma par défaut configuré) — deux tables `users`
  dans `public` et `auth` ne doivent jamais entrer en collision.
- **Descripteur** (§4) : tous les types et dépendances y sont qualifiés.

---

## 3. Inventaire : on garde, on refond, on retire

| Élément | Sort | Note |
|---|---|---|
| `pgweave-ast` | Garder, étendre | DDL manquants (§P1), spans sur tous les nœuds, IDs déterministes par fichier |
| Parser PL/pgSQL (lexer + parser) | Garder, étendre | Cas restants de `SPECS-schema-plpgsql.md` §6 |
| Réécritures textuelles du parser | **Retirer** | Remplacées par D1 |
| `pgweave-diagnostics`, directives `pgweave-disable…` | Garder | |
| `pgweave-macro`, source maps, `build`/expansion | Garder, étendre | Fragments d'expression (§P6) |
| `SchemaProvider`/`SchemaResolver`/`SchemaIndex` | **Refondre** → `pgweave-catalog` | D2, D3 |
| `pgweave-semantic` + `TypeContext` du checker | **Fusionner** → `pgweave-sema` | D4 |
| `builtins.rs` écrit à la main | **Retirer** | Généré (D9, principe 1) |
| Règles checker/lint | Garder, porter sur le HIR | |
| `pgweave-codegen::diff`, sûreté de migration | Garder, étendre | Modèle complet D2 |
| `pgweave-codegen::ts_types` | **Retirer** | Bug d'ordre `HashMap` ; remplacé par le descripteur (§P8) |
| LSP + extension VS Code | Garder | Rebranché sur salsa/HIR, pas de nouvelle feature avant P4 |
| `npm/pgweave` (`db.query<T>` = cast) | **Geler** | Repris en §P8 sur le descripteur |
| Bindings WASM/Node | Geler | Repris en §P8 |
| `ROADMAP.md` | Archiver | Bandeau renvoyant ici |

---

## 4. Le descripteur de requête

Contrat entre l'analyse et toutes les cibles. Format JSON stable, versionné
(`"version": 1`), ordonné de façon déterministe.

```jsonc
{
  "version": 1,
  "name": "page_by_id",                  // requête nommée, ou dérivé du hash
  "sql": "SELECT id, title, creation_date FROM pages WHERE id = $1",
  "command": "select",                   // select | insert | update | delete | call | copy_in
  "params": [
    { "name": "id", "type": { "pg": "pg_catalog.int8", "oid": 20 }, "nullable": false }
  ],
  "columns": [
    { "name": "id",            "type": { "pg": "pg_catalog.int8" },        "nullable": false },
    { "name": "title",         "type": { "pg": "pg_catalog.text" },        "nullable": false },
    { "name": "creation_date", "type": { "pg": "pg_catalog.timestamptz" }, "nullable": true }
  ],
  "cardinality": "optional",             // many | one | optional | none
  "depends_on": ["public.pages"],        // objets de catalogue, pour l'invalidation
  "fingerprint": "…"                     // hash(sql normalisé + définitions de depends_on)
}
```

- **Cardinalité** inférée de façon conservatrice : égalité sur une clé
  unique/PK → `optional` ; `INSERT … VALUES (une ligne) RETURNING` →
  `one` ; agrégat sans `GROUP BY` → `one` ; statement sans `RETURNING` →
  `none`. Sinon `many`. Surchargeable explicitement, jamais devinée au-delà.
- Les types sont des références au catalogue (y compris enums, domaines,
  composites, types d'extension) ; la correspondance vers un type de langage
  est faite par la cible, via la table de correspondance (D9).
- Cache sur disque (`.pgweave/queries/<fingerprint>.json`), utilisable hors
  ligne en CI et par les outils qui ne veulent pas embarquer l'analyseur.

---

## 5. DX visée côté Rust

Illustratif — les noms définitifs sont fixés en P5.

```rust
// Requête inline : analysée à la compilation, erreurs SQL au bon span Rust.
let page = pgweave::query!(
    "SELECT id, title, creation_date FROM pages WHERE id = $id",
    id = page_id,
)
.fetch_optional(&client)        // cardinalité optional → Option<Row>
.await?;
// page: Option<PageByIdRow { id: i64, title: String, creation_date: Option<OffsetDateTime> }>

// Requête nommée, définie dans queries/pages.sql, réutilisable et
// servant de type de référence :
let rows = queries::pages::list_recent(&client, ListRecentParams { limit: 20 }).await?;

// COPY typé : liste de colonnes vérifiée contre le catalogue, flux CSV ou binaire.
let n = pgweave::copy_in!("etablissement_staging", columns = @etablissement_columns, format = csv(header))
    .from_reader(&client, csv_stream)
    .await?;
```

Points de conception :

- **Recompilation correcte** : la macro émet un `include_bytes!` des fichiers
  dont dépend la requête (connus via `depends_on`), pour que rustc recompile
  quand le schéma change — sans dépendre d'API instables.
- **Performance de compilation** : un catalogue salsa par processus de macro,
  chargement lazy ; budget mesuré en P5 (cf. §7).
- **Requêtes nommées** : générées par `build.rs` (code inspectable, pas de
  magie de macro) à partir du même descripteur que `query!`.
- **Types** : enums PG → enums Rust générées ; domaines → newtypes optionnels ;
  types d'extension → via le pack (D9) ; dates via `time` ou `chrono` au
  choix du projet (feature).

---

## 6. Hors périmètre volontaire (phases de design ultérieures)

- **RLS et privilèges** (`GRANT`/`REVOKE`, rôles) : le parse, le modèle de
  catalogue et le diff restent assurés (une migration ne doit perdre ni une
  policy ni un droit). La DX d'écriture lisible fera l'objet d'une phase de
  design dédiée.
- **JSONB** : typé `jsonb` opaque (→ `serde_json::Value` ou type utilisateur
  explicite). Le typage structurel des chemins JSON est une phase de design.
- **Instrumentation runtime** (`SPECS-schema-plpgsql.md` §3) : conservée
  comme idée, non planifiée.
- **Marqueurs de hash dans les migrations** (§4 de la même spec) : après P7.
- **Introspection d'une base live** comme source de schéma : non nécessaire à
  la cible (D10) ; utile pour l'import brownfield, réévaluée en P7.
- **Extension JetBrains**, polish LSP au-delà de l'existant.

---

## 7. Stratégie de qualité

Mise en place dès P0 ; chaque phase l'enrichit.

- **Oracle différentiel PostgreSQL** (CI, conteneur PG épinglé sur la
  version cible) : pour chaque requête du corpus de test, comparer les types
  de paramètres et de colonnes que nous inférons avec ceux renvoyés par le
  `Prepare` de PostgreSQL. Tout écart est un bug ou une divergence documentée.
  Les migrations générées sont appliquées puis le catalogue réel introspecté
  est comparé au catalogue attendu.
- **Corpus réels** : migrations Mesalia, schéma siren, schéma Sherpa — lus
  depuis un chemin fourni par variable d'environnement, **jamais copiés dans
  le dépôt**. Critère : zéro faux positif, zéro statement non analysé non
  justifié.
- **Snapshots** (`insta`) pour diagnostics, HIR, descripteurs, code généré.
- **Fuzzing** du parser et du lexer PL/pgSQL : aucune panique.
- **Benchmarks** (`divan` ou `criterion`) avec budgets suivis en CI :
  indexation Mesalia, analyse d'une requête à froid / à chaud, temps de
  compilation d'un crate de 200 `query!`.
- `make ci` reste la porte d'entrée unique ; zéro warning clippy.

---

## 8. Phases

Chaque phase a des critères de sortie vérifiables. On ne démarre pas la
suivante sur une phase non terminée, sauf parallélisation explicitement notée.

### P0 — Remise à plat (fondations de travail) — ✅ terminée (2026-11-01)

- ✅ Documentation alignée sur la réalité (`CLAUDE.md`, bandeau `ROADMAP.md`,
  README).
- ✅ Oracle différentiel PG 18 : crate `pgweave-oracle`, corpus
  `crates/pgweave-oracle/cases`, rapport instantané (`insta`), `make oracle`
  (Docker) et job CI `postgres:18`. Base : 65 % des colonnes conformes, 0 %
  des paramètres (aucune inférence de paramètre).
- ✅ Rapport « taux d'analyse » sur Mesalia : `cargo xtask corpus` (corpus
  par variable d'environnement, agrégats sans noms), `docs/baselines/`.
  87 % des statements analysés, 220/220 migrations valides sur PG 18,
  27 faux positifs tous dus au repli textuel des corps PL/pgSQL.
- ✅ Identifiants de nœuds déterministes : numérotation par parse
  (`crates/pgweave-parser/src/ids.rs`), le couple (fichier, `NodeId`)
  identifie un nœud.
- ✅ Spike salsa (D3) : GO, `docs/decisions/0001-salsa.md`.
- ✅ Format des requêtes nommées figé : `docs/named-queries.md` (en-tête
  `-- name: x [:one|:optional|:many|:none]`, `$name`, vérifié par `check`
  et le LSP).
- ✅ Format de config projet figé : `docs/config.md` (`[postgres] version`
  obligatoire, fichier unique `pgweave.toml`, clés strictes). `search_path`
  et schéma par défaut par répertoire y sont décrits comme évolutions P2.
- ✅ Renommage en `pgweave` (crates `pgweave-*`, binaires `pgweave` /
  `pgweave-lsp`, config `pgweave.toml`, directives `pgweave-disable…`,
  extension VS Code, CI). Les extensions de fichier `.pgsql` / `.pgmacro` et
  l'identifiant de langage `pgsql` sont conservés : ils nomment le langage,
  pas l'outil. Aucun alias de compatibilité.

**Sortie** : CI avec oracle opérationnel ; rapport de base Mesalia publié
dans `docs/` ; décision salsa actée ; formats requêtes nommées et config
publiés.

### P1 — Parser fidèle — ✅ terminée (2026-11-01, `e1c6b1f`)

Bilan : Mesalia 1740/1740 statements et 28/28 corps analysés, siren
294/294 et 10/10 ; aucune réécriture textuelle (dialecte enveloppe +
grammaire pgweave sur les tokens) ; parser PL/pgSQL réécrit sur les tokens
du dialecte (expressions SQL ordinaires, spans) ; identifiants fidèles
(principe 8) ; en-tête `-- pgweave:` lu ; fuzzing déterministe sans panique
(`crates/pgweave-analysis/tests/no_panic.rs`). Un audit des champs
sqlparser ignorés a supprimé une série de pertes silencieuses (fenêtres,
`FILTER`, arguments nommés, `NOT VALID`, `USING`, `DEFAULT VALUES`…).
Sherpa : pas de corpus de schéma exploitable (bac à sable), sans objet.
Restent pour P2 : 10 faux positifs `column-not-found` Mesalia (vérification
contre l'état final, résolus par le rejeu) et les `function-not-found` sur
des fonctions intégrées ou d'extensions (catalogue généré, packs D9).

- `PgsqlDialect` wrapper ; suppression de toutes les réécritures textuelles.
- Couverture DDL du périmètre : `CREATE TYPE` (enum, composite), `CREATE
  DOMAIN`, `CREATE VIEW`, `CREATE SEQUENCE`, `CREATE SCHEMA`, `CREATE
  EXTENSION`, colonnes `GENERATED ALWAYS AS (…) [STORED | VIRTUAL]` (en
  PG 18, sans mot-clé = `VIRTUAL`) et `IDENTITY`, `RETURNING OLD/NEW`
  (PG 18),
  contraintes nommées, contraintes `EXCLUDE USING gist` (Mesalia, via
  `btree_gist`), `CREATE TEMP TABLE … ON COMMIT`, `CREATE PROCEDURE` natif,
  `CALL`, `COPY … FROM STDIN`, `CREATE/ALTER/DROP EXTENSION` (avec `SCHEMA`
  et `VERSION`), `ALTER TYPE … ADD VALUE / RENAME`, `ALTER … SET SCHEMA`,
  `SET search_path`, variantes `ALTER` utilisées par les corpus, `COMMENT ON`.
- Identifiants quotés conservés avec leur casse dans l'AST (principe 8).
- En-tête `-- pgweave: postgres=… extensions=…` reconnu (D9).
- PL/pgSQL : cas restants de `SPECS-schema-plpgsql.md` §6 (`RETURNING …
  INTO`, variables record, `VALUES` avec variables, `FOR … IN`, `RETURN
  QUERY`, `GET DIAGNOSTICS`, `EXCEPTION`), suppression du repli textuel
  (tout corps non parsé = diagnostic, pas d'analyse dégradée).
- Spans sur tous les nœuds d'expression et de DDL.

**Sortie** : corpus Mesalia/siren/Sherpa parsés à 100 % ou chaque échec
justifié par un ticket ; fuzzing sans panique ; aucune réécriture textuelle
restante.

### P2 — Catalogue — 🚧 en cours (état au 2026-11-01, `eb58037`)

Fait : crate `pgweave-catalog` (modèle D2/D11, casse exacte), packs générés
(`cargo xtask catalog-gen` : postgres@18, citext, btree_gist, btree_gin,
pg_trgm, unaccent, fuzzystrmatch, postgis@3.6.4), rejeu ordonné des DDL
(`Replay`, blocs `DO` évalués sur tables virtuelles, `ddl-not-replayed`
sinon), `ProjectCatalog` qui remplace `SchemaResolver`/`DdlSchemaProvider`/
`builtins.rs` (supprimés) pour checker, analyse, CLI et LSP, résolution
exacte partout, diagnostics D9 (extension manquante suggérée, en-têtes de
migration), import brownfield `pgweave schema import`, base salsa
`pgweave-project` avec test d'incrémentalité. Mesalia : catalogue reconstruit
identique au réel, 0 diagnostic de `check`, import vérifié (schéma exporté →
base vierge → catalogue identique).

siren (`cargo xtask corpus siren --path ~/dev/creat/siren/migrations
--layout diesel --pg-url postgres://postgres:pgweave@localhost:54319/postgres
--details`, conteneur `pgweave-postgis`, image `postgis/postgis:18-3.6` en
amd64) : catalogue et import identiques au réel, sauf les 2 triggers posés
par `diesel_manage_updated_at` (SQL dynamique : `ddl-not-replayed`).
Exécuter ce helper Diesel, très courant, demanderait d'évaluer `EXECUTE
format(…)` : à reprendre avec l'interprétation des corps (P3+).

Reste, dans l'ordre :
1. ✅ siren (défauts `DEFAULT NULL` stockés selon la coercion, pack
   `btree_gin` enregistré : l'import passe).
2. Brancher le LSP sur `pgweave-project` (passer les macros du projet en
   entrée salsa, diagnostics de rejeu par fichier).
3. Portées routine/bloc dans le catalogue : reportées en P3 avec le HIR
   (le `TypeContext` du checker les porte en attendant).
4. Clore P2 : baselines (`docs/baselines/`), statut ci-dessous.

- Crate `pgweave-catalog` : modèle D2, couches, schémas et `search_path`
  (D11), clés sensibles à la casse (principe 8).
- Générateur (`xtask catalog-gen`) : extrait `pg_namespace`, `pg_type`,
  `pg_proc`, `pg_operator`, `pg_cast`, `pg_aggregate`, `pg_opclass`,
  `pg_am` d'un PG épinglé → pack `postgres@<major>` versionné dans le dépôt.
  Packs d'extension par delta avant/après `CREATE EXTENSION` : `citext` et
  `btree_gist` d'abord (Mesalia), puis PostGIS (siren).
- Contributeurs actifs (D9) : version PG depuis la config, extensions depuis
  l'état reconstruit ; diagnostics d'extension manquante / inutilisée /
  indisponible ; vérification des en-têtes de migration.
- Base de projet salsa : fichiers → déclarations → objets résolus à la
  demande ; application ordonnée des DDL (`ALTER` incluses).
- Portées routine/bloc : paramètres, variables, tables temporaires créées
  dans le corps (visibles après leur création), `NEW`/`OLD` selon timing,
  événements et niveau (row/statement), variables spéciales (`FOUND`,
  `TG_OP`…).
- Migration du LSP et de la CLI sur cette base ; suppression de
  `SchemaProvider`/`SchemaResolver`/`SchemaIndex`/`builtins.rs`.

- **Import brownfield** (avancé depuis P7, chemin critique Mesalia) :
  rejouer les migrations Prisma → fichiers de schéma déclaratifs organisés
  par schéma PG (`pgweave schema import`) + migration baseline avec en-tête.
  L'import ne fait qu'émettre le catalogue reconstruit ; il ne demande pas le
  diff de P7.

**Sortie** : catalogue de Mesalia reconstruit identique (à l'oracle) au
catalogue réel obtenu en appliquant ses migrations sur un PG vierge, y
compris extensions et schémas ; schéma déclaratif de Mesalia importé et
relu ; modification d'un fichier → seules les requêtes dépendantes sont
recalculées (test).

### P3 — Analyse sémantique unifiée

- Crate `pgweave-sema` : normalisation → résolution → HIR typé.
- Système de types fidèle : catégories, casts implicites/assignation,
  résolution de surcharges d'opérateurs et fonctions, littéraux `unknown`,
  `UNION`/`CASE`/`COALESCE`/`VALUES`, tableaux, enums, domaines.
- Nullabilité (D5). Inférence des paramètres `$n`/`$name` (D6).
- Sémantique des constructions du périmètre : insertion/mise à jour interdite
  d'une colonne generated ; expressions `CHECK` et `GENERATED` typées dans le
  contexte de leur table ; `DEFAULT` ; `ON CONFLICT` ; `RETURNING` ; `CALL`
  vérifié contre la signature de procédure ; corps de trigger vérifié par
  table (existant, porté).
- Checker et lint portés sur le HIR ; suppression de `pgweave-semantic` et du
  `TypeContext`.

**Sortie** : oracle différentiel vert sur le corpus de requêtes (≥ 300
requêtes représentatives, dont les corps de Mesalia) ; zéro faux positif sur
les corpus ; compatibilité de types vérifiée (`WHERE text_col = 42` →
diagnostic).

### P4 — Descripteur de requête

- Crate `pgweave-query` : descripteur §4, cardinalité, `depends_on`,
  fingerprint, cache disque.
- Requêtes nommées en fichiers (`queries/*.sql`, en-tête `-- name: …` ou
  macro dédiée), réécriture `$name` → positionnel.
- CLI `pgweave describe <fichier|requête>` et sortie JSON.

**Sortie** : descripteurs déterministes (snapshots) ; refus explicite quand
un type est inconnu ; oracle vert sur types de paramètres et colonnes.

### P5 — Intégration Rust

- `pgweave-runtime` : exécution D8, `fetch_one`/`fetch_optional`/`fetch_all`/
  `execute` alignés sur la cardinalité, flux (`RowStream`), transactions,
  `copy_in` typé (CSV avec validation d'en-tête optionnelle, binaire avec
  structs).
- `pgweave-macros` : `query!`, `query_file!`, `copy_in!` ; diagnostics SQL
  re-situés dans le source Rust ; recompilation correcte (§5).
- Génération `build.rs` des requêtes nommées.
- Correspondance de types Rust, extensible par pack (PostGIS).
- Exemple de bout en bout reproduisant le chargement siren (staging + COPY +
  swap) sur `tokio-postgres`.

**Sortie** : un service d'exemple compile et tourne contre PG en CI ;
budget de compilation respecté ; benchmark d'exécution comparé à
`tokio-postgres` écrit à la main (écart cible : négligeable) et à sqlx.

### P6 — Fragments et macros d'expression

- Fragments nommés réutilisables (liste de colonnes, prédicats), typés dans
  leur contexte d'utilisation, avec source maps (mécanisme existant étendu).
- Une requête nommée utilisable comme type de référence par d'autres
  (vérification de compatibilité, jamais de cast forcé).
- Expansion visible (LSP) et éjectable (`pgweave build`), comme les macros
  actuelles.

**Sortie** : la liste de colonnes siren et les projections récurrentes de
Mesalia/Sherpa exprimées une seule fois.

### P7 — Migrations

- Diff sur le modèle complet D2 (schémas, extensions, types, domaines, enums
  y compris `ADD VALUE`, fonctions/procédures avec ordre de dépendances,
  triggers, generated, checks, exclusions, policies, privilèges).
- Émission automatique de `CREATE/ALTER/DROP EXTENSION` et `CREATE/DROP
  SCHEMA` (D9, D10, D11) ; en-tête de contexte sur chaque migration.
- Règles de sûreté étendues (existant M001–M003).
- Vérification par oracle : migration appliquée sur PG → catalogue
  introspecté == schéma déclaratif.

**Sortie** : une évolution réelle du schéma Mesalia produite par `pgweave
migrate create` et validée par l'oracle, dont un ajout d'extension.

### P8 — TypeScript

- Extraction des requêtes dans les fichiers TS (parser oxc), descripteurs →
  `.d.ts`, plugin Vite + mode watch, runtime fin sur `pg`/`postgres`.
- Contrainte à assumer : un *tagged template* perd le type littéral de sa
  chaîne en TypeScript. Le typage passe donc par des surcharges générées
  indexées par la chaîne exacte (modèle du « client preset » de
  graphql-codegen) ou par un plugin du language service — choix à trancher
  en ouverture de phase.
- Reprise ou remplacement de `npm/pgweave` et des bindings.

**Sortie** : l'exemple `getPage` de la DX cible typé de bout en bout, sans
cast.

### Parallélisation possible

- P1 (parser) et le générateur de catalogue de P2 sont indépendants.
- P6 peut démarrer dès P4.
- P7 peut démarrer dès P3 (le diff ne dépend pas du descripteur).

### Trajectoire Mesalia (transition sans raccourci)

La brique gRPC démarre avant que P5 soit livrée. Plutôt que d'accélérer les
phases au prix de leur robustesse, on organise la transition pour que le
code écrit d'ici là **migre mécaniquement** vers le code généré :

1. **Dès le démarrage de la brique** : requêtes écrites dans des fichiers
   `queries/*.sql` au format des requêtes nommées (P4) et avec la syntaxe
   `$name` (D6) ; exécution via `tokio-postgres` et une fine couche écrite à
   la main qui respecte la forme du futur runtime (une fonction par requête,
   struct de paramètres, struct de ligne). `pgweave check` (existant) vérifie
   déjà l'existence des tables/colonnes en CI.
2. **Le format des requêtes nommées est donc figé en P0**, pas en P4 — c'est
   la seule avance de phase demandée par Mesalia.
3. **Prisma reste propriétaire des migrations** jusqu'à la fin de P2 (import
   relu). Ensuite, plus aucune migration Prisma : les nouvelles migrations
   sont écrites en SQL dans le dépôt, vérifiées par `pgweave check`, puis
   générées par `pgweave migrate create` à partir de P7.
4. **À la livraison de P5** : la couche manuelle est remplacée requête par
   requête par le code généré ; la compilation signale chaque écart de type
   ou de nullabilité que la couche manuelle masquait.

---

## 9. Organisation cible des crates

```
crates/
  pgweave-ast/          AST syntaxique, spans, IDs déterministes
  pgweave-parser/       PgsqlDialect + PL/pgSQL
  pgweave-macro/        macros, fragments, source maps
  pgweave-catalog/      modèle d'objets, couches, search_path, données générées
  pgweave-project/      base salsa : fichiers, révisions, requêtes mémoïsées
  pgweave-sema/         normalisation, résolution, types, nullabilité → HIR
  pgweave-checker/      règles de correction sur le HIR
  pgweave-lint/         règles de style / perf / sûreté de migration
  pgweave-query/        descripteur de requête, cache
  pgweave-codegen/      émission SQL, diff de migrations
  pgweave-diagnostics/  diagnostics, directives
  pgweave-lsp/  pgweave-cli/
rust/
  pgweave-runtime/      exécution tokio-postgres
  pgweave-macros/       proc-macros
contributors/         packs générés (D9) : catalogue + correspondances de types
  postgres-18/
  citext/  btree_gist/  postgis/
xtask/                catalog-gen, corpus, bench
```

`pgweave-analysis` (pipeline partagé CLI/LSP aujourd'hui) est absorbé par
`pgweave-project`.

---

## 10. Questions ouvertes

À trancher au plus tard à l'ouverture de la phase indiquée.

| Question | Phase |
|---|---|
| Quelles versions majeures PG générer et maintenir en packs | P2 |
| Date de bascule « plus de migration Prisma » pour Mesalia | P2 |
| `time` ou `chrono` par défaut | P5 |
| Surcharges générées vs plugin language service pour TS | P8 |
