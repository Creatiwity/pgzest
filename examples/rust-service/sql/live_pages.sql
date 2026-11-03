-- Pages with a given status, most recent first.
SELECT id, title
FROM pages
WHERE status = 'live'
  AND title ILIKE $pattern
ORDER BY created_at DESC;
