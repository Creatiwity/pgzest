# Requêtes typées en Rust

> Statut : **utilisable** (P5 en cours, 2026-11-02). Exemple complet :
> [`examples/rust-service`](../examples/rust-service), testé contre
> PostgreSQL 18 en CI.

Les requêtes SQL d'un projet deviennent des fonctions Rust typées, à la
compilation et **sans connexion à une base**. Les types viennent du
catalogue que pgweave reconstruit en rejouant le schéma et les migrations
du projet (`pgweave.toml`). L'exécution passe par `tokio-postgres`.

Deux façons d'écrire une requête :

- **requête nommée** dans `queries/**/*.sql`, compilée par la macro
  `queries!` ou par `build.rs` : c'est la forme principale (enums générés,
  `COPY`, fragments, lignes partagées), un module Rust par fichier ;
- **`sql!`**, une requête définie dans le code Rust : même génération
  qu'une requête nommée, la chaîne ne contenant que le SQL ;
- **`query!`**, en ligne dans une expression, pour une requête ponctuelle.

## Mise en place

```toml
# pgweave.toml
[postgres]
version = 18

[schema]
directory = "schema"        # ou [migrations] directory = "migrations"

[queries]
directory = "queries"
```

```toml
# Cargo.toml
[dependencies]
pgweave-runtime = { path = "…/crates/pgweave-runtime" }
pgweave-macros  = { path = "…/crates/pgweave-macros" }   # queries!, query!
tokio-postgres  = { version = "0.7", features = ["with-time-0_3", "with-uuid-1"] }
time = "0.3"      # dates (ou chrono, voir plus bas)
uuid = "1"        # si le schéma a des uuid
```

```rust
// src/queries.rs — tout le contenu est généré à la compilation
pgweave_macros::queries!("queries");
```

```rust
// src/lib.rs
pub mod queries;
// queries/order/book.sql, requête `get` → crate::queries::order::book::get
```

`queries!` prend un répertoire (un module par fichier et par
sous-répertoire, les enums PostgreSQL à la racine) ou un fichier (ses
requêtes à l'endroit de l'appel). Des requêtes peuvent vivre près du code
qui les utilise : déclarez leur répertoire dans `[queries] include`, puis
`pgweave_macros::queries!("src/order/queries");` dans `src/order/mod.rs`.
Chaque fichier lu est surveillé ; un fichier **ajouté** à un répertoire
n'est vu qu'à la recompilation du module qui appelle la macro.

### Requêtes dans le code : `sql!`

```rust
// src/carte.rs
pgweave_macros::sql! {
    /// Les plats d'un restaurant, du moins cher au plus cher.
    pub dishes_by_price: many -> crate::queries::menu::DishCardRow,
    r#"SELECT @dish_card(d) FROM dishes d
       WHERE d.restaurant_id = $restaurant ORDER BY d.price"#
}
// crate::carte::dishes_by_price(&client, DishesByPriceParams { restaurant })
```

- Une requête par macro ; nom, cardinalité (optionnelle, inférée sinon),
  documentation et visibilité sont des paramètres, la chaîne ne contient que
  le SQL (fragments `@nom` compris).
- `-> Chemin` reprend la struct de ligne d'une autre requête, `sql!` ou
  fichier, par son chemin Rust. La compatibilité (noms, ordre, types,
  nullabilité jamais relâchée) est vérifiée à la compilation, par des
  assertions constantes : `` `price` peut être nulle ici, pas dans
  crate::queries::menu::DishCardRow ``.
- Modifier la chaîne relance l'analyse immédiatement, dans l'éditeur aussi
  (l'entrée de la macro change) ; « aller à la définition » mène au nom
  écrit dans la macro.

Dans VS Code, l'extension pgweave traite la chaîne comme un fichier de
requêtes : coloration SQL (`$param`, `@fragment` compris), diagnostics
positionnés dans la chaîne, survol (descripteur sur le nom, type d'une
colonne, fragment), complétion. Elle désactive les jetons sémantiques de
rust-analyzer sur les chaînes (`rust-analyzer.semanticHighlighting.strings.enable`),
qui recouvriraient la coloration. Hors d'un projet pgweave (`pgweave.toml`),
un `query!` homonyme (sqlx…) n'est pas analysé.

### Enums : `#[types]`

Une requête `sql!` qui utilise un enum PostgreSQL ne peut pas le définir
elle-même (deux requêtes du même module le définiraient deux fois). Les
enums du projet sont générés une fois, dans un module déclaré :

```toml
# pgweave.toml
[rust]
types = "crate::db"
```

```rust
// src/lib.rs
#[pgweave_macros::types]
pub mod db {}
// crate::db::DishKind, crate::db::OrderStatus…
```

Avec `[rust] types`, `queries!` et `build.rs` y renvoient aussi au lieu de
générer leurs propres enums.

### Rafraîchissement dans l'éditeur

rust-analyzer ne relance une macro que si son entrée change. Pour `sql!`
et `query!`, l'entrée est le SQL : toute modification est prise en compte
à la frappe. Pour `queries!("dossier")`, l'entrée est le chemin : une
modification d'un `.sql` n'est vue par rust-analyzer qu'au redémarrage du
serveur (`cargo check`, lui, la voit toujours). Un changement de schéma
est dans le même cas pour toutes les formes.

Variante sans macro, depuis `build.rs` (le répertoire `[queries] directory`
entier, y compris ses fichiers ajoutés, surveillés par Cargo) :

```rust
// build.rs  ([build-dependencies] pgweave-build)
fn main() {
    pgweave_build::Builder::new()
        .generate()
        .unwrap_or_else(|e| panic!("pgweave : {e}"));
}

// src/lib.rs
pub mod queries {
    include!(concat!(env!("OUT_DIR"), "/pgweave_queries.rs"));
}
```

Le schéma vient de fichiers déclaratifs (`[schema]`) ou des migrations
(`[migrations] directory`), rejouées dans l'ordre. Formats reconnus :
fichiers numérotés (`0001_init.sql`), Prisma
(`<horodatage>_<nom>/migration.sql`), Diesel (`<date>_<nom>/up.sql`, les
`down.sql` n'étant jamais appliqués). Un projet Prisma existant se branche
donc sans conversion :

```toml
[migrations]
directory = "packages/models/prisma/migrations"
```

Une requête que l'analyse ne décrit pas fait **échouer la compilation**, avec
sa position dans le fichier SQL. Modifier le schéma, une migration ou une
requête relance la génération : rust-analyzer suit, sans watcher à lancer. `pgweave describe --rust queries/x.sql`
affiche le code généré sans compiler ; `pgweave describe` seul affiche les
descripteurs JSON.

## Requêtes nommées

Format des fichiers : [`named-queries.md`](named-queries.md).

```sql
-- name: page_by_id
SELECT id, slug, title, status, published_on FROM pages WHERE id = $id;
```

devient :

```rust
pub struct PageByIdParams { pub id: i64 }
pub struct PageByIdRow {
    pub id: i64,
    pub slug: String,
    pub title: String,
    pub status: PageStatus,               // enum PostgreSQL → enum Rust
    pub published_on: Option<time::Date>, // colonne nullable → Option
}
pub async fn page_by_id<C: GenericClient + ?Sized>(client: &C, params: PageByIdParams)
    -> Result<Option<PageByIdRow>, pgweave_runtime::Error>;
```

`client` est tout ce qui implémente `GenericClient` : client, transaction,
connexion de pool.

### Cardinalité

Le type de retour suit le nombre de lignes, **inféré** de façon
conservatrice :

| Requête | Cardinalité | Retour |
|---|---|---|
| égalités sur toutes les colonnes d'une clé primaire, d'une contrainte ou d'un index unique (sans prédicat) | `optional` | `Option<Row>` |
| `INSERT … VALUES (une ligne) RETURNING` | `one` | `Row` |
| agrégat sans `GROUP BY` (`count(*)`…) | `one` | `Row` |
| `LIMIT 1` | `optional` | `Option<Row>` |
| sans `RETURNING` | `none` | `u64` (lignes touchées) |
| sinon | `many` | `Vec<Row>`, et `x_stream` en flux |

Une déclaration dans l'en-tête (`-- name: x :one`) **restreint**
l'inférence : `:one` sur une requête `optional` affirme que la ligne
existe, et c'est vérifié à l'exécution (`Error::RowCount`). Elle ne peut
pas la contredire (`:none` sur une requête qui renvoie des lignes est une
erreur).

### Nullabilité

Jamais supposée (PLAN.md D5) :

- **colonne** : `T` seulement si sa non-nullité est établie (`NOT NULL`
  conservé à travers les jointures, `count(*)`, littéral…), sinon
  `Option<T>` ;
- **paramètre** : affecté à une colonne (`VALUES`, `SET`), il en prend la
  nullabilité (`Option<T>` pour une colonne nullable) ; ailleurs
  (`WHERE id = $id`), il est attendu non nul.

### Types

| PostgreSQL | Rust |
|---|---|
| `bool` | `bool` |
| `int2`, `int4`, `int8` | `i16`, `i32`, `i64` |
| `float4`, `float8` | `f32`, `f64` |
| `numeric` | `rust_decimal::Decimal` |
| `text`, `varchar`, `char(n)`, `name`, `citext` | `String` (paramètre : `&str`) |
| `bytea` | `Vec<u8>` (paramètre : `&[u8]`) |
| `uuid` | `uuid::Uuid` |
| `json`, `jsonb` | `serde_json::Value` |
| `date`, `time`, `timestamp`, `timestamptz` | `time::Date`, `time::Time`, `time::PrimitiveDateTime`, `time::OffsetDateTime` (ou `chrono`) |
| tableau `T[]` | `Vec<Option<T>>` (paramètre : `&[T]`) |
| enum du projet | enum Rust généré (variantes en PascalCase) |
| domaine | type de son type de base |

Un type absent de la table est un **refus**, jamais un repli : on le
déclare dans `build.rs`.

```rust
pgweave_build::Builder::new()
    .temporal(pgweave_codegen::rust::Temporal::Chrono)
    .type_override("public.geometry", "geo_types::Geometry<f64>")
    .generate()
```

Les noms de colonnes deviennent des champs en snake_case (`"createdAt"` →
`created_at`). Une colonne sans nom Rust (`?column?`) ou deux colonnes de
même nom sont refusées : leur donner un alias.

### Exécution

- Types des paramètres tous intégrés : `query_typed`, en un seul aller-retour.
- Type du projet en paramètre (enum) : la requête est préparée par le
  serveur, qui en fixe les types (un aller-retour de plus).
- Coût mesuré (`make bench`) : identique à `tokio-postgres` écrit à la
  main (rapport 0,995 sur `page_by_id`).

## `COPY … FROM STDIN`

Une requête nommée peut être un `COPY` :

```sql
-- name: load_staging
COPY etablissements_staging (siret, nom, effectif, ouvert) FROM STDIN (FORMAT binary);

-- name: load_staging_csv
COPY etablissements_staging FROM STDIN (FORMAT csv, HEADER true);
```

- format binaire : `load_staging(&tx, records)` où `records` itère sur des
  `LoadStagingRecord { siret: &str, nom: &str, effectif: Option<i32>, … }` ;
- texte ou CSV : `load_staging_csv(&tx, reader)` où `reader` est un
  `tokio::io::AsyncRead`, transmis tel quel.

`COPY` demande un `Client` ou une `Transaction` (trait `CopyClient`). Les
colonnes générées ne sont pas alimentées (et refusées si on les cite).
L'exemple enchaîne staging, `COPY` et bascule `INSERT … ON CONFLICT` dans
une transaction.

## `query!`

```rust
let title = pgweave_macros::query!("SELECT title FROM pages WHERE id = $id", id = id)
    .fetch(&client)          // optional → Option<Row>
    .await?
    .map(|r| r.title);

let n = pgweave_macros::query!(
    "UPDATE pages SET title = $title WHERE author = $author",
    title = "Bonjour",
    author = author,
)
.execute(&client)            // sans ligne → u64
.await?;
```

- paramètres nommés (`$nom`), liés par `nom = valeur` ; chaque valeur est
  vérifiée au type attendu ;
- `fetch` renvoie ce que promet la cardinalité ; `fetch_one` /
  `fetch_optional` restreignent (vérifié à l'exécution) ;
- une erreur SQL est une erreur de compilation sur la chaîne ;
- pas d'enum ni de `COPY` en ligne : ils demandent des types uniques dans
  le crate, donc une requête nommée.

`query_file!("sql/live_pages.sql", pattern = p)` fait de même avec une
requête lue dans un fichier du crate (chemin relatif à son `Cargo.toml`) :
le fichier est inclus pour la recompilation, et une erreur est positionnée
dedans (`sql/live_pages.sql:5:7: [column-not-found] …`).

## Erreurs

`pgweave_runtime::Error` : `Db` (erreur PostgreSQL ou de connexion),
`RowCount` (nombre de lignes contraire à la cardinalité), `Decode`
(valeur non décodable), `Io` (lecture des données d'un `COPY`). Chacune
nomme la requête.
