use pgzest_catalog::ProjectCatalog;
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
pub struct CheckResult {
    inner: Vec<pgzest_diagnostics::Diagnostic>,
}

#[wasm_bindgen]
impl CheckResult {
    pub fn diagnostics_json(&self) -> String {
        serde_json::to_string(&self.inner).unwrap_or_else(|e| format!("{{\"error\":\"{}\"}}", e))
    }

    pub fn has_errors(&self) -> bool {
        self.inner
            .iter()
            .any(|d| d.severity == pgzest_diagnostics::Severity::Error)
    }

    pub fn error_count(&self) -> usize {
        self.inner
            .iter()
            .filter(|d| d.severity == pgzest_diagnostics::Severity::Error)
            .count()
    }

    pub fn warning_count(&self) -> usize {
        self.inner
            .iter()
            .filter(|d| d.severity == pgzest_diagnostics::Severity::Warning)
            .count()
    }
}

#[wasm_bindgen]
pub fn check(sql: &str, schema_sql: &str) -> CheckResult {
    let schema = ProjectCatalog::from_sql(schema_sql).unwrap_or_else(|_| ProjectCatalog::empty());
    let diags = pgzest_checker::check_sql(sql, &schema);
    CheckResult { inner: diags }
}

#[wasm_bindgen]
pub fn lint(sql: &str) -> String {
    let stmts = match pgzest_parser::parse(sql) {
        Ok(s) => s,
        Err(e) => {
            return format!("{{\"error\":\"parse error: {}\"}}", e);
        }
    };
    let result = pgzest_lint::lint_statements(&stmts, &pgzest_lint::LintConfig::default());
    serde_json::to_string(&result.diagnostics)
        .unwrap_or_else(|e| format!("{{\"error\":\"{}\"}}", e))
}

#[wasm_bindgen]
pub fn migrate_diff(old_sql: &str, new_sql: &str) -> String {
    // An unreadable schema does not yield an empty migration: the error is
    // rendered as a SQL comment.
    match pgzest_catalog::diff::diff_sql(old_sql, new_sql) {
        Ok(m) => {
            let mut out = m.to_sql();
            for u in &m.unsupported {
                out.push_str(&format!("\n-- unsupported: {}: {}", u.object, u.change));
            }
            out
        }
        Err(e) => format!("-- error: {e}"),
    }
}

#[wasm_bindgen]
pub fn parse_and_emit(sql: &str) -> String {
    match pgzest_parser::parse(sql) {
        Ok(stmts) => stmts
            .iter()
            .map(pgzest_codegen::emit_statement)
            .collect::<Vec<_>>()
            .join(";\n"),
        Err(e) => format!("-- error: {}", e),
    }
}

/// Generate TypeScript type declarations from a SQL file.
/// Returns a string of TypeScript interface definitions.
#[wasm_bindgen]
pub fn generate_types(sql: &str, schema_sql: &str) -> String {
    // If schema_sql is provided and non-empty, use it to enrich the analysis
    let combined = if schema_sql.is_empty() {
        sql.to_string()
    } else {
        // Prepend schema DDL so the codegen knows about table columns
        format!("{}\n\n{}", schema_sql, sql)
    };
    pgzest_codegen::generate_ts_types_from_sql(&combined)
}

/// Parse a SQL file and return its AST as JSON.
/// Useful for debugging or building custom tools on top of the parser.
#[wasm_bindgen]
pub fn parse_to_json(sql: &str) -> String {
    match pgzest_parser::parse(sql) {
        Ok(stmts) => serde_json::to_string_pretty(&stmts)
            .unwrap_or_else(|e| format!("{{\"error\":\"{}\"}}", e)),
        Err(e) => format!("{{\"error\":\"parse error: {}\"}}", e),
    }
}
