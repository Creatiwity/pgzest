use crate::rule::{CheckRule, RuleCtx};
use pgweave_ast::Statement;
use pgweave_ast::ddl::CreateTrigger;
use pgweave_diagnostics::Diagnostic;
use pgweave_schema::provider::SchemaProvider;
use std::collections::HashMap;

pub struct Checker {
    rules: Vec<Box<dyn CheckRule>>,
}

impl Checker {
    pub fn new(rules: Vec<Box<dyn CheckRule>>) -> Self {
        Self { rules }
    }

    pub fn with_builtin_rules() -> Self {
        Self::new(crate::rules::builtin_rules())
    }

    pub fn add_rule(&mut self, rule: Box<dyn CheckRule>) {
        self.rules.push(rule);
    }

    pub fn run(&self, stmts: &[Statement], schema: &dyn SchemaProvider) -> Vec<Diagnostic> {
        self.run_with_context(stmts, &[], schema)
    }

    /// Like `run`, but also takes `context_stmts` that contribute to the
    /// trigger map (and any future cross-file analysis) without themselves
    /// being checked.  Pass `CREATE TRIGGER` statements from other files here
    /// so that `RETURNS TRIGGER` functions can resolve NEW/OLD schemas.
    pub fn run_with_context(
        &self,
        stmts: &[Statement],
        context_stmts: &[Statement],
        schema: &dyn SchemaProvider,
    ) -> Vec<Diagnostic> {
        // Build trigger_map from ALL statements (target + context) so that
        // RETURNS TRIGGER functions find their trigger tables even when the
        // CREATE TRIGGER is in a different file.
        let mut trigger_map = build_trigger_map(context_stmts);
        for (key, triggers) in build_trigger_map(stmts) {
            trigger_map.entry(key).or_default().extend(triggers);
        }

        let mut diags = Vec::new();
        for stmt in stmts {
            self.check_statement(stmt, schema, &trigger_map, &mut diags);
        }
        diags
    }

    fn check_statement(
        &self,
        stmt: &Statement,
        schema: &dyn SchemaProvider,
        trigger_map: &HashMap<String, Vec<&CreateTrigger>>,
        diags: &mut Vec<Diagnostic>,
    ) {
        use pgweave_ast::{DdlStatement, Statement::*};

        let first_new = diags.len();

        for rule in &self.rules {
            let mut ctx = RuleCtx {
                schema,
                diags,
                options: serde_json::Value::Null,
                trigger_map,
            };
            match stmt {
                Select(sel) => rule.check_select(sel, &mut ctx),
                Insert(ins) => rule.check_insert(ins, &mut ctx),
                Update(upd) => rule.check_update(upd, &mut ctx),
                Delete(del) => rule.check_delete(del, &mut ctx),
                Ddl(ddl) => match ddl {
                    DdlStatement::CreateFunction(cf) => rule.check_function(cf, &mut ctx),
                    DdlStatement::CreateProcedure(cp) => rule.check_procedure(cp, &mut ctx),
                    DdlStatement::CreatePolicy(policy) => rule.check_policy(policy, &mut ctx),
                    DdlStatement::CreateTable(ct) => rule.check_table(ct, &mut ctx),
                    _ => {}
                },
            }
        }

        // Backfill the statement-level span on every diagnostic that was emitted
        // without an explicit span — this places squiggles on the correct statement
        // rather than defaulting to position 0 in the file.
        // `pgweave_ast::stmt_span` est le helper partagé ; ce module en avait une
        // copie privée identique.
        if let Some(span) = pgweave_ast::stmt_span(stmt).map(|sp| pgweave_diagnostics::Span {
            start: sp.start as usize,
            end: sp.end as usize,
        }) {
            for d in &mut diags[first_new..] {
                if d.span.is_none() {
                    d.span = Some(span.clone());
                }
            }
        }
    }
}

/// Scan all statements for CREATE TRIGGER and return a map from
/// qualified function name → list of triggers referencing that function.
/// Function names are stored as `"schema.name"` (defaulting to `"public"`).
fn build_trigger_map(stmts: &[Statement]) -> HashMap<String, Vec<&CreateTrigger>> {
    use pgweave_ast::{DdlStatement, Statement::Ddl};
    let mut map: HashMap<String, Vec<&CreateTrigger>> = HashMap::new();
    for stmt in stmts {
        if let Ddl(DdlStatement::CreateTrigger(trig)) = stmt {
            let func_key = pgweave_schema::provider::qualify_str(
                &pgweave_schema::provider::object_name_to_string(&trig.function),
                "public",
            );
            map.entry(func_key).or_default().push(trig);
        }
    }
    map
}
