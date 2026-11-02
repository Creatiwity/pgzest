-- Motifs applicatifs : CTE modifiantes, JSON en FROM, regroupements,
-- fonctions courantes, pagination, paramètres tableaux.

-- name: cte_insert_returning
WITH new_user AS (INSERT INTO users (handle) VALUES ($handle) RETURNING id, handle)
SELECT id, handle, length(handle) FROM new_user;

-- name: cte_delete_then_count
WITH gone AS (DELETE FROM events WHERE at < $before RETURNING user_id)
SELECT user_id, count(*) FROM gone GROUP BY user_id;

-- name: cte_update_join
WITH bumped AS (UPDATE users SET karma = karma + 1 WHERE id = $id RETURNING id, karma)
SELECT p.title, b.karma FROM posts p JOIN bumped b ON b.id = p.author;

-- name: jsonb_each_from
SELECT u.handle, kv.key, kv.value FROM users u, jsonb_each(u.profile) AS kv;

-- name: jsonb_each_text_from
SELECT key, value FROM jsonb_each_text($doc::jsonb);

-- name: json_array_elements_from
SELECT e.value ->> 'name', jsonb_typeof(e.value) FROM events, jsonb_array_elements(payload -> 'items') AS e;

-- name: json_to_recordset
SELECT * FROM json_to_recordset('[{"a":1,"b":"x"}]') AS r(a int, b text);

-- name: jsonb_set_ops
SELECT jsonb_set(profile, '{name}', to_jsonb($name::text)), profile - 'secret', profile || '{"v": 2}', jsonb_strip_nulls(profile), profile #- '{a,b}' FROM users;

-- name: jsonb_path
SELECT jsonb_path_query_first(payload, '$.a'), jsonb_path_exists(payload, '$.b'), payload @? '$.c' FROM events;

-- name: rollup
SELECT kind, date_trunc('day', at) AS day, count(*) FROM events GROUP BY ROLLUP (kind, date_trunc('day', at));

-- name: grouping_sets
SELECT kind, user_id, count(*), grouping(kind, user_id) FROM events GROUP BY GROUPING SETS ((kind), (user_id), ());

-- name: string_toolbox
SELECT left(handle, 3), right(handle, 2), lpad(handle, 10, '*'), rpad(handle, 10), initcap(handle), reverse(handle), repeat(handle, 2), split_part(handle, '_', 1), replace(handle, '_', '-'), translate(handle, 'ab', 'xy') FROM users;

-- name: string_format
SELECT format('%s has %s karma', handle, karma), concat_ws(', ', handle, bio, karma), quote_ident(handle), quote_literal(bio), to_hex(karma), encode(sha256(handle::bytea), 'hex'), md5(bio) FROM users;

-- name: numeric_toolbox
SELECT power(karma, 2), sqrt(score), ln(score + 1), log(10, (score + 1)::numeric), mod(karma, 7), div(karma, 7), abs(score), sign(karma), width_bucket(score, 0, 100, 10), random(), pi(), greatest(karma, 0) FROM users, posts;

-- name: date_toolbox
SELECT make_date(2024, 1, 31), make_interval(days => 3), justify_hours(interval '30 hours'), date_bin('15 minutes', at, timestamp '2024-01-01'), to_timestamp(1700000000), to_date('2024-01-01', 'YYYY-MM-DD'), age(joined), extract(isodow FROM at) FROM users, events;

-- name: interval_math
SELECT at - interval '1 hour', interval '1 day' * 3, interval '1 day' / 2, joined - deleted_at, (deleted_at - joined) > interval '30 days', posted_on - current_date FROM users, posts, events;

-- name: keyset_pagination
SELECT id, title, posted_on FROM posts WHERE (posted_on, id) < ($last_date, $last_id) ORDER BY posted_on DESC, id DESC LIMIT $page_size;

-- name: offset_pagination
SELECT id, title, count(*) OVER () AS total FROM posts ORDER BY id LIMIT 20 OFFSET $offset;

-- name: array_param_uuid
SELECT handle FROM users WHERE id = ANY($ids::uuid[]);

-- name: array_param_in_insert
INSERT INTO posts (author, title, body, tags) VALUES ($author, $title, $body, $tags) RETURNING id, tags;

-- name: array_contains_param
SELECT id FROM posts WHERE tags @> $wanted AND NOT (tags && $blocked);

-- name: array_unnest_multi
SELECT a, b FROM unnest(ARRAY[1, 2], ARRAY['x', 'y']) AS t(a, b);

-- name: array_agg_ordered
SELECT author, array_agg(title ORDER BY score DESC), string_agg(DISTINCT tags[1], ', ') FROM posts GROUP BY author;

-- name: soft_delete_filter
SELECT handle, deleted_at IS NOT NULL AS deleted, coalesce(deleted_at, 'infinity') FROM users WHERE deleted_at IS NULL OR deleted_at > $since;

-- name: returning_old_new
UPDATE users SET karma = karma * 2 WHERE handle = $handle RETURNING old.karma AS before, new.karma AS after;

-- name: upsert_do_nothing
INSERT INTO users (handle, bio) VALUES ($handle, $bio) ON CONFLICT DO NOTHING RETURNING id;

-- name: upsert_where
INSERT INTO users (handle, karma) VALUES ($handle, $karma) ON CONFLICT (handle) DO UPDATE SET karma = users.karma + excluded.karma WHERE users.deleted_at IS NULL RETURNING karma;

-- name: delete_in_subquery
DELETE FROM posts WHERE author IN (SELECT id FROM users WHERE deleted_at IS NOT NULL) RETURNING id, title;

-- name: update_case_param
UPDATE posts SET score = CASE WHEN $boost THEN score * 2 ELSE score END WHERE id = $id RETURNING score;

-- name: insert_default_values
INSERT INTO users DEFAULT VALUES RETURNING id, handle, joined;

-- name: correlated_max
SELECT p.* FROM posts p WHERE p.score = (SELECT max(score) FROM posts q WHERE q.author = p.author);

-- name: any_subquery
SELECT handle FROM users WHERE karma > ALL (SELECT score::int FROM posts) OR id = ANY (SELECT author FROM posts);

-- name: lateral_function
SELECT u.handle, s.n FROM users u, LATERAL generate_series(1, u.karma) AS s(n);

-- name: window_named_frame
SELECT id, avg(score) OVER w, lead(title, 1, '') OVER w, nth_value(id, 2) OVER w FROM posts WINDOW w AS (PARTITION BY author ORDER BY posted_on RANGE BETWEEN interval '7 days' PRECEDING AND CURRENT ROW);

-- name: filter_and_case_agg
SELECT count(*) FILTER (WHERE kind = 'click') AS clicks, sum(CASE WHEN kind = 'view' THEN 1 ELSE 0 END) AS views, avg(extract(hour FROM at)) FROM events;

-- name: distinct_count_pairs
SELECT count(DISTINCT (user_id, kind)), count(DISTINCT kind) FROM events;

-- name: nullif_division
SELECT karma / nullif($divisor, 0), score / nullif(karma, 0) FROM users, posts;

-- name: boolean_params
SELECT id FROM users WHERE ($only_active::boolean IS NOT TRUE OR deleted_at IS NULL) AND karma >= coalesce($min_karma, 0);

-- name: text_search
SELECT id, to_tsvector('simple', title) @@ plainto_tsquery('simple', $q), ts_rank(to_tsvector(body), to_tsquery($q2)) FROM posts;

-- name: json_build_nested
SELECT json_build_object('user', u.handle, 'posts', (SELECT json_agg(json_build_object('id', p.id, 'title', p.title)) FROM posts p WHERE p.author = u.id)) FROM users u;

-- name: values_join
SELECT u.handle, v.label FROM users u JOIN (VALUES ('admin', 1), ('mod', 2)) AS v(label, rank) ON v.rank <= u.karma;

-- name: union_order_limit
(SELECT id::text AS ref, title FROM posts) UNION ALL (SELECT id::text, kind FROM events) ORDER BY ref LIMIT 10;

-- name: exists_param
SELECT EXISTS (SELECT 1 FROM users WHERE handle = $handle);

-- name: scalar_param_math
SELECT $a::numeric * 2, $b::int + $c::int, $d::text || '!';

-- name: order_by_output_name_over_join
SELECT DISTINCT ON (p.author, p.title) p.author, p.title
FROM posts p JOIN users u ON u.id = p.author
ORDER BY author, title;

-- name: group_by_prefers_input_column
SELECT u.karma AS id, count(*) FROM users u JOIN posts p ON p.author = u.id GROUP BY u.karma;

-- name: row_in_subquery
SELECT title FROM posts WHERE (author, posted_on) IN (SELECT user_id, at::date FROM events WHERE kind = $kind);

-- name: normalize_form
SELECT normalize(bio, NFKC), bio IS NOT NULL FROM users;

-- name: named_window
SELECT author, sum(score) OVER w FROM posts WINDOW w AS (PARTITION BY author ORDER BY posted_on);
