//! Type-checker : une passe linéaire sur l'AST normalisé.
//!
//! Ce crate ne possède plus ni les types de diagnostic ni les types de schéma :
//! - `Diagnostic`/`Severity`/`Span` et les codes → `pgweave-diagnostics`
//! - `SchemaProvider` et les types de schéma → `pgweave-schema`
//!
//! Il en est un consommateur, comme le linter, le codegen et l'analyse
//! sémantique. Ce qui doit enchaîner plusieurs passes vit dans
//! `pgweave-analysis`, jamais ici.

pub mod check;
pub mod checker;
pub mod rule;
pub mod rules;

pub use check::{check_sql, check_sql_self_contained, check_statements};
pub use checker::Checker;

/// Constructions de la grammaire SQL que le parser représente comme des
/// appels de fonction, mais qui n'en sont pas : elles n'ont pas d'entrée dans
/// `pg_proc`, donc ne sont pas cherchées dans le catalogue. (Grammaire, pas
/// catalogue : la liste est celle de PostgreSQL, elle ne se génère pas.)
pub(crate) fn is_sql_special_form(name: &str) -> bool {
    matches!(
        name,
        "coalesce"
            | "nullif"
            | "greatest"
            | "least"
            | "array"
            | "row"
            | "current_date"
            | "current_time"
            | "current_timestamp"
            | "localtime"
            | "localtimestamp"
            | "current_role"
            | "current_catalog"
            | "user"
    )
}
