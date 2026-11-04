use crate::plugin::Plugin;
use pgzest_checker::rule::CheckRule;
use pgzest_schema::provider::SchemaProvider;
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
        r.register(Box::new(crate::prisma::plugin::PrismaPlugin));
        // Documented for a long time (`postgis = {}`) but never registered:
        // enabling it did nothing, silently.
        r.register(Box::new(crate::postgis::PostGisPlugin));
        r
    }

    pub fn register(&mut self, plugin: Box<dyn Plugin>) {
        self.plugins.insert(plugin.name().to_string(), plugin);
    }

    /// Checks that every name designates a registered plugin.
    ///
    /// Otherwise an unknown name (typo, plugin not shipped) would be
    /// silently ignored: the project would believe the plugin is active.
    pub fn check_names<'a>(
        &self,
        names: impl IntoIterator<Item = &'a String>,
    ) -> Result<(), String> {
        let unknown: Vec<&str> = names
            .into_iter()
            .filter(|n| !self.plugins.contains_key(n.as_str()))
            .map(String::as_str)
            .collect();
        if unknown.is_empty() {
            return Ok(());
        }
        let mut known: Vec<&str> = self.plugins.keys().map(String::as_str).collect();
        known.sort_unstable();
        Err(format!(
            "[plugins] builtin : plugin(s) inconnu(s) : {} (disponibles : {})",
            unknown.join(", "),
            known.join(", ")
        ))
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
