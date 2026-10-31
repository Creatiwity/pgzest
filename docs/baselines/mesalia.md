# Corpus `mesalia` — taux d'analyse

> Généré le 2026-11-01 par `cargo xtask corpus mesalia` (pgweave `62f1e61`).
> Fichiers retenus : Prisma : `*/migration.sql`, triés par répertoire horodaté.
> Agrégats uniquement : aucun identifiant du corpus n'apparaît ici.

## Lecture

| Mesure | Valeur |
|---|--:|
| Fichiers | 220 |
| Fichiers illisibles | 0 |
| Statements | 1740 |
| Analysés (dans l'AST) | 1519 (87 %) |
| Écartés : illisibles pour sqlparser | 14 |
| Écartés : non modélisés | 207 |
| Fonctions et procédures | 28 |
| Corps PL/pgSQL analysés | 10 |
| Corps `LANGUAGE sql` analysés | 1 |
| Corps non analysés | 17 |

## Statements illisibles, par forme

| Forme | Nombre |
|---|--:|
| `DO …` | 10 |
| `ALTER TABLE … ADD CONSTRAINT … EXCLUDE USING …` | 4 |

## Statements non modélisés, par forme

| Forme | Nombre |
|---|--:|
| `CREATE TYPE … AS ENUM …` | 75 |
| `DROP INDEX …` | 28 |
| `ALTER TYPE … ADD VALUE …` | 24 |
| `DROP TABLE …` | 15 |
| `DROP TRIGGER IF EXISTS … ON …` | 10 |
| `DROP TYPE …` | 9 |
| `ALTER TYPE … RENAME TO …` | 8 |
| `CREATE OR REPLACE VIEW … AS SELECT DISTINCT …` | 8 |
| `DROP VIEW …` | 8 |
| `ALTER TABLE … RENAME COLUMN … TO …` | 4 |
| `DROP FUNCTION IF EXISTS …` | 4 |
| `BEGIN` | 3 |
| `COMMIT` | 3 |
| `CREATE VIEW … AS SELECT DISTINCT ON …` | 3 |
| `ALTER TABLE … VALIDATE CONSTRAINT …` | 1 |
| `CREATE EXTENSION IF NOT EXISTS …` | 1 |
| `SET …` | 1 |
| `TRUNCATE TABLE … CASCADE` | 1 |
| `UPDATE … SET … COUNT FROM SELECT …` | 1 |

## Diagnostics de `check`

Chaque migration est vérifiée contre l'index de **tout** le corpus (état final), comme le fait `pgweave check` aujourd'hui. Les migrations ayant été appliquées en production, une erreur est a priori un faux positif.

Fichiers avec au moins une erreur : 2/220. Statements absents de l'index : 238.

| Sévérité | Code | Nombre |
|---|---|--:|
| error | `table-not-found` | 27 |
| warning | `function-not-found` | 27 |

## Rejeu sur PostgreSQL

PostgreSQL 18 : 220/220 migrations appliquées sur une base vierge.

## Temps (indicatif, dépend de la machine)

| Étape | Durée |
|---|--:|
| Parse de tous les fichiers | 176 ms |
| Construction de l'index (`SchemaResolver`) | 307 ms |
| Vérification de tous les fichiers | 22 ms |
