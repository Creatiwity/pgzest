-- Customers.

-- name: upsert_customer
-- Unconditional `ON CONFLICT … DO UPDATE`: the row is always returned
-- (`one`). `phone` goes into a nullable column: `Option<&str>`.
INSERT INTO customers (email, full_name, phone)
VALUES ($email, $full_name, $phone)
ON CONFLICT (email) DO UPDATE SET full_name = excluded.full_name
RETURNING id, loyalty_points;

-- name: customer_by_email
-- `email` (citext) has a unique index: at most one row.
SELECT id, full_name, phone, loyalty_points
FROM customers
WHERE email = $email;
