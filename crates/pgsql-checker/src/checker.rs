use std::collections::HashMap;
use pgsql_ast::Statement;
use pgsql_ast::ddl::CreateTrigger;
use crate::rule::{CheckRule, RuleCtx};
use crate::schema::SchemaProvider;
use crate::diagnostic::Diagnostic;

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
        use pgsql_ast::{DdlStatement, Statement::*};

        let first_new = diags.len();

        for rule in &self.rules {
            let mut ctx = RuleCtx { schema, diags, options: serde_json::Value::Null, trigger_map };
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
        if let Some(span) = stmt_span(stmt) {
            for d in &mut diags[first_new..] {
                if d.span.is_none() {
                    d.span = Some(span.clone());
                }
            }
        }
    }
}

/// Extract the byte-offset span from any Statement variant.
/// Returns `None` for statements whose AST node doesn't carry a span or
/// whose span is the sentinel DUMMY value (start == 0 && end == 0).
fn stmt_span(stmt: &Statement) -> Option<crate::diagnostic::Span> {
    use pgsql_ast::{DdlStatement, Statement::*};
    let (start, end) = match stmt {
        Select(s)  => (s.span.start, s.span.end),
        Insert(s)  => (s.span.start, s.span.end),
        Update(s)  => (s.span.start, s.span.end),
        Delete(s)  => (s.span.start, s.span.end),
        Ddl(ddl)   => match ddl {
            DdlStatement::CreateFunction(cf)  => (cf.span.start, cf.span.end),
            DdlStatement::CreateProcedure(cp) => (cp.span.start, cp.span.end),
            DdlStatement::CreatePolicy(p)     => (p.span.start,  p.span.end),
            DdlStatement::CreateTable(ct)     => (ct.span.start, ct.span.end),
            _ => return None,
        },
    };
    // Span { 0, 0 } is the DUMMY sentinel — only skip when both are 0.
    if start == 0 && end == 0 { return None; }
    Some(crate::diagnostic::Span { start: start as usize, end: end as usize })
}

/// Scan all statements for CREATE TRIGGER and return a map from
/// qualified function name → list of triggers referencing that function.
/// Function names are stored as `"schema.name"` (defaulting to `"public"`).
fn build_trigger_map(stmts: &[Statement]) -> HashMap<String, Vec<&CreateTrigger>> {
    use pgsql_ast::{DdlStatement, Statement::Ddl};
    let mut map: HashMap<String, Vec<&CreateTrigger>> = HashMap::new();
    for stmt in stmts {
        if let Ddl(DdlStatement::CreateTrigger(trig)) = stmt {
            let func_key = crate::schema::qualify_str(
                &crate::schema::object_name_to_string(&trig.function),
                "public",
            );
            map.entry(func_key).or_default().push(trig);
        }
    }
    map
}
