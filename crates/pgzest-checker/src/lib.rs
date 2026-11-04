//! Statement checking: semantic analysis (`pgzest-sema`),
//! plus plugin rules.
//!
//! The built-in checks (tables, columns, types, routine bodies) are those
//! of `pgzest-sema`; this crate is their facade, and the host of the
//! rules contributed by plugins ([`rule::CheckRule`]), which see the
//! schema through `SchemaProvider`.

pub mod check;
pub mod checker;
pub mod rule;

pub use check::{check_script, check_sql, check_sql_self_contained, check_statements};
pub use checker::Checker;
