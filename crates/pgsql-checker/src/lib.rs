pub mod builtins;
pub mod check;
pub mod checker;
pub mod diagnostic;
pub mod rule;
pub mod rules;
pub mod schema;

// Re-export the most commonly used types at the crate root.
pub use check::{check_sql, check_sql_self_contained, check_statements};
pub use checker::Checker;
pub use diagnostic::{Diagnostic, Severity, Span};
pub use schema::{
    ColumnInfo, CompositeSchemaProvider, DdlSchemaProvider, FunctionSignature,
    IntrospectionJsonProvider, RlsAwareSchemaProvider, SchemaProvider, TableSchema,
    detect_default_schema, detect_search_path, object_name_to_string, qualify_object_name, qualify_str,
};

