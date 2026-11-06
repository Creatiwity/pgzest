---
title: "Editor"
description: "The VS Code extension — diagnostics as you type, hover, completion and macro expansion, in SQL files and inside Rust strings."
---

The pgzest language server (`pgzest lsp`) and its VS Code extension give the same
answers as `pgzest check`, as you type, from the same project model —
the unsaved text of an open file taking precedence over the disk.

## Features

- **Diagnostics** in `.sql` and `.pgsql` files, and inside the SQL
  strings of `sql!` and `query!` in Rust files. Editing a fragment, a
  `.pgmacro` or `pgzest.toml` rechecks the open files at once.
- **Hover**: the type and nullability of a column, a variable or an
  expression; the descriptor of a named query on its `-- name:` header
  (parameters, columns, number of rows); a fragment's documentation and
  the SQL it inserts.
- **Completion**: keywords, tables after `FROM`/`JOIN`, columns of the
  relations in scope, qualified (`alias.`) or not.
- **Macro expansion**: **⌘⌥E** (`Ctrl+Alt+E`), or the "Show expansion"
  link above each macro of a `.pgsql` file, opens the generated SQL in a
  read-only tab.
- **Highlighting** of `$parameters` and `@fragments`, inside Rust strings
  too.

## Installing

Install **pgzest** (publisher Creatiwity) from the
[VS Code Marketplace](https://marketplace.visualstudio.com/items?itemName=Creatiwity.pgzest),
or from [Open VSX](https://open-vsx.org/extension/Creatiwity/pgzest) for
VSCodium, Cursor and the other editors that use it.

On macOS (Apple silicon), Linux (x64, arm64) and Windows (x64), the
extension ships the `pgzest` binary that runs the server: nothing else to
install. Elsewhere, install the CLI (`cargo install pgzest --locked`) and
keep it on the `PATH`, or set `pgzest.server.path`. The server is
`pgzest lsp`, over stdio: any LSP client can start it.

| Setting | Default | Description |
|---------|---------|-------------|
| `pgzest.server.path` | `""` | the `pgzest` binary to run; empty: the shipped one, otherwise `pgzest` on the `PATH` |
| `pgzest.schemaFile` | `""` | a schema file for files outside a pgzest project |

From a clone of the repository, the extension runs in development mode
with the `pgzest` built in `target/`:

```sh
make demo          # try it on the demo
make vscode-dev    # build pgzest and the extension, then F5 in editors/vscode
```

With rust-analyzer, the extension turns off rust-analyzer's semantic
highlighting of strings, which would cover the SQL highlighting.
