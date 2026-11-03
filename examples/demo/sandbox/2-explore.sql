-- Hover the names: the editor gives the type and nullability of what is
-- under the cursor, from the analysis of the query.

-- name: todays_orders
SELECT o.id,
       o.status,
       c.email,
       o.placed_at,
       order_total(o.id) AS total
FROM orders o
LEFT JOIN customers c ON c.id = o.customer_id
WHERE o.placed_at >= current_date
ORDER BY o.placed_at DESC;

-- name: dishes_by_tag
-- Array parameter, overlap operator.
SELECT name, tags
FROM dishes
WHERE tags && $tags
  AND restaurant_id = $restaurant;

-- name: monthly_revenue
SELECT date_trunc('month', o.placed_at) AS month,
       sum(i.quantity * i.unit_price) AS revenue_cents,
       count(DISTINCT o.id) AS orders
FROM orders o
JOIN order_items i ON i.order_id = o.id
WHERE o.status = 'served'
GROUP BY 1
ORDER BY 1;
