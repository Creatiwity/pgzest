-- name: insert_page
INSERT INTO pages (slug, title, tags, author)
VALUES ($slug, $title, $tags, $author)
RETURNING id, status, created_at;

-- name: page_by_id
SELECT id, slug, title, status, tags, author, published_on
FROM pages
WHERE id = $id;

-- name: pages_by_status
SELECT id, title
FROM pages
WHERE status = $status
ORDER BY id;

-- name: publish
UPDATE pages
SET status = 'live', published_on = $on
WHERE slug = $slug
RETURNING id, published_on;

-- name: count_pages
SELECT count(*) AS n FROM pages;

-- name: delete_drafts
DELETE FROM pages WHERE status = 'draft';
