# 0001 — salsa as the catalog's incremental engine

- **Status**: accepted (GO), 2026-11-01
- **Context**: PLAN.md, decision D3 and P0 item "salsa spike"
- **Evidence**: `spikes/salsa-catalog` (throwaway, removed in P2 — see git history)

## Question

The catalog (D2) must be: cheap to index, resolved **on demand** (only
referenced objects are analyzed), organized in **scopes** (project,
routine, block) and **incremental** for the LSP. Is salsa — used by
rust-analyzer and `ty` — a good fit, or is a custom cache needed?

Abandonment criteria set in advance (D3): inability to express the
"routine" scope cleanly, or indexing overhead > 2× on Mesalia.

## What the spike builds

```text
SourceFile (input) ──► file_declarations ──► table_index ─┐
       │               (token scan,           (name → files,
       │                no parse)              in order)
       └──────────► parse_file ◄──────────────────────────┴─ resolve_table(name)
                    (full parse,                            (DDL applied in
                     only on demand)                         file order)

routine_scope(name): parameters, variables, temporary tables of the body
resolve_in_routine(routine, name): temporary table first, then global
```

The DDL application semantics reproduce those of the current
`SchemaResolver`, so that both can be compared exactly.

## Results

### Properties (tests `spikes/salsa-catalog/tests/spike.rs`)

| Property | Result |
|---|---|
| The index is built **without parsing** any file | ✅ |
| Resolving a table parses **only** the files that touch it, and applies `CREATE` then `ALTER` in order | ✅ |
| Editing a file without changing its declarations: only its declarations are re-read, **nothing else is recomputed** (the index and tables are reused) | ✅ |
| Adding a column recomputes only the table concerned | ✅ |
| Routine scope: parameters, `DECLARE` variables, temporary tables of the body; a temporary table shadows the global table and does not exist outside the routine | ✅ |

### Mesalia (220 migrations, `--release` build)

| Measure | Value |
|---|--:|
| Tables | 134 |
| Equivalence with `SchemaResolver` (columns, types, nullability) | **134/134 identical** |
| Current `SchemaResolver::from_chunks` (eager) | 51.5 ms |
| salsa: index only | 4.3 ms |
| salsa: index + resolution of **all** tables | 23.4 ms |
| salsa: after editing a file, resolution of all tables | 0.07 ms |
| Queries re-executed after that edit | 3 (declarations and parse of the file, the only table it touches) |

The "overhead < 2×" criterion is comfortably met: salsa is **faster** than
the existing code even when resolving everything (the existing code also
does macro desugaring and a second index; the comparison stays favorable at
comparable scope), and an edit costs microseconds.

## Decision

**GO.** `pgweave-project` (P2) is built on salsa, with a **pinned** version
(`=0.28.5`); version bumps are deliberate and isolated.

## What the spike taught us, to carry into P2

1. **The AST does not implement `Eq`**: `parse_file` is therefore `no_eq`,
   and any text change re-executes the queries that read this parse (they
   then stop if their result is identical). For finer granularity:
   comparable AST, or parse **per statement** (key = statement text), so
   that editing a function does not re-read the other statements of the
   file.
2. **The declaration scan duplicates knowledge from the parser** (shape of
   statement heads). Safeguard: a systematic scan ↔ full parse equivalence
   test, like the one done here on Mesalia. The scan will have to cover
   everything P1 models (types, views, extensions, `DROP`…).
3. **Moving salsa API**: the `Update` trait became `SalsaValue` between
   versions. Hence the exact pinning and the isolation of salsa inside
   `pgweave-project`: other crates manipulate our own types.
4. **Not yet evaluated**: parallel queries and cancellation (the LSP
   cancels an obsolete analysis), on-disk persistence (`persistence`
   feature, useful for the Rust macro cache in P5). To be validated at the
   start of P2 and P5.
5. **Identifier case**: the spike reproduces the lowercasing of the current
   code for comparison; the P2 catalog will apply principle 8 (quoted
   identifiers preserved).
