CREATE TYPE page_status AS ENUM ('draft', 'live', 'archived');

CREATE DOMAIN slug AS VARCHAR(80) CHECK (VALUE ~ '^[a-z0-9-]+$');

CREATE TABLE pages (
    id           BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    slug         slug NOT NULL UNIQUE,
    title        TEXT NOT NULL,
    status       page_status NOT NULL DEFAULT 'draft',
    tags         TEXT[] NOT NULL DEFAULT '{}',
    author       UUID,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    published_on DATE
);

-- Batch loading (siren pattern): COPY into a staging table, then swap into
-- the final table.
CREATE TABLE etablissements (
    siret    CHAR(14) PRIMARY KEY,
    nom      TEXT NOT NULL,
    effectif INT,
    ouvert   DATE
);

CREATE UNLOGGED TABLE etablissements_staging (LIKE etablissements);

CREATE INDEX etablissements_search ON etablissements USING gin ((@etablissement_search));
