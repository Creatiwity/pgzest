-- Ce fichier doit être entièrement propre.
--
-- `users` et `organizations` ne sont pas définies ici : elles viennent de
-- schema/tables/, indexé automatiquement via pgweave.toml.
-- Aucun --schema à passer.

SELECT u.email, o.name
FROM users u
JOIN organizations o ON o.id = u.org_id
WHERE u.is_admin = true;

-- last_seen_at vient de migrations/000001_init.sql (ALTER TABLE).
SELECT email, last_seen_at FROM users;
