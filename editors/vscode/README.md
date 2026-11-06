# pgzest for VS Code

Raw PostgreSQL, with zest. [pgzest](https://github.com/Creatiwity/pgzest)
checks your SQL the way PostgreSQL would — against your schema and
migrations, without a database — and this extension brings its answers
into the editor as you type.

## Features

- **Diagnostics** in `.sql` and `.pgsql` files, and inside the SQL strings
  of `sql!` and `query!` in Rust files: unknown tables and columns, type
  errors, function and operator resolution, PL/pgSQL bodies, lint rules.
- **Hover**: the type and nullability of a column, a variable or an
  expression; the descriptor of a named query (parameters, columns,
  number of rows).
- **Completion**: keywords, tables, the columns of the relations in scope.
- **Macro expansion**: `Cmd+Alt+E` / `Ctrl+Alt+E` opens the SQL a macro
  desugars into.
- **Highlighting** of `$parameters` and `@fragments`, inside Rust strings
  too.

The extension reads the project's `pgzest.toml` — the same
configuration as the `pgzest` CLI, so the editor and CI agree.

## Requirements

None on macOS (Apple silicon), Linux (x64, arm64) and Windows (x64): the
`pgzest` binary that runs the language server ships with the extension.
Elsewhere, install it (`cargo install pgzest`) and put it on the `PATH`,
or set `pgzest.server.path`.

## Settings

| Setting | Default | Description |
|---------|---------|-------------|
| `pgzest.server.path` | `""` | the `pgzest` binary to run (`pgzest lsp`); empty: the shipped one, otherwise `pgzest` on the `PATH` |
| `pgzest.schemaFile` | `""` | a schema file for files outside a pgzest project |

## Learn more

- [Documentation](https://pgzest.creatiwity.net) — [getting started](https://pgzest.creatiwity.net/getting-started/)
- [Changelog](https://github.com/Creatiwity/pgzest/blob/main/CHANGELOG.md)
- [Issues](https://github.com/Creatiwity/pgzest/issues)

Made by [Creatiwity](https://creatiwity.net). Licensed under either of
Apache-2.0 or MIT, at your option.
