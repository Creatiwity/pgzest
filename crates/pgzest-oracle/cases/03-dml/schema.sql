CREATE TABLE pages (
    id            BIGINT PRIMARY KEY,
    title         TEXT NOT NULL,
    body          TEXT,
    views         INTEGER NOT NULL DEFAULT 0,
    updated_at    TIMESTAMPTZ
);
