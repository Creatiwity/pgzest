-- Sandbox: this file is neither a migration nor a query compiled into
-- Rust. The editor checks it against the project schema; each query
-- below contains a deliberate error, underlined in red. Fix them one by
-- one to watch the diagnostic disappear.

-- name: wrong_type
-- `name` is text: PostgreSQL would reject `text = integer`.
SELECT id FROM restaurants WHERE name = 42;

-- name: unknown_column
SELECT id, title FROM dishes;

-- name: unknown_table
SELECT * FROM meals;

-- name: unknown_enum
-- 'dinner' is not a dish_kind value… but that is a run-time error (the
-- literal is converted at execution), not an analysis error: only the
-- type is checked here. Replace 42 with 'main':
SELECT name FROM dishes WHERE kind = 42;

-- name: untyped_parameter
-- Nothing fixes the type of $x: no Rust function is possible.
SELECT $x;

-- name: contradicted_cardinality :none
-- Declared as returning no row, but it is a SELECT.
SELECT id FROM orders;

-- name: forbidden_generated_column
-- `id` is GENERATED ALWAYS AS IDENTITY.
INSERT INTO restaurants (id, slug, name) VALUES (1, 'x', 'X');

-- name: default_out_of_context
SELECT default FROM orders;

-- name: ambiguous
-- `id` exists in both tables: which one?
SELECT id, name FROM restaurants JOIN dishes ON dishes.restaurant_id = restaurants.id;

-- name: incomplete_card :many -> menu::dish_card
-- Declares that it returns a dish's card (`DishCardRow`), but `vegetarian`
-- is missing. Add it: the LEFT JOIN then makes every column of `d`
-- nullable, and the shared struct would lie. Write `@dish_card(d)` and
-- replace LEFT JOIN with JOIN.
SELECT d.id, d.name, d.kind, d.price
FROM restaurants r
LEFT JOIN dishes d ON d.restaurant_id = r.id;
