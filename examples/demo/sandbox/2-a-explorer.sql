-- Survolez les noms : l'éditeur donne le type et la nullabilité de ce qui
-- est sous le curseur, d'après l'analyse de la requête.

-- name: commandes_du_jour
SELECT o.id,
       o.status,
       c.email,
       o.placed_at,
       order_total(o.id) AS total
FROM orders o
LEFT JOIN customers c ON c.id = o.customer_id
WHERE o.placed_at >= current_date
ORDER BY o.placed_at DESC;

-- name: plats_par_tag
-- Paramètre tableau, opérateur de recouvrement.
SELECT name, tags
FROM dishes
WHERE tags && $tags
  AND restaurant_id = $restaurant;

-- name: ca_mensuel
SELECT date_trunc('month', o.placed_at) AS month,
       sum(i.quantity * i.unit_price) AS revenue_cents,
       count(DISTINCT o.id) AS orders
FROM orders o
JOIN order_items i ON i.order_id = o.id
WHERE o.status = 'served'
GROUP BY 1
ORDER BY 1;
