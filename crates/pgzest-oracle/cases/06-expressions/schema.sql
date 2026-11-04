CREATE TYPE order_status AS ENUM ('pending', 'paid', 'shipped');

CREATE TABLE customers (
    id         BIGSERIAL PRIMARY KEY,
    email      VARCHAR(320) NOT NULL UNIQUE,
    name       TEXT,
    birthdate  DATE,
    tags       TEXT[] NOT NULL DEFAULT '{}',
    settings   JSONB NOT NULL DEFAULT '{}',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE orders (
    id          BIGSERIAL PRIMARY KEY,
    customer_id BIGINT NOT NULL REFERENCES customers (id),
    status      order_status NOT NULL DEFAULT 'pending',
    total       NUMERIC(12, 2) NOT NULL,
    discount    REAL,
    items       INTEGER[] NOT NULL,
    placed_at   TIMESTAMP NOT NULL,
    note        CHAR(10),
    paid        BOOLEAN NOT NULL DEFAULT false
);
