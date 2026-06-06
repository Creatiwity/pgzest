# pgsql-tools WASM Plugin SDK

## Overview

WASM plugins let you add custom lint rules to pgsql-tools without modifying the core.
A WASM plugin is compiled to `.wasm` and loaded via `pgsql.toml`.

## Required Exports

```c
// Plugin identity
char* pgsql_plugin_name();         // null-terminated UTF-8 name
int   pgsql_plugin_version();      // 100*major + minor

// Memory management (must expose an allocator)
void* pgsql_alloc(int size);
void  pgsql_free(void* ptr);

// Check hooks (optional — implement what you need)
// Input: JSON string (SelectStmt / CreateTable AST node)
// Output: JSON array of WasmDiagnostic, null-terminated
char* pgsql_check_select(char* json, int len);
char* pgsql_check_table(char* json, int len);
```

## WasmDiagnostic JSON format

```json
[
  {
    "code": "W099",
    "message": "custom rule violation",
    "severity": "warning",
    "hint": "optional hint string"
  }
]
```

## Host Imports

```c
// Available as imports from "env"
char* pgsql_get_table(char* name, int len);  // → JSON TableSchema or null
void  pgsql_log(char* msg, int len);         // log to stderr
```

## pgsql.toml

```toml
wasm_plugins = [
  "plugins/my-custom-rule.wasm",
  "plugins/naming-enforcer.wasm",
]
```

## Example (Rust plugin compiled to WASM)

See `examples/wasm-plugin/` for a minimal Rust plugin.
