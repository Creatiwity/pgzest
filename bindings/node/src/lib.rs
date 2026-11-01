use napi_derive::napi;

#[napi(object)]
pub struct JsDiagnostic {
    pub severity: String,
    pub code: String,
    pub message: String,
}

#[napi]
pub fn check(sql: String, schema_sql: String) -> Vec<JsDiagnostic> {
    let schema = pgweave_catalog::ProjectCatalog::from_sql(&schema_sql)
        .unwrap_or_else(|_| pgweave_catalog::ProjectCatalog::empty());
    let diagnostics = pgweave_checker::check_sql(&sql, &schema);
    diagnostics
        .into_iter()
        .map(|d| JsDiagnostic {
            severity: format!("{:?}", d.severity).to_lowercase(),
            code: d.code.to_string(),
            message: d.message,
        })
        .collect()
}

#[napi]
pub fn lint(sql: String) -> Vec<JsDiagnostic> {
    let stmts = match pgweave_parser::parse(&sql) {
        Ok(s) => s,
        Err(e) => {
            return vec![JsDiagnostic {
                severity: "error".to_string(),
                code: "E000".to_string(),
                message: format!("parse error: {e}"),
            }];
        }
    };
    let result = pgweave_lint::lint_statements(&stmts, &pgweave_lint::LintConfig::default());
    result
        .diagnostics
        .into_iter()
        .map(|d| JsDiagnostic {
            severity: format!("{:?}", d.severity).to_lowercase(),
            code: d.code.to_string(),
            message: d.message,
        })
        .collect()
}

#[napi]
pub fn migrate_diff(old_sql: String, new_sql: String) -> String {
    let old_stmts = pgweave_parser::parse(&old_sql).unwrap_or_default();
    let new_stmts = pgweave_parser::parse(&new_sql).unwrap_or_default();
    pgweave_codegen::diff_schemas(&old_stmts, &new_stmts).to_sql()
}

#[napi]
pub fn parse_and_emit(sql: String) -> String {
    match pgweave_parser::parse(&sql) {
        Ok(stmts) => stmts
            .iter()
            .map(pgweave_codegen::emit_statement)
            .collect::<Vec<_>>()
            .join(";\n"),
        Err(e) => format!("-- error: {e}"),
    }
}

/// Generate TypeScript type declarations from SQL.
/// If schema_sql is non-empty, it's prepended to sql for enriched analysis.
#[napi]
pub fn generate_types(sql: String, schema_sql: String) -> String {
    let combined = if schema_sql.is_empty() {
        sql
    } else {
        format!("{}\n\n{}", schema_sql, sql)
    };
    pgweave_codegen::generate_ts_types_from_sql(&combined)
}

/// Parse SQL and return AST as a JSON string.
#[napi]
pub fn parse_to_json(sql: String) -> String {
    match pgweave_parser::parse(&sql) {
        Ok(stmts) => serde_json::to_string_pretty(&stmts)
            .unwrap_or_else(|e| format!("{{\"error\":\"{}\"}}", e)),
        Err(e) => format!("{{\"error\":\"parse error: {e}\"}}"),
    }
}
