// abi.rs: serialized types for the ABI
//
// ABI WASM (JSON-over-linear-memory):
//
// Exports expected from every WASM plugin:
//   pgzest_plugin_name() -> i32          // ptr to a null-terminated UTF-8 string
//   pgzest_plugin_version() -> i32       // version int (100*major + minor)
//   pgzest_alloc(size: i32) -> i32       // allocator exposed to the host
//   pgzest_free(ptr: i32)                // frees an allocation
//   pgzest_check_select(ptr: i32, len: i32) -> i32  // JSON SelectStmt → JSON [Diagnostic]
//   pgzest_check_table(ptr: i32, len: i32) -> i32   // JSON CreateTable → JSON [Diagnostic]
//
// Imports provided by the host:
//   env.pgzest_get_table(name_ptr: i32, name_len: i32) -> i32  // → JSON TableSchema or null
//   env.pgzest_log(ptr: i32, len: i32)                         // console log from the plugin
//
// Rule codes:
//   Plugin rule codes must be prefixed with the plugin name and a `/`,
//   e.g. `"my-plugin/my-rule"`. This namespacing prevents collisions with
//   built-in rule IDs (e.g. "column-not-found") and makes the rule origin
//   unambiguous in diagnostics and config overrides.

use serde::{Deserialize, Serialize};

/// Diagnostic serialized as JSON to cross the WASM boundary.
#[derive(Debug, Serialize, Deserialize)]
pub struct WasmDiagnostic {
    pub code: String,
    pub message: String,
    pub severity: String, // "error" | "warning" | "info"
    pub hint: Option<String>,
}

impl From<WasmDiagnostic> for pgzest_diagnostics::Diagnostic {
    fn from(d: WasmDiagnostic) -> Self {
        use pgzest_diagnostics::Severity;
        // Leak the code string to produce a &'static str.
        // This is intentional: WASM plugin diagnostic codes live for the
        // duration of the process (the set of distinct codes is bounded).
        let code: &'static str = Box::leak(d.code.into_boxed_str());
        let sev = match d.severity.as_str() {
            "error" => Severity::Error,
            "info" => Severity::Info,
            _ => Severity::Warning,
        };
        let mut diag = match sev {
            Severity::Error => pgzest_diagnostics::Diagnostic::error(code, d.message),
            Severity::Warning => pgzest_diagnostics::Diagnostic::warning(code, d.message),
            Severity::Info => pgzest_diagnostics::Diagnostic::info(code, d.message),
        };
        if let Some(h) = d.hint {
            diag = diag.with_hint(h);
        }
        diag
    }
}
