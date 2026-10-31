CREATE TABLE authors (
    id    UUID PRIMARY KEY,
    name  TEXT NOT NULL,
    email TEXT
);

CREATE TABLE pages (
    id        BIGINT PRIMARY KEY,
    author_id UUID REFERENCES authors (id),
    title     TEXT NOT NULL,
    views     INTEGER NOT NULL DEFAULT 0
);
