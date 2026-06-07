use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::role::FileRole;
use crate::source::{DirSource, FileSource, MultiSource, SchemaSource};

// ── Top-level config ──────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Default)]
pub struct ProjectConfig {
    #[serde(default)]
    pub schema: SchemaConfig,
    #[serde(default)]
    pub migrations: MigrationsConfig,
    #[serde(default)]
    pub instrumentation: InstrumentationConfig,
    #[serde(default)]
    pub lint: LintProjectConfig,
    #[serde(default)]
    pub plugins: PluginsConfig,
}

// ── Schema section ────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Default)]
pub struct SchemaConfig {
    #[serde(default)]
    pub sources: Vec<SourceEntry>,
    #[serde(default = "default_search_path")]
    pub search_path: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct SourceEntry {
    pub path: String,
    /// "migration" | "schema" | "query" — if absent, auto-detected per file.
    pub role: Option<String>,
}

fn default_search_path() -> Vec<String> {
    vec!["public".into()]
}

// ── Migrations section ────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct MigrationsConfig {
    #[serde(default = "default_migrations_dir")]
    pub directory: String,
    /// `{sequence:06}_{name}.sql` or timestamp-based, etc.
    #[serde(default = "default_format")]
    pub format: String,
    #[serde(default = "default_hash_journal")]
    pub hash_journal: String,
}

impl Default for MigrationsConfig {
    fn default() -> Self {
        Self {
            directory: default_migrations_dir(),
            format: default_format(),
            hash_journal: default_hash_journal(),
        }
    }
}

fn default_migrations_dir() -> String { "migrations/".into() }
fn default_format()        -> String { "{sequence:06}_{name}.sql".into() }
fn default_hash_journal()  -> String { "pgsql-tools-hashes.json".into() }

// ── Instrumentation section ───────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct InstrumentationConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_log_level")]
    pub log_level: String,   // "none" | "step" | "debug"
    #[serde(default = "default_log_target")]
    pub log_target: String,  // "notify" | "table" | "both"
}

impl Default for InstrumentationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            log_level: default_log_level(),
            log_target: default_log_target(),
        }
    }
}

fn default_true()       -> bool   { true }
fn default_log_level()  -> String { "step".into() }
fn default_log_target() -> String { "notify".into() }

// ── Lint section ──────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Default)]
pub struct LintProjectConfig {
    #[serde(default)]
    pub disabled: Vec<String>,
}

// ── Plugins section ───────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Default)]
pub struct PluginsConfig {
    #[serde(default)]
    pub enabled: Vec<String>,
}

// ── Loading ───────────────────────────────────────────────────────────────────

impl ProjectConfig {
    pub const FILENAME: &'static str = "pgsql-tools.toml";

    /// Load from a specific `pgsql-tools.toml` path.
    pub fn load(path: &Path) -> Result<Self, String> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        toml::from_str(&content)
            .map_err(|e| format!("cannot parse {}: {e}", path.display()))
    }

    /// Walk up from `start` (file or directory) looking for `pgsql-tools.toml`.
    /// Returns the project root directory and the loaded config.
    pub fn find_and_load(start: &Path) -> Option<(PathBuf, Self)> {
        let mut dir: &Path = if start.is_file() { start.parent()? } else { start };
        loop {
            let candidate = dir.join(Self::FILENAME);
            if candidate.exists() {
                let config = Self::load(&candidate).ok()?;
                return Some((dir.to_path_buf(), config));
            }
            dir = dir.parent()?;
        }
    }

    /// Build a `MultiSource` from this config, resolving paths relative to `base_dir`.
    pub fn to_source(&self, base_dir: &Path) -> MultiSource {
        let mut sources: Vec<Box<dyn SchemaSource>> = Vec::new();

        for entry in &self.schema.sources {
            let path = base_dir.join(&entry.path);
            let role_override = entry
                .role
                .as_deref()
                .and_then(FileRole::from_str);

            if path.is_dir() {
                let mut src = DirSource::new(&path);
                if let Some(r) = role_override {
                    src.role_override = Some(r);
                }
                sources.push(Box::new(src));
            } else if path.exists() || !path.to_str().map(|s| s.contains('*')).unwrap_or(false) {
                let mut src = FileSource::new(&path);
                if let Some(r) = role_override {
                    src.role_override = Some(r);
                }
                sources.push(Box::new(src));
            }
        }

        // Default fallback: scan migrations/ directory if no sources configured.
        if sources.is_empty() {
            let mig = base_dir.join(&self.migrations.directory);
            if mig.is_dir() {
                sources.push(Box::new(DirSource::new(mig)));
            }
        }

        MultiSource::new(sources)
    }
}
