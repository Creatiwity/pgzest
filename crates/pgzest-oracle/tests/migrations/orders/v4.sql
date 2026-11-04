-- Fourth version: partial rollback: schema, extension and
-- row-level security removed, view function rebuilt again.
CREATE TYPE order_status AS ENUM ('draft', 'placed', 'cancelled', 'served');

CREATE TABLE customers (
  id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  email TEXT NOT NULL UNIQUE,
  name TEXT NOT NULL,
  loyalty INTEGER NOT NULL DEFAULT 0 CHECK (loyalty >= 0),
  owner TEXT NOT NULL DEFAULT current_user
);

CREATE TABLE orders (
  id BIGSERIAL PRIMARY KEY,
  customer_id BIGINT NOT NULL REFERENCES customers (id),
  status order_status NOT NULL DEFAULT 'draft',
  amount BIGINT NOT NULL,
  note TEXT,
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX orders_customer_id ON orders (customer_id, status);

CREATE FUNCTION order_total(o BIGINT) RETURNS NUMERIC
LANGUAGE sql STABLE AS $$ SELECT amount::numeric FROM orders WHERE id = o $$;

CREATE VIEW order_totals AS
SELECT o.id, order_total(o.id) AS total FROM orders o;

CREATE FUNCTION touch() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  NEW.updated_at := now();
  RETURN NEW;
END
$$;
CREATE TRIGGER orders_touch BEFORE INSERT OR UPDATE ON orders
FOR EACH ROW EXECUTE FUNCTION touch();
