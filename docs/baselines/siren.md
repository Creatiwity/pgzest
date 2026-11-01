# Corpus `siren` — taux d'analyse

> Généré le 2026-11-01 par `cargo xtask corpus siren` (pgweave `ae04fa0`).
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

Chaque migration est vérifiée contre le catalogue rejoué jusqu'à elle (incluse), catalogue `postgres@18` et extensions compris. Les migrations ayant été appliquées en production, une erreur est a priori un faux positif.

Fichiers avec au moins une erreur : 0/21. Statements absents de l'index : 0.

| Sévérité | Code | Nombre |
|---|---|--:|
| warning | `trigger-no-context` | 1 |

## Rejeu sur PostgreSQL

PostgreSQL 18 : 21/21 migrations appliquées sur une base vierge.

## Catalogue reconstruit par pgweave

Rejeu des migrations dans l'ordre sur le catalogue `postgres@18` (packs d'extension compris).

| Diagnostic du rejeu | Nombre |
|---|--:|
| `ddl-not-replayed` | 4 |

Comparaison au catalogue réel (objets du projet, hors membres d'extensions ; expressions comparées par leur présence) :

| Objets | Réels | Identiques | Non établis (P3) | Différents | Manquants | En trop |
|---|--:|--:|--:|--:|--:|--:|
| colonnes | 232 | 232 | 0 | 0 | 0 | 0 |
| contraintes | 12 | 12 | 0 | 0 | 0 | 0 |
| extensions | 5 | 5 | 0 | 0 | 0 | 0 |
| index | 45 | 45 | 0 | 0 | 0 | 0 |
| policies | 0 | 0 | 0 | 0 | 0 | 0 |
| relations | 12 | 12 | 0 | 0 | 0 | 0 |
| routines | 8 | 8 | 0 | 0 | 0 | 0 |
| schémas | 1 | 1 | 0 | 0 | 0 | 0 |
| séquences | 3 | 3 | 0 | 0 | 0 | 0 |
| triggers | 2 | 0 | 0 | 0 | 2 | 0 |
| types | 12 | 12 | 0 | 0 | 0 | 0 |

## Import (schéma déclaratif)

Le catalogue reconstruit, exporté en SQL (`pgweave_catalog::dump`) et appliqué à une base vierge, comparé au catalogue réel des migrations :

| Objets | Réels | Identiques | Différents | Manquants | En trop |
|---|--:|--:|--:|--:|--:|
| colonnes | 232 | 232 | 0 | 0 | 0 |
| contraintes | 12 | 12 | 0 | 0 | 0 |
| extensions | 5 | 5 | 0 | 0 | 0 |
| index | 45 | 45 | 0 | 0 | 0 |
| policies | 0 | 0 | 0 | 0 | 0 |
| relations | 12 | 12 | 0 | 0 | 0 |
| routines | 8 | 8 | 0 | 0 | 0 |
| schémas | 1 | 1 | 0 | 0 | 0 |
| séquences | 3 | 3 | 0 | 0 | 0 |
| triggers | 2 | 0 | 0 | 2 | 0 |
| types | 12 | 12 | 0 | 0 | 0 |

## Temps (indicatif, dépend de la machine)

| Étape | Durée |
|---|--:|
| Parse de tous les fichiers | 29 ms |
| Vérification de tous les fichiers | 63 ms |
| Rejeu sur le catalogue (`pgweave-catalog`) | 39 ms |
