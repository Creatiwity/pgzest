use crate::check::plpgsql::TriggerCtx;
use crate::rule::{CheckRule, RuleCtx};
use pgweave_ast::ddl::{
    CreateFunction, CreateProcedure, FunctionBody, ReturnType, TriggerEvent, TriggerForEach,
};
use pgweave_diagnostics::{Diagnostic, codes};
use pgweave_schema::provider::{SchemaProvider, object_name_to_string};

pub struct FunctionBodyCheck;

impl CheckRule for FunctionBodyCheck {
    fn name(&self) -> &'static str {
        "function-body-check"
    }

    fn check_function(&self, cf: &CreateFunction, ctx: &mut RuleCtx) {
        let func_name = object_name_to_string(&cf.name);
        if cf.return_type != ReturnType::Trigger {
            check_function_body(cf, &func_name, None, ctx.schema, ctx.diags);
            return;
        }

        let mut trigger_ctxs = resolve_trigger_ctxs(&func_name, ctx);
        match trigger_ctxs.len() {
            0 => {
                ctx.diags.push(
                    Diagnostic::warning(
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
                check_function_body(cf, &func_name, None, ctx.schema, ctx.diags);
            }
            1 => {
                let tc = trigger_ctxs.pop();
                check_function_body(cf, &func_name, tc, ctx.schema, ctx.diags);
            }
            // Une fonction trigger partagée entre plusieurs tables (le classique
            // `set_updated_at`) est un motif PostgreSQL idiomatique : NEW et OLD
            // prennent le type de la table qui déclenche. Le corps est donc
            // vérifié une fois par table, comme PostgreSQL l'exécuterait.
            _ => {
                let passes: Vec<(String, Vec<Diagnostic>)> = trigger_ctxs
                    .into_iter()
                    .map(|tc| {
                        let table = tc.table.clone();
                        let mut d = Vec::new();
                        check_function_body(cf, &func_name, Some(tc), ctx.schema, &mut d);
                        (table, d)
                    })
                    .collect();
                ctx.diags.extend(merge_per_table_passes(passes));
            }
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
                    raw, &proc_name, None, ctx.schema, ctx.diags,
                );
            }
            FunctionBody::SqlQuery(_) => {}
        }
    }
}

fn check_function_body(
    cf: &CreateFunction,
    func_name: &str,
    trigger_ctx: Option<TriggerCtx>,
    schema: &dyn SchemaProvider,
    diags: &mut Vec<Diagnostic>,
) {
    match &cf.body {
        FunctionBody::Parsed(body) => {
            crate::check::plpgsql::check_plpgsql_ast(
                body,
                func_name,
                &cf.return_type,
                trigger_ctx,
                &cf.params,
                schema,
                diags,
            );
        }
        FunctionBody::Raw(raw) => {
            // Raw path can't do plvar-aware analysis; pass table name for
            // the text-based NEW/OLD column checker at least.
            let tbl = trigger_ctx.as_ref().map(|tc| tc.table.as_str());
            crate::check::plpgsql::check_plpgsql_body(raw, func_name, tbl, schema, diags);
        }
        FunctionBody::SqlQuery(_) => {} // handled by SelectColumnCheck
    }
}

/// Resolve one trigger context per distinct table whose `CREATE TRIGGER`
/// references this `RETURNS TRIGGER` function, in order of first appearance.
/// Each context only carries the events and level of the triggers on its own
/// table.
fn resolve_trigger_ctxs(func_name: &str, ctx: &RuleCtx) -> Vec<TriggerCtx> {
    let func_key = pgweave_schema::provider::qualify_str(func_name, "public");
    let Some(triggers) = ctx.trigger_map.get(&func_key) else {
        return Vec::new();
    };

    let mut ctxs: Vec<TriggerCtx> = Vec::new();
    for trig in triggers {
        let table =
            pgweave_schema::provider::qualify_str(&object_name_to_string(&trig.table), "public");
        let idx = match ctxs.iter().position(|c| c.table == table) {
            Some(i) => i,
            None => {
                ctxs.push(TriggerCtx {
                    table,
                    events: Vec::new(),
                    has_statement_level: false,
                    has_insert: false,
                    has_delete: false,
                    has_update: false,
                });
                ctxs.len() - 1
            }
        };
        let tc = &mut ctxs[idx];
        if trig.for_each == TriggerForEach::Statement {
            tc.has_statement_level = true;
        }
        for ev in &trig.events {
            match ev {
                TriggerEvent::Insert => tc.has_insert = true,
                TriggerEvent::Delete => tc.has_delete = true,
                TriggerEvent::Update(_) => tc.has_update = true,
                _ => {}
            }
            tc.events.push(ev.clone());
        }
    }
    ctxs
}

/// Fusionne les diagnostics des passes « une par table ».
///
/// Un diagnostic produit par toutes les passes ne dépend pas de la table
/// (variable non déclarée, corps vide…) : il est émis une seule fois, tel quel.
/// Un diagnostic produit par une partie des passes seulement dépend de la
/// table : il est émis une fois, et nomme les tables concernées si son message
/// ne le fait pas déjà — sinon on ne saurait pas quel trigger casse.
fn merge_per_table_passes(passes: Vec<(String, Vec<Diagnostic>)>) -> Vec<Diagnostic> {
    type Key = (&'static str, String, Option<(usize, usize)>);
    let key = |d: &Diagnostic| -> Key {
        (
            d.code,
            d.message.clone(),
            d.span.as_ref().map(|s| (s.start, s.end)),
        )
    };

    // Ordre de première apparition, pour une sortie stable.
    let mut merged: Vec<(Key, Diagnostic, Vec<&str>)> = Vec::new();
    for (table, diags) in &passes {
        for d in diags {
            let k = key(d);
            match merged.iter_mut().find(|(mk, _, _)| *mk == k) {
                Some((_, _, tables)) => {
                    if !tables.contains(&table.as_str()) {
                        tables.push(table);
                    }
                }
                None => merged.push((k, d.clone(), vec![table])),
            }
        }
    }

    let bare = |t: &str| t.rsplit('.').next().unwrap_or(t).to_string();
    merged
        .into_iter()
        .map(|(_, mut d, tables)| {
            if tables.len() < passes.len() && !tables.iter().any(|t| d.message.contains(&bare(t))) {
                d.message = format!("{} (trigger on {})", d.message, tables.join(", "));
            }
            d
        })
        .collect()
}
