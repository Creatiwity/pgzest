-- Dishes and menu.

-- name: add_dish
-- `kind` is a PostgreSQL enum (generated Rust enum), `price` a domain
-- (passed as its base type, `i32`).
INSERT INTO dishes (restaurant_id, name, kind, price, vegetarian, tags)
VALUES ($restaurant, $name, $kind, $price, $vegetarian, $tags)
RETURNING id;

-- name: menu_of
-- Reading a view: its columns are typed from its query.
-- Several rows: `Vec<…>`, and `menu_of_stream` as a stream.
SELECT name, kind, price, vegetarian
FROM menu
WHERE restaurant = $restaurant
ORDER BY kind, name;

-- name: dish_counts
-- Aggregate without GROUP BY: exactly one row; `count(*)` is never NULL.
SELECT count(*) AS total,
       count(*) FILTER (WHERE vegetarian) AS vegetarian,
       min(price) AS cheapest
FROM dishes
WHERE restaurant_id = $restaurant;

-- name: dish_card :optional
-- A dish's card. The following queries return the same card
-- (`-> dish_card`): a single Rust struct, `DishCardRow`, for all of them.
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
-- Typed COPY: the generated function reads a CSV stream.
COPY dishes (restaurant_id, name, kind, price, vegetarian) FROM STDIN (FORMAT csv, HEADER true);
