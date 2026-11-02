/// Rendu SQL de l'AST, désormais dans `pgweave-ast`.
pub use pgweave_ast::emit;
pub mod inject;
pub mod rust;
pub mod ts_types;

pub use emit::{emit_create_table, emit_object_name, emit_statement};
pub use ts_types::{generate_ts_types, generate_ts_types_from_sql};
