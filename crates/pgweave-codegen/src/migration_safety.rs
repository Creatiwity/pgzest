//! Migration safety checks — detect dangerous migration patterns before applying them.
//!
//! These rules operate on a `MigrationDiff` (the output of `diff_schemas`)
//! and produce diagnostics about unsafe operations.

use crate::diff::{DiffOp, MigrationDiff};
use pgweave_diagnostics::Diagnostic;

/// Check a migration diff for potentially dangerous operations.
/// Returns a list of warnings/errors to show to the user before applying.
pub fn check_migration_safety(diff: &MigrationDiff) -> Vec<Diagnostic> {
    let mut diags = Vec::new();
    for op in &diff.operations {
        diags.extend(check_op(op));
    }
    diags
}

fn check_op(op: &DiffOp) -> Vec<Diagnostic> {
    match op {
        // M001 — ADD COLUMN NOT NULL without DEFAULT
        DiffOp::AddColumn { table, column } => {
            if !column.nullable && column.default.is_none() {
                let has_col_default = column
                    .constraints
                    .iter()
                    .any(|c| matches!(c, pgweave_ast::ddl::ColumnConstraint::Default(_)));
                if !has_col_default {
                    return vec![
                        Diagnostic::warning(
                            "M001",
                            format!(
                                "Adding NOT NULL column '{}.{}' without DEFAULT — \
                            requires full table rewrite on PostgreSQL < 11 and \
                            acquires ACCESS EXCLUSIVE lock on validation",
                                table, column.name.value
                            ),
                        )
                        .with_hint(
                            "Add a DEFAULT clause, or: (1) add as nullable, (2) backfill, \
                        (3) set NOT NULL separately with a NOT VALID constraint first.",
                        ),
                    ];
                }
            }
            vec![]
        }

        // M002 — DROP COLUMN (irreversible, data loss)
        DiffOp::DropColumn { table, column } => {
            vec![Diagnostic::warning(
                "M002",
                format!(
                    "Dropping column '{}.{}' is irreversible — all data in this column will be lost",
                    table, column
                ),
            )
            .with_hint(
                "Ensure data has been migrated or is no longer needed. \
                Consider soft-deleting (add 'deleted_at') if rollback might be needed."
            )]
        }

        // M003 — DROP TABLE (very destructive)
        DiffOp::DropTable { name } => {
            vec![Diagnostic::warning(
                "M003",
                format!(
                    "Dropping table '{}' is irreversible — all rows and the table definition will be lost",
                    name
                ),
            )
            .with_hint(
                "Verify that all application code and foreign keys referencing this table have been removed first."
            )]
        }

        // Everything else is safe (CREATE, index changes, etc.)
        _ => vec![],
    }
}
