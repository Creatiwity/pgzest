-- The action's columns change, row-level security enabled and forced.
CREATE TABLE org (id BIGINT PRIMARY KEY);
CREATE TABLE member (org_id BIGINT REFERENCES org, id BIGINT, PRIMARY KEY (org_id, id));
CREATE TABLE doc (
    id BIGINT PRIMARY KEY,
    org_id BIGINT NOT NULL REFERENCES org,
    author BIGINT DEFAULT 0,
    FOREIGN KEY (org_id, author) REFERENCES member ON DELETE SET DEFAULT (author)
);
ALTER TABLE doc ENABLE ROW LEVEL SECURITY;
ALTER TABLE doc FORCE ROW LEVEL SECURITY;
CREATE POLICY doc_org ON doc USING (org_id = current_setting('app.org_id')::bigint);
