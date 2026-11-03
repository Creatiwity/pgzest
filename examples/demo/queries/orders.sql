-- Orders.

-- name: open_order
INSERT INTO orders (restaurant_id, customer_id)
VALUES ($restaurant, $customer)
RETURNING id, status;

-- name: add_item
-- The unit price is frozen from the dish at order time.
INSERT INTO order_items (order_id, dish_id, quantity, unit_price)
SELECT $order, d.id, $quantity, d.price
FROM dishes d
WHERE d.id = $dish
RETURNING quantity, unit_price;

-- name: set_status
-- Moving an order to `served` fires the loyalty trigger.
UPDATE orders
SET status = $status
WHERE id = $id
RETURNING id, status, updated_at;

-- name: order_summary :optional
-- GROUP BY: the analysis infers `many`; the `:optional` declaration narrows
-- it (contract checked at run time). LEFT JOIN: `customer` may be NULL.
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
-- No RETURNING: the number of deleted rows (`u64`).
DELETE FROM orders
WHERE status = 'draft' AND updated_at < $before;

-- name: orders_of_restaurant :many
-- `$status?`: optional filter, `Option<OrderStatus>` in Rust; when absent,
-- all the restaurant's orders.
SELECT o.id, o.status, o.placed_at
FROM orders o
WHERE o.restaurant_id = $restaurant
  AND ($status?::order_status IS NULL OR o.status = $status)
ORDER BY o.id;

-- name: orders_page :many
-- Filtered pagination: each filter provided adds its condition (`[[ … ]]`,
-- `Option` in Rust), the sort order is a choice (`{{ … }}`, Rust enum). All
-- eight variants are checked at compile time; none is assembled at run
-- time.
SELECT o.id, o.status, o.placed_at
FROM orders o
WHERE o.restaurant_id = $restaurant
[[ AND o.status = $status ]]
[[ AND o.placed_at >= $since ]]
ORDER BY {{ sort: recent = o.placed_at DESC NULLS LAST, o.id DESC | oldest = o.id }}
LIMIT $limit;
