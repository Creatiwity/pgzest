pub mod scope;
pub mod symbol;
pub mod infer;
pub mod analyze;

pub use analyze::{analyze, SemanticResult};
pub use scope::{ScopeId, ScopeTree};
pub use symbol::{Symbol, SymbolKind, SymbolTable};
pub use infer::{OutputColumn, data_type_to_ts};

/// Convert ObjectName to a simple string (last part only for schema-qualified names).
pub(crate) fn schema_name(name: &pgsql_ast::expr::ObjectName) -> String {
    name.0.last()
        .map(|i| i.value.clone())
        .unwrap_or_default()
}

/// Analyze and return both diagnostics and the semantic result.
/// Useful for tooling that needs both (LSP, codegen, type generation).
pub fn analyze_sql(
    sql: &str,
    schema: &dyn pgsql_checker::SchemaProvider,
) -> (Vec<pgsql_checker::Diagnostic>, SemanticResult) {
    match pgsql_parser::parse(sql) {
        Ok(stmts) => {
            let semantic = analyze(&stmts, schema);
            let diags = pgsql_checker::check_statements(&stmts, schema);
            (diags, semantic)
        }
        Err(e) => {
            let dummy_schema = pgsql_checker::DdlSchemaProvider::from_statements(&[]);
            (
                vec![pgsql_checker::Diagnostic::error("E000", format!("parse error: {e}"))],
                analyze(&[], &dummy_schema),
            )
        }
    }
}
