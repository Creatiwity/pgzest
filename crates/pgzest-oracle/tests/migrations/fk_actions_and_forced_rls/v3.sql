-- No longer forced; the action covers the whole key again.
CREATE TABLE org (id BIGINT PRIMARY KEY);
CREATE TABLE member (org_id BIGINT REFERENCES org, id BIGINT, PRIMARY KEY (org_id, id));
CREATE TABLE doc (
    id BIGINT PRIMARY KEY,
    org_id BIGINT NOT NULL REFERENCES org,
    author BIGINT DEFAULT 0,
    FOREIGN KEY (org_id, author) REFERENCES member ON DELETE SET NULL
);
ALTER TABLE doc ENABLE ROW LEVEL SECURITY;
CREATE POLICY doc_org ON doc USING (org_id = current_setting('app.org_id')::bigint);
