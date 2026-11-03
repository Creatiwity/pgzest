-- Plats et menu.

-- name: add_dish
-- `kind` est un enum PostgreSQL (enum Rust généré), `price` un domaine
-- (passé sous son type de base, `i32`).
INSERT INTO dishes (restaurant_id, name, kind, price, vegetarian, tags)
VALUES ($restaurant, $name, $kind, $price, $vegetarian, $tags)
RETURNING id;

-- name: menu_of
-- Lecture d'une vue : ses colonnes sont typées depuis sa requête.
-- Plusieurs lignes : `Vec<…>`, et `menu_of_stream` en flux.
SELECT name, kind, price, vegetarian
FROM menu
WHERE restaurant = $restaurant
ORDER BY kind, name;

-- name: dish_counts
-- Agrégat sans GROUP BY : exactement une ligne ; `count(*)` n'est jamais NULL.
SELECT count(*) AS total,
       count(*) FILTER (WHERE vegetarian) AS vegetarian,
       min(price) AS cheapest
FROM dishes
WHERE restaurant_id = $restaurant;

-- name: dish_card :optional
-- La fiche d'un plat. Les requêtes suivantes renvoient la même fiche
-- (`-> dish_card`) : une seule struct Rust, `DishCardRow`, pour toutes.
SELECT @dish_card(d)
FROM dishes d
WHERE d.id = $id;

-- name: vegetarian_dishes :many -> dish_card
SELECT @dish_card(d)
FROM dishes d
JOIN restaurants r ON r.id = d.restaurant_id
WHERE r.slug = $restaurant AND d.vegetarian
ORDER BY d.price;

-- name: import_dishes
-- COPY typé : la fonction générée lit un flux CSV.
COPY dishes (restaurant_id, name, kind, price, vegetarian) FROM STDIN (FORMAT csv, HEADER true);
