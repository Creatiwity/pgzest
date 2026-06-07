/// DDL role-aware lint rules.
///
/// These rules live in `pgsql-schema` (not `pgsql-lint`) because they require
/// knowledge of `FileRole`, which is defined here and would create a cycle if
/// added to the lint crate.
use pgsql_ast::{AlterTableOp, DdlStatement, Statement};
use pgsql_checker::diagnostic::{Diagnostic, Severity};

use crate::role::FileRole;

// ── Public entry point ────────────────────────────────────────────────────────

/// Run DDL role rules against `stmts` and return diagnostics.
pub fn check_ddl_role(stmts: &[Statement], role: &FileRole) -> Vec<Diagnostic> {
    match role {
        FileRole::Migration { .. } => vec![],
        FileRole::Schema  => check_schema_file(stmts),
        FileRole::Query   => check_query_file(stmts),
    }
}

// ── Query file: no DDL at all ─────────────────────────────────────────────────

fn check_query_file(stmts: &[Statement]) -> Vec<Diagnostic> {
    stmts
        .iter()
        .filter_map(|stmt| {
            if let Statement::Ddl(_) = stmt {
                Some(Diagnostic {
                    code: "E-DDL-001",
                    message: "DDL statement not allowed in a query file; move it to a migration"
                        .into(),
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

// ── Schema file: warn on destructive DDL ─────────────────────────────────────

fn check_schema_file(stmts: &[Statement]) -> Vec<Diagnostic> {
    stmts
        .iter()
        .filter_map(|stmt| {
            let Statement::Ddl(ddl) = stmt else { return None };
            ddl_schema_diag(ddl)
        })
        .collect()
}

fn ddl_schema_diag(ddl: &DdlStatement) -> Option<Diagnostic> {
    match ddl {
        DdlStatement::AlterTable(at) => {
            for op in &at.operations {
                match op {
                    AlterTableOp::DropColumn { .. } => {
                        return Some(Diagnostic {
                            code: "W-DDL-002",
                            message: "DROP COLUMN in a schema file — use a migration instead"
                                .into(),
                            severity: Severity::Warning,
                            span: None,
                            hint: None,
                        });
                    }
                    _ => {}
                }
            }
            // ADD COLUMN in a schema file is unusual but not necessarily wrong
            // (some projects maintain a schema.sql that mirrors the DB state).
            None
        }
        _ => None,
    }
}
