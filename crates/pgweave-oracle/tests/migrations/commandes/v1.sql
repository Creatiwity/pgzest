-- Première version : tables, enum, clé étrangère, vue, fonction SQL,
-- trigger PL/pgSQL.
CREATE TYPE order_status AS ENUM ('draft', 'placed', 'served');

CREATE TABLE customers (
  id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  email TEXT NOT NULL UNIQUE,
  name TEXT NOT NULL,
  legacy_code TEXT
);

CREATE TABLE orders (
  id BIGSERIAL PRIMARY KEY,
  customer_id BIGINT NOT NULL REFERENCES customers (id),
  status order_status NOT NULL DEFAULT 'draft',
  amount INTEGER NOT NULL,
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX orders_customer_id ON orders (customer_id);

CREATE TABLE coupons (
  code TEXT PRIMARY KEY,
  order_id BIGINT REFERENCES orders (id)
);

CREATE FUNCTION order_total(o BIGINT) RETURNS INTEGER
LANGUAGE sql STABLE AS $$ SELECT amount FROM orders WHERE id = o $$;

CREATE VIEW order_amounts AS
SELECT o.id, o.amount FROM orders o;

CREATE FUNCTION touch() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  NEW.updated_at := now();
  RETURN NEW;
END
$$;
CREATE TRIGGER orders_touch BEFORE UPDATE ON orders
FOR EACH ROW EXECUTE FUNCTION touch();
