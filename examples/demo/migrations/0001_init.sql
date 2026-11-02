-- pgweave: postgres=18 extensions=citext@1.8
-- Restaurants, plats, clients et commandes.

CREATE EXTENSION IF NOT EXISTS citext;

CREATE TYPE dish_kind AS ENUM ('starter', 'main', 'dessert', 'drink');
CREATE TYPE order_status AS ENUM ('draft', 'placed', 'served', 'cancelled');

-- Un prix en centimes, jamais négatif.
CREATE DOMAIN cents AS INTEGER CHECK (VALUE >= 0);

CREATE TABLE restaurants (
    id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    slug       TEXT NOT NULL UNIQUE CHECK (slug ~ '^[a-z0-9-]+$'),
    name       TEXT NOT NULL,
    opened_on  DATE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE dishes (
    id            BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    restaurant_id BIGINT NOT NULL REFERENCES restaurants (id) ON DELETE CASCADE,
    name          TEXT NOT NULL,
    kind          dish_kind NOT NULL,
    price         cents NOT NULL,
    vegetarian    BOOLEAN NOT NULL DEFAULT false,
    tags          TEXT[] NOT NULL DEFAULT '{}',
    UNIQUE (restaurant_id, name)
);

CREATE TABLE customers (
    id         UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    email      CITEXT NOT NULL,
    full_name  TEXT NOT NULL,
    phone      TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
-- Comme Prisma pose ses `@unique` : un index unique, pas une contrainte.
CREATE UNIQUE INDEX customers_email_key ON customers (email);

CREATE TABLE orders (
    id            BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    restaurant_id BIGINT NOT NULL REFERENCES restaurants (id),
    customer_id   UUID REFERENCES customers (id),
    status        order_status NOT NULL DEFAULT 'draft',
    placed_at     TIMESTAMPTZ,
    note          TEXT,
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE order_items (
    order_id BIGINT NOT NULL REFERENCES orders (id) ON DELETE CASCADE,
    dish_id  BIGINT NOT NULL REFERENCES dishes (id),
    quantity INTEGER NOT NULL CHECK (quantity > 0),
    unit_price cents NOT NULL,
    PRIMARY KEY (order_id, dish_id)
);

CREATE INDEX orders_restaurant_idx ON orders (restaurant_id);
CREATE INDEX orders_customer_idx ON orders (customer_id);
CREATE INDEX order_items_dish_idx ON order_items (dish_id);

-- Horodatage de mise à jour, partagé par plusieurs tables : le corps est
-- vérifié une fois par table (NEW y a le type de chacune).
CREATE FUNCTION touch_updated_at() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    NEW.updated_at := now();
    RETURN NEW;
END;
$$;

CREATE TRIGGER restaurants_touch BEFORE UPDATE ON restaurants
    FOR EACH ROW EXECUTE FUNCTION touch_updated_at();
CREATE TRIGGER orders_touch BEFORE UPDATE ON orders
    FOR EACH ROW EXECUTE FUNCTION touch_updated_at();

-- Total d'une commande, en centimes.
CREATE FUNCTION order_total(p_order BIGINT) RETURNS BIGINT LANGUAGE sql STABLE AS $$
    SELECT coalesce(sum(quantity * unit_price), 0)::bigint
    FROM order_items
    WHERE order_id = p_order
$$;
