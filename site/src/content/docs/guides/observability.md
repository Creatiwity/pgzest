---
title: "Observable routines"
description: "Isolated temp tables, snapshots, run logs, OpenTelemetry traces of the triggers a statement fired, a depth limit for endless cascades, and a development loop."
---

PL/pgSQL routines and triggers are a black box: which trigger ran, in
what order, how long, on which rows, and why it loops. pgweave instruments
the routines written as [macros](../fragments-and-macros/) — `@function`,
`@procedure`, `@trigger` — from the analysis of their body. A plain
`CREATE FUNCTION` stays as written: instrumenting a routine is opting in,
and the code that does it is in the migration, for review.

## Isolated temp tables and snapshots

```sql
@function close_month(m DATE): BIGINT {
    DECLARE total BIGINT;
    @temp_table work (order_id BIGINT PRIMARY KEY, amount INT NOT NULL);
    INSERT INTO work SELECT id, total FROM orders WHERE month = m;
    @snapshot filled;
    UPDATE work SET amount = amount * 2 WHERE amount < 10;
    SELECT sum(amount) INTO total FROM work;
    RETURN total;
}
```

- **`@temp_table name (columns);`** — a temp table private to each run. A
  plain temp table is shared by the whole session: a nested or recursive
  call, or one per loop iteration, mixes its rows with the caller's.
  pgweave gives each run an id and rewrites every access: reads see the
  run's rows only, `UPDATE` and `DELETE` touch them only, keys are unique
  per run, and the rows are deleted when the run returns.
- **`@snapshot label;`** — at the `debug` log level, copies the run's rows
  of each temp table and the variables in scope into
  `pgweave_debug.snapshot` and `pgweave_debug.snapshot_rows`. At any other
  level it emits nothing.

An access pgweave cannot rewrite is an error (`TRUNCATE`, `RETURNING *`, a
CTE of the same name); dynamic SQL (`EXECUTE`) in such a body is a
warning, since it is not rewritten.

## Run logs

```toml
# pgweave.toml
[log]
level = "step"     # none (default) | info | step | debug
target = "table"   # table (default) | notify | notice
```

The level is part of the generated code: below it, nothing is emitted.
Analysis, the editor, `pgweave build` and `pgweave migrate` all see the
routines at this level, and a migration records it in its header
(`-- pgweave: postgres=18 log=step`).

| Level | Recorded per run |
|-------|------------------|
| `none` | nothing |
| `info` | routine, start, duration, the run that caused it (calling routine, or the statement that fired a trigger), depth, the caller's trace, the `SQLSTATE` of an error the routine handled |
| `step` | plus each `INSERT`, `UPDATE`, `DELETE`, `EXECUTE`: kind, table, source line, row count |
| `debug` | plus the `@snapshot` copies |

| Target | Where |
|--------|-------|
| `table` | unlogged tables `pgweave_log.run` and `pgweave_log.step`, created by the migrations |
| `notify` | one JSON `pg_notify('pgweave_log', …)` per entry |
| `notice` | a `RAISE NOTICE 'pgweave_log'` when a run starts and when it ends, sent to the client at once — even when the transaction then fails: the target for traces |

The recording changes neither `ROW_COUNT` nor `FOUND` for the body. A run
whose error leaves the routine is rolled back with its log (`table`,
`notify`); `notice` events are already on the client.

## Traces

With the `notice` target, a run reports itself only when the caller's
trace is **sampled**: the W3C `traceparent` the application passes, with
its `sampled` flag set. A request that is not traced pays about a
microsecond per run.

`pgweave-runtime` reads the events off the connection and hands each
finished tree of runs — the routine called, the triggers it fired, the
routines they called, with their timings and steps — to the application.
With the `opentelemetry` feature, `trace::otel::export` turns a tree into
spans under the application's span, through the `opentelemetry` API only:
the tracer provider, its exporter and the backend are yours.

```rust
use pgweave_runtime::trace::{self, Tracing};

let tracing = Tracing::new(move |run| trace::otel::export(&tracer, &run, &Context::current()));
let (client, connection) = config.connect(NoTls).await?;
// `drive` replaces `connection.await`; other notices and LISTEN
// notifications go to the closure.
tokio::spawn({ let t = tracing.clone(); async move { t.drive(connection, |_| {}).await } });

trace::set_traceparent(&client, &traceparent, true).await?; // in the transaction
if let Err(e) = client.query("SELECT add_line($1, $2)", &[&order, &amount]).await {
    tracing.fail_open(&e); // the runs the failure left open, as error spans
}
```

The demo prints the tree instead of exporting it:

```text
orders_award_loyalty_fn — 0.79 ms
  · UPDATE customers (schema/2-loyalty.sql:16): 1 row(s)
    └ loyalty_points_of — 0.34 ms
```

By hand, in `psql`:
`SET pgweave.traceparent = '00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01'`.

## Endless cascades

```sql
SET pgweave.max_depth = 50;
```

A logged routine called deeper than this raises `54001` naming itself, and
the trace shows the chain of calls that led there — a trigger cascade that
loops fails fast instead of exhausting the stack. The guard works whether
or not the request is traced. `statement_timeout` stops a loop that does
not recurse: the runs it left open are closed as failed.

## What it costs

Measured on PostgreSQL 18, with a function that inserts a row, fires a
trigger updating another table and returns the result of a second
function — three runs per call, 11 µs per call without logging:

| Level, target | Per run |
|---------------|---------|
| `none` | 0 (no code generated) |
| `info`, `table` | ~6 µs |
| `step`, `table` or `notify` | ~12 µs |
| `notice`, trace not sampled | ~1 µs |
| `notice`, sampled trace | ~33 µs |

Compared with the round trip of a query (100 µs to a few ms), negligible
for business routines; for a trigger fired by a bulk load of millions of
rows, keep the level at `none`. Of a sampled run's ~33 µs, ~23 µs is
sending the two events to the client.

## A development loop

```sh
pgweave dev --url postgres://localhost/myapp_dev   # or DATABASE_URL
```

Applies the routines of the schema files — functions, procedures,
triggers — to a development database at the `debug` level, then again
each time a file is saved: logs and snapshots are there without a
separate build. One transaction per file; a database error is shown at
its place in the source. Tables stay with migrations.
