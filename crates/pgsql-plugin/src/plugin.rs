use pgsql_checker::{rule::CheckRule, SchemaProvider};
use serde_json::Value;

/// A plugin bundles optional schema contribution + lint rules + codegen hooks.
/// Implement only the methods relevant to your plugin.
pub trait Plugin: Send + Sync {
    /// Unique plugin identifier (e.g. "prisma", "rls-enforcer").
    fn name(&self) -> &'static str;

    /// Provide an additional SchemaProvider to merge with the base schema.
    /// Return None if this plugin doesn't contribute schema data.
    fn schema_provider(&self, options: &Value) -> Option<Box<dyn SchemaProvider>> {
        let _ = options;
        None
    }

    /// Lint rules contributed by this plugin.
    fn rules(&self, options: &Value) -> Vec<Box<dyn CheckRule>> {
        let _ = options;
        vec![]
    }
}
