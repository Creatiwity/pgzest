use napi_derive::napi;

#[napi(object)]
pub struct JsDiagnostic {
    pub severity: String,
    pub code: String,
    pub message: String,
}

#[napi]
pub fn check(sql: String, schema_sql: String) -> Vec<JsDiagnostic> {
    let schema = pgzest_catalog::ProjectCatalog::from_sql(&schema_sql)
        .unwrap_or_else(|_| pgzest_catalog::ProjectCatalog::empty());
    let diagnostics = pgzest_checker::check_sql(&sql, &schema);
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
    let stmts = match pgzest_parser::parse(&sql) {
        Ok(s) => s,
        Err(e) => {
            return vec![JsDiagnostic {
                severity: "error".to_string(),
                code: "E000".to_string(),
                message: format!("parse error: {e}"),
            }];
        }
    };
    let result = pgzest_lint::lint_statements(&stmts, &pgzest_lint::LintConfig::default());
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
    // An unreadable schema does not yield an empty migration: the error is
    // rendered as a SQL comment.
    match pgzest_catalog::diff::diff_sql(&old_sql, &new_sql) {
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

#[napi]
pub fn parse_and_emit(sql: String) -> String {
    match pgzest_parser::parse(&sql) {
        Ok(stmts) => stmts
            .iter()
            .map(pgzest_codegen::emit_statement)
            .collect::<Vec<_>>()
            .join(";\n"),
        Err(e) => format!("-- error: {e}"),
    }
}

/// Parse SQL and return AST as a JSON string.
#[napi]
pub fn parse_to_json(sql: String) -> String {
    match pgzest_parser::parse(&sql) {
        Ok(stmts) => serde_json::to_string_pretty(&stmts)
            .unwrap_or_else(|e| format!("{{\"error\":\"{}\"}}", e)),
        Err(e) => format!("{{\"error\":\"parse error: {e}\"}}"),
    }
}
