-- Pages d'un statut, les plus récentes d'abord.
SELECT id, title
FROM pages
WHERE status = 'live'
  AND title ILIKE $pattern
ORDER BY created_at DESC;
