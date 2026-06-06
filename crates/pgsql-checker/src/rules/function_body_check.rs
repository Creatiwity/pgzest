use crate::rule::{CheckRule, RuleCtx};
use pgsql_ast::ddl::{CreateFunction, CreateProcedure, FunctionBody, ReturnType};

pub struct FunctionBodyCheck;

impl CheckRule for FunctionBodyCheck {
    fn name(&self) -> &'static str {
        "function-body-check"
    }

    fn check_function(&self, cf: &CreateFunction, ctx: &mut RuleCtx) {
        let func_name = crate::schema::object_name_to_string(&cf.name);
        match &cf.body {
            FunctionBody::Parsed(body) => {
                crate::check::plpgsql::check_plpgsql_ast(
                    body,
                    &func_name,
                    &cf.return_type,
                    None,
                    &cf.params,
                    ctx.schema,
                    ctx.diags,
                );
            }
            FunctionBody::Raw(raw) => {
                crate::check::plpgsql::check_plpgsql_body(
                    raw,
                    &func_name,
                    None,
                    ctx.schema,
                    ctx.diags,
                );
            }
            FunctionBody::SqlQuery(_) => {} // handled by SelectColumnCheck
        }
    }

    fn check_procedure(&self, cp: &CreateProcedure, ctx: &mut RuleCtx) {
        let proc_name = crate::schema::object_name_to_string(&cp.name);
        let return_type = ReturnType::Void;
        match &cp.body {
            FunctionBody::Parsed(body) => {
                crate::check::plpgsql::check_plpgsql_ast(
                    body,
                    &proc_name,
                    &return_type,
                    None,
                    &cp.params,
                    ctx.schema,
                    ctx.diags,
                );
            }
            FunctionBody::Raw(raw) => {
                crate::check::plpgsql::check_plpgsql_body(
                    raw,
                    &proc_name,
                    None,
                    ctx.schema,
                    ctx.diags,
                );
            }
            FunctionBody::SqlQuery(_) => {}
        }
    }
}
