// abi.rs — types sérialisés pour l'ABI
//
// ABI WASM (JSON-over-linear-memory):
//
// Exports attendus de tout plugin WASM:
//   pgsql_plugin_name() -> i32          // ptr vers une string UTF-8 null-terminated
//   pgsql_plugin_version() -> i32       // version int (100*major + minor)
//   pgsql_alloc(size: i32) -> i32       // allocateur exposé à l'hôte
//   pgsql_free(ptr: i32)                // libère une allocation
//   pgsql_check_select(ptr: i32, len: i32) -> i32  // JSON SelectStmt → JSON [Diagnostic]
//   pgsql_check_table(ptr: i32, len: i32) -> i32   // JSON CreateTable → JSON [Diagnostic]
//
// Imports fournis par l'hôte:
//   env.pgsql_get_table(name_ptr: i32, name_len: i32) -> i32  // → JSON TableSchema ou null
//   env.pgsql_log(ptr: i32, len: i32)                         // console log depuis le plugin

use serde::{Deserialize, Serialize};

/// Diagnostic sérialisé pour passage JSON à travers la frontière WASM.
#[derive(Debug, Serialize, Deserialize)]
pub struct WasmDiagnostic {
    pub code: String,
    pub message: String,
    pub severity: String, // "error" | "warning" | "info"
    pub hint: Option<String>,
}

impl From<WasmDiagnostic> for pgsql_checker::Diagnostic {
    fn from(d: WasmDiagnostic) -> Self {
        use pgsql_checker::Severity;
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
            Severity::Error => pgsql_checker::Diagnostic::error(code, d.message),
            Severity::Warning => pgsql_checker::Diagnostic::warning(code, d.message),
            Severity::Info => pgsql_checker::Diagnostic::info(code, d.message),
        };
        if let Some(h) = d.hint {
            diag = diag.with_hint(h);
        }
        diag
    }
}
