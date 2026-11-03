# pgweave playground

Demo project. Opened automatically by **F5** from `editors/vscode/`.

## Running

```bash
make lsp                 # builds the LSP server (target/debug/pgweave-lsp)
code editors/vscode      # then F5 → "Run Extension (playground)"
```

The extension finds `target/debug/pgweave-lsp` on its own. An
"Extension Development Host" window opens on this folder.

From the CLI, in this folder:

```bash
pgweave check queries/02-errors.sql
pgweave lint  queries/05-lint.sql
```

## What to look at

| File | What to observe |
|---|---|
| `queries/01-ok.sql` | Entirely clean. `users` and `organizations` come from `schema/tables/`, indexed through the config — no `--schema` to pass. |
| `queries/02-errors.sql` | Nonexistent column and table. The hint lists the available columns. |
| `queries/03-directives.sql` | Comment directives, ESLint-style. Only the last `SELECT` should be underlined. |
| `queries/04-tolerance-parser.sql` | An unreadable statement is reported as a warning without interrupting the analysis of the rest. |
| `queries/05-lint.sql` | Lint rules. **Invisible with `pgweave check`**: the linter is a separate command. Visible in VS Code, where the LSP runs both. |
| `queries/06-macros.pgsql` | Macros. Analyzed directly by `check` and by the LSP — no need for `build`. Cursor in a macro + **⌘⌥E** to see its expansion. Contains a multi-table trigger and a custom macro (`macros/audited.pgmacro`). |

Also try:

- **Hover** a column in `01-ok.sql` → its type.
- **Completion** after `SELECT ` or after `u.` in a query on `users`.
- Edit `schema/tables/users.sql` (add a column), then go back to
  `queries/01-ok.sql`: the index is rebuilt.
- Set `unparsable = "error"` in `pgweave.toml`: the warning in
  `04-tolerance-parser.sql` becomes an error.

## Layout

```
pgweave.toml   ← defines the scope, the tolerance, the severities
schema/
  tables/                 ← indexed recursively…
  functions/              ← …subdirectories included
migrations/
  000001_init.sql         ← indexed with its sequence number
queries/                  ← outside the schema scope: Query role, DDL forbidden
```

`schema/` is canonical: the migrations are derived from it. The two must
stay consistent — here `last_seen_at` appears in both.

## Limitations you will see

- **Type-checker** diagnostics target the faulty expression (`nope` is
  underlined, not the whole `SELECT`). **Lint** rules target the whole
  statement — the nodes they care about are not always expressions.
- An expression synthesized by the tool has no position: the `true` coming
  from desugaring `IS TRUE`, for example. A diagnostic on it falls back to
  the statement.
- `table-not-in-scope` (reference to a table missing from the `FROM`) does
  not fire at top level yet.
- The same trigger function serving several tables produces a false
  positive `trigger-ambiguous-table`.
