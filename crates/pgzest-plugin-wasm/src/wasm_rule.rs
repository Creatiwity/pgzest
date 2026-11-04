use crate::wasm_plugin::WasmPlugin;
use pgzest_ast::ddl::CreateTable;
use pgzest_ast::stmt::SelectStmt;
use pgzest_checker::rule::{CheckRule, RuleCtx};
use std::sync::Arc;

/// Adapts a WasmPlugin as a CheckRule for use in the Checker walker.
pub struct WasmCheckRule {
    plugin: Arc<WasmPlugin>,
}

impl WasmCheckRule {
    pub fn new(plugin: Arc<WasmPlugin>) -> Self {
        Self { plugin }
    }
}

impl CheckRule for WasmCheckRule {
    fn name(&self) -> &'static str {
        "wasm-plugin"
    }

    fn check_select(&self, stmt: &SelectStmt, ctx: &mut RuleCtx) {
        match serde_json::to_string(stmt) {
            Ok(json) => {
                let diags = self.plugin.check_select_json(&json, ctx.schema);
                ctx.diags.extend(diags);
            }
            Err(e) => {
                eprintln!(
                    "pgzest[wasm:{}]: failed to serialize SelectStmt: {}",
                    self.plugin.name(),
                    e
                );
            }
        }
    }

    fn check_table(&self, ct: &CreateTable, ctx: &mut RuleCtx) {
        match serde_json::to_string(ct) {
            Ok(json) => {
                let diags = self.plugin.check_table_json(&json, ctx.schema);
                ctx.diags.extend(diags);
            }
            Err(e) => {
                eprintln!(
                    "pgzest[wasm:{}]: failed to serialize CreateTable: {}",
                    self.plugin.name(),
                    e
                );
            }
        }
    }
}
