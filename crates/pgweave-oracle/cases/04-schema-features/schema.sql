CREATE EXTENSION citext;

CREATE TYPE page_status AS ENUM ('draft', 'review', 'published');

CREATE DOMAIN positive_int AS INTEGER CHECK (VALUE > 0);

CREATE TABLE products (
    id           BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    sku          CITEXT NOT NULL UNIQUE,
    status       page_status NOT NULL DEFAULT 'draft',
    quantity     positive_int NOT NULL,
    unit_price   NUMERIC(10, 2) NOT NULL CHECK (unit_price >= 0),
    total_stored NUMERIC(12, 2) GENERATED ALWAYS AS (quantity * unit_price) STORED,
    -- PG 18 refuse une expression VIRTUAL qui utilise un type utilisateur
    -- (« generation expression uses user-defined type ») : `quantity`, de
    -- domaine positive_int, n'est donc utilisable que dans la colonne STORED.
    -- Règle à porter dans le checker (P3).
    price_with_tax NUMERIC(12, 2) GENERATED ALWAYS AS (unit_price * 1.2) VIRTUAL,
    CONSTRAINT sku_not_blank CHECK (length(sku) > 0)
);
