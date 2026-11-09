-- Foreign keys: referenced columns omitted (the primary key), and
-- ON DELETE SET NULL on one column of a composite key (PostgreSQL 15).
CREATE TABLE org (id BIGINT PRIMARY KEY);
CREATE TABLE member (org_id BIGINT REFERENCES org, id BIGINT, PRIMARY KEY (org_id, id));
CREATE TABLE doc (
    id BIGINT PRIMARY KEY,
    org_id BIGINT NOT NULL REFERENCES org,
    author BIGINT,
    FOREIGN KEY (org_id, author) REFERENCES member ON DELETE SET NULL (author)
);
