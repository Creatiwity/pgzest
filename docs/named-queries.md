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
en-tête      := "-- name:" nom cardinalité? FIN_DE_LIGNE      (en début de ligne)
nom          := [a-z_][a-z0-9_]*
cardinalité  := ":one" | ":optional" | ":many" | ":none"
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
  `[queries] directory`, sans extension : `queries/admin/pages.sql` →
  `admin::pages` en Rust. Deux fichiers peuvent donc définir le même nom.

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
- Les macros (`.pgsql`) ne sont pas encore acceptées dans les requêtes
  nommées : un fichier de requêtes est un `.sql`. Les fragments réutilisables
  arrivent en P6.

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
