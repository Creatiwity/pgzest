# Démo pgweave : du SQL au Rust typé

Une petite application de commandes (restaurants, plats, clients,
commandes), écrite comme un projet réel :

```
pgweave.toml          où sont le schéma, les migrations, les requêtes
schema/               l'état complet de la base, écrit à la main (déclaratif)
migrations/           son historique, généré par `pgweave migrate create`
macros/               fragments : du SQL écrit une fois, inséré par `@nom`
queries/              les requêtes nommées, un module Rust par fichier
sandbox/              zone d'essai : erreurs volontaires, requêtes à survoler
data/dishes.csv       importé par COPY
src/queries.rs        une ligne : `pgweave_macros::queries!("queries");`
src/main.rs           le programme qui utilise le code généré
```

Le schéma n'est **jamais lu dans une base** : pgweave rejoue `schema/` sur
un catalogue PostgreSQL 18 reconstruit (types, fonctions, opérateurs,
extensions), et analyse chaque requête comme PostgreSQL le ferait. Les
migrations sont rejouées à part : elles doivent mener exactement à ce
schéma.

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

`sandbox/1-erreurs.sql` contient dix requêtes fautives, soulignées en
rouge. Corrigez-les une à une pour voir le diagnostic disparaître :

- types incompatibles (`text = integer`, enum comparé à un entier) ;
- colonne ou table inconnue, colonne ambiguë ;
- valeur explicite dans une colonne `GENERATED ALWAYS` ;
- `DEFAULT` hors d'un `INSERT … VALUES` ou d'un `SET` ;
- paramètre dont rien ne fixe le type, cardinalité déclarée contredite ;
- requête qui prétend renvoyer la fiche d'un plat (`-> dish_card`) sans en
  avoir les colonnes.

En CLI, même verdict : `../../target/debug/pgweave check sandbox/*.sql`.

### 4. Écrire une projection une seule fois

`macros/projections.sql` déclare des **fragments** : `dish_card(d)`, la
fiche d'un plat, et `affordable(d)`, un prédicat. Dans `queries/menu.sql`,
survolez `@dish_card(d)` : l'éditeur montre sa documentation et le SQL
inséré (`d.id, d.name, …`). `@affordable(d)` est inséré entre parenthèses,
pour que le `AND` qui l'entoure ne le découpe pas.

`vegetarian_dishes` (même fichier) déclare `-> dish_card`, et
`affordable_dishes` (`queries/promotions/deals.sql`, un autre module)
`-> menu::dish_card` : leurs lignes ont la forme de celles de `dish_card`,
vérifiée colonne par colonne, et le code Rust n'a qu'une struct,
`queries::menu::DishCardRow`, pour les trois requêtes.

Ajoutez `d.tags` au fragment, sans rien toucher d'autre : les trois
requêtes changent ensemble, et `DishCardRow` gagne un champ.

### 5. Le schéma déclaratif, les migrations générées

`schema/` décrit la base telle qu'elle doit être : les `CREATE TABLE`
complets, les fonctions, les triggers, la vue. Ajoutez une colonne à
`orders` dans `schema/1-commandes.sql`, **sans sauvegarder** :

```sql
    tip        cents,
```

- Dans `sandbox/2-a-explorer.sql`, ajoutez `o.tip` à une requête : elle est
  acceptée, le texte non sauvegardé du schéma fait foi.
- `migrations/0003_menu_public.sql`, la dernière migration, porte une
  information : les migrations ne mènent plus au schéma.

Sauvegardez, puis :

```sh
../../target/debug/pgweave migrate create pourboire
```

`migrations/0004_pourboire.sql` est écrite (`ALTER TABLE orders ADD COLUMN
tip cents`), avec son en-tête de contexte, après vérification par rejeu (une
colonne déclarée au milieu de la table vaut une note : PostgreSQL l'ajoute
à la fin) ;
l'information disparaît, et `pgweave migrate check` (pour la CI) est vert.
Une suppression de colonne serait refusée sans `--allow-destructive` ; un
index créé sur une table existante est signalé (écritures bloquées pendant
sa construction). `0003_menu_public.sql` a été générée ainsi : la vue
n'existait que dans le schéma.

Les corps PL/pgSQL sont vérifiés aussi. Dans `schema/2-fidelite.sql`,
changez `NEW.customer_id` en `NEW.client_id` : le trigger est attaché à
`orders`, qui n'a pas ce champ, et l'erreur l'indique.

### 6. Le code Rust généré

`src/queries.rs` tient en une ligne : `pgweave_macros::queries!("queries");`.
La macro décrit chaque requête de `queries/` à la compilation et génère un
module par fichier : `queries::menu`, `queries::orders`,
`queries::promotions::deals`… Les enums PostgreSQL sont à la racine
(`queries::DishKind`). Pour lire le code produit :

```sh
../../target/debug/pgweave describe --rust queries/menu.sql
```

Dans `src/main.rs`, faites « Go to Definition » sur
`queries::menu::add_dish` (rust-analyzer), ou survolez-le :
`AddDishParams { kind: DishKind, price: i32, tags: &[String], … }`.

Cassez une requête de `queries/` (par exemple `restaurant_id` →
`restaurant`), puis `cargo build -p pgweave-demo` : la compilation
échoue, avec la position de l'erreur dans le fichier SQL. Aucune base
n'est contactée pour cela.

`src/lib.rs` montre `query!`, une requête écrite dans le code Rust et
vérifiée à la compilation de la même façon.

### 7. L'exécution

```sh
make demo-run
```

Le programme recrée une base `pgweave_demo`, y applique les migrations
(comme un déploiement : le schéma est leur cible, pas un script), importe
les plats par `COPY` (CSV), puis déroule le scénario. On y voit :

- les enums Rust générés, et le domaine `cents` passé en `i32` ;
- deux listes de plats décodées dans la même struct (`DishCardRow`) ;
- le menu lu en flux ;
- un client dont l'e-mail `citext` ignore la casse ;
- une commande dans une transaction ;
- le trigger de fidélité qui crédite les points quand la commande est servie.

## Sous le capot

- Guide d'intégration Rust : [`docs/rust.md`](../../docs/rust.md).
- Format des requêtes nommées : [`docs/named-queries.md`](../../docs/named-queries.md).
- Ce qui est vérifié, et comment (oracle PostgreSQL) : [`PLAN.md`](../../PLAN.md).
