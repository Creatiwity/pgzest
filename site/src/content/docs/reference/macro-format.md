---
title: "Macro format"
description: "Declaring a macro in a .pgmacro file — the format the built-in @function, @procedure and @trigger are written in."
---

A project macro is a `*.pgmacro` file in the macros directory (`[macros]
directory`, `macros/` by default). The built-in `@function`, `@procedure`
and `@trigger` are written in this same format, and a project macro with
the name of a built-in one replaces it.

```text
macro audited
header {name:ident}

template
CREATE TABLE {name} (
{body},
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
end
```

```sql
@audited audit_log {
    id   UUID PRIMARY KEY,
    note TEXT
}
```

## Directives

| Directive | Meaning |
|-----------|---------|
| `macro <name>` | the name invoked, `@name` |
| `header <pattern>` | what follows `@name` up to the opening brace: holes `{name:kind}`, optional groups between `[ ]`, the rest literal |
| `default <hole> = <text>` | the value of a hole left out |
| `flag <word> -> <text>` | a keyword whose presence renders `<text>` in `{options}` |
| `option <word>[ = <default>] -> <text with {}>` | a keyword taking a value, rendered in `{options}` |
| `wrap <hole> -> <text with {}>` | how an optional hole renders when given; absent, it renders nothing (`\n` is a line break) |
| `template` … `end` | the expansion: `{hole}` interpolates, `{body}` inserts the block, `{declares}` the `DECLARE`s found in it, `{options}` the flags and options |

Lines starting with `#` are comments.

## Hole kinds

A closed set, which keeps the format from drifting into a template
language:

| Kind | Takes |
|------|-------|
| `ident` | an identifier, possibly qualified: `users`, `public.users` |
| `word` | a single word: `before`, `row` |
| `words` | several words: `insert or update`, `update of status` |
| `parens` | the content up to the matching parenthesis, verbatim — the pattern writes the parentheses: `({params:parens})` |
| `type` | a SQL type, `SETOF x` and `TABLE (…)` included |
| `list` | an identifier, or a parenthesized list: `users`, `(a, b, c)` — when the hole is named `tables`, the macro is rendered once per table |
| `keywords` | the rest, up to the brace: flags and options |

Every hole the header captures must appear in the template: a definition
that would drop what the user wrote is refused when it loads.

## The built-in `@trigger`

```text
macro trigger
header {name:ident} {timing:word} {events:words} on {tables:list} for each {each:word}[ when ({when:parens})]
wrap when -> \nWHEN ({})

template
CREATE OR REPLACE FUNCTION {name}_fn()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $pgzest_macro$
{declares}BEGIN
{body}
END;
$pgzest_macro$;

CREATE TRIGGER {name}
{timing} {events} ON {tables}
FOR EACH {each}{when}
EXECUTE FUNCTION {name}_fn();
end
```

PL/pgSQL bodies between `$pgzest_macro$` tags are the ones pgzest
instruments: [isolated temp tables, snapshots, run logs](../../guides/observability/).
