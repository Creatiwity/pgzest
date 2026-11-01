use pgweave_catalog::ProjectCatalog;
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
pub struct CheckResult {
    inner: Vec<pgweave_diagnostics::Diagnostic>,
}

#[wasm_bindgen]
impl CheckResult {
    pub fn diagnostics_json(&self) -> String {
        serde_json::to_string(&self.inner).unwrap_or_else(|e| format!("{{\"error\":\"{}\"}}", e))
    }

    pub fn has_errors(&self) -> bool {
        self.inner
            .iter()
            .any(|d| d.severity == pgweave_diagnostics::Severity::Error)
    }

    pub fn error_count(&self) -> usize {
        self.inner
            .iter()
            .filter(|d| d.severity == pgweave_diagnostics::Severity::Error)
            .count()
    }

    pub fn warning_count(&self) -> usize {
        self.inner
            .iter()
            .filter(|d| d.severity == pgweave_diagnostics::Severity::Warning)
            .count()
    }
}

#[wasm_bindgen]
pub fn check(sql: &str, schema_sql: &str) -> CheckResult {
    let schema = ProjectCatalog::from_sql(schema_sql).unwrap_or_else(|_| ProjectCatalog::empty());
    let diags = pgweave_checker::check_sql(sql, &schema);
    CheckResult { inner: diags }
}

#[wasm_bindgen]
pub fn lint(sql: &str) -> String {
    let stmts = match pgweave_parser::parse(sql) {
        Ok(s) => s,
        Err(e) => {
            return format!("{{\"error\":\"parse error: {}\"}}", e);
        }
    };
    let result = pgweave_lint::lint_statements(&stmts, &pgweave_lint::LintConfig::default());
    serde_json::to_string(&result.diagnostics)
        .unwrap_or_else(|e| format!("{{\"error\":\"{}\"}}", e))
}

#[wasm_bindgen]
pub fn migrate_diff(old_sql: &str, new_sql: &str) -> String {
    let old_stmts = pgweave_parser::parse(old_sql).unwrap_or_default();
    let new_stmts = pgweave_parser::parse(new_sql).unwrap_or_default();
    pgweave_codegen::diff_schemas(&old_stmts, &new_stmts).to_sql()
}

#[wasm_bindgen]
pub fn parse_and_emit(sql: &str) -> String {
    match pgweave_parser::parse(sql) {
        Ok(stmts) => stmts
            .iter()
            .map(pgweave_codegen::emit_statement)
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
    pgweave_codegen::generate_ts_types_from_sql(&combined)
}

/// Parse a SQL file and return its AST as JSON.
/// Useful for debugging or building custom tools on top of the parser.
#[wasm_bindgen]
pub fn parse_to_json(sql: &str) -> String {
    match pgweave_parser::parse(sql) {
        Ok(stmts) => serde_json::to_string_pretty(&stmts)
            .unwrap_or_else(|e| format!("{{\"error\":\"{}\"}}", e)),
        Err(e) => format!("{{\"error\":\"parse error: {}\"}}", e),
    }
}
