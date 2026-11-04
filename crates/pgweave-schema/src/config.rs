//! Project configuration: `pgweave.toml`, format v1.
//!
//! Reference specification: `docs/config.md`. Any change to this file must
//! be reflected there.
//!
//! Design rules:
//! - **Strict**: an unknown key is an error (`deny_unknown_fields`). A typo
//!   in a key must never turn into a silently ignored setting.
//! - **Nothing accepted without effect**: a key exists in the format only if
//!   the tool applies it. Planned changes are described in `docs/config.md`
//!   but rejected until they are implemented.
//! - **Errors surfaced**: an unreadable or invalid file is an error for the
//!   caller, never a fallback to the default config.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::role::{FileRole, sequence_from_path};
use crate::source::{DirSource, MigrationDirSource, MultiSource, SchemaSource};

/// PostgreSQL major versions for which analysis is available.
///
/// A version missing from this list is rejected rather than analyzed with the
/// rules of another one: that would be guessing (PLAN.md, D9).
pub const SUPPORTED_POSTGRES_VERSIONS: &[u32] = &[18];

// ── Top-level config ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectConfig {
    /// Target PostgreSQL context. Required.
    pub postgres: PostgresConfig,
    #[serde(default)]
    pub schema: SchemaConfig,
    #[serde(default)]
    pub migrations: MigrationsConfig,
    #[serde(default)]
    pub queries: QueriesConfig,
    #[serde(default)]
    pub macros: MacrosConfig,
    #[serde(default)]
    pub parse: ParseConfig,
    /// Severity overrides per diagnostic code, checker and lint alike.
    #[serde(default)]
    pub rules: BTreeMap<String, RuleLevel>,
    #[serde(default)]
    pub plugins: PluginsConfig,
    #[serde(default)]
    pub rust: RustConfig,
    #[serde(default)]
    pub log: LogConfig,
}

// ── Run logs ──────────────────────────────────────────────────────────────────

/// Run logs of the PL/pgSQL macro routines (PLAN.md, P9). The level is part
/// of the generated code: analysis, migrations and `pgweave build` all see
/// the routines instrumented at this level, and each generated migration
/// records it in its header (`log=…`).
#[derive(Debug, Clone, Copy, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct LogConfig {
    #[serde(default)]
    pub level: LogLevelConfig,
    #[serde(default)]
    pub target: LogTargetConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum LogLevelConfig {
    #[default]
    None,
    Info,
    Step,
    Debug,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum LogTargetConfig {
    #[default]
    Table,
    Notify,
}

impl LogConfig {
    pub fn instrumentation(&self) -> pgweave_macro::Instrumentation {
        use pgweave_macro::{LogLevel, LogTarget};
        pgweave_macro::Instrumentation {
            log: match self.level {
                LogLevelConfig::None => LogLevel::None,
                LogLevelConfig::Info => LogLevel::Info,
                LogLevelConfig::Step => LogLevel::Step,
                LogLevelConfig::Debug => LogLevel::Debug,
            },
            target: match self.target {
                LogTargetConfig::Table => LogTarget::Table,
                LogTargetConfig::Notify => LogTarget::Notify,
            },
        }
    }
}

// ── Rust ──────────────────────────────────────────────────────────────────────

/// Generated Rust code (`pgweave-build`, `pgweave-macros`).
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct RustConfig {
    /// Module where `pgweave_macros::types!()` generates the project enums
    /// (`"crate::db"`). When set, query code refers to it instead of defining
    /// its own; required as soon as a `sql!` query uses an enum.
    pub types: Option<String>,
}

// ── PostgreSQL ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PostgresConfig {
    /// Targeted major version (e.g. `18`). Must be listed in
    /// [`SUPPORTED_POSTGRES_VERSIONS`].
    pub version: u32,
}

// ── Schema ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct SchemaConfig {
    /// Directory of declarative schema files, walked recursively.
    ///
    /// Absent: every file outside migrations and queries is a schema file,
    /// and the indexing scope is the project root (see
    /// [`ProjectConfig::to_source`]). Present: only the files of this
    /// directory (and of `include`) are schema files.
    pub directory: Option<String>,
    /// Additional directories to index, on top of `directory`.
    ///
    /// Useful when the DDL is split (`db/tables/`, `db/functions/`) without a
    /// dedicated common parent directory.
    #[serde(default)]
    pub include: Vec<String>,
    /// The schema alone describes the target state (PLAN.md, D10): analysis
    /// only replays it, migrations are replayed separately and compared to the
    /// schema (`schema-drift`), and `pgweave migrate create` generates the
    /// next one. Otherwise, the schema completes the state reached by the
    /// migrations.
    #[serde(default)]
    pub declarative: bool,
}

// ── Migrations ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MigrationsConfig {
    /// Migrations directory. Each file in it is indexed with the sequence
    /// taken from its name.
    #[serde(default = "default_migrations_dir")]
    pub directory: String,
}

impl Default for MigrationsConfig {
    fn default() -> Self {
        Self {
            directory: default_migrations_dir(),
        }
    }
}

fn default_migrations_dir() -> String {
    "migrations".into()
}

// ── Queries ───────────────────────────────────────────────────────────────────

/// Named query files (format: `docs/named-queries.md`).
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct QueriesConfig {
    /// Directory of named query files, walked recursively.
    /// Absent: the project has no named queries.
    pub directory: Option<String>,
    /// Additional query directories, close to the code that uses them
    /// (`src/order/queries`). Each one is the root of its own module tree
    /// (`pgweave_macros::queries!`).
    #[serde(default)]
    pub include: Vec<String>,
}

// ── Macros ────────────────────────────────────────────────────────────────────

/// Where to find the project macros.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MacrosConfig {
    /// Directory of `*.pgmacro` files, walked recursively.
    ///
    /// A project macro with the name of a provided macro replaces it: this is
    /// the only way to adjust a provided macro without forking the tool.
    #[serde(default = "default_macros_dir")]
    pub directory: String,
}

impl Default for MacrosConfig {
    fn default() -> Self {
        Self {
            directory: default_macros_dir(),
        }
    }
}

fn default_macros_dir() -> String {
    "macros".into()
}

// ── Parse ─────────────────────────────────────────────────────────────────────

/// What to do with a statement the parser did not put in the AST.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SkipLevel {
    /// Say nothing. Reserve it for gradual migrations to the tool: silence
    /// becomes indistinguishable from success again.
    Ignore,
    /// Report without failing (`check` exits 0).
    #[default]
    Warn,
    /// Report and fail (`check` exits 1).
    Error,
}

/// Parser tolerance policy.
///
/// By default, an unreadable statement produces a **warning**: it stays
/// visible without breaking the build, which allows adopting the tool on an
/// existing codebase. Set `unparsable = "error"` once the project is clean,
/// or `"ignore"` for the duration of the migration.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParseConfig {
    /// Statement sqlparser-rs could not read. May be valid PostgreSQL out of
    /// scope *or* a syntax error — both are indistinguishable at this level,
    /// hence the `warn` default rather than `error`.
    #[serde(default = "default_unparsable")]
    pub unparsable: SkipLevel,
    /// Valid statement deliberately not modeled in the AST.
    ///
    /// Defaults to `warn`, not `ignore`: "we know the SQL is fine" does not
    /// justify silence, since the statement still disappears from the
    /// analysis. Ignore a `CREATE VIEW`, and the view exists for no other
    /// file — without anything saying so.
    #[serde(default = "default_unsupported")]
    pub unsupported: SkipLevel,
}

impl Default for ParseConfig {
    fn default() -> Self {
        Self {
            unparsable: default_unparsable(),
            unsupported: default_unsupported(),
        }
    }
}

fn default_unparsable() -> SkipLevel {
    SkipLevel::Warn
}
fn default_unsupported() -> SkipLevel {
    SkipLevel::Warn
}

// ── Rules ─────────────────────────────────────────────────────────────────────

/// Severity forced on a diagnostic code. Same vocabulary as `[parse]`
/// (`warn`, not `warning`): a single word set for the whole config.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleLevel {
    Error,
    #[serde(rename = "warn")]
    Warning,
    Info,
    /// The diagnostic is removed.
    Off,
}

impl RuleLevel {
    /// Textual form expected by `pgweave_diagnostics::apply_rule_overrides`.
    pub fn as_str(self) -> &'static str {
        match self {
            RuleLevel::Error => "error",
            RuleLevel::Warning => "warning",
            RuleLevel::Info => "info",
            RuleLevel::Off => "off",
        }
    }
}

// ── Plugins ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct PluginsConfig {
    /// Provided plugins to enable, with their options: `name = { … }`.
    /// An unknown name is an error, reported by the plugin loader.
    #[serde(default)]
    pub builtin: BTreeMap<String, serde_json::Value>,
    /// External WASM plugins, paths relative to the config file.
    #[serde(default)]
    pub wasm: Vec<String>,
}

// ── Errors ────────────────────────────────────────────────────────────────────

/// Unreadable or invalid config. The message names the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError(pub String);

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConfigError {}

// ── Loading ───────────────────────────────────────────────────────────────────

impl ProjectConfig {
    pub const FILENAME: &'static str = "pgweave.toml";

    /// Loads and validates a config file.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| ConfigError(format!("cannot read {}: {e}", path.display())))?;
        Self::parse(&content).map_err(|e| ConfigError(format!("{}: {e}", path.display())))
    }

    /// Parses and validates config content (no file, for tests).
    pub fn parse(content: &str) -> Result<Self, ConfigError> {
        let config: Self = toml::from_str(content).map_err(|e| ConfigError(e.to_string()))?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if !SUPPORTED_POSTGRES_VERSIONS.contains(&self.postgres.version) {
            let supported: Vec<String> = SUPPORTED_POSTGRES_VERSIONS
                .iter()
                .map(u32::to_string)
                .collect();
            return Err(ConfigError(format!(
                "[postgres] version = {}: unsupported version (available: {})",
                self.postgres.version,
                supported.join(", ")
            )));
        }
        Ok(())
    }

    /// Walks up from `start` until `pgweave.toml` is found.
    ///
    /// `Ok(None)`: no file found. `Err`: a file exists but is invalid — the
    /// caller must report it, not ignore it.
    pub fn find_and_load(start: &Path) -> Result<Option<(PathBuf, Self)>, ConfigError> {
        let Some(root) = Self::find_root(start) else {
            return Ok(None);
        };
        let config = Self::load(&root.join(Self::FILENAME))?;
        Ok(Some((root, config)))
    }

    /// Directory containing the `pgweave.toml` closest to `start`.
    pub fn find_root(start: &Path) -> Option<PathBuf> {
        let mut dir: &Path = if start.is_file() {
            start.parent()?
        } else {
            start
        };
        loop {
            if dir.join(Self::FILENAME).exists() {
                return Some(dir.to_path_buf());
            }
            dir = dir.parent()?;
        }
    }

    /// Severity overrides in the form expected by
    /// `pgweave_diagnostics::apply_rule_overrides`.
    pub fn rule_overrides(&self) -> std::collections::HashMap<String, String> {
        self.rules
            .iter()
            .map(|(code, level)| (code.clone(), level.as_str().to_string()))
            .collect()
    }

    /// Enabled provided plugins, in the form expected by the registry.
    pub fn builtin_plugins(&self) -> std::collections::HashMap<String, serde_json::Value> {
        self.plugins
            .builtin
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// Builds the project indexing scope.
    ///
    /// Contents:
    /// 1. `migrations.directory` (recursive), each file with its sequence
    /// 2. `schema.directory` (recursive) **if configured**, otherwise the
    ///    project root minus migrations and queries — to match
    ///    [`Self::classify_file`]
    /// 3. `schema.include`: additional directories
    ///
    /// Named queries are never indexed: they declare no schema objects.
    pub fn to_source(&self, base_dir: &Path) -> MultiSource {
        let mut sources: Vec<Box<dyn SchemaSource>> = Vec::new();

        let mig_dir = base_dir.join(&self.migrations.directory);
        if mig_dir.is_dir() {
            sources.push(Box::new(MigrationDirSource::new(mig_dir.clone())));
        }

        match self.schema.directory {
            Some(ref sdir) => {
                let schema_dir = base_dir.join(sdir);
                if schema_dir.is_dir() {
                    sources.push(Box::new(DirSource::new(schema_dir, FileRole::Schema)));
                }
            }
            // No dedicated directory: the whole project is the schema, except
            // migrations — already indexed above with their sequence — and
            // named queries.
            None => {
                // Fragments of the macros directory are not schema.
                let mut dir = DirSource::new(base_dir.to_path_buf(), FileRole::Schema)
                    .excluding(&mig_dir)
                    .excluding(base_dir.join(&self.macros.directory));
                for qdir in self.query_roots(base_dir) {
                    dir = dir.excluding(qdir);
                }
                sources.push(Box::new(dir));
            }
        }

        for extra in &self.schema.include {
            let dir = base_dir.join(extra);
            if dir.is_dir() {
                sources.push(Box::new(DirSource::new(dir, FileRole::Schema)));
            }
        }

        MultiSource::new(sources)
    }

    /// Query directories: `directory` then `include`.
    pub fn query_roots(&self, base_dir: &Path) -> Vec<std::path::PathBuf> {
        self.queries
            .directory
            .iter()
            .chain(&self.queries.include)
            .map(|d| base_dir.join(d))
            .collect()
    }

    /// True if `path` is in a query directory.
    pub fn is_query_file(&self, path: &Path, base_dir: &Path) -> bool {
        self.query_roots(base_dir)
            .iter()
            .any(|d| is_within(path, d))
    }

    /// Query directory containing `path`, and the module path of the file in
    /// that directory's tree (`order/book.sql` → `["order", "book"]`). A
    /// query file outside those directories (draft, sandbox) is attached to
    /// the root of `directory`.
    pub fn query_module(
        &self,
        path: &Path,
        base_dir: &Path,
    ) -> Option<(std::path::PathBuf, Vec<String>)> {
        let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
        let file = canon(path);
        self.query_roots(base_dir)
            .into_iter()
            .find_map(|root| {
                let rel = file.strip_prefix(canon(&root)).ok()?.with_extension("");
                let segments = rel
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect();
                Some((root, segments))
            })
            .or_else(|| {
                let dir = base_dir.join(self.queries.directory.as_ref()?);
                Some((dir, Vec::new()))
            })
    }

    /// Classifies a file by its position in the project.
    ///
    /// Rules (driven by the config, no heuristic on the name):
    /// 1. In `migrations.directory` → Migration (sequence taken from the name)
    /// 2. In `queries.directory` or `queries.include` → Query
    /// 3. In `schema.directory` or `schema.include` → Schema
    /// 4. Otherwise: Schema if `schema.directory` is absent (the whole project
    ///    is schema), Query if it is present
    pub fn classify_file(&self, path: &Path, base_dir: &Path) -> FileRole {
        let mig_dir = base_dir.join(&self.migrations.directory);
        if is_within(path, &mig_dir) {
            let path_canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
            return FileRole::Migration {
                sequence: sequence_from_path(&path_canon),
            };
        }

        if self.is_query_file(path, base_dir) {
            return FileRole::Query;
        }

        if is_within(path, &base_dir.join(&self.macros.directory)) {
            return FileRole::Fragments;
        }

        let Some(ref sdir) = self.schema.directory else {
            return FileRole::Schema;
        };
        let in_schema = std::iter::once(sdir)
            .chain(self.schema.include.iter())
            .any(|d| is_within(path, &base_dir.join(d)));
        if in_schema {
            FileRole::Schema
        } else {
            FileRole::Query
        }
    }
}

/// Is `path` under `dir`? Compares canonical paths when they exist, so as
/// not to depend on `./` or symbolic links.
fn is_within(path: &Path, dir: &Path) -> bool {
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    path.starts_with(&dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_config_only_requires_the_version() {
        let c = ProjectConfig::parse("[postgres]\nversion = 18\n").unwrap();
        assert_eq!(c.postgres.version, 18);
        assert_eq!(c.migrations.directory, "migrations");
        assert_eq!(c.macros.directory, "macros");
        assert!(c.queries.directory.is_none());
        assert_eq!(c.parse.unparsable, SkipLevel::Warn);
    }

    #[test]
    fn postgres_version_is_required() {
        let err = ProjectConfig::parse("[schema]\ndirectory = \"schema\"\n").unwrap_err();
        assert!(err.0.contains("postgres"), "{err}");
    }

    #[test]
    fn unsupported_version_is_rejected() {
        let err = ProjectConfig::parse("[postgres]\nversion = 15\n").unwrap_err();
        assert!(err.0.contains("unsupported version"), "{err}");
    }

    #[test]
    fn unknown_key_is_an_error() {
        for content in [
            "[postgres]\nversion = 18\nsearch_paht = [\"public\"]\n",
            "[postgres]\nversion = 18\n[schema]\ndirectroy = \"schema\"\n",
            "[postgres]\nversion = 18\n[instrumentation]\nenabled = true\n",
        ] {
            assert!(
                ProjectConfig::parse(content).is_err(),
                "should have been rejected:\n{content}"
            );
        }
    }

    #[test]
    fn log_level_and_target() {
        let c = ProjectConfig::parse("[postgres]\nversion = 18\n").unwrap();
        assert_eq!(
            c.log.instrumentation(),
            pgweave_macro::Instrumentation::default()
        );
        let c = ProjectConfig::parse(
            "[postgres]\nversion = 18\n[log]\nlevel = \"step\"\ntarget = \"notify\"\n",
        )
        .unwrap();
        let i = c.log.instrumentation();
        assert_eq!(i.log, pgweave_macro::LogLevel::Step);
        assert_eq!(i.target, pgweave_macro::LogTarget::Notify);
        for bad in ["level = \"trace\"", "target = \"file\"", "levle = \"info\""] {
            let content = format!("[postgres]\nversion = 18\n[log]\n{bad}\n");
            assert!(ProjectConfig::parse(&content).is_err(), "{bad}");
        }
    }

    #[test]
    fn rules_and_plugins_are_read() {
        let c = ProjectConfig::parse(
            "[postgres]\nversion = 18\n\
             [rules]\nL001 = \"off\"\n\"column-not-found\" = \"warn\"\n\
             [plugins]\nbuiltin = { prisma = { schema = \"prisma/schema.prisma\" } }\n\
             wasm = [\"plugins/a.wasm\"]\n",
        )
        .unwrap();
        let overrides = c.rule_overrides();
        assert_eq!(overrides["L001"], "off");
        assert_eq!(overrides["column-not-found"], "warning");
        assert_eq!(
            c.builtin_plugins()["prisma"]["schema"],
            "prisma/schema.prisma"
        );
        assert_eq!(c.plugins.wasm, vec!["plugins/a.wasm"]);
    }

    #[test]
    fn unknown_rule_level_is_rejected() {
        for level in ["warning", "fatal"] {
            let content = format!("[postgres]\nversion = 18\n[rules]\nL001 = \"{level}\"\n");
            assert!(ProjectConfig::parse(&content).is_err(), "{level}");
        }
    }

    #[test]
    fn query_directories_and_modules() {
        let c = ProjectConfig::parse(
            "[postgres]\nversion = 18\n[schema]\ndirectory = \"schema\"\n\
             [queries]\ndirectory = \"queries\"\ninclude = [\"src/order/queries\"]\n",
        )
        .unwrap();
        let base = Path::new("/project");
        let m = |p: &str| {
            c.query_module(Path::new(p), base)
                .map(|(r, s)| (r, s.join("::")))
        };
        assert_eq!(
            m("/project/queries/order/book.sql"),
            Some((base.join("queries"), "order::book".into()))
        );
        assert_eq!(
            m("/project/src/order/queries/book.sql"),
            Some((base.join("src/order/queries"), "book".into()))
        );
        // Outside query directories: attached to the main root.
        assert_eq!(
            m("/project/sandbox/x.sql"),
            Some((base.join("queries"), String::new()))
        );
        assert_eq!(
            c.classify_file(Path::new("/project/src/order/queries/book.sql"), base),
            FileRole::Query
        );
        assert_eq!(
            c.classify_file(Path::new("/project/macros/f.sql"), base),
            FileRole::Fragments
        );
    }
}
