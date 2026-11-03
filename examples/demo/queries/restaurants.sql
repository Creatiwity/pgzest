-- Restaurants. Hover a `-- name:` header: the editor shows the
-- descriptor (parameters, columns, row count).

-- name: create_restaurant
INSERT INTO restaurants (slug, name, opened_on)
VALUES ($slug, $name, $opened_on)
RETURNING id, created_at;

-- name: restaurant_by_slug
-- `slug` is UNIQUE: at most one row, hence `Option<…>` in Rust.
SELECT id, name, opened_on, created_at
FROM restaurants
WHERE slug = $slug;

-- name: rename_restaurant
UPDATE restaurants
SET name = $name
WHERE id = $id
RETURNING name, updated_at;
