pub mod diff;
/// Rendu SQL de l'AST, désormais dans `pgweave-ast`.
pub use pgweave_ast::emit;
pub mod inject;
pub mod migration_safety;
pub mod rust;
pub mod ts_types;

pub use diff::{DiffOp, MigrationDiff, diff_schemas};
pub use emit::{emit_create_table, emit_object_name, emit_statement};
pub use migration_safety::check_migration_safety;
pub use ts_types::{generate_ts_types, generate_ts_types_from_sql};
