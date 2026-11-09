---
title: "Checking SQL"
description: "pgzest check — errors as PostgreSQL would report them, what is not analyzed, and how to tune both."
---

```sh
pgzest check schema/*.sql queries/*.sql
```

`check` reports what PostgreSQL would refuse, with PostgreSQL's own words,
at the faulty expression rather than the whole statement. On the demo's
sandbox:

```text
8:34: error[function-not-found]: operator does not exist: text = integer
  hint: No operator matches the given name and argument types. You might need to add explicit type casts.
11:12: error[column-not-found]: column "title" does not exist
14:15: error[table-not-found]: relation "meals" does not exist
24:1: error[query-parameter-untyped]: the type of parameter $x is not established by its usage
  hint: specify the type with a cast: $x::type
28:1: error[query-cardinality-mismatch]: declared `none`, but the query returns rows (inferred: `many`)
32:50: error[generated-column]: cannot insert a non-DEFAULT value into column "id"
35:8: error[default-not-allowed]: DEFAULT is not allowed in this context
39:8: error[ambiguous-column]: column reference "id" is ambiguous
```

It exits 1 on an error, 0 otherwise: ready for CI. Every code is listed
in the [diagnostics reference](../../reference/diagnostics/).

## What is checked

- **Statements in order.** Each one is checked against the catalog as it
  stands at that point: a column added by a later migration does not
  exist yet.
- **Queries**: names resolved, overloads chosen as PostgreSQL chooses
  them, types compatible, `DEFAULT` and generated columns where
  PostgreSQL accepts them, the declared cardinality of a named query
  against the inferred one.
- **PL/pgSQL bodies**: variables, `%TYPE`, records, cursors, `RAISE`
  arguments, the queries inside the body with their variables typed — and
  a trigger function once per table that fires it, `NEW` and `OLD` of
  that table's type.
- **Table expressions**: `DEFAULT`, `CHECK`, generated columns, domain
  checks, in the context of their table.

## Nothing is guessed

A statement the parser cannot read, or a construct the analysis does not
cover, is reported instead of skipped silently — an incomplete tool that
says so, rather than a misleading one:

| Code | Meaning | Default |
|------|---------|---------|
| `unparsable-statement` | the parser could not read it: valid PostgreSQL outside the supported scope, or a syntax error | `warn` |
| `unsupported-statement` | valid, deliberately not modeled (`GRANT`…) | `warn` |
| `unparsed-function-body` | a PL/pgSQL body the parser could not read: it is not analyzed | follows `unparsable` |
| `syntax-not-modeled` | read, but part of it is lost (`DEFERRABLE` on a foreign key…): the analysis, the migrations and the emitted SQL lack it | `warn` |
| `not-analyzed` | an expression or construct the analysis does not conclude on | `info` |

`syntax-not-modeled` comes from a check of every statement: each word
written must come back in what pgzest emits from its model of the
statement. A word that does not is a gap of pgzest, not of your SQL; the
warning's hint links to a prefilled GitHub issue (the lost words and the
kind of statement, never your SQL), for anyone to report it or close it.

```toml
# pgzest.toml
[parse]
unparsable  = "warn"    # error | warn | ignore
unsupported = "warn"
```

```sh
pgzest check q.sql --on-unparsable error    # tighten in CI
```

A statement the parser could not read in a schema file also removes the
objects it defines: `check` warns that the following `does not exist`
errors may come from it.

## Severity and suppressions

Any code's severity can be changed for the project:

```toml
[rules]
"L001" = "off"
"L005" = "error"
```

Or silenced at one place, ESLint-style, in a line (`--`) or block
(`/* */`) comment:

```sql
-- pgzest-disable-next-line                          -- every rule
-- pgzest-disable-next-line column-not-found         -- one
-- pgzest-disable-next-line column-not-found, L001   -- several
-- pgzest-enable-next-line L001                      -- re-enabled locally
-- pgzest-disable                                    -- start of a block
-- pgzest-enable                                     -- end of the block
-- pgzest-disable-file                               -- the whole file
```

A justification can follow a second `--`:

```sql
-- pgzest-disable-next-line unparsable-statement -- PG 18 syntax, see #214
```

## The project's scope

`check` and the editor share one model: every file of the project is
replayed into the catalog, then the files you ask about are reported on.
The scope is what `pgzest.toml` declares:

```toml
[migrations]
directory = "migrations"     # replayed in order
[schema]
directory = "schema"         # walked recursively
include = ["db/functions"]   # more directories
[queries]
directory = "queries"        # named queries, on the final schema
```

Without `[schema] directory`, the whole project is the scope.
`node_modules/`, `target/`, `dist/` and hidden directories are never
walked. `--no-project` checks a file on its own, `--schema other.sql`
adds a schema source.

## Lint

```sh
pgzest lint query.sql
```

Style and practice rules — `SELECT *`, a table without a primary key,
`NOT IN (subquery)`, an unindexed foreign key, a leading `%` in `LIKE`… —
listed with the [diagnostics](../../reference/diagnostics/#lint).
