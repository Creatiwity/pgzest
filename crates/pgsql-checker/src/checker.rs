use pgsql_ast::Statement;
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
        let mut diags = Vec::new();
        for stmt in stmts {
            self.check_statement(stmt, schema, &mut diags);
        }
        diags
    }

    fn check_statement(&self, stmt: &Statement, schema: &dyn SchemaProvider, diags: &mut Vec<Diagnostic>) {
        use pgsql_ast::{DdlStatement, Statement::*};

        for rule in &self.rules {
            let mut ctx = RuleCtx { schema, diags, options: serde_json::Value::Null };
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
    }
}
