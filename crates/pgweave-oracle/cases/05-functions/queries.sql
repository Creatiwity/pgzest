-- SQL and PL/pgSQL functions called from a query, procedure.

-- name: table_function
SELECT * FROM page_stats($min_views);

-- name: table_function_columns
SELECT page_id, score FROM page_stats(10) WHERE page_title <> '';

-- name: scalar_function :one
SELECT page_title($id) AS title;

-- name: call_procedure :none
CALL reset_views($id);
