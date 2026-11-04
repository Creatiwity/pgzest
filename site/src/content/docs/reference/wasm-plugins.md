---
title: "WASM plugins"
description: "Writing a pgzest plugin compiled to WebAssembly."
---

## Overview

WASM plugins let you add custom lint rules to pgzest without modifying the core.
A WASM plugin is compiled to `.wasm` and declared in `pgzest.toml` (see [pgzest.toml](../config/)).

## Required Exports

```c
// Plugin identity
char* pgzest_plugin_name();         // null-terminated UTF-8 name
int   pgzest_plugin_version();      // 100*major + minor

// Memory management (must expose an allocator)
void* pgzest_alloc(int size);
void  pgzest_free(void* ptr);

// Check hooks (optional — implement what you need)
// Input: JSON string (SelectStmt / CreateTable AST node)
// Output: JSON array of WasmDiagnostic, null-terminated
char* pgzest_check_select(char* json, int len);
char* pgzest_check_table(char* json, int len);
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
char* pgzest_get_table(char* name, int len);  // → JSON TableSchema or null
void  pgzest_log(char* msg, int len);         // log to stderr
```

## pgzest.toml

Paths are relative to the config file. A plugin that fails to load is an error.

```toml
[plugins]
wasm = [
  "plugins/my-custom-rule.wasm",
  "plugins/naming-enforcer.wasm",
]
```

## Example (Rust plugin compiled to WASM)

See `examples/wasm-plugin/` for a minimal Rust plugin.
