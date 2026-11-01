use crate::rule::{CheckRule, RuleCtx};
use pgweave_ast::ddl::{CreateFunction, FunctionBody};
use std::collections::HashSet;

pub struct SelectColumnCheck;

impl CheckRule for SelectColumnCheck {
    fn name(&self) -> &'static str {
        "select-column-check"
    }

    fn check_select(&self, stmt: &pgweave_ast::stmt::SelectStmt, ctx: &mut RuleCtx) {
        let diags = crate::check::select::check_select(stmt, ctx.schema);
        ctx.diags.extend(diags);
    }

    fn check_function(&self, cf: &CreateFunction, ctx: &mut RuleCtx) {
        if let FunctionBody::SqlQuery(sel) = &cf.body {
            let plvars: HashSet<String> = cf
                .params
                .iter()
                .filter_map(|p| p.name.as_ref())
                .map(|n| n.value.to_string())
                .collect();
            let diags = crate::check::select::check_select_with_plvars(sel, ctx.schema, &plvars);
            ctx.diags.extend(diags);
        }
    }
}
