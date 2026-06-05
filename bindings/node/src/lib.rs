use napi_derive::napi;

#[napi(object)]
pub struct JsDiagnostic {
    pub severity: String,
    pub code: String,
    pub message: String,
}

#[napi]
pub fn check(sql: String, schema_sql: String) -> Vec<JsDiagnostic> {
    let schema = pgsql_checker::DdlSchemaProvider::from_sql(&schema_sql)
        .unwrap_or_else(|_| pgsql_checker::DdlSchemaProvider::from_statements(&[]));
    let diagnostics = pgsql_checker::check_sql(&sql, &schema);
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
    let stmts = match pgsql_parser::parse(&sql) {
        Ok(s) => s,
        Err(e) => {
            return vec![JsDiagnostic {
                severity: "error".to_string(),
                code: "E000".to_string(),
                message: format!("parse error: {e}"),
            }];
        }
    };
    let result = pgsql_lint::lint_statements(&stmts, &pgsql_lint::LintConfig::default());
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
    let old_stmts = pgsql_parser::parse(&old_sql).unwrap_or_default();
    let new_stmts = pgsql_parser::parse(&new_sql).unwrap_or_default();
    pgsql_codegen::diff_schemas(&old_stmts, &new_stmts).to_sql()
}

#[napi]
pub fn parse_and_emit(sql: String) -> String {
    match pgsql_parser::parse(&sql) {
        Ok(stmts) => stmts
            .iter()
            .map(pgsql_codegen::emit_statement)
            .collect::<Vec<_>>()
            .join(";\n"),
        Err(e) => format!("-- error: {e}"),
    }
}
