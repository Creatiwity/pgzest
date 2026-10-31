# pgweave-wasm

WASM bindings for pgweave, exposing the parser, checker, linter, and codegen crates to JavaScript/TypeScript environments.

## Prerequisites

- Rust toolchain with `wasm32-unknown-unknown` target
- `wasm-pack`

```bash
rustup target add wasm32-unknown-unknown
cargo install wasm-pack
```

## Build

```bash
./build.sh
```

This runs `wasm-pack build --target bundler --out-dir pkg`, producing a `pkg/` directory with the compiled WASM module and generated JS/TS glue code ready for bundlers (webpack, Vite, Rollup).

## API

### `check(sql: string, schema_sql: string): CheckResult`

Type-checks `sql` against a schema defined by `schema_sql` (DDL statements).

```ts
const result = check("SELECT id FROM users", "CREATE TABLE users (id INT);");
result.has_errors();       // boolean
result.error_count();      // number
result.warning_count();    // number
result.diagnostics_json(); // JSON string of Diagnostic[]
```

### `lint(sql: string): string`

Runs the linter with default config. Returns a JSON string of lint diagnostics.

```ts
const diags = JSON.parse(lint("SELECT * FROM users"));
```

### `migrate_diff(old_sql: string, new_sql: string): string`

Computes a migration SQL diff between two DDL schemas.

```ts
const migration = migrate_diff(
  "CREATE TABLE users (id INT);",
  "CREATE TABLE users (id INT, name TEXT);"
);
// "ALTER TABLE users ADD COLUMN name TEXT;"
```

### `parse_and_emit(sql: string): string`

Parses SQL and re-emits it in normalized form (evaluation order: FROM → JOIN → WHERE → SELECT …). Returns the normalized SQL string or an error comment.

```ts
const normalized = parse_and_emit("SELECT id FROM users WHERE id = 1");
```

## Usage (bundler)

```ts
import init, { check, lint, migrate_diff, parse_and_emit } from "./pkg/pgweave_wasm";

await init();

const result = check("SELECT foo FROM bar", "CREATE TABLE bar (id INT);");
console.log(result.diagnostics_json());
```

## Output targets

Change `--target bundler` in `build.sh` for other environments:

| Target     | Use case                        |
|------------|---------------------------------|
| `bundler`  | webpack / Vite / Rollup (default) |
| `web`      | Direct `<script type="module">` in browser |
| `nodejs`   | CommonJS for Node.js            |
| `deno`     | Deno ESM                        |
