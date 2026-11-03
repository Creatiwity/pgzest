-- Columns, aliases, literals, scalar expressions, aggregates.

-- name: all_columns
SELECT * FROM pages;

-- name: some_columns
SELECT id, title, creation_date FROM pages;

-- name: qualified_star
SELECT p.* FROM pages p;

-- name: aliases
SELECT id AS page_id, title AS page_title FROM pages;

-- name: literals
SELECT 1 AS one, 1.5 AS ratio, 'x' AS letter, true AS flag, NULL AS nothing;

-- name: arithmetic
SELECT views + 1 AS next_views, views * 2.5 AS scaled, id + views AS mixed FROM pages;

-- name: text_ops
SELECT title || ' — ' || slug AS label, upper(title) AS shout, length(title) AS len FROM pages;

-- name: comparisons
SELECT views > 10 AS popular, body IS NULL AS empty, title LIKE 'A%' AS starts_a FROM pages;

-- name: case_coalesce
SELECT CASE WHEN published THEN 'live' ELSE 'draft' END AS status,
       coalesce(body, '') AS body_or_empty,
       coalesce(rating, 0) AS rating_or_zero
FROM pages;

-- name: casts
SELECT id::text AS id_text, views::bigint AS views_big, creation_date::date AS day FROM pages;

-- name: aggregates
SELECT count(*) AS n, count(body) AS with_body, sum(views) AS total_views,
       avg(views) AS mean_views, max(creation_date) AS latest, min(rating) AS worst
FROM pages;

-- name: group_by
SELECT published, count(*) AS n FROM pages GROUP BY published;

-- name: now_and_arrays
SELECT now() AS at, tags, array_length(tags, 1) AS tag_count FROM pages;

-- name: by_id :optional
SELECT id, title FROM pages WHERE id = $id;

-- name: search
SELECT id, title FROM pages WHERE title ILIKE $pattern AND views >= $min_views LIMIT $limit;

-- name: by_ids
SELECT id FROM pages WHERE id = ANY($ids);

-- name: in_window
SELECT id FROM pages WHERE creation_date BETWEEN $from AND $to;
