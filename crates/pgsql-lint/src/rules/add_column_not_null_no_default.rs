/// L008 — ALTER TABLE ADD COLUMN NOT NULL without a DEFAULT value.
///
/// On PostgreSQL < 11, this rewrites the entire table. Even on modern PG,
/// it acquires an ACCESS EXCLUSIVE lock while the NOT NULL constraint is
/// validated. Always provide a DEFAULT when adding NOT NULL columns to
/// existing tables.
use pgsql_ast::ddl::{AlterTableOp, ColumnConstraint, DdlStatement};
use pgsql_ast::stmt::Statement;
use pgsql_checker::diagnostic::Diagnostic;

use crate::config::{LintConfig, RuleLevel};

pub const CODE: &str = "L008";

pub fn check(stmt: &Statement, config: &LintConfig) -> Vec<Diagnostic> {
    if config.rules.add_column_not_null_no_default == RuleLevel::Off {
        return vec![];
    }

    let Statement::Ddl(DdlStatement::AlterTable(at)) = stmt else {
        return vec![];
    };

    let mut diags = Vec::new();
    let table_name = at.name.0.last().map(|i| i.value.as_str()).unwrap_or("<unknown>");

    for op in &at.operations {
        if let AlterTableOp::AddColumn(col_def) = op {
            let is_not_null = !col_def.nullable;
            let has_default = col_def.default.is_some()
                || col_def.constraints.iter().any(|c| {
                    matches!(c, ColumnConstraint::Default(_))
                });

            if is_not_null && !has_default {
                let col_name = &col_def.name.value;
                let msg = format!(
                    "ALTER TABLE {} ADD COLUMN {} NOT NULL has no DEFAULT — dangerous on existing tables",
                    table_name, col_name
                );
                let d = match config.rules.add_column_not_null_no_default {
                    RuleLevel::Error => Diagnostic::error(CODE, msg),
                    _ => Diagnostic::warning(CODE, msg),
                }
                .with_hint("Add a DEFAULT value, or add the column as nullable first then set NOT NULL after backfilling.");
                diags.push(d);
            }
        }
    }
    diags
}
