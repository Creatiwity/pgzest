# pgweave WASM Plugin SDK

## Overview

WASM plugins let you add custom lint rules to pgweave without modifying the core.
A WASM plugin is compiled to `.wasm` and loaded via `pgsql.toml`.

## Required Exports

```c
// Plugin identity
char* pgweave_plugin_name();         // null-terminated UTF-8 name
int   pgweave_plugin_version();      // 100*major + minor

// Memory management (must expose an allocator)
void* pgweave_alloc(int size);
void  pgweave_free(void* ptr);

// Check hooks (optional — implement what you need)
// Input: JSON string (SelectStmt / CreateTable AST node)
// Output: JSON array of WasmDiagnostic, null-terminated
char* pgweave_check_select(char* json, int len);
char* pgweave_check_table(char* json, int len);
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
char* pgweave_get_table(char* name, int len);  // → JSON TableSchema or null
void  pgweave_log(char* msg, int len);         // log to stderr
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
