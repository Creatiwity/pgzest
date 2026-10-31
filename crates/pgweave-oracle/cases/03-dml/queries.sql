-- INSERT / UPDATE / DELETE, avec et sans RETURNING.

-- name: insert_page :none
INSERT INTO pages (id, title, body) VALUES ($id, $title, $body);

-- name: insert_returning :one
INSERT INTO pages (id, title) VALUES ($id, $title) RETURNING id, title, views;

-- name: upsert :one
INSERT INTO pages (id, title) VALUES ($id, $title)
ON CONFLICT (id) DO UPDATE SET title = excluded.title, updated_at = now()
RETURNING id, updated_at;

-- name: rename :optional
UPDATE pages SET title = $title, updated_at = now() WHERE id = $id RETURNING id, title;

-- name: bump_views :none
UPDATE pages SET views = views + $delta WHERE id = $id;

-- name: delete_page :optional
DELETE FROM pages WHERE id = $id RETURNING id;

-- name: insert_select :none
INSERT INTO pages (id, title) SELECT id + 1000, title FROM pages WHERE views > $threshold;
