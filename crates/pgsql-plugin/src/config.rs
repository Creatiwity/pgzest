use serde::Deserialize;
use std::collections::HashMap;

/// Parsed representation of pgsql.toml.
#[derive(Debug, Deserialize, Default)]
pub struct PgsqlConfig {
    #[serde(default)]
    pub plugins: HashMap<String, serde_json::Value>,

    #[serde(default)]
    pub rules: HashMap<String, RuleConfig>,

    #[serde(default)]
    pub schema: SchemaConfig,

    /// Paths to external WASM plugin files.
    #[serde(default)]
    pub wasm_plugins: Vec<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct SchemaConfig {
    /// Path to an external DDL schema file.
    pub file: Option<String>,
}

/// Rule config: can be just a severity string or a full object.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum RuleConfig {
    Level(String),
    Full {
        level: String,
        #[serde(flatten)]
        options: serde_json::Value,
    },
}

impl RuleConfig {
    pub fn level(&self) -> &str {
        match self {
            RuleConfig::Level(s) => s,
            RuleConfig::Full { level, .. } => level,
        }
    }
}

/// Re-exported for convenience; same as PgsqlConfig.
pub type PluginConfig = PgsqlConfig;

impl PgsqlConfig {
    /// Load from a pgsql.toml file. Returns default config if the file doesn't exist.
    pub fn load(path: &std::path::Path) -> Result<Self, ConfigError> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(path)?;
        let config: Self = toml::from_str(&content)?;
        Ok(config)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("toml parse error: {0}")]
    Toml(#[from] toml::de::Error),
}
