-- Schema objects: domains, composites, enums, views, schemas, generated
-- and identity columns, project functions, citext extension.

-- name: domain_columns
SELECT login, balance, balance + 1, login || '!', upper(login) FROM accounts;

-- name: domain_param
SELECT id FROM accounts WHERE login = $login AND balance > $min;

-- name: domain_insert
INSERT INTO accounts (login, balance) VALUES ($login, $balance) RETURNING id, login, balance;

-- name: composite_fields
SELECT (home).city, (home).zip, home FROM accounts;

-- name: composite_constructor
SELECT ROW('a', 'b', 'c')::address, (ROW('a', 'b', 'c')::address).city;

-- name: enum_ops
SELECT plan, plan::text, plan < 'team', plans_seen, plan = ANY(plans_seen), array_length(plans_seen, 1) FROM accounts;

-- name: enum_param_array
SELECT id FROM accounts WHERE plan = ANY($plans);

-- name: enum_range
SELECT enum_range(NULL::plan), enum_first(NULL::plan);

-- name: view_select
SELECT * FROM account_totals;

-- name: view_filter_param
SELECT login, total FROM account_totals WHERE total > $threshold ORDER BY total DESC;

-- name: schema_view
SELECT id, days_late, amount FROM billing.overdue;

-- name: qualified_table
SELECT i.id, i.due_on, i.due_on - i.issued_on AS terms, i.lines -> 0 FROM billing.invoices i;

-- name: identity_insert
INSERT INTO billing.invoices (account_id, amount, issued_on) VALUES ($account, $amount, $issued) RETURNING id, due_on;

-- name: setof_function
SELECT id, amount FROM invoices_of($account);

-- name: setof_function_star
SELECT * FROM invoices_of(1) AS inv;

-- name: returns_table_function
SELECT plan, n FROM plan_counts() ORDER BY n DESC;

-- name: rowtype_argument
SELECT account_label(a) FROM accounts a;

-- name: out_params_function
SELECT * FROM split_amount(100, 3);

-- name: out_params_scalar
SELECT split_amount($total, $parts);

-- name: citext_compare
SELECT id FROM accounts WHERE login = 'ADMIN@example.com';

-- name: json_aggregation
SELECT account_id, json_agg(amount), jsonb_agg(lines), jsonb_object_agg(id, amount) FROM billing.invoices GROUP BY account_id;

-- name: row_to_json
SELECT row_to_json(a), to_json(a.home) FROM accounts a;

-- name: regex
SELECT login ~ '^a', login ~* 'X', regexp_replace(login::text, 'a', 'b'), regexp_matches(login::text, '(.)'), substring(login::text FROM '@(.*)$') FROM accounts;

-- name: string_array
SELECT string_to_array('a,b', ','), array_to_string(plans_seen, ','), unnest(plans_seen) FROM accounts;

-- name: date_arith
SELECT current_date - issued_on, issued_on + 7, issued_on + time '10:00', now() - created, created::date, extract(epoch FROM created) FROM billing.invoices, accounts;

-- name: series_timestamps
SELECT d FROM generate_series(now(), now() + interval '7 days', interval '1 day') AS d;

-- name: integer_division
SELECT 7 / 2, 7.0 / 2, 7 % 2, 2 ^ 3, |/ 16, @ -5, 5 & 3, 5 | 3, 5 # 3, ~5, 1 << 4;

-- name: numeric_rounding
SELECT round(amount, 1), trunc(amount), ceil(amount), floor(amount), amount::int, round(2.5), round(2.5::float8) FROM billing.invoices;

-- name: window_frames
SELECT id, sum(amount) OVER (ORDER BY issued_on ROWS BETWEEN 2 PRECEDING AND CURRENT ROW), avg(amount) OVER (PARTITION BY account_id), ntile(4) OVER (ORDER BY amount), cume_dist() OVER (ORDER BY amount) FROM billing.invoices;

-- name: grouping_aggregates
SELECT account_id, every(amount > 0), bool_or(amount > 100), stddev(amount), variance(amount), count(DISTINCT issued_on) FROM billing.invoices GROUP BY account_id;

-- name: param_is_null_or
SELECT id FROM accounts WHERE ($plan::plan IS NULL OR plan = $plan) AND created > $since;

-- name: param_between
SELECT id FROM billing.invoices WHERE issued_on BETWEEN $start AND $end;

-- name: param_like_concat
SELECT id FROM accounts WHERE login::text LIKE $prefix || '%';

-- name: param_insert_select
INSERT INTO billing.invoices (account_id, amount, issued_on) SELECT id, $amount, current_date FROM accounts WHERE plan = $plan RETURNING id;

-- name: update_from_values
UPDATE accounts a SET plan = v.plan::plan FROM (VALUES (1, 'pro'), (2, 'team')) AS v(id, plan) WHERE a.id = v.id RETURNING a.id, a.plan;

-- name: update_composite_field
UPDATE accounts SET home.city = $city WHERE id = $id RETURNING home;

-- name: delete_returning_all
DELETE FROM billing.invoices WHERE issued_on < $before RETURNING *;

-- name: fetch_first_for_update
SELECT id, plan FROM accounts ORDER BY id FETCH FIRST 5 ROWS ONLY FOR UPDATE;

-- name: exists_correlated_view
SELECT a.login FROM accounts a WHERE NOT EXISTS (SELECT 1 FROM billing.overdue o WHERE o.account_id = a.id);

-- name: intersect_except
SELECT id FROM accounts INTERSECT SELECT account_id FROM billing.invoices EXCEPT SELECT 0;

-- name: nested_cte_view
WITH big AS (SELECT * FROM account_totals WHERE total > 100) SELECT b.login, b.invoices, i.amount FROM big b JOIN billing.invoices i ON i.account_id = b.id;

-- name: case_enum_text
SELECT CASE plan WHEN 'free' THEN 0 WHEN 'pro' THEN 10 ELSE 20 END AS price, CASE WHEN plan = 'free' THEN plan END FROM accounts;

-- name: coalesce_domain
SELECT coalesce(balance, 0), coalesce(home, ROW('', '', '')::address) FROM accounts;

-- name: array_param_unnest
SELECT x FROM unnest($ids::int[]) AS x;
