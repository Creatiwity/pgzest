use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::role::{sequence_from_path, FileRole};
use crate::source::{DirSource, MigrationDirSource, MultiSource, SchemaSource};

// ── Top-level config ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ProjectConfig {
    #[serde(default)]
    pub migrations: MigrationsConfig,
    #[serde(default)]
    pub schema: SchemaConfig,
    #[serde(default)]
    pub instrumentation: InstrumentationConfig,
    #[serde(default)]
    pub lint: LintProjectConfig,
    #[serde(default)]
    pub plugins: PluginsConfig,
}

// ── Migrations ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct MigrationsConfig {
    /// Directory containing numbered migration files.
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

// ── Schema ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Default)]
pub struct SchemaConfig {
    /// Optional directory for static DDL files (schema.sql, types.sql, …).
    /// If absent: every non-migration file opened in the editor is treated as Schema.
    /// If present: only files in this directory are Schema; others are Query.
    pub directory: Option<String>,
    #[serde(default = "default_search_path")]
    pub search_path: Vec<String>,
}

fn default_search_path() -> Vec<String> { vec!["public".into()] }

// ── Instrumentation ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct InstrumentationConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_log_level")]
    pub log_level: String,
    #[serde(default = "default_log_target")]
    pub log_target: String,
}

impl Default for InstrumentationConfig {
    fn default() -> Self {
        Self { enabled: true, log_level: default_log_level(), log_target: default_log_target() }
    }
}

fn default_true()       -> bool   { true }
fn default_log_level()  -> String { "step".into() }
fn default_log_target() -> String { "notify".into() }

// ── Lint ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Default)]
pub struct LintProjectConfig {
    #[serde(default)]
    pub disabled: Vec<String>,
}

// ── Plugins ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Default)]
pub struct PluginsConfig {
    #[serde(default)]
    pub enabled: Vec<String>,
    /// Paths to external WASM plugin files (relative to the config file).
    #[serde(default)]
    pub wasm: Vec<String>,
}

// ── Loading ───────────────────────────────────────────────────────────────────

impl ProjectConfig {
    pub const FILENAME: &'static str = "pgsql-tools.config.toml";

    pub fn load(path: &Path) -> Result<Self, String> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        toml::from_str(&content)
            .map_err(|e| format!("cannot parse {}: {e}", path.display()))
    }

    /// Walk up from `start` looking for `pgsql-tools.config.toml`.
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

    /// Build a `MultiSource` from this config.
    ///
    /// Includes:
    /// 1. Migration files from `migrations.directory`
    /// 2. Schema files from `schema.directory` (if configured)
    pub fn to_source(&self, base_dir: &Path) -> MultiSource {
        let mut sources: Vec<Box<dyn SchemaSource>> = Vec::new();

        // Migration files — each gets sequence from filename
        let mig_dir = base_dir.join(&self.migrations.directory);
        if mig_dir.is_dir() {
            sources.push(Box::new(MigrationDirSource::new(mig_dir)));
        }

        // Schema files (explicit directory)
        if let Some(ref sdir) = self.schema.directory {
            let schema_dir = base_dir.join(sdir);
            if schema_dir.is_dir() {
                sources.push(Box::new(DirSource::new(schema_dir, FileRole::Schema)));
            }
        }

        MultiSource::new(sources)
    }

    /// Classify any file by path relative to the project root.
    ///
    /// Rules (config-driven, no filename heuristics):
    /// 1. In `migrations.directory` → Migration (sequence from filename)
    /// 2. In `schema.directory` (if configured) → Schema
    /// 3. Otherwise:
    ///    - If no `schema.directory` configured → Schema (the whole project is schema)
    ///    - If `schema.directory` configured    → Query (SFC/template files, future)
    pub fn classify_file(&self, path: &Path, base_dir: &Path) -> FileRole {
        let mig_dir = base_dir.join(&self.migrations.directory);
        let mig_canon = mig_dir.canonicalize().unwrap_or(mig_dir);
        let path_canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());

        if path_canon.starts_with(&mig_canon) {
            return FileRole::Migration { sequence: sequence_from_path(&path_canon) };
        }

        if let Some(ref sdir) = self.schema.directory {
            let schema_dir = base_dir.join(sdir);
            let schema_canon = schema_dir.canonicalize().unwrap_or(schema_dir);
            if path_canon.starts_with(&schema_canon) {
                return FileRole::Schema;
            }
            // schema.directory is set but this file isn't in it → Query
            return FileRole::Query;
        }

        // No schema.directory configured → every non-migration file is Schema
        FileRole::Schema
    }
}
