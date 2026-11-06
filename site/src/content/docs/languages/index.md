---
title: "Languages"
description: "One analysis, one query descriptor, a generator per language: Rust today, TypeScript next."
sidebar:
  label: "Overview"
---

pgzest analyzes SQL once, whatever language calls it. Each query becomes
a **descriptor** — its parameters, its columns with their types and
nullability, the number of rows it returns, the objects it depends on —
and a generator per language turns descriptors into typed code.

What happens on the SQL side is shared: the analysis, the migrations,
the editor on `.sql` files, the instrumented routines. What is per
language is the generated code and its runtime: the functions and types,
SQL written inside the language's own strings, the collector that turns
routine runs into OpenTelemetry spans.

| Language | Status | Driver | What you get |
|----------|--------|--------|--------------|
| [Rust](rust/) | **Available** | `tokio-postgres` | Named queries, `sql!` and `query!` compiled at build time; parameter and row structs, enums, `COPY`; routine traces as OpenTelemetry spans |
| [TypeScript](typescript/) | **Next** | `pg` or `postgres` (planned) | Named queries to `.d.ts` with a thin runtime first, then queries written in TypeScript files |
| .NET | Considered | Npgsql | Not started |

## Any other language: the descriptor as JSON

Until a language has its generator, `pgzest describe` prints the
descriptors of a project's named queries as JSON: everything a
generator needs, checked against the project's schema. See the
[CLI reference](../reference/cli/).

```sh
pgzest describe queries/restaurants.sql
```
