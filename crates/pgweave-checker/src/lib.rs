//! Type-checker : une passe linéaire sur l'AST normalisé.
//!
//! Ce crate ne possède plus ni les types de diagnostic ni les types de schéma :
//! - `Diagnostic`/`Severity`/`Span` et les codes → `pgweave-diagnostics`
//! - `SchemaProvider` et les types de schéma → `pgweave-schema`
//!
//! Il en est un consommateur, comme le linter, le codegen et l'analyse
//! sémantique. Ce qui doit enchaîner plusieurs passes vit dans
//! `pgweave-analysis`, jamais ici.

pub mod builtins;
pub mod check;
pub mod checker;
pub mod rule;
pub mod rules;

pub use check::{check_sql, check_sql_self_contained, check_statements};
pub use checker::Checker;
