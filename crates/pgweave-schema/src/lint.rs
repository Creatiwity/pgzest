/// DDL role-aware lint rules.
use pgweave_ast::{AlterTableOp, DdlStatement, Statement};
use pgweave_diagnostics::{Diagnostic, Severity};

use crate::role::FileRole;

// ── Public entry point ────────────────────────────────────────────────────────

pub fn check_ddl_role(stmts: &[Statement], role: &FileRole) -> Vec<Diagnostic> {
    match role {
        FileRole::Migration { .. } => vec![],
        FileRole::Schema => check_schema_file(stmts),
        FileRole::Query => check_query_file(stmts),
    }
}

// ── Query file: no DDL ────────────────────────────────────────────────────────

fn check_query_file(stmts: &[Statement]) -> Vec<Diagnostic> {
    stmts
        .iter()
        .filter_map(|stmt| {
            if let Statement::Ddl(_) = stmt {
                Some(Diagnostic {
                    code: "E-DDL-001",
                    message: "DDL not allowed in a query file — use a migration".into(),
                    severity: Severity::Error,
                    span: None,
                    hint: None,
                })
            } else {
                None
            }
        })
        .collect()
}

// ── Schema file: CREATE-only ──────────────────────────────────────────────────
//
// Schema files are the canonical source of truth for CREATE TABLE.
// Any structural ALTER TABLE (add/drop/alter column) is an error here because
// it would make the CREATE TABLE definition incomplete by itself.

fn check_schema_file(stmts: &[Statement]) -> Vec<Diagnostic> {
    stmts
        .iter()
        .filter_map(|stmt| {
            let Statement::Ddl(ddl) = stmt else {
                return None;
            };
            ddl_schema_error(ddl)
        })
        .collect()
}

fn ddl_schema_error(ddl: &DdlStatement) -> Option<Diagnostic> {
    let DdlStatement::AlterTable(at) = ddl else {
        return None;
    };

    for op in &at.operations {
        let msg: Option<&'static str> = match op {
            AlterTableOp::AddColumn(_) => Some(
                "ADD COLUMN in a schema file makes CREATE TABLE incomplete — \
                 add the column directly to the CREATE TABLE definition",
            ),
            AlterTableOp::DropColumn { .. } => Some(
                "DROP COLUMN in a schema file — \
                 remove the column from the CREATE TABLE definition instead",
            ),
            AlterTableOp::AlterColumn { .. } => Some(
                "ALTER COLUMN in a schema file — \
                 update the column type directly in the CREATE TABLE definition",
            ),
            _ => None,
        };

        if let Some(message) = msg {
            return Some(Diagnostic {
                code: "E-DDL-002",
                message: message.into(),
                severity: Severity::Error,
                span: None,
                hint: Some(
                    "Schema files should only contain CREATE statements. \
                     Use a migration file for structural changes."
                        .into(),
                ),
            });
        }
    }

    None
}
