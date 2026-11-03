-- Indexed with its sequence number (000001), never confused with the schema.
--
-- In the intended workflow, schema/ is canonical and the migrations are
-- derived from it: this migration reflects last_seen_at, already present in
-- schema/tables/users.sql. The two must stay consistent.
ALTER TABLE users ADD COLUMN last_seen_at TIMESTAMPTZ;
