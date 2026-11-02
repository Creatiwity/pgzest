# Corpus `mesalia` — taux d'analyse

> Généré le 2026-11-03 par `cargo xtask corpus mesalia` (pgweave `d49e938`).
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

Chaque migration est vérifiée contre le catalogue rejoué jusqu'à elle (incluse), catalogue `postgres@18` et extensions compris. Les migrations ayant été appliquées en production, une erreur est a priori un faux positif.

Fichiers avec au moins une erreur : 0/220. Statements absents de l'index : 0.

Aucun diagnostic.

## Rejeu sur PostgreSQL

PostgreSQL 18 : 220/220 migrations appliquées sur une base vierge.

## Catalogue reconstruit par pgweave

Rejeu des migrations dans l'ordre sur le catalogue `postgres@18` (packs d'extension compris).

Aucun diagnostic de rejeu.

Comparaison au catalogue réel (objets du projet, hors membres d'extensions ; expressions comparées par leur présence) :

| Objets | Réels | Identiques | Non établis (P3) | Différents | Manquants | En trop |
|---|--:|--:|--:|--:|--:|--:|
| colonnes | 1622 | 1622 | 0 | 0 | 0 | 0 |
| contraintes | 488 | 488 | 0 | 0 | 0 | 0 |
| extensions | 2 | 2 | 0 | 0 | 0 | 0 |
| index | 405 | 405 | 0 | 0 | 0 | 0 |
| policies | 0 | 0 | 0 | 0 | 0 | 0 |
| relations | 122 | 122 | 0 | 0 | 0 | 0 |
| routines | 9 | 9 | 0 | 0 | 0 | 0 |
| schémas | 1 | 1 | 0 | 0 | 0 | 0 |
| séquences | 2 | 2 | 0 | 0 | 0 | 0 |
| triggers | 14 | 14 | 0 | 0 | 0 | 0 |
| types | 254 | 254 | 0 | 0 | 0 | 0 |

## Import (schéma déclaratif)

Le catalogue reconstruit, exporté en SQL (`pgweave_catalog::dump`) et appliqué à une base vierge, comparé au catalogue réel des migrations :

| Objets | Réels | Identiques | Différents | Manquants | En trop |
|---|--:|--:|--:|--:|--:|
| colonnes | 1622 | 1622 | 0 | 0 | 0 |
| contraintes | 488 | 488 | 0 | 0 | 0 |
| extensions | 2 | 2 | 0 | 0 | 0 |
| index | 405 | 405 | 0 | 0 | 0 |
| policies | 0 | 0 | 0 | 0 | 0 |
| relations | 122 | 122 | 0 | 0 | 0 |
| routines | 9 | 9 | 0 | 0 | 0 |
| schémas | 1 | 1 | 0 | 0 | 0 |
| séquences | 2 | 2 | 0 | 0 | 0 |
| triggers | 14 | 14 | 0 | 0 | 0 |
| types | 254 | 254 | 0 | 0 | 0 |

Au texte près, selon PostgreSQL (`format_type`, `pg_get_*def` : défauts, index, vues, corps de routines ; oracle des migrations de P7, noms des contraintes NOT NULL exceptés), 3864 ligne(s) de description comparée(s) : bases identiques.

## Requêtes générées (oracle)

Sondes tirées du catalogue reconstruit, préparées par PostgreSQL sur la base des migrations et décrites par `pgweave-sema` (types des paramètres, noms et types des colonnes ; un refus des deux côtés compte comme accord). Les requêtes des corps de routines en vigueur sont préparées avec leurs variables en paramètres typés (`PREPARE`), comme PL/pgSQL les exécute ; seuls les types des colonnes sont comparés :

| Sonde | Total | Identiques |
|---|--:|--:|
| INSERT … VALUES ($1) | 1598 | 1598 |
| SELECT * | 122 | 122 |
| WHERE colonne = $1 | 1622 | 1622 |
| requête de corps de routine | 58 | 58 |

Requêtes de corps non sondées (non analysées par pgweave, ou variable sans position dans l'AST) : 0.

## Temps (indicatif, dépend de la machine)

| Étape | Durée |
|---|--:|
| Parse de tous les fichiers | 143 ms |
| Vérification de tous les fichiers | 618 ms |
| Rejeu sur le catalogue (`pgweave-catalog`) | 78 ms |
