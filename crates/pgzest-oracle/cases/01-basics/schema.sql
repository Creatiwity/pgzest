CREATE TABLE pages (
    id            BIGINT PRIMARY KEY,
    slug          VARCHAR(120) NOT NULL UNIQUE,
    title         TEXT NOT NULL,
    body          TEXT,
    views         INTEGER NOT NULL DEFAULT 0,
    rating        NUMERIC(3, 2),
    published     BOOLEAN NOT NULL DEFAULT false,
    author_id     UUID,
    creation_date TIMESTAMPTZ NOT NULL DEFAULT now(),
    publish_on    DATE,
    tags          TEXT[] NOT NULL DEFAULT '{}'
);
