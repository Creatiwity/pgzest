pub mod builtins;
pub mod check;
pub mod checker;
pub mod codes;
pub mod diagnostic;
pub mod rule;
pub mod rules;
pub mod schema;

// Re-export the most commonly used types at the crate root.
pub use check::{check_sql, check_sql_self_contained, check_statements};
pub use checker::Checker;
pub use codes::*;
pub use diagnostic::{Diagnostic, Severity, Span};
pub use schema::{
    ColumnInfo, CompositeSchemaProvider, DdlSchemaProvider, FunctionSignature,
    IntrospectionJsonProvider, RlsAwareSchemaProvider, SchemaProvider, TableSchema,
    detect_default_schema, detect_search_path, object_name_to_string, qualify_object_name,
    qualify_str,
};

/// Apply per-rule severity overrides to a diagnostics list.
/// Rules set to "off" are removed. Others get their severity updated.
pub fn apply_rule_overrides(
    diags: &mut Vec<Diagnostic>,
    rules: &std::collections::HashMap<String, String>,
) {
    if rules.is_empty() {
        return;
    }
    diags.retain_mut(|d| {
        if let Some(level) = rules.get(d.code) {
            match level.as_str() {
                "off" => return false,
                "error" => d.severity = Severity::Error,
                "warning" => d.severity = Severity::Warning,
                "info" => d.severity = Severity::Info,
                _ => {}
            }
        }
        true
    });
}
