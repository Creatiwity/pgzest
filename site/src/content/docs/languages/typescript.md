---
title: "TypeScript"
description: "Typed PostgreSQL queries in TypeScript: the next language of pgzest."
sidebar:
  badge:
    text: "next"
    variant: "caution"
---

:::caution[Not available yet]
TypeScript is the next language of pgzest. Nothing on this page can be
used today; it says what is planned, so you can follow it.
:::

The same analysis as for Rust — the catalog rebuilt from your schema or
migrations, every query typed as PostgreSQL would — with code generated
for TypeScript:

1. **Named queries first.** The `.sql` files of `queries/` become `.d.ts`
   declarations, with a thin runtime over `pg` or `postgres`: parameters
   and rows typed, `null` exactly where a value can be null, the return
   type following the number of rows.
2. **Then queries in TypeScript files.** A SQL string written in a `.ts`
   file, found by the generator and typed in place, with a Vite plugin
   and a watch mode.

Until then, `pgzest describe` gives the descriptors of named queries as
JSON — see [Languages](../).
