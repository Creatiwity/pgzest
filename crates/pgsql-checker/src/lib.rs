pub mod builtins;
pub mod check;
pub mod diagnostic;
pub mod schema;

// Re-export the most commonly used types at the crate root.
pub use check::{check_sql, check_sql_self_contained, check_statements};
pub use diagnostic::{Diagnostic, Severity, Span};
pub use schema::{
    ColumnInfo, DdlSchemaProvider, FunctionSignature, IntrospectionJsonProvider, SchemaProvider,
    TableSchema,
};
