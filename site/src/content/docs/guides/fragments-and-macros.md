---
title: "Fragments and macros"
description: "SQL written once and inserted with @name; @function, @procedure and @trigger without the boilerplate."
---

Two ways to stop repeating SQL. Both expand to plain SQL — readable by
`psql`, `pg_dump` and every migration tool — and diagnostics point at
what you wrote, not at the expansion.

## Fragments

A recurring piece of SQL — a column list, an expression, a predicate — is
written once in a `.sql` file of the macros directory (`macros/` by
default), and inserted with `@name` anywhere: schema, migrations, named
queries, Rust strings.

```sql
-- macros/search.sql
-- fragment: search_vector(t)
-- Must stay identical to the index expression: it is written only here.
to_tsvector('french'::regconfig, coalesce(t.nom, ''))
```

```sql
CREATE INDEX etab_search ON etablissements USING gin ((@search_vector));

-- name: search
SELECT e.siret FROM etablissements e
WHERE @search_vector(e) @@ plainto_tsquery('french', $q);
```

- Parameters are relation qualifiers, nothing else: `@search_vector(e)`
  turns `t.` into `e.`, `@search_vector` removes it.
- An expression body is inserted in parentheses, so the surrounding
  `AND` cannot split it; nesting is allowed, cycles are refused.
- The fragment is checked in every statement that inserts it, and an
  error in it is reported on the `@name(…)` that brought it.
- Hovering `@name` in the editor shows its documentation — the comments
  above it — and the SQL it inserts.

A query can also reuse the row type of another one, its columns checked
name by name: `-- name: vegetarian_dishes -> dish_card`. See
[named queries](../../reference/named-queries/).

## Routine macros

```sql
@function user_count(target_org UUID): INTEGER stable {
    DECLARE total INTEGER;
    SELECT count(*) INTO total FROM users WHERE org_id = target_org;
    RETURN total;
}
```

expands to the `CREATE OR REPLACE FUNCTION … LANGUAGE plpgsql AS $$
DECLARE … BEGIN … END $$` it stands for; a `DECLARE` written next to its
use moves before `BEGIN`.

| Macro | Expands to |
|---|---|
| `@function name(params)[: TYPE] [attributes] { … }` | a PL/pgSQL (or SQL) function |
| `@procedure name(params) [attributes] { … }` | a procedure |
| `@trigger name timing events on table(s) for each row\|statement [when (…)] { … }` | the trigger function **and** its `CREATE TRIGGER` |

`@function` attributes: `immutable`, `stable`, `volatile`,
`security_definer`, `strict`, `leakproof`, `language <lang>`, `cost N`,
`rows N`. Without `: TYPE`, a function returns `void`.

```sql
@trigger orders_award_loyalty after update of status on orders for each row
    when (NEW.status = 'served' AND OLD.status IS DISTINCT FROM 'served') {
    UPDATE customers
    SET loyalty_points = loyalty_points + loyalty_points_of(NEW.id)
    WHERE id = NEW.customer_id;
    RETURN NULL;
}
```

`RETURN` stays explicit: an `AFTER` or a `FOR EACH STATEMENT` trigger does
not follow the conventions of a `BEFORE … FOR EACH ROW`, and an implicit
rule varying with timing would be a trap. On several tables —
`on (users, organizations)` — one function and one trigger are generated
per table.

Inside these bodies, pgzest adds what plain PL/pgSQL lacks:
[isolated temp tables, snapshots and run logs](../observability/).

## Expanding

`pgzest check` and the editor expand on their own. To produce plain SQL
for `psql` or a deployment:

```sh
pgzest macros               # the available macros
pgzest build q.pgsql        # prints the expansion
pgzest build q.pgsql --out-dir gen
```

In VS Code, **⌘⌥E** (`Ctrl+Alt+E`) opens the expansion of the macro under
the cursor in a read-only tab.

Your own macros are declared in the same format as the built-in ones:
[macro format](../../reference/macro-format/).
