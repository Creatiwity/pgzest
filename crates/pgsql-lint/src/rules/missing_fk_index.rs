/// L005 — Foreign key column without a covering index.
///
/// Unindexed FK columns cause full table scans on every FK lookup and CASCADE
/// operation. Always index the referencing column(s).
use std::collections::HashSet;

use pgsql_ast::ddl::{ColumnConstraint, DdlStatement, TableConstraint};
use pgsql_ast::stmt::Statement;
use pgsql_checker::diagnostic::Diagnostic;

use crate::config::{LintConfig, RuleLevel};

pub const CODE: &str = "L005";

/// Check all statements together (needs cross-statement context: tables vs indexes).
pub fn check_all(stmts: &[Statement], config: &LintConfig) -> Vec<Diagnostic> {
    if config.rules.missing_fk_index == RuleLevel::Off {
        return vec![];
    }

    // Collect all (table_name_lower, column_name_lower) pairs that are FK columns
    let mut fk_cols: Vec<(String, String)> = Vec::new();

    // Collect all indexed (table_name_lower, first_col_lower) pairs from CREATE INDEX
    let mut indexed: HashSet<(String, String)> = HashSet::new();

    for stmt in stmts {
        let Statement::Ddl(ddl) = stmt else { continue };
        match ddl {
            DdlStatement::CreateTable(ct) => {
                let table = ct.name.0.last().map(|i| i.value.to_lowercase()).unwrap_or_default();
                // Column-level FK
                for col in &ct.columns {
                    for constraint in &col.constraints {
                        if matches!(constraint, ColumnConstraint::References { .. }) {
                            fk_cols.push((table.clone(), col.name.value.to_lowercase()));
                        }
                    }
                }
                // Table-level FK
                for tc in &ct.constraints {
                    if let TableConstraint::ForeignKey { columns, .. } = tc {
                        for col in columns {
                            fk_cols.push((table.clone(), col.value.to_lowercase()));
                        }
                    }
                }
            }
            DdlStatement::CreateIndex(ci) => {
                let table = ci.table.0.last().map(|i| i.value.to_lowercase()).unwrap_or_default();
                // An index covers the FK column if it's the first (or only) index column
                if let Some(first_col) = ci.columns.first() {
                    indexed.insert((table, first_col.value.to_lowercase()));
                }
            }
            _ => {}
        }
    }

    // Also: PRIMARY KEY implicitly creates an index — skip PK columns
    let mut pk_cols: HashSet<(String, String)> = HashSet::new();
    for stmt in stmts {
        let Statement::Ddl(DdlStatement::CreateTable(ct)) = stmt else { continue };
        let table = ct.name.0.last().map(|i| i.value.to_lowercase()).unwrap_or_default();
        for col in &ct.columns {
            for c in &col.constraints {
                if matches!(c, ColumnConstraint::PrimaryKey | ColumnConstraint::Unique) {
                    pk_cols.insert((table.clone(), col.name.value.to_lowercase()));
                }
            }
        }
        for tc in &ct.constraints {
            match tc {
                TableConstraint::PrimaryKey { columns } | TableConstraint::Unique { columns } => {
                    if let Some(first) = columns.first() {
                        pk_cols.insert((table.clone(), first.value.to_lowercase()));
                    }
                }
                _ => {}
            }
        }
    }

    let mut diags = Vec::new();
    for (table, col) in &fk_cols {
        if !indexed.contains(&(table.clone(), col.clone()))
            && !pk_cols.contains(&(table.clone(), col.clone()))
        {
            let msg = format!(
                "Column '{}.{}' is a foreign key but has no covering index",
                table, col
            );
            let d = match config.rules.missing_fk_index {
                RuleLevel::Error => Diagnostic::error(CODE, msg),
                _ => Diagnostic::warning(CODE, msg),
            }
            .with_hint(format!(
                "Add: CREATE INDEX ON {}({});",
                table, col
            ));
            diags.push(d);
        }
    }
    diags
}
