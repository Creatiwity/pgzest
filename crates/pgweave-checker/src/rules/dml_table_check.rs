use crate::rule::{CheckRule, RuleCtx};
use pgweave_ast::stmt::{DeleteStmt, InsertStmt, UpdateStmt};
use pgweave_diagnostics::codes;

pub struct DmlTableCheck;

impl CheckRule for DmlTableCheck {
    fn name(&self) -> &'static str {
        "dml-table-check"
    }

    fn check_insert(&self, stmt: &InsertStmt, ctx: &mut RuleCtx) {
        let name = pgweave_schema::provider::object_name_to_string(&stmt.table);
        if ctx.schema.get_table(&name).is_none() {
            ctx.diags.push(
                pgweave_diagnostics::Diagnostic::error(
                    codes::TABLE_NOT_FOUND,
                    format!("table '{}' does not exist", name),
                )
                .with_hint("check the table name or provide a schema with that table"),
            );
        }
    }

    fn check_update(&self, stmt: &UpdateStmt, ctx: &mut RuleCtx) {
        let name = pgweave_schema::provider::object_name_to_string(&stmt.table.name);
        if ctx.schema.get_table(&name).is_none() {
            ctx.diags.push(pgweave_diagnostics::Diagnostic::error(
                codes::TABLE_NOT_FOUND,
                format!("table '{}' does not exist", name),
            ));
        }
    }

    /// `COPY table (colonnes)` et `ANALYZE`/`VACUUM table (colonnes)` : la
    /// table et chaque colonne doivent exister. Pour `COPY`, c'est le contrat
    /// sur lequel s'appuie le chargement typé (P5).
    fn check_utility(&self, u: &pgweave_ast::Utility, ctx: &mut RuleCtx) {
        use pgweave_ast::utility::{CopySource, UtilityKind};
        match &u.kind {
            UtilityKind::Copy(copy) => {
                if let CopySource::Table { name, columns } = &copy.source {
                    check_table_columns(name, columns, ctx);
                }
            }
            UtilityKind::Maintenance { tables, .. } => {
                for t in tables {
                    check_table_columns(&t.table, &t.columns, ctx);
                }
            }
            _ => {}
        }
    }

    fn check_delete(&self, stmt: &DeleteStmt, ctx: &mut RuleCtx) {
        let name = pgweave_schema::provider::object_name_to_string(&stmt.table.name);
        if ctx.schema.get_table(&name).is_none() {
            ctx.diags.push(pgweave_diagnostics::Diagnostic::error(
                codes::TABLE_NOT_FOUND,
                format!("table '{}' does not exist", name),
            ));
        }
    }
}

fn check_table_columns(
    name: &pgweave_ast::ObjectName,
    columns: &[pgweave_ast::Ident],
    ctx: &mut RuleCtx,
) {
    let name = pgweave_schema::provider::object_name_to_string(name);
    let Some(table) = ctx.schema.get_table(&name) else {
        ctx.diags.push(pgweave_diagnostics::Diagnostic::error(
            codes::TABLE_NOT_FOUND,
            format!("table '{}' does not exist", name),
        ));
        return;
    };
    for col in columns {
        if table.get_column(&col.value).is_none() {
            ctx.diags.push(
                pgweave_diagnostics::Diagnostic::error(
                    codes::COLUMN_NOT_FOUND,
                    format!("column '{}' does not exist in table '{}'", col.value, name),
                )
                .with_hint(format!(
                    "available columns: {}",
                    table
                        .columns
                        .iter()
                        .map(|c| c.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
            );
        }
    }
}
