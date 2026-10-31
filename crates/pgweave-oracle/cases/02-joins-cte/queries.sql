-- Jointures, sous-requêtes, CTE, opérations ensemblistes.

-- name: inner_join
SELECT p.id, p.title, a.name FROM pages p JOIN authors a ON a.id = p.author_id;

-- name: left_join
SELECT p.id, a.name AS author_name, a.email FROM pages p LEFT JOIN authors a ON a.id = p.author_id;

-- name: using_join
SELECT title, name FROM pages JOIN authors ON authors.id = pages.author_id WHERE authors.id = $author_id;

-- name: scalar_subquery
SELECT a.id, (SELECT count(*) FROM pages p WHERE p.author_id = a.id) AS page_count FROM authors a;

-- name: exists_filter
SELECT a.name FROM authors a WHERE EXISTS (SELECT 1 FROM pages p WHERE p.author_id = a.id);

-- name: cte
WITH popular AS (
    SELECT author_id, sum(views) AS total FROM pages GROUP BY author_id
)
SELECT a.name, popular.total FROM popular JOIN authors a ON a.id = popular.author_id;

-- name: union_all
SELECT id::text AS key, title AS label FROM pages
UNION ALL
SELECT id::text, name FROM authors;

-- name: aggregate_join
SELECT a.id, a.name, count(p.id) AS pages, coalesce(sum(p.views), 0) AS views
FROM authors a LEFT JOIN pages p ON p.author_id = a.id
GROUP BY a.id, a.name;
