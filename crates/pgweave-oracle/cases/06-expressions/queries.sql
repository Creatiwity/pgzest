-- Expressions : arithmétique, chaînes, dates, JSON, tableaux, conversions,
-- CASE, agrégats, fenêtres, paramètres dans tous les contextes.

-- name: arithmetic_mix
SELECT total * 2, total + 1.5, discount * 2, discount + total, id / 2, id % 3, -total FROM orders;

-- name: integer_literals
SELECT 1, 2147483648, 9223372036854775808, 1.5, 1e3, -7;

-- name: string_functions
SELECT upper(name), length(email), substr(email, 1, 3), position('@' IN email), btrim(name), name || ' <' || email || '>' FROM customers;

-- name: string_concat_number
SELECT 'n°' || id, id || '', email || 1 FROM customers;

-- name: like_patterns
SELECT id FROM customers WHERE email LIKE $pattern OR name ILIKE $pattern2;

-- name: date_math
SELECT birthdate + 1, birthdate - 1, now() - created_at, created_at + interval '1 day', placed_at - interval '2 hours', birthdate + interval '1 month' FROM customers, orders;

-- name: date_functions
SELECT date_trunc('month', created_at), extract(year FROM created_at), date_part('day', birthdate), age(birthdate), to_char(created_at, 'YYYY') FROM customers;

-- name: at_time_zone
SELECT created_at AT TIME ZONE 'UTC', placed_at AT TIME ZONE 'Europe/Paris' FROM customers, orders;

-- name: current_values
SELECT now(), current_date, current_timestamp, localtimestamp, current_user;

-- name: jsonb_access
SELECT settings -> 'theme', settings ->> 'theme', settings #> '{a,b}', settings #>> '{a,b}', settings ? 'x', settings @> '{"a": 1}' FROM customers;

-- name: jsonb_param_filter
SELECT id FROM customers WHERE settings ->> 'lang' = $lang;

-- name: jsonb_build
SELECT jsonb_build_object('id', id, 'email', email), to_jsonb(tags), jsonb_array_length(settings -> 'list') FROM customers;

-- name: array_ops
SELECT tags[1], tags[1:2], array_length(tags, 1), cardinality(items), items || 4, 0 || items, tags @> ARRAY['vip'], array_append(items, 5) FROM customers, orders;

-- name: array_any
SELECT id FROM orders WHERE 3 = ANY(items) AND status = ANY($statuses);

-- name: array_param_any
SELECT id FROM customers WHERE id = ANY($ids);

-- name: array_constructor
SELECT ARRAY[1, 2, 3], ARRAY[1, 2.5], ARRAY['a', 'b'], ARRAY[id, customer_id], ARRAY[ARRAY[1, 2], ARRAY[3, 4]] FROM orders;

-- name: casts
SELECT id::text, total::int, '42'::bigint, CAST(placed_at AS date), status::text, '2024-01-01'::date, DATE '2024-01-01', TIMESTAMP '2024-01-01 10:00', note::varchar FROM orders;

-- name: case_expressions
SELECT CASE WHEN paid THEN 'yes' ELSE 'no' END, CASE status WHEN 'paid' THEN 1 WHEN 'shipped' THEN 2 END, CASE WHEN total > 100 THEN total ELSE 0 END, CASE WHEN paid THEN discount ELSE total END FROM orders;

-- name: coalesce_family
SELECT coalesce(name, email), coalesce(discount, 0), greatest(total, 10), least(discount, 0.5), nullif(name, ''), coalesce(birthdate, now()) FROM customers, orders;

-- name: boolean_logic
SELECT paid AND total > 0, NOT paid, paid IS TRUE, discount IS NULL, total IS DISTINCT FROM 0, total BETWEEN 1 AND 10 FROM orders;

-- name: in_list
SELECT id FROM orders WHERE status IN ('paid', 'shipped') AND customer_id IN ($a, $b);

-- name: aggregates
SELECT count(*), count(discount), sum(total), avg(total), min(placed_at), max(status), sum(id), avg(discount), bool_and(paid), string_agg(note, ','), array_agg(id) FROM orders;

-- name: aggregate_filter_group
SELECT customer_id, count(*) FILTER (WHERE paid), sum(total) FILTER (WHERE status = 'paid') FROM orders GROUP BY customer_id HAVING count(*) > $min_orders;

-- name: window_functions
SELECT id, row_number() OVER w, rank() OVER w, lag(total) OVER w, sum(total) OVER (PARTITION BY customer_id), first_value(placed_at) OVER w FROM orders WINDOW w AS (PARTITION BY customer_id ORDER BY placed_at);

-- name: ordered_set
SELECT percentile_cont(0.5) WITHIN GROUP (ORDER BY total), mode() WITHIN GROUP (ORDER BY status) FROM orders;

-- name: distinct_on
SELECT DISTINCT ON (customer_id) customer_id, placed_at, total FROM orders ORDER BY customer_id, placed_at DESC;

-- name: limit_offset_params
SELECT id FROM orders ORDER BY id LIMIT $limit OFFSET $offset;

-- name: param_cast
SELECT $value::int + 1, $label::text;

-- name: param_in_case
SELECT CASE WHEN total > $threshold THEN 'big' ELSE 'small' END FROM orders;

-- name: param_coalesce
SELECT coalesce($maybe, total) FROM orders;

-- name: param_function_arg
SELECT date_trunc($unit, created_at) FROM customers;

-- name: enum_compare_param
SELECT id FROM orders WHERE status = $status;

-- name: generate_series_from
SELECT g, g * 2 FROM generate_series(1, 10) AS g;

-- name: unnest_from
SELECT t, o.id FROM orders o, unnest(o.items) AS t;

-- name: lateral_subquery
SELECT c.id, last.total FROM customers c CROSS JOIN LATERAL (SELECT total FROM orders o WHERE o.customer_id = c.id ORDER BY placed_at DESC LIMIT 1) AS last;

-- name: scalar_subquery
SELECT id, (SELECT count(*) FROM orders o WHERE o.customer_id = c.id) AS n, (SELECT max(total) FROM orders) FROM customers c;

-- name: exists_and_in_subquery
SELECT id FROM customers c WHERE EXISTS (SELECT 1 FROM orders o WHERE o.customer_id = c.id) AND id IN (SELECT customer_id FROM orders WHERE paid);

-- name: union_literals
SELECT 1 AS n, 'a' AS label UNION ALL SELECT 2.5, 'b' UNION ALL SELECT NULL, NULL;

-- name: union_mixed_types
SELECT id, total FROM orders UNION SELECT customer_id, discount FROM orders;

-- name: recursive_cte
WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 10) SELECT i, i * 1.5 FROM n;

-- name: cte_chain
WITH paid AS (SELECT * FROM orders WHERE paid), totals AS (SELECT customer_id, sum(total) AS s FROM paid GROUP BY customer_id) SELECT c.email, t.s FROM totals t JOIN customers c ON c.id = t.customer_id;

-- name: values_select
SELECT * FROM (VALUES (1, 'a'), (2, 'b')) AS v(n, label);

-- name: full_join
SELECT c.id, o.id FROM customers c FULL JOIN orders o ON o.customer_id = c.id;

-- name: natural_join_star
SELECT * FROM orders NATURAL JOIN (SELECT id AS customer_id, email FROM customers) AS c;

-- name: insert_returning_params
INSERT INTO orders (customer_id, total, items, placed_at) VALUES ($customer, $total, $items, $placed) RETURNING id, status, total;

-- name: insert_select
INSERT INTO customers (email, name) SELECT email || '.bak', name FROM customers WHERE id = $id RETURNING *;

-- name: update_params
UPDATE orders SET total = total * $factor, discount = $discount, status = $status WHERE id = $id RETURNING id, total;

-- name: upsert
INSERT INTO customers (email, name) VALUES ($email, $name) ON CONFLICT (email) DO UPDATE SET name = excluded.name RETURNING id, (xmax = 0) AS inserted;

-- name: delete_using
DELETE FROM orders o USING customers c WHERE c.id = o.customer_id AND c.email = $email RETURNING o.id;
