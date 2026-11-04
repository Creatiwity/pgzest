---
title: "How it works"
description: "From SQL files to a catalog, typed queries, migrations and editor feedback — without a database."
---

pgweave never reads your schema from a database. It rebuilds it.

## 1. The catalog, replayed

The project's schema files, or its migrations in order, are parsed and
replayed on a model of PostgreSQL's catalog: tables, columns, types,
domains, enums, functions, operators, casts, triggers, policies,
extensions. The built-in part — every function, operator and cast of
PostgreSQL 18, and of extensions such as `citext`, `pg_trgm` or PostGIS —
is generated from a real server and versioned in the repository.

Each statement is checked as it applies, against the state of the catalog
at that point, as PostgreSQL would run it. A PL/pgSQL body is checked
against the final state; a trigger function once per table that fires
it, `NEW` and `OLD` taking that table's row type.

## 2. Queries, typed

A query is normalized into its logical evaluation order — `FROM`, `JOIN`,
`WHERE`, `GROUP BY`, `SELECT` — then typed: overloads resolved, unknown
literals and parameters typed by their use, nullability carried through
joins and expressions. The result is a **descriptor**: the parameters,
the columns with their types and nullability, the number of rows
(`none`, `one`, `optional`, `many`), the objects it depends on.

What the analysis cannot establish is reported (`not-analyzed`,
`query-parameter-untyped`…), never filled in with a guess.

## 3. Everything else reads the descriptor

- **Rust**: `queries!`, `sql!`, `query!` or `build.rs` turn descriptors
  into functions, parameter and row structs, enums — at compile time.
- **Migrations**: two catalogs — the migrations' and the declarative
  schema's — are compared object by object, and the difference is written
  as a migration, verified by replaying it.
- **Editor**: the language server shares the same project database, with
  the unsaved text of open files taking precedence over the disk.

## Checked against PostgreSQL

An oracle keeps the model honest. It applies the same schemas and
prepares the same queries on a real PostgreSQL 18, and compares what
PostgreSQL says with what pgweave says:

| What | Result |
|------|--------|
| Columns and parameters of 243 queries | 749/751 and 146/146 identical |
| Function and operator resolutions | 19,032/19,032 identical |
| Generated migrations | applied, they yield the declarative schema |
| Run logs, traces, isolated temp tables | behavior checked on the server |

The two columns left are a `ROWS FROM (…)` construct the SQL parser does
not read yet: they are reported, not guessed. The oracle runs in CI on
every change; any difference, an improvement included, fails it until
reviewed.

## Under the hood

pgweave is a Rust workspace. The parser is
[`sqlparser-rs`](https://github.com/apache/datafusion-sqlparser-rs) with a
PostgreSQL dialect wrapper, plus a PL/pgSQL parser of its own; the
semantic analysis, catalog, descriptors, code generation, migrations and
language server are separate crates sharing one AST. The design
decisions and the phase plan are in
[`PLAN.md`](https://github.com/Creatiwity/pgweave/blob/main/PLAN.md).
