---
title: "Named queries"
description: "The query file format: headers, parameters, cardinality, variants, fragments."
---

The format is stable: changes are additive. Reference implementation:
`crates/pgweave-query`.

A named query is a SQL statement, written in a `.sql` file of the
`[queries] directory` (see [`pgweave.toml`](../config/)), preceded by a
header that gives it a name. It is the unit from which typed calls are
generated: one function per query, one parameter struct, one row struct
(Rust today, TypeScript next).

`pgweave check` and the editor check the format of these files, each query
against the schema, and anything that would prevent describing it (untyped
parameter, contradicted cardinality). `pgweave describe` produces the
descriptors; their use in Rust is described in
[Typed queries in Rust](../../guides/rust/).

## Example

```sql
-- queries/pages.sql
-- Comments before the first header document the file.

-- name: page_by_id :optional
-- A page by its identifier.
SELECT id, title, creation_date
FROM pages
WHERE id = $id;

-- name: list_recent
SELECT id, title
FROM pages
ORDER BY creation_date DESC
LIMIT $limit;

-- name: rename_page :one
UPDATE pages
SET title = $title
WHERE id = $id
RETURNING id, title;
```

## Grammar

```
file         := preamble? query*
preamble     := (comment | blank line)*
query        := header doc* statement
header       := "-- name:" name cardinality? reference? END_OF_LINE  (at start of line)
name         := [a-z_][a-z0-9_]*
cardinality  := ":one" | ":optional" | ":many" | ":none"
reference    := "->" (module "::")* name
doc          := "--" text END_OF_LINE                         (before the statement)
statement    := a single SQL statement, optional trailing ";"
```

### Header

- Starts the line: `-- name:` at the end of a line of code is not a header
  (and it is an error, so that it is not silently lost).
- Whitespace around `name:` is free; `-- name: x` is the canonical form.
- A `-- name:` inside a string or a `$$…$$` body is never a header: the
  splitting relies on the SQL tokenizer, not on lines.

### Name

- ASCII `snake_case`: `[a-z_][a-z0-9_]*`. Usable as-is as a Rust
  identifier, losslessly convertible to `camelCase` for TypeScript.
- Unique within the file. The **module** is the file's path relative to its
  query directory, without extension: `queries/admin/pages.sql` →
  `admin::pages` in Rust (`queries::admin::pages::page_by_id`). Two files
  can therefore define the same name. Each segment must be a valid Rust
  module name (snake_case).

### Cardinality

Number of rows returned:

| Marker | Meaning | Generated call |
|---|---|---|
| `:one` | exactly one row | `fetch_one` → `Row` |
| `:optional` | zero or one row | `fetch_optional` → `Option<Row>` |
| `:many` | zero, one or more | `fetch_all` / stream → `Vec<Row>` |
| `:none` | no row (no `RETURNING`) | `execute` → number of affected rows |

The marker is **optional**: cardinality is inferred (equality on a unique
key → `:optional`, etc.):

- without a marker, the inference is authoritative;
- with a marker, it is checked against the inference: it can **tighten** a
  cautious inference (`:one` where the analysis sees `:optional`, when the
  caller knows a row exists) but not contradict it (`:none` on a `SELECT`
  is an error).

### Documentation

The `--` lines between the header and the statement are the query's
documentation, carried over into the generated code.

### Statement

- **A single** statement per query; the trailing `;` is optional.
- Any SQL outside a query (before the first header, or a second statement
  under the same header) is an error.
- A query file is a `.sql`. It can insert the project's **fragments**
  (`@name`, see below); statement macros (`@function`…) have no place
  there.

## Variant queries

When the plan must depend on which filters are present, or the ordering
varies, use conditional blocks and choices:

```sql
-- name: orders_page :many
SELECT o.id, o.status, o.placed_at
FROM orders o
WHERE o.restaurant_id = $restaurant
[[ AND o.status = $status ]]
[[ AND o.placed_at >= $since ]]
ORDER BY {{ sort: recent = o.placed_at DESC NULLS LAST, o.id DESC | oldest = o.id }}
LIMIT $limit;
```

- `[[ … ]]`: included if its parameter is provided (`Option<T>` in Rust).
  A block depends on **a single** parameter, used only in blocks, without
  `?`; a two-value filter is written as two blocks.
- `{{ name: a = … | b = … }}`: a choice, a Rust enum
  (`OrdersPageSort::Recent`). An alternative contains no parameter.
- Each combination is a **variant**, described at compile time (at most
  256); all must return the same columns. An error in a variant is
  reported at its position, with the variant as a note.
- At run time, the generated function picks the variant with an
  exhaustive `match`; no SQL is composed.
- `[[` after an expression (subscript or array constructor, `ARRAY[[`)
  does not open a block; nothing is recognized inside a string or a
  comment. No block inside a block, nor a choice inside a block.

## Fragments

A recurring piece of SQL (column list, expression, predicate) is written
once, in a `.sql` of `[macros] directory`, and inserted with `@name`:

```sql
-- macros/pages.sql
-- fragment: page_card(p)
-- A page's card, as lists display it.
p.id, p.title, p.status

-- fragment: published(p)
p.status = 'live' AND p.published_on <= current_date
```

```sql
-- name: recent_pages :many
SELECT @page_card(x) FROM pages x WHERE @published(x) ORDER BY x.id DESC;
-- analyzed and sent:
SELECT x.id, x.title, x.status FROM pages x
WHERE (x.status = 'live' AND x.published_on <= current_date) ORDER BY x.id DESC
```

- A fragment's parameters are **qualifiers**: `@page_card(x)` replaces
  `p.` with `x.`, `@page_card` removes it (`id, title, status`). Nothing
  else is substituted.
- A body that is a non-atomic expression is inserted in parentheses (the
  neighboring operator cannot split it); a list, an aliased item or an
  atom is inserted as-is.
- A fragment can contain parameters (`$status`): they become the query's,
  in order of first appearance **after** insertion.
- The fragment is checked where it is inserted; an error coming from its
  text is reported at the invocation (`@page_card(x)`).
- Fragments are also usable in the schema (`CREATE INDEX … ((@expr))`):
  the expression of an index and that of the queries that must use it can
  no longer diverge.

## Reference rows

`-> other` declares that the query returns rows of the same shape as those
of query `other` in the same file; `-> admin::pages::other` designates one
in another file, by its module from the root of the query directory. The
generated code reuses its row struct (`OtherRow`, or
`super::admin::pages::OtherRow`) instead of creating one.

```sql
-- name: page_card :optional
SELECT @page_card(p) FROM pages p WHERE p.id = $id;

-- name: pages_of_author :many -> page_card
SELECT @page_card(p) FROM pages p WHERE p.author = $author;
```

Checked, never forced: same columns in the same order, of the same type; a
column that is non-null in the reference must be non-null here too (a
nullable column in the reference accepts an always-present value). The
reference is a query in the same query directory, which does not itself
reuse the rows of another.

## Parameters

Two notations, **never mixed** within the same query:

| Notation | Example | Recommended |
|---|---|---|
| Named | `$page_id` | yes |
| Positional | `$1`, `$2` | for existing SQL |

Named parameters:

- same rule as query names: ASCII `snake_case` (`$page_id`, not
  `$pageId`);
- the same name used several times is **a single** parameter;
- the order of parameters is the order of **first appearance**;
- `$name` immediately followed by `$` is the start of a dollar-quoted
  string (`$tag$…$tag$`), not a parameter — a PostgreSQL rule;
- a `$name` inside a string or a comment is not a parameter.

Positional parameters: from `$1` to `$n` with no gap.

### Optional parameter: `$name?`

A `?` attached to the name declares that the value
may be missing: the parameter is nullable in the descriptor, `Option<T>` in
Rust, whatever the analysis infers. The query is written for it:

```sql
-- name: orders_of_restaurant :many
SELECT o.id, o.status FROM orders o
WHERE o.restaurant_id = $restaurant
  AND ($status?::order_status IS NULL OR o.status = $status);
```

- The marker on one occurrence is enough; it is removed from the SQL sent
  (`$2::order_status IS NULL OR o.status = $2`).
- A single prepared query: a missing filter is a true condition. When the
  plan must depend on which filters are present, see
  [variant queries](#variant-queries).
- The jsonb `?` operator is written surrounded by spaces
  (`$doc ? 'key'`); a positional parameter (`$1?`) is never optional.

### Rewriting

The SQL sent to the server is always positional. Each `$name` is replaced
by `$n`, `n` being its rank of first appearance:

```sql
UPDATE pages SET title = $title WHERE id = $id
-- sent:
UPDATE pages SET title = $1 WHERE id = $2
```

The rest of the text is preserved byte for byte (`$id::bigint` casts,
comments, formatting).

## Diagnostics

| Code | Case |
|---|---|
| `query-header-invalid` | invalid name, unknown cardinality, extra text, header not at start of line |
| `query-without-name` | SQL outside a named query (reported once per file) |
| `query-empty` | header without a statement |
| `query-multiple-statements` | more than one statement under a header |
| `query-duplicate-name` | name already used in the file (the first definition is kept) |
| `query-mixed-parameters` | `$name` and `$n` in the same query |
| `query-parameter-gap` | `$1`, `$3` without `$2` |
| `query-parameter-invalid` | `$0`, `$pageId` |
| `query-file-extension` | query file that is not a `.sql` |
| `parse-error` | file the SQL tokenizer cannot read (unterminated string…) |
| `macro-error` | unknown fragment in macro position, invalid arguments, recursive fragments |
| `query-row-mismatch` | `-> other`: reference not found or incompatible rows |

They add to the usual diagnostics (`column-not-found`, …) and follow
`[rules]` and the `-- pgweave-disable…` directives like all the others.
