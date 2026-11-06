use pgzest_analysis::rule::CheckRule;
use serde_json::Value;

/// A plugin contributes check rules. It never contributes schema: the
/// catalog comes from the project's SQL and the generated packs only
/// (PLAN.md D2, D9).
pub trait Plugin: Send + Sync {
    /// Unique plugin identifier (e.g. "rls-enforcer").
    fn name(&self) -> &'static str;

    /// Lint rules contributed by this plugin.
    fn rules(&self, options: &Value) -> Vec<Box<dyn CheckRule>> {
        let _ = options;
        vec![]
    }
}
