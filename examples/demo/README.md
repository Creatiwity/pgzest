# Démo pgweave : du SQL au Rust typé

Une petite application de commandes (restaurants, plats, clients,
commandes), écrite comme un projet réel :

```
pgweave.toml          où sont les migrations, le schéma, les requêtes
migrations/           l'historique du schéma (enums, domaine, citext, triggers PL/pgSQL)
schema/               une vue déclarative, rejouée après les migrations
queries/              les requêtes nommées → fonctions Rust générées
sandbox/              zone d'essai : erreurs volontaires, requêtes à survoler
data/dishes.csv       importé par COPY
build.rs, src/        le programme Rust qui utilise le code généré
```

Le schéma n'est **jamais lu dans une base** : pgweave rejoue les migrations
sur un catalogue PostgreSQL 18 reconstruit (types, fonctions, opérateurs,
extensions), et analyse chaque requête comme PostgreSQL le ferait.

## Lancer

Depuis la racine du dépôt :

```sh
make demo        # compile le serveur de langage et l'extension, ouvre VS Code sur la démo
make demo-run    # démarre PostgreSQL 18 (Docker) et exécute le scénario Rust
```

`make demo` ouvre une fenêtre VS Code avec l'extension pgweave chargée en
mode développement : rien n'est installé dans votre VS Code. Pour
l'exploration Rust (aller à la définition du code généré, erreurs de
`query!` en direct), installez l'extension recommandée `rust-analyzer`.

## Visite guidée

### 1. Ce que l'éditeur sait d'une requête

Ouvrez `queries/restaurants.sql` et survolez la ligne `-- name: restaurant_by_slug`.
Le survol montre le **descripteur** de la requête : ses paramètres, ses
colonnes avec leur type et leur nullabilité, et son nombre de lignes
(`optional`, parce que `slug` est `UNIQUE`). C'est exactement ce que
devient la fonction Rust.

À essayer dans `queries/` :

- `menu.sql`, `menu_of` : les colonnes d'une vue sont nullables (PostgreSQL
  ne garde pas `NOT NULL` dans une vue), d'où des `Option` en Rust ;
- `menu.sql`, `dish_counts` : `count(*)` est non nul, `min(price)` peut
  l'être, l'agrégat sans `GROUP BY` renvoie exactement une ligne ;
- `customers.sql`, `upsert_customer` : `$phone` va dans une colonne
  nullable, donc `Option<&str>` ; l'upsert sans condition renvoie toujours
  sa ligne (`one`) ;
- `orders.sql`, `order_summary :optional` : l'analyse infère `many` (il y a
  un `GROUP BY`), la déclaration restreint à `optional`.

### 2. Le type de ce qui est sous le curseur

Dans `sandbox/2-a-explorer.sql`, survolez `c.email` (un `citext`, nullable
à cause du `LEFT JOIN`), `o.status` (l'enum `order_status`), `order_total`
(la fonction SQL de la migration)… Le
survol vient de l'analyse de la requête entière : un même nom (`id`) a le
type de la table qu'il désigne là où il est.

### 3. Les erreurs, comme PostgreSQL les dirait

`sandbox/1-erreurs.sql` contient neuf requêtes fautives, soulignées en
rouge. Corrigez-les une à une pour voir le diagnostic disparaître :

- types incompatibles (`text = integer`, enum comparé à un entier) ;
- colonne ou table inconnue, colonne ambiguë ;
- valeur explicite dans une colonne `GENERATED ALWAYS` ;
- `DEFAULT` hors d'un `INSERT … VALUES` ou d'un `SET` ;
- paramètre dont rien ne fixe le type, cardinalité déclarée contredite.

En CLI, même verdict : `../../target/debug/pgweave check sandbox/*.sql`.

### 4. Le schéma vit avec l'éditeur

Ouvrez `migrations/0002_loyalty.sql` et ajoutez une colonne, **sans
sauvegarder** :

```sql
ALTER TABLE orders ADD COLUMN tip INTEGER;
```

Dans `sandbox/2-a-explorer.sql`, ajoutez `o.tip` à une requête : elle est
acceptée, car le texte non sauvegardé de la migration fait foi. Annulez
la modification, et l'erreur revient.

Les corps PL/pgSQL sont vérifiés aussi. Dans `0002_loyalty.sql`, changez
`NEW.customer_id` en `NEW.client_id` : le trigger est attaché à `orders`,
qui n'a pas ce champ, et l'erreur l'indique.

### 5. Le code Rust généré

`build.rs` (trois lignes) décrit chaque requête de `queries/` et génère le
module `queries`. Pour le lire :

```sh
../../target/debug/pgweave describe --rust queries/*.sql
```

Dans `src/main.rs`, faites « Go to Definition » sur `queries::add_dish`
(rust-analyzer), ou survolez-le : `AddDishParams { kind: DishKind, price: i32, tags: &[String], … }`.

Cassez une requête de `queries/` (par exemple `restaurant_id` →
`restaurant`), puis `cargo build -p pgweave-demo` : la compilation
échoue, avec la position de l'erreur dans le fichier SQL. Aucune base
n'est contactée pour cela.

`src/lib.rs` montre `query!`, une requête écrite dans le code Rust et
vérifiée à la compilation de la même façon.

### 6. L'exécution

```sh
make demo-run
```

Le programme recrée une base `pgweave_demo`, y applique les migrations,
importe les plats par `COPY` (CSV), puis déroule le scénario. On y voit :

- les enums Rust générés, et le domaine `cents` passé en `i32` ;
- le menu lu en flux ;
- un client dont l'e-mail `citext` ignore la casse ;
- une commande dans une transaction ;
- le trigger de fidélité qui crédite les points quand la commande est servie.

## Sous le capot

- Guide d'intégration Rust : [`docs/rust.md`](../../docs/rust.md).
- Format des requêtes nommées : [`docs/named-queries.md`](../../docs/named-queries.md).
- Ce qui est vérifié, et comment (oracle PostgreSQL) : [`PLAN.md`](../../PLAN.md).
