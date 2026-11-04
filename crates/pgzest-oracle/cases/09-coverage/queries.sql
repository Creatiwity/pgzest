-- Coverage: windows, ordered aggregates, LATERAL, recursion, sets,
-- intervals, full text, regular expressions, advanced DML.

-- name: window_rank
SELECT name, budget, rank() OVER (ORDER BY budget DESC), row_number() OVER (PARTITION BY level ORDER BY id), dense_rank() OVER w FROM teams WINDOW w AS (ORDER BY name);

-- name: window_value_functions
SELECT sensor, at, lag(value) OVER (PARTITION BY sensor ORDER BY at), lead(value, 2, 0) OVER (PARTITION BY sensor ORDER BY at), first_value(at) OVER (PARTITION BY sensor ORDER BY at) FROM readings;

-- name: window_frame
SELECT sensor, avg(value) OVER (PARTITION BY sensor ORDER BY at ROWS BETWEEN 2 PRECEDING AND CURRENT ROW), sum(value) OVER (ORDER BY at RANGE BETWEEN INTERVAL '1 hour' PRECEDING AND CURRENT ROW) FROM readings;

-- name: ntile_percent
SELECT id, ntile(4) OVER (ORDER BY salary), percent_rank() OVER (ORDER BY salary), cume_dist() OVER (ORDER BY salary) FROM people;

-- name: ordered_aggregates
SELECT team_id, string_agg(nick, ', ' ORDER BY nick), array_agg(id ORDER BY born DESC), jsonb_agg(mail) FROM people GROUP BY team_id;

-- name: within_group
SELECT percentile_cont(0.5) WITHIN GROUP (ORDER BY salary), percentile_disc(0.9) WITHIN GROUP (ORDER BY born), mode() WITHIN GROUP (ORDER BY team_id) FROM people;

-- name: filter_aggregates
SELECT count(*) FILTER (WHERE salary > 1000), sum(salary) FILTER (WHERE born IS NOT NULL), bool_and(salary > 0), bool_or(nick IS NULL) FROM people;

-- name: statistical_aggregates
SELECT stddev(salary), variance(price), corr(price, 1.0), every(price > 0), max(born), min(note) FROM people, bookings;

-- name: lateral_top_n
SELECT t.name, p.mail FROM teams t CROSS JOIN LATERAL (SELECT mail FROM people WHERE team_id = t.id ORDER BY salary DESC LIMIT 3) p;

-- name: lateral_left_join
SELECT t.id, last.at FROM teams t LEFT JOIN LATERAL (SELECT max(born) AS at FROM people WHERE team_id = t.id) last ON true;

-- name: recursive_tree
WITH RECURSIVE tree AS (
    SELECT id, parent_id, name, 1 AS depth, ARRAY[id] AS path FROM teams WHERE parent_id IS NULL
    UNION ALL
    SELECT c.id, c.parent_id, c.name, tree.depth + 1, tree.path || c.id FROM teams c JOIN tree ON c.parent_id = tree.id
)
SELECT id, name, depth, path FROM tree ORDER BY path;

-- name: materialized_cte
WITH rich AS MATERIALIZED (SELECT id, salary FROM people WHERE salary > $min)
SELECT count(*), max(salary) FROM rich;

-- name: set_operations
SELECT id FROM teams INTERSECT SELECT team_id FROM people EXCEPT SELECT 0;

-- name: union_types
SELECT salary FROM people UNION SELECT budget FROM teams UNION ALL SELECT 1.5;

-- name: generate_series_dates
SELECT d::date, extract(isodow FROM d) FROM generate_series(now(), now() + interval '7 days', interval '1 day') AS g(d);

-- name: unnest_ordinality
SELECT p.id, s.skill, s.n FROM people p, unnest(p.skills) WITH ORDINALITY AS s(skill, n);

-- name: rows_from
SELECT * FROM ROWS FROM (generate_series(1, 3), unnest(ARRAY['a', 'b'])) AS r(n, letter);

-- name: range_operators
SELECT id, lower(during), upper(during), during && tstzrange(now(), now() + interval '1 day'), upper_inf(during), seats @> 3, isempty(seats) FROM bookings;

-- name: range_param
SELECT id FROM bookings WHERE during @> $at::timestamptz;

-- name: range_aggregate
SELECT person, range_agg(during), range_intersect_agg(seats) FROM bookings GROUP BY person;

-- name: full_text
SELECT id, ts_rank(doc, to_tsquery('english', $q)), doc @@ plainto_tsquery($q2) FROM people WHERE doc @@ websearch_to_tsquery('simple', $q3);

-- name: regex
SELECT nick ~ '^a', nick ~* $pattern, nick !~ 'z', regexp_replace(nick, '\d+', ''), regexp_match(mail, '@(.*)$'), regexp_split_to_array(mail, '@'), nick SIMILAR TO 'a%' FROM people;

-- name: string_misc
SELECT left(nick, 2), right(nick, 2), lpad(nick, 10, '*'), repeat('x', 3), reverse(nick), initcap(nick), split_part(mail, '@', 2), format('%s <%s>', nick, mail), concat_ws(' ', nick, mail), md5(mail), starts_with(mail, 'a') FROM people;

-- name: numeric_functions
SELECT round(budget), round(budget, 1), trunc(price), ceil(price), floor(budget), abs(salary), sign(price), power(price, 2), sqrt(price), mod(salary, 7), div(budget, 3), random(), pi() FROM teams, bookings, people;

-- name: bit_ops
SELECT flags & B'00000001', flags | B'10000000', ~flags, flags << 1, get_bit(flags, 0), salary & 255, salary # 1 FROM readings, people;

-- name: date_misc
SELECT date_bin(interval '15 minutes', at, timestamptz '2024-01-01'), make_interval(days => 3), to_timestamp(0), justify_hours(interval '30 hours'), born + make_interval(years => 1), at::time, extract(epoch FROM at) FROM readings, people;

-- name: overlaps
SELECT (born, born + 30) OVERLAPS (current_date, current_date + 1) FROM people;

-- name: json_functions
SELECT jsonb_build_array(count(p.id), max(mail)), json_object_agg(nick, salary), jsonb_typeof(to_jsonb(min(born))), to_json(array_agg(nick)), row_to_json(t) FROM people p, teams t GROUP BY t.id;

-- name: json_table_like
SELECT x.k, x.v FROM jsonb_to_record('{"k": "a", "v": 2}'::jsonb) AS x(k text, v int);

-- name: exists_and_scalar
SELECT t.id, EXISTS (SELECT 1 FROM people p WHERE p.team_id = t.id), (SELECT count(*) FROM people p WHERE p.team_id = t.id), (SELECT max(salary) FROM people) FROM teams t;

-- name: not_exists_anti_join
SELECT t.name FROM teams t WHERE NOT EXISTS (SELECT FROM people p WHERE p.team_id = t.id);

-- name: any_all_subquery
SELECT id FROM people WHERE salary > ALL (SELECT salary FROM people WHERE team_id = $team) AND team_id = ANY (SELECT id FROM teams WHERE level = 'high');

-- name: row_comparison
SELECT id FROM people WHERE (team_id, salary) > ($team, $salary) AND ROW(team_id, born) IS NOT NULL;

-- name: distinct_from_params
SELECT id FROM people WHERE nick IS DISTINCT FROM $nick AND born IS NOT DISTINCT FROM $born;

-- name: nulls_ordering
SELECT id, nick FROM people ORDER BY nick DESC NULLS LAST, born ASC NULLS FIRST FETCH FIRST 10 ROWS ONLY;

-- name: limit_offset_params
SELECT id FROM people ORDER BY id LIMIT $limit OFFSET $offset;

-- name: group_by_ordinal_having
SELECT team_id, count(*) AS n FROM people GROUP BY 1 HAVING count(*) > $min ORDER BY n DESC;

-- name: group_by_alias_expression
SELECT date_trunc('year', born) AS y, avg(salary) FROM people GROUP BY y ORDER BY y;

-- name: cube_sets
SELECT team_id, level, count(*), grouping(team_id, level) FROM people JOIN teams ON teams.id = people.team_id GROUP BY CUBE (team_id, level);

-- name: domain_and_enum
SELECT mail, lower(mail), mail || '!', level, level > 'low', level::text, enum_range(NULL::level) FROM people, teams;

-- name: enum_param
SELECT id FROM teams WHERE level = $level OR level = ANY($levels);

-- name: insert_select_returning
INSERT INTO people (team_id, mail, nick) SELECT id, name || '@x.org', name FROM teams WHERE level = 'high' RETURNING id, mail, nick;

-- name: insert_on_conflict_update
INSERT INTO readings (sensor, at, value) VALUES ($sensor, $at, $value)
ON CONFLICT (sensor, at) DO UPDATE SET value = excluded.value + readings.value WHERE readings.value IS NOT NULL
RETURNING sensor, value;

-- name: insert_on_conflict_nothing
INSERT INTO teams (name) VALUES ($name) ON CONFLICT DO NOTHING RETURNING id;

-- name: update_from_join
UPDATE people p SET salary = p.salary + t.budget::int, nick = coalesce(p.nick, t.name) FROM teams t WHERE t.id = p.team_id AND t.level = $level RETURNING p.id, p.salary;

-- name: update_row_subquery
UPDATE teams SET (name, budget) = (SELECT name || '*', budget * 2 FROM teams t2 WHERE t2.id = teams.parent_id) WHERE parent_id IS NOT NULL;

-- name: delete_using_returning
DELETE FROM bookings b USING people p WHERE p.id = b.person AND p.team_id = $team RETURNING b.id, b.price, p.mail;

-- name: values_join
SELECT v.k, t.name FROM (VALUES (1, 'a'), (2, 'b')) AS v(k, label) JOIN teams t ON t.id = v.k;

-- name: case_in_aggregate
SELECT sum(CASE WHEN salary > 1000 THEN 1 ELSE 0 END), avg(CASE level WHEN 'high' THEN budget END) FROM people, teams;

-- name: coalesce_subquery
SELECT coalesce((SELECT max(price) FROM bookings WHERE person = p.id), 0) FROM people p;

-- name: array_functions
SELECT array_position(skills, 'sql'), array_remove(skills, 'x'), array_to_string(skills, ','), string_to_array(mail, '@'), skills && ARRAY[$skill], unnest(skills) FROM people;

-- name: uuid_and_hash
SELECT id, id::text, gen_random_uuid(), sha256(convert_to(note, 'UTF8')) FROM bookings;

-- name: interval_math
SELECT upper(during) - lower(during), (upper(during) - lower(during)) / 2, interval '1 hour' * 3, extract(hour FROM upper(during) - lower(during)) FROM bookings;

-- name: param_in_select_list
SELECT $label::text AS label, $n::int + 1, id FROM teams;

-- name: param_typed_by_function
SELECT id FROM people WHERE born > current_date - $days::int AND length(nick) < $len;

-- name: param_typed_by_insert_values
INSERT INTO bookings (person, during, price) VALUES ($person, tstzrange($from, $to), $price) RETURNING id;

-- name: count_distinct
SELECT count(DISTINCT team_id), count(DISTINCT (team_id, nick)) FROM people;

-- name: self_join_aliases
SELECT a.name AS child, b.name AS parent FROM teams a LEFT JOIN teams b ON b.id = a.parent_id;

-- name: full_outer_join_using
SELECT id, t.name, p.mail FROM teams t FULL JOIN people p USING (id);

-- name: natural_join
SELECT * FROM (SELECT id, name FROM teams) a NATURAL JOIN (SELECT id, mail FROM people) b;
