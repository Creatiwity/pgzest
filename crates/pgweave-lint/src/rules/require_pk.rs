/// L002 — CREATE TABLE without a primary key constraint.
///
/// Every table should have a primary key so rows can be uniquely identified
/// and to support replication, foreign keys, and ORM ergonomics.
use pgweave_ast::ddl::{ColumnConstraint, DdlStatement, TableConstraint};
use pgweave_ast::stmt::Statement;
use pgweave_diagnostics::Diagnostic;

use crate::config::{LintConfig, RuleLevel};

pub const CODE: &str = "L002";

pub fn check(stmt: &Statement, config: &LintConfig) -> Vec<Diagnostic> {
    if config.rules.require_primary_key == RuleLevel::Off {
        return vec![];
    }

    let Statement::Ddl(DdlStatement::CreateTable(ct)) = stmt else {
        return vec![];
    };

    let has_table_pk = ct
        .constraints
        .iter()
        .any(|c| matches!(c, TableConstraint::PrimaryKey { .. }));

    let has_column_pk = ct.columns.iter().any(|col| {
        col.constraints
            .iter()
            .any(|c| matches!(c, ColumnConstraint::PrimaryKey))
    });

    if has_table_pk || has_column_pk {
        return vec![];
    }

    let table_name = ct
        .name
        .0
        .last()
        .map(|i| i.value.as_str())
        .unwrap_or("<unknown>");

    let msg = format!("Table `{table_name}` has no primary key");
    let d = match config.rules.require_primary_key {
        RuleLevel::Error => Diagnostic::error(CODE, msg),
        _ => Diagnostic::warning(CODE, msg),
    }
    .with_hint("Add a PRIMARY KEY constraint or column to uniquely identify rows.");

    vec![d]
}
