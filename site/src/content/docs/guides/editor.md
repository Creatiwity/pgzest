---
title: "Editor"
description: "The VS Code extension — diagnostics as you type, hover, completion and macro expansion, in SQL files and inside Rust strings."
---

The `pgzest-lsp` language server and its VS Code extension give the same
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

The extension is not on the marketplace yet. From a clone of the
repository:

```sh
make demo          # try it on the demo, in development mode
make vscode-dev    # build the server and the extension, then F5 in editors/vscode
```

The extension finds `pgzest-lsp` in the repository's `target/` in
development; elsewhere, set its path.

| Setting | Default | Description |
|---------|---------|-------------|
| `pgzest.lspPath` | `pgzest-lsp` | path to the `pgzest-lsp` binary |
| `pgzest.schemaFile` | `""` | a schema file for files outside a pgzest project |

With rust-analyzer, the extension turns off rust-analyzer's semantic
highlighting of strings, which would cover the SQL highlighting.
