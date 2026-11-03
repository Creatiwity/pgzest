-- Commandes.

-- name: open_order
INSERT INTO orders (restaurant_id, customer_id)
VALUES ($restaurant, $customer)
RETURNING id, status;

-- name: add_item
-- Le prix unitaire est figé depuis le plat au moment de la commande.
INSERT INTO order_items (order_id, dish_id, quantity, unit_price)
SELECT $order, d.id, $quantity, d.price
FROM dishes d
WHERE d.id = $dish
RETURNING quantity, unit_price;

-- name: set_status
-- Passer une commande à `served` déclenche le trigger de fidélité.
UPDATE orders
SET status = $status
WHERE id = $id
RETURNING id, status, updated_at;

-- name: order_summary :optional
-- GROUP BY : l'analyse infère `many` ; la déclaration `:optional` restreint
-- (contrat vérifié à l'exécution). LEFT JOIN : `customer` peut être NULL.
SELECT o.id,
       o.status,
       c.full_name AS customer,
       order_total(o.id) AS total_cents,
       count(i.dish_id) AS lines
FROM orders o
LEFT JOIN customers c ON c.id = o.customer_id
LEFT JOIN order_items i ON i.order_id = o.id
WHERE o.id = $id
GROUP BY o.id, c.full_name;

-- name: revenue_by_kind
SELECT d.kind,
       sum(i.quantity * i.unit_price) AS revenue_cents
FROM order_items i
JOIN dishes d ON d.id = i.dish_id
JOIN orders o ON o.id = i.order_id
WHERE o.status = 'served'
GROUP BY d.kind
ORDER BY d.kind;

-- name: purge_drafts
-- Sans RETURNING : le nombre de lignes supprimées (`u64`).
DELETE FROM orders
WHERE status = 'draft' AND updated_at < $before;

-- name: orders_of_restaurant :many
-- `$status?` : filtre optionnel, `Option<OrderStatus>` en Rust ; absent,
-- toutes les commandes du restaurant.
SELECT o.id, o.status, o.placed_at
FROM orders o
WHERE o.restaurant_id = $restaurant
  AND ($status?::order_status IS NULL OR o.status = $status)
ORDER BY o.id;
