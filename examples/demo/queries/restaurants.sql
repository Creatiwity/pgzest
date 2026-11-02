-- Restaurants. Survolez un en-tête `-- name:` : l'éditeur montre le
-- descripteur (paramètres, colonnes, nombre de lignes).

-- name: create_restaurant
INSERT INTO restaurants (slug, name, opened_on)
VALUES ($slug, $name, $opened_on)
RETURNING id, created_at;

-- name: restaurant_by_slug
-- `slug` est UNIQUE : au plus une ligne, donc `Option<…>` en Rust.
SELECT id, name, opened_on, created_at
FROM restaurants
WHERE slug = $slug;

-- name: rename_restaurant
UPDATE restaurants
SET name = $name
WHERE id = $id
RETURNING name, updated_at;
