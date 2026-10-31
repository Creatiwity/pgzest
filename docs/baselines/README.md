# Rapports de référence (P0)

Deux mesures de départ, à refaire après chaque phase pour suivre les
progrès. Les chiffres ci-dessous datent du 2026-11-01 (commit `62f1e61`).

| Rapport | Ce qu'il mesure | Comment le régénérer |
|---|---|---|
| [`crates/pgweave-oracle/tests/snapshots/oracle__rapport.snap`](../../crates/pgweave-oracle/tests/snapshots/oracle__rapport.snap) | Types des paramètres et des colonnes, confrontés au `Prepare` de PostgreSQL 18, sur le corpus public `crates/pgweave-oracle/cases` | `make oracle` (vérifie), `make oracle-accept` (met à jour) |
| [`mesalia.md`](mesalia.md) | Taux d'analyse et faux positifs sur les 220 migrations Prisma de Mesalia (corpus privé, agrégats sans noms) | `PGWEAVE_CORPUS_MESALIA=… cargo xtask corpus mesalia --layout prisma --out docs/baselines/mesalia.md` (avec `make oracle-up` pour le rejeu sur PG 18) |

## Oracle : ce que pgweave affirme vs PostgreSQL 18

- **Colonnes : 73/112 conformes (65 %). Paramètres : 0/29 (0 %)** — aucune
  inférence de type de paramètre n'existe.
- **Repli silencieux sur `text`** : quand le type d'une colonne n'est pas
  inféré, `pgweave-semantic` affirme `text` (`coalesce` numérique, `max` d'un
  `timestamptz`, `array_length`, sous-requête scalaire, agrégat dans une
  CTE…). C'est exactement ce que le principe « ne jamais deviner » interdit :
  un générateur produirait un type faux sans le savoir. À remplacer par un
  inconnu explicite (P3).
- **Littéraux et arithmétique** : `1.5` est `numeric` pour PostgreSQL (pas
  `float8`), `int * numeric` est `numeric`, `sum(int4)` est `int8` — les
  règles de résolution de PostgreSQL ne sont pas suivies (P3, D1 du système
  de types).
- **Syntaxe PG 18 inconnue de sqlparser** : `GENERATED ALWAYS AS (…)
  VIRTUAL` fait écarter **toute** la table, dont toutes les colonnes passent
  ensuite en `text` (P1).
- **Non modélisés** : `CREATE TYPE … AS ENUM`, `CREATE DOMAIN`,
  `CREATE EXTENSION`, `UNION`, `CALL`, colonnes de `RETURNING` (P1–P3).
- **Réécriture textuelle** : le `CREATE DOMAIN … CHECK (VALUE > 0)` est cité
  avec `CHECK (true)` — une réécriture avant parse a altéré le statement
  (dette D1).
- **Règle PostgreSQL 18 découverte** : une expression de colonne générée
  `VIRTUAL` ne peut pas utiliser de type utilisateur (domaine, enum…) ; à
  porter dans le checker (P3).
- **Corrigé pendant P0** : l'expansion de `SELECT *` suivait l'ordre d'une
  `HashMap` (résultat différent à chaque exécution, colonnes en double) et
  `t.*` ne produisait rien.

## Mesalia : taux d'analyse et faux positifs

- **Corpus valide** : les 220 migrations s'appliquent sur un PostgreSQL 18
  vierge. Tout diagnostic d'erreur de `check` est donc un faux positif.
- **87 % des statements analysés** (1519/1740). Les écarts se concentrent
  sur peu de constructions :
  - **enums** : 116 statements (`CREATE TYPE … AS ENUM` ×75, `ALTER TYPE …
    ADD VALUE` ×24, `DROP TYPE` ×9, `ALTER TYPE … RENAME` ×8) — central pour
    Mesalia, priorité P1/P2 ;
  - **suppressions non modélisées** (`DROP INDEX/TABLE/TRIGGER/FUNCTION/
    VIEW`, ~64) : l'index continue de croire à des objets supprimés ;
  - **vues** (11 créations), **blocs `DO`** (10), **contraintes `EXCLUDE
    USING gist`** (4, extension `btree_gist`), `BEGIN`/`COMMIT`/`SET`.
- **Fonctions : 17 corps sur 28 non analysés.** Causes : `DROP TABLE` dans
  un corps (8), statement `WITH …` (CTE) dans un corps (7), `CASE … WHEN`
  dans une expression (2).
- **Faux positifs : 27 erreurs `table-not-found`, toutes dans 2 migrations**
  dont les fonctions de trigger retombent sur le repli textuel. Les « tables
  inconnues » sont :
  - des **tables temporaires** créées dans le corps
    (`CREATE TEMP TABLE IF NOT EXISTS …`) ;
  - des **tables de transition** de triggers niveau statement
    (`REFERENCING OLD TABLE AS … NEW TABLE AS …`).
  C'est exactement le contexte « tables temporaires et variables propres à
  une procédure » du catalogue à portées (D2) : à couvrir en P1 (parse) et
  P2 (portée routine).
- **27 avertissements `function-not-found`** : uniquement des fonctions
  intégrées à PostgreSQL absentes du catalogue écrit à la main
  (`pg_trigger_depth` ×26, `normalize` ×1). Confirme D9 : le catalogue
  intégré doit être généré depuis un vrai PostgreSQL.
- **Temps** : parse 176 ms, construction de l'index 307 ms, vérification
  22 ms pour 220 fichiers. Référence pour le critère du spike salsa
  (surcoût d'indexation < 2×).

## Priorités qui en découlent pour P1

Par impact sur Mesalia, à coût comparable :

1. Enums (`CREATE/ALTER/DROP TYPE`) et `DROP …` modélisés.
2. Corps PL/pgSQL : `DROP TABLE`, statements `WITH`, `CASE … WHEN` — puis
   suppression du repli textuel.
3. Tables temporaires et tables de transition (`REFERENCING`) dans la portée
   de la routine.
4. `GENERATED … VIRTUAL`, `EXCLUDE USING`, vues, `DO`.
