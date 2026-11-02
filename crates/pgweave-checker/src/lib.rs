//! Vérification des statements : l'analyse sémantique (`pgweave-sema`),
//! plus les règles des plugins.
//!
//! Les vérifications intégrées (tables, colonnes, types, corps de routines)
//! sont celles de `pgweave-sema` ; ce crate en est la façade, et l'hôte des
//! règles apportées par les plugins ([`rule::CheckRule`]), qui voient le
//! schéma par `SchemaProvider`.

pub mod check;
pub mod checker;
pub mod rule;

pub use check::{check_script, check_sql, check_sql_self_contained, check_statements};
pub use checker::Checker;
