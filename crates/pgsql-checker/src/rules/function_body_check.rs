use crate::check::plpgsql::TriggerCtx;
use crate::codes;
use crate::rule::{CheckRule, RuleCtx};
use crate::schema::object_name_to_string;
use pgsql_ast::ddl::{CreateFunction, CreateProcedure, FunctionBody, ReturnType, TriggerEvent, TriggerForEach};

pub struct FunctionBodyCheck;

impl CheckRule for FunctionBodyCheck {
    fn name(&self) -> &'static str {
        "function-body-check"
    }

    fn check_function(&self, cf: &CreateFunction, ctx: &mut RuleCtx) {
        let func_name = object_name_to_string(&cf.name);
        let trigger_ctx = if cf.return_type == ReturnType::Trigger {
            let diags_before = ctx.diags.len();
            let tc = resolve_trigger_ctx(&func_name, ctx);
            let e005_fired = ctx.diags.len() > diags_before;
            if tc.is_none() && !e005_fired {
                ctx.diags.push(
                    crate::diagnostic::Diagnostic::warning(
                        codes::TRIGGER_NO_CONTEXT,
                        format!(
                            "trigger function '{}' has no associated CREATE TRIGGER in the \
                             analyzed files: NEW/OLD schema cannot be verified",
                            func_name
                        ),
                    )
                    .with_hint(
                        "pass the file containing CREATE TRIGGER as a --schema argument \
                         so the trigger table can be resolved",
                    ),
                );
            }
            tc
        } else {
            None
        };
        match &cf.body {
            FunctionBody::Parsed(body) => {
                crate::check::plpgsql::check_plpgsql_ast(
                    body,
                    &func_name,
                    &cf.return_type,
                    trigger_ctx,
                    &cf.params,
                    ctx.schema,
                    ctx.diags,
                );
            }
            FunctionBody::Raw(raw) => {
                let tbl = trigger_ctx.as_ref().map(|tc| tc.table.as_str());
                // Raw path can't do plvar-aware analysis; pass table name for
                // the text-based NEW/OLD column checker at least.
                // SAFETY: `tbl` borrows `trigger_ctx` which is in scope here.
                crate::check::plpgsql::check_plpgsql_body(
                    raw,
                    &func_name,
                    tbl,
                    ctx.schema,
                    ctx.diags,
                );
            }
            FunctionBody::SqlQuery(_) => {} // handled by SelectColumnCheck
        }
    }

    fn check_procedure(&self, cp: &CreateProcedure, ctx: &mut RuleCtx) {
        let proc_name = object_name_to_string(&cp.name);
        let return_type = ReturnType::Void;
        match &cp.body {
            FunctionBody::Parsed(body) => {
                crate::check::plpgsql::check_plpgsql_ast(
                    body,
                    &proc_name,
                    &return_type,
                    None, // procedures are never triggers
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

/// Resolve the trigger context for a `RETURNS TRIGGER` function by scanning the
/// trigger map built from the current file's `CREATE TRIGGER` statements.
///
/// Emits E005 if multiple triggers reference the same function but point to
/// different tables — the checker cannot determine which schema to use for NEW/OLD.
fn resolve_trigger_ctx(func_name: &str, ctx: &mut RuleCtx) -> Option<TriggerCtx> {
    let func_key = crate::schema::qualify_str(func_name, "public");
    let triggers = ctx.trigger_map.get(&func_key)?;

    // Collect distinct trigger tables.
    let tables: Vec<String> = {
        let mut seen = std::collections::HashSet::new();
        triggers
            .iter()
            .map(|t| crate::schema::qualify_str(&object_name_to_string(&t.table), "public"))
            .filter(|t| seen.insert(t.clone()))
            .collect()
    };

    if tables.len() > 1 {
        ctx.diags.push(
            crate::diagnostic::Diagnostic::error(
                codes::TRIGGER_AMBIGUOUS_TABLE,
                format!(
                    "trigger function '{}' is used by triggers on different tables ({}): \
                     NEW/OLD schema is ambiguous",
                    func_name,
                    tables.join(", ")
                ),
            )
            .with_hint(
                "split the function into per-table variants or use a generic RECORD body",
            ),
        );
        return None;
    }

    let table = tables.into_iter().next()?;

    let mut has_insert = false;
    let mut has_delete = false;
    let mut has_update = false;
    let mut has_statement_level = false;
    let mut all_events: Vec<TriggerEvent> = Vec::new();

    for trig in triggers {
        if trig.for_each == TriggerForEach::Statement {
            has_statement_level = true;
        }
        for ev in &trig.events {
            match ev {
                TriggerEvent::Insert => has_insert = true,
                TriggerEvent::Delete => has_delete = true,
                TriggerEvent::Update(_) => has_update = true,
                _ => {}
            }
            all_events.push(ev.clone());
        }
    }

    Some(TriggerCtx {
        table,
        events: all_events,
        has_statement_level,
        has_insert,
        has_delete,
        has_update,
    })
}
