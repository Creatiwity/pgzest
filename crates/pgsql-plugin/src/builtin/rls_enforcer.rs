use pgsql_ast::ddl::CreateTable;
use pgsql_ast::stmt::SelectStmt;
use pgsql_checker::rule::{CheckRule, RuleCtx};
use pgsql_schema::provider::object_name_to_string;

pub struct RlsEnforcerPlugin;

impl crate::plugin::Plugin for RlsEnforcerPlugin {
    fn name(&self) -> &'static str {
        "rls-enforcer"
    }

    fn rules(&self, _options: &serde_json::Value) -> Vec<Box<dyn CheckRule>> {
        vec![Box::new(RequireRlsRule), Box::new(NoSelectStarRule)]
    }
}

/// rls/table-without-rls: table does not have RLS enabled
/// This is a warning (not error) because RLS can be enabled separately via ALTER TABLE.
pub struct RequireRlsRule;

impl CheckRule for RequireRlsRule {
    fn name(&self) -> &'static str {
        "require-rls"
    }

    fn check_table(&self, ct: &CreateTable, ctx: &mut RuleCtx) {
        let table_name = object_name_to_string(&ct.name);
        if let Some(provider) = ctx.schema.as_rls_aware()
            && !provider.table_has_rls(&table_name)
        {
            ctx.diags.push(
                pgsql_diagnostics::Diagnostic::warning(
                    "rls/table-without-rls",
                    format!("table '{}' does not have RLS enabled", table_name),
                )
                .with_hint(format!(
                    "add: ALTER TABLE {} ENABLE ROW LEVEL SECURITY",
                    table_name
                )),
            );
        }
    }
}

/// rls/select-star: SELECT * in application queries
pub struct NoSelectStarRule;

impl CheckRule for NoSelectStarRule {
    fn name(&self) -> &'static str {
        "no-select-star"
    }

    fn check_select(&self, stmt: &SelectStmt, ctx: &mut RuleCtx) {
        use pgsql_ast::SelectItem;
        for item in &stmt.columns {
            if matches!(item, SelectItem::Wildcard) {
                ctx.diags.push(
                    pgsql_diagnostics::Diagnostic::warning(
                        "rls/select-star",
                        "SELECT * is not recommended in application queries".to_string(),
                    )
                    .with_hint("list columns explicitly for better performance and type safety"),
                );
                break;
            }
        }
    }
}
