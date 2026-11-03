# Requêtes nommées — format de fichier, v1

> Statut : **figé** (P0, 2026-11-01). Les évolutions seront additives.
> Implémentation de référence : `crates/pgweave-query`.

Une requête nommée est un statement SQL, écrit dans un fichier `.sql` du
répertoire `[queries] directory` (voir [`config.md`](config.md)), précédé
d'un en-tête qui lui donne un nom. C'est l'unité à partir de laquelle seront
générés les appels typés (Rust en P5, TypeScript en P8) : une fonction par
requête, une struct de paramètres, une struct de ligne.

`pgweave check` et l'éditeur vérifient le format de ces fichiers, chaque
requête contre le schéma, et ce qui empêcherait de la décrire (paramètre
non typé, cardinalité contredite). `pgweave describe` produit les
descripteurs ; leur usage en Rust est décrit dans [`rust.md`](rust.md).

## Exemple

```sql
-- queries/pages.sql
-- Les commentaires avant le premier en-tête documentent le fichier.

-- name: page_by_id :optional
-- Une page par son identifiant.
SELECT id, title, creation_date
FROM pages
WHERE id = $id;

-- name: list_recent
SELECT id, title
FROM pages
ORDER BY creation_date DESC
LIMIT $limit;

-- name: rename_page :one
UPDATE pages
SET title = $title
WHERE id = $id
RETURNING id, title;
```

## Grammaire

```
fichier      := préambule? requête*
préambule    := (commentaire | ligne vide)*
requête      := en-tête doc* statement
en-tête      := "-- name:" nom cardinalité? référence? FIN_DE_LIGNE  (en début de ligne)
nom          := [a-z_][a-z0-9_]*
cardinalité  := ":one" | ":optional" | ":many" | ":none"
référence    := "->" (module "::")* nom     (ajouté en P6, 2026-11-03)
doc          := "--" texte FIN_DE_LIGNE                       (avant le statement)
statement    := un seul statement SQL, ";" final optionnel
```

### En-tête

- Commence la ligne : `-- name:` en fin de ligne de code n'est pas un en-tête
  (et c'est une erreur, pour ne pas le perdre en silence).
- Les espaces autour de `name:` sont libres ; `-- name: x` est la forme
  canonique.
- Un `-- name:` dans une chaîne ou un corps `$$…$$` n'est jamais un en-tête :
  le découpage s'appuie sur le tokenizer SQL, pas sur les lignes.

### Nom

- `snake_case` ASCII : `[a-z_][a-z0-9_]*`. Utilisable tel quel comme
  identifiant Rust, convertible sans perte en `camelCase` pour TypeScript.
- Unique dans le fichier. Le **module** est le chemin du fichier relatif à
  son répertoire de requêtes, sans extension : `queries/admin/pages.sql` →
  `admin::pages` en Rust (`queries::admin::pages::page_by_id`). Deux
  fichiers peuvent donc définir le même nom. Chaque segment doit faire un
  nom de module Rust (snake_case).

### Cardinalité

Nombre de lignes renvoyées :

| Marqueur | Sens | Appel généré (P5, indicatif) |
|---|---|---|
| `:one` | exactement une ligne | `fetch_one` → `Row` |
| `:optional` | zéro ou une ligne | `fetch_optional` → `Option<Row>` |
| `:many` | zéro, une ou plusieurs | `fetch_all` / flux → `Vec<Row>` |
| `:none` | pas de ligne (sans `RETURNING`) | `execute` → nombre de lignes affectées |

Le marqueur est **optionnel**. À partir de P4, la cardinalité est inférée
(égalité sur une clé unique → `:optional`, etc. — PLAN.md §4) :

- sans marqueur, l'inférence fait foi ;
- avec marqueur, il est vérifié contre l'inférence : il peut **resserrer**
  une inférence prudente (`:one` là où l'analyse voit `:optional`, quand
  l'appelant sait qu'une ligne existe) mais pas la contredire (`:none` sur
  un `SELECT` est une erreur).

D'ici P4, le marqueur est lu et validé (valeur connue) mais pas encore
confronté à l'inférence.

### Documentation

Les lignes `--` entre l'en-tête et le statement sont la documentation de la
requête, reprise dans le code généré.

### Statement

- **Un seul** statement par requête ; le `;` final est optionnel.
- Tout SQL hors d'une requête (avant le premier en-tête, ou un second
  statement sous un même en-tête) est une erreur.
- Un fichier de requêtes est un `.sql`. Il peut insérer les **fragments**
  du projet (`@nom`, voir plus bas) ; les macros de statement (`@function`…)
  n'y ont pas leur place.

## Requêtes à variantes

Quand le plan doit dépendre des filtres présents, ou que le tri varie,
blocs conditionnels et choix (ajoutés le 2026-11-03) :

```sql
-- name: orders_page :many
SELECT o.id, o.status, o.placed_at
FROM orders o
WHERE o.restaurant_id = $restaurant
[[ AND o.status = $status ]]
[[ AND o.placed_at >= $since ]]
ORDER BY {{ tri: recent = o.placed_at DESC NULLS LAST, o.id DESC | ancien = o.id }}
LIMIT $limit;
```

- `[[ … ]]` : inclus si son paramètre est fourni (`Option<T>` en Rust). Un
  bloc dépend d'**un seul** paramètre, utilisé seulement dans des blocs,
  sans `?` ; un filtre à deux valeurs s'écrit en deux blocs.
- `{{ nom: a = … | b = … }}` : un choix, enum Rust (`OrdersPageTri::Recent`).
  Une possibilité ne contient pas de paramètre.
- Chaque combinaison est une **variante**, décrite à la compilation (au
  plus 256) ; toutes doivent renvoyer les mêmes colonnes. Une erreur d'une
  variante est rapportée à sa position, la variante en indice.
- À l'exécution, la fonction générée choisit la variante par un `match`
  exhaustif ; aucun SQL n'est composé.
- `[[` après une expression (indice ou constructeur de tableau, `ARRAY[[`)
  n'ouvre pas de bloc ; rien n'est reconnu dans une chaîne ou un
  commentaire. Pas de bloc dans un bloc, ni de choix dans un bloc.

## Fragments

Un morceau de SQL qui revient (liste de colonnes, expression, prédicat)
s'écrit une fois, dans un `.sql` de `[macros] directory`, et s'insère par
`@nom` :

```sql
-- macros/pages.sql
-- fragment: page_card(p)
-- La fiche d'une page, telle que l'affichent les listes.
p.id, p.title, p.status

-- fragment: published(p)
p.status = 'live' AND p.published_on <= current_date
```

```sql
-- name: recent_pages :many
SELECT @page_card(x) FROM pages x WHERE @published(x) ORDER BY x.id DESC;
-- analysé et envoyé :
SELECT x.id, x.title, x.status FROM pages x
WHERE (x.status = 'live' AND x.published_on <= current_date) ORDER BY x.id DESC
```

- Les paramètres d'un fragment sont des **qualificatifs** : `@page_card(x)`
  remplace `p.` par `x.`, `@page_card` le retire (`id, title, status`).
  Rien d'autre n'est substitué.
- Un corps qui est une expression non atomique est inséré entre
  parenthèses (l'opérateur voisin ne peut pas le découper) ; une liste, un
  élément aliasé ou un atome est inséré tel quel.
- Un fragment peut contenir des paramètres (`$status`) : ils deviennent
  ceux de la requête, dans l'ordre de première apparition **après**
  insertion.
- Le fragment est vérifié là où il est inséré ; une erreur qui vient de son
  texte est rapportée à l'invocation (`@page_card(x)`).
- Les fragments servent aussi dans le schéma (`CREATE INDEX … ((@expr))`) :
  l'expression d'un index et celle des requêtes qui doivent l'utiliser ne
  peuvent plus diverger.

## Lignes de référence

`-> autre` déclare que la requête renvoie des lignes de la même forme que
celles de la requête `autre` du même fichier ; `-> admin::pages::autre`
désigne celle d'un autre fichier, par son module depuis la racine du
répertoire de requêtes. Le code généré reprend sa struct de ligne
(`AutreRow`, ou `super::admin::pages::AutreRow`) au lieu d'en créer une.

```sql
-- name: page_card :optional
SELECT @page_card(p) FROM pages p WHERE p.id = $id;

-- name: pages_of_author :many -> page_card
SELECT @page_card(p) FROM pages p WHERE p.author = $author;
```

Vérifié, jamais forcé : mêmes colonnes dans le même ordre, du même type ;
une colonne non nulle dans la référence doit l'être aussi ici (une colonne
nullable dans la référence accepte une valeur toujours présente). La
référence est une requête du même répertoire de requêtes, qui ne reprend
pas elle-même les lignes d'une autre.

## Paramètres

Deux notations, **jamais mélangées** dans une même requête :

| Notation | Exemple | Recommandée |
|---|---|---|
| Nommée | `$page_id` | oui |
| Positionnelle | `$1`, `$2` | pour du SQL existant |

Paramètres nommés :

- même règle que les noms de requête : `snake_case` ASCII (`$page_id`, pas
  `$pageId`) ;
- un même nom utilisé plusieurs fois est **un seul** paramètre ;
- l'ordre des paramètres est l'ordre de **première apparition** ;
- `$name` immédiatement suivi de `$` est un début de chaîne dollar
  (`$tag$…$tag$`), pas un paramètre — règle de PostgreSQL ;
- un `$name` dans une chaîne ou un commentaire n'est pas un paramètre.

Paramètres positionnels : de `$1` à `$n` sans trou.

### Paramètre optionnel : `$nom?`

Un `?` collé au nom (ajouté le 2026-11-03) déclare que la valeur peut
manquer : le paramètre est nullable dans le descripteur, `Option<T>` en
Rust, quoi que déduise l'analyse. La requête est écrite pour :

```sql
-- name: orders_of_restaurant :many
SELECT o.id, o.status FROM orders o
WHERE o.restaurant_id = $restaurant
  AND ($status?::order_status IS NULL OR o.status = $status);
```

- Le marqueur sur une occurrence suffit ; il est retiré du SQL envoyé
  (`$2::order_status IS NULL OR o.status = $2`).
- Une seule requête préparée : un filtre absent est une condition vraie.
  Quand le plan doit dépendre des filtres présents, voir les
  [requêtes à variantes](#requêtes-à-variantes).
- L'opérateur jsonb `?` s'écrit entouré d'espaces (`$doc ? 'clé'`) ; un
  paramètre positionnel (`$1?`) n'est jamais optionnel.

### Réécriture

Le SQL envoyé au serveur est toujours positionnel. Chaque `$name` est
remplacé par `$n`, `n` étant son rang de première apparition :

```sql
UPDATE pages SET title = $title WHERE id = $id
-- envoyé :
UPDATE pages SET title = $1 WHERE id = $2
```

Le reste du texte est conservé à l'octet près (casts `$id::bigint`,
commentaires, mise en forme).

## Diagnostics

| Code | Cas |
|---|---|
| `query-header-invalid` | nom invalide, cardinalité inconnue, texte en trop, en-tête hors début de ligne |
| `query-without-name` | SQL hors d'une requête nommée (signalé une fois par fichier) |
| `query-empty` | en-tête sans statement |
| `query-multiple-statements` | plus d'un statement sous un en-tête |
| `query-duplicate-name` | nom déjà utilisé dans le fichier (la première définition est gardée) |
| `query-mixed-parameters` | `$nom` et `$n` dans une même requête |
| `query-parameter-gap` | `$1`, `$3` sans `$2` |
| `query-parameter-invalid` | `$0`, `$pageId` |
| `query-file-extension` | fichier de requêtes qui n'est pas un `.sql` |
| `parse-error` | fichier que le tokenizer SQL ne sait pas lire (chaîne non terminée…) |
| `macro-error` | fragment inconnu en position de macro, arguments invalides, fragments récursifs |
| `query-row-mismatch` | `-> autre` : référence introuvable ou lignes incompatibles |

Ils s'ajoutent aux diagnostics habituels (`column-not-found`, …) et suivent
`[rules]` et les directives `-- pgweave-disable…` comme tous les autres.

## Écrire du code appelant avant P5 (trajectoire Mesalia)

Tant que la génération Rust (P5) n'existe pas, le code appelant s'écrit à la
main. Pour que la bascule vers le code généré soit mécanique, il reprend la
forme qui sera générée :

- un module par fichier de requêtes, une fonction par requête, du même nom ;
- une struct de paramètres (champs = paramètres nommés, dans l'ordre de
  première apparition) et une struct de ligne (champs = colonnes, dans
  l'ordre du `SELECT`/`RETURNING`) ;
- le SQL est **lu depuis le fichier** de requêtes (pas recopié dans le code
  Rust), réécrit en positionnel avec `pgweave_query`, puis exécuté via
  `tokio-postgres` ;
- la fonction d'appel suit la cardinalité (`Option<Row>` pour `:optional`…).

```rust
// Forme cible ; à la livraison de P5 ce module est remplacé par le code
// généré, et le compilateur signale tout écart de type ou de nullabilité.
pub mod pages {
    pub struct PageByIdParams { pub id: i64 }
    pub struct PageByIdRow { pub id: i64, pub title: String, pub creation_date: Option<time::OffsetDateTime> }

    pub async fn page_by_id(
        client: &impl tokio_postgres::GenericClient,
        params: &PageByIdParams,
    ) -> Result<Option<PageByIdRow>, tokio_postgres::Error> {
        /* … */
    }
}
```

`pgweave check queries/*.sql` en CI garantit dès maintenant que ces requêtes
respectent le format et référencent des tables et colonnes existantes.
