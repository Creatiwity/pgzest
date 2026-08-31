use crate::rule::{CheckRule, RuleCtx};
use pgsql_ast::stmt::{DeleteStmt, InsertStmt, UpdateStmt};
use pgsql_diagnostics::codes;

pub struct DmlTableCheck;

impl CheckRule for DmlTableCheck {
    fn name(&self) -> &'static str {
        "dml-table-check"
    }

    fn check_insert(&self, stmt: &InsertStmt, ctx: &mut RuleCtx) {
        let name = pgsql_schema::provider::object_name_to_string(&stmt.table);
        if ctx.schema.get_table(&name).is_none() {
            ctx.diags.push(
                pgsql_diagnostics::Diagnostic::error(
                    codes::TABLE_NOT_FOUND,
                    format!("table '{}' does not exist", name),
                )
                .with_hint("check the table name or provide a schema with that table"),
            );
        }
    }

    fn check_update(&self, stmt: &UpdateStmt, ctx: &mut RuleCtx) {
        let name = pgsql_schema::provider::object_name_to_string(&stmt.table.name);
        if ctx.schema.get_table(&name).is_none() {
            ctx.diags.push(pgsql_diagnostics::Diagnostic::error(
                codes::TABLE_NOT_FOUND,
                format!("table '{}' does not exist", name),
            ));
        }
    }

    fn check_delete(&self, stmt: &DeleteStmt, ctx: &mut RuleCtx) {
        let name = pgsql_schema::provider::object_name_to_string(&stmt.table.name);
        if ctx.schema.get_table(&name).is_none() {
            ctx.diags.push(pgsql_diagnostics::Diagnostic::error(
                codes::TABLE_NOT_FOUND,
                format!("table '{}' does not exist", name),
            ));
        }
    }
}
