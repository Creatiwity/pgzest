# Corpus `mesalia` — taux d'analyse

> Généré le 2026-11-01 par `cargo xtask corpus mesalia` (pgweave `8c70a08`).
> Fichiers retenus : Prisma : `*/migration.sql`, triés par répertoire horodaté.
> Agrégats uniquement : aucun identifiant du corpus n'apparaît ici.

## Lecture

| Mesure | Valeur |
|---|--:|
| Fichiers | 220 |
| Fichiers illisibles | 0 |
| Statements | 1740 |
| Analysés (dans l'AST) | 1740 (100 %) |
| Écartés : illisibles pour sqlparser | 0 |
| Écartés : non modélisés | 0 |
| Fonctions et procédures | 28 |
| Corps PL/pgSQL analysés | 27 |
| Corps `LANGUAGE sql` analysés | 1 |
| Corps non analysés | 0 |

## Diagnostics de `check`

Chaque migration est vérifiée contre l'index de **tout** le corpus (état final), comme le fait `pgweave check` aujourd'hui. Les migrations ayant été appliquées en production, une erreur est a priori un faux positif.

Fichiers avec au moins une erreur : 1/220. Statements absents de l'index : 0.

| Sévérité | Code | Nombre |
|---|---|--:|
| error | `column-not-found` | 10 |
| warning | `function-not-found` | 53 |

## Rejeu sur PostgreSQL

PostgreSQL 18 : 220/220 migrations appliquées sur une base vierge.

## Temps (indicatif, dépend de la machine)

| Étape | Durée |
|---|--:|
| Parse de tous les fichiers | 142 ms |
| Construction de l'index (`SchemaResolver`) | 444 ms |
| Vérification de tous les fichiers | 10 ms |
