# Corpus `siren` — taux d'analyse

> Généré le 2026-11-01 par `cargo xtask corpus siren` (pgweave `ea3952e`).
> Fichiers retenus : Diesel : `*/up.sql`, triés par répertoire horodaté.
> Agrégats uniquement : aucun identifiant du corpus n'apparaît ici.

## Lecture

| Mesure | Valeur |
|---|--:|
| Fichiers | 21 |
| Fichiers illisibles | 0 |
| Statements | 294 |
| Analysés (dans l'AST) | 294 (100 %) |
| Écartés : illisibles pour sqlparser | 0 |
| Écartés : non modélisés | 0 |
| Fonctions et procédures | 10 |
| Corps PL/pgSQL analysés | 5 |
| Corps `LANGUAGE sql` analysés | 5 |
| Corps non analysés | 0 |

## Diagnostics de `check`

Chaque migration est vérifiée contre l'index de **tout** le corpus (état final), comme le fait `pgweave check` aujourd'hui. Les migrations ayant été appliquées en production, une erreur est a priori un faux positif.

Fichiers avec au moins une erreur : 0/21. Statements absents de l'index : 0.

| Sévérité | Code | Nombre |
|---|---|--:|
| warning | `function-not-found` | 7 |
| warning | `trigger-no-context` | 1 |

## Rejeu sur PostgreSQL

PostgreSQL 18 : 14/21 migrations appliquées sur une base vierge.

Première migration refusée : n° 15 — « extension "…" is not available » (noms masqués).

## Temps (indicatif, dépend de la machine)

| Étape | Durée |
|---|--:|
| Parse de tous les fichiers | 28 ms |
| Construction de l'index (`SchemaResolver`) | 37 ms |
| Vérification de tous les fichiers | 0 ms |
