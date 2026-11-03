-- Coverage schema: intervals, full-text search, hierarchies,
-- measurements, bookings.

CREATE TYPE level AS ENUM ('low', 'mid', 'high');

CREATE DOMAIN email AS TEXT CHECK (VALUE LIKE '%@%');

CREATE TABLE teams (
    id        SERIAL PRIMARY KEY,
    parent_id INTEGER REFERENCES teams (id),
    name      TEXT NOT NULL,
    budget    NUMERIC(12, 2),
    level     level NOT NULL DEFAULT 'low'
);

CREATE TABLE people (
    id       BIGSERIAL PRIMARY KEY,
    team_id  INTEGER REFERENCES teams (id),
    mail     email NOT NULL,
    nick     VARCHAR(30),
    born     DATE,
    salary   INTEGER,
    skills   TEXT[] NOT NULL DEFAULT '{}',
    doc      TSVECTOR
);

CREATE TABLE bookings (
    id       UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    person   BIGINT NOT NULL REFERENCES people (id),
    during   TSTZRANGE NOT NULL,
    seats    INT4RANGE,
    price    DOUBLE PRECISION NOT NULL,
    note     TEXT
);

CREATE TABLE readings (
    sensor   TEXT NOT NULL,
    at       TIMESTAMPTZ NOT NULL,
    value    REAL,
    flags    BIT(8),
    PRIMARY KEY (sensor, at)
);
