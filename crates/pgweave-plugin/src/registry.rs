use crate::plugin::Plugin;
use pgweave_checker::rule::CheckRule;
use pgweave_schema::provider::SchemaProvider;
use std::collections::HashMap;

/// Central registry of available plugins.
/// Built-in plugins are registered at startup; external plugins register themselves.
pub struct PluginRegistry {
    plugins: HashMap<String, Box<dyn Plugin>>,
}

impl PluginRegistry {
    pub fn new() -> Self {
        let mut r = Self {
            plugins: HashMap::new(),
        };
        // Register built-in plugins
        r.register(Box::new(crate::builtin::RlsEnforcerPlugin));
        // Register Prisma plugin
        r.register(Box::new(crate::prisma::plugin::PrismaPlugin));
        r
    }

    pub fn register(&mut self, plugin: Box<dyn Plugin>) {
        self.plugins.insert(plugin.name().to_string(), plugin);
    }

    /// Collect all rules from enabled plugins (those listed in config).
    pub fn collect_rules(
        &self,
        enabled: &HashMap<String, serde_json::Value>,
    ) -> Vec<Box<dyn CheckRule>> {
        enabled
            .iter()
            .filter_map(|(name, opts)| self.plugins.get(name).map(|p| p.rules(opts)))
            .flatten()
            .collect()
    }

    /// Collect all schema providers from enabled plugins.
    pub fn collect_schema_providers(
        &self,
        enabled: &HashMap<String, serde_json::Value>,
    ) -> Vec<Box<dyn SchemaProvider>> {
        enabled
            .iter()
            .filter_map(|(name, opts)| self.plugins.get(name)?.schema_provider(opts))
            .collect()
    }
}

impl Default for PluginRegistry {
    fn default() -> Self {
        Self::new()
    }
}
