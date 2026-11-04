-- This file should be entirely clean.
--
-- `users` and `organizations` are not defined here: they come from
-- schema/tables/, indexed automatically through pgzest.toml.
-- No --schema to pass.

SELECT u.email, o.name
FROM users u
JOIN organizations o ON o.id = u.org_id
WHERE u.is_admin = true;

-- last_seen_at comes from migrations/000001_init.sql (ALTER TABLE).
SELECT email, last_seen_at FROM users;
