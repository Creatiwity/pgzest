-- Indexée avec sa séquence (000001), jamais confondue avec le schéma.
--
-- Dans le workflow visé, schema/ est canonique et les migrations en sont
-- dérivées : cette migration reflète last_seen_at, déjà présente dans
-- schema/tables/users.sql. Les deux doivent rester cohérents.
ALTER TABLE users ADD COLUMN last_seen_at TIMESTAMPTZ;
