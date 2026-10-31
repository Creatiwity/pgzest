use pgweave_ast::ddl::{CreateFunction, CreatePolicy, CreateProcedure, CreateTable, CreateTrigger};
use pgweave_ast::stmt::{DeleteStmt, InsertStmt, SelectStmt, UpdateStmt};
use pgweave_diagnostics::Diagnostic;
use pgweave_schema::provider::SchemaProvider;
use std::collections::HashMap;

/// Context passed to every rule invocation.
pub struct RuleCtx<'a> {
    /// Schema access (tables, functions).
    pub schema: &'a dyn SchemaProvider,
    /// Sink for diagnostics emitted by this rule.
    pub diags: &'a mut Vec<Diagnostic>,
    /// Rule-specific options. Not populated from `pgweave.toml` (v1 has no
    /// per-rule options); always `serde_json::Value::Null` today.
    pub options: serde_json::Value,
    /// All CREATE TRIGGER statements in the checked file, keyed by lowercase function name.
    /// Used by function rules to resolve the trigger table for RETURNS TRIGGER functions.
    pub trigger_map: &'a HashMap<String, Vec<&'a CreateTrigger>>,
}

/// A lint rule. Override only the methods relevant to this rule;
/// all methods are default no-ops.
pub trait CheckRule: Send + Sync {
    /// Short identifier used in diagnostic codes, e.g. "no-select-star".
    fn name(&self) -> &'static str;

    fn check_select(&self, _stmt: &SelectStmt, _ctx: &mut RuleCtx) {}
    fn check_insert(&self, _stmt: &InsertStmt, _ctx: &mut RuleCtx) {}
    fn check_update(&self, _stmt: &UpdateStmt, _ctx: &mut RuleCtx) {}
    fn check_delete(&self, _stmt: &DeleteStmt, _ctx: &mut RuleCtx) {}
    fn check_function(&self, _cf: &CreateFunction, _ctx: &mut RuleCtx) {}
    fn check_procedure(&self, _cp: &CreateProcedure, _ctx: &mut RuleCtx) {}
    fn check_policy(&self, _policy: &CreatePolicy, _ctx: &mut RuleCtx) {}
    fn check_table(&self, _ct: &CreateTable, _ctx: &mut RuleCtx) {}
}
