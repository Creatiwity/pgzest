-- Identity, citext, enum, domaine, CHECK, colonnes générées (PG 18 : VIRTUAL).

-- name: all_products
SELECT id, sku, status, quantity, unit_price, total_stored, price_with_tax FROM products;

-- name: by_sku :optional
SELECT id, status FROM products WHERE sku = $sku;

-- name: by_status
SELECT id FROM products WHERE status = $status;

-- name: insert_product :one
INSERT INTO products (sku, quantity, unit_price) VALUES ($sku, $quantity, $unit_price)
RETURNING id, total_stored, price_with_tax;

-- name: enum_cast
SELECT status::text AS status_text, 'review'::page_status AS fixed FROM products;
