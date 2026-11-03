-- Promotions: a subdirectory is a module (`queries::promotions::deals`).
-- The reused card comes from another file: `-> menu::dish_card`, a path
-- from the root of `queries/`.

-- name: affordable_dishes :many -> menu::dish_card
-- `@affordable(d)` is a predicate: inserted in parentheses.
SELECT @dish_card(d)
FROM dishes d
WHERE d.restaurant_id = $restaurant AND @affordable(d)
ORDER BY d.price, d.name;
