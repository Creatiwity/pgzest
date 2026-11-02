-- Clients.

-- name: upsert_customer
-- `ON CONFLICT … DO UPDATE` sans condition : la ligne est toujours
-- renvoyée (`one`). `phone` va dans une colonne nullable : `Option<&str>`.
INSERT INTO customers (email, full_name, phone)
VALUES ($email, $full_name, $phone)
ON CONFLICT (email) DO UPDATE SET full_name = excluded.full_name
RETURNING id, loyalty_points;

-- name: customer_by_email
-- `email` (citext) a un index unique : au plus une ligne.
SELECT id, full_name, phone, loyalty_points
FROM customers
WHERE email = $email;
