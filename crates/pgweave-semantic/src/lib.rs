pub mod analyze;
pub mod infer;
pub mod scope;
pub mod symbol;

pub use analyze::{SemanticResult, analyze};
pub use infer::{OutputColumn, data_type_to_sql, data_type_to_ts};
pub use scope::{ScopeId, ScopeTree};
pub use symbol::{Symbol, SymbolKind, SymbolTable};

/// Convert ObjectName to a simple string (last part only for schema-qualified names).
pub(crate) fn schema_name(name: &pgweave_ast::expr::ObjectName) -> String {
    name.0.last().map(|i| i.value.clone()).unwrap_or_default()
}
