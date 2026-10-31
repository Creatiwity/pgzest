use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::role::{FileRole, sequence_from_path};
use crate::source::{DirSource, MigrationDirSource, MultiSource, SchemaSource};

// ── Top-level config ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ProjectConfig {
    #[serde(default)]
    pub macros: MacrosConfig,
    #[serde(default)]
    pub parse: ParseConfig,
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

// ── Macros ────────────────────────────────────────────────────────────────────

/// Où trouver les macros du projet.
#[derive(Debug, Clone, Deserialize)]
pub struct MacrosConfig {
    /// Répertoire des `*.pgmacro`, parcouru récursivement.
    ///
    /// Une macro du projet portant le nom d'une macro fournie la remplace :
    /// c'est le seul moyen d'ajuster une macro fournie sans forker l'outil.
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

/// Que faire d'un statement que le parser n'a pas mis dans l'AST.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SkipLevel {
    /// Ne rien dire. À réserver aux migrations progressives vers l'outil :
    /// le silence redevient indiscernable du succès.
    Ignore,
    /// Signaler sans faire échouer (`check` sort 0).
    #[default]
    Warn,
    /// Signaler et faire échouer (`check` sort 1).
    Error,
}

/// Politique de tolérance du parser.
///
/// Par défaut, un statement illisible produit un **warning** : il reste visible
/// sans casser le build, ce qui permet d'adopter l'outil sur une base
/// existante. Passer `unparsable = "error"` quand le projet est propre, ou
/// `"ignore"` le temps de la migration.
#[derive(Debug, Clone, Deserialize)]
pub struct ParseConfig {
    /// Statement que sqlparser-rs n'a pas su lire. Peut être du PostgreSQL
    /// valide hors périmètre (GRANT, CREATE EXTENSION) *ou* une faute de
    /// syntaxe — les deux sont indiscernables à ce niveau, d'où le défaut
    /// `warn` plutôt que `error`.
    #[serde(default = "default_unparsable")]
    pub unparsable: SkipLevel,
    /// Statement valide mais volontairement pas modélisé dans l'AST.
    ///
    /// Défaut passé de `ignore` à `warn` : « on sait que le SQL est bon » ne
    /// justifiait pas le silence, puisque le statement disparaît quand même de
    /// l'analyse. Un `CREATE VIEW` ignoré, et la vue n'existe pour aucun autre
    /// fichier — sans que rien ne le dise. Un warning ne fait pas échouer
    /// `check`, il rend seulement la lacune visible.
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

fn default_migrations_dir() -> String {
    "migrations/".into()
}
fn default_format() -> String {
    "{sequence:06}_{name}.sql".into()
}
fn default_hash_journal() -> String {
    "pgweave-hashes.json".into()
}

// ── Schema ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Default)]
pub struct SchemaConfig {
    /// Répertoire des fichiers DDL statiques (schema.sql, types.sql, …),
    /// parcouru récursivement.
    ///
    /// Absent : tout fichier non-migration est Schema, et le scope
    /// d'indexation est la racine du projet (voir [`ProjectConfig::to_source`]).
    /// Présent : seuls les fichiers de ce répertoire sont Schema, les autres
    /// sont Query.
    pub directory: Option<String>,
    /// Répertoires supplémentaires à indexer, en plus de `directory`.
    ///
    /// Utile quand le DDL est éclaté (`db/tables/`, `db/functions/`) sans
    /// répertoire parent commun dédié.
    #[serde(default)]
    pub include: Vec<String>,
    #[serde(default = "default_search_path")]
    pub search_path: Vec<String>,
}

fn default_search_path() -> Vec<String> {
    vec!["public".into()]
}

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
        Self {
            enabled: true,
            log_level: default_log_level(),
            log_target: default_log_target(),
        }
    }
}

fn default_true() -> bool {
    true
}
fn default_log_level() -> String {
    "step".into()
}
fn default_log_target() -> String {
    "notify".into()
}

// ── Lint ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Default)]
pub struct LintProjectConfig {
    #[serde(default)]
    pub disabled: Vec<String>,
    /// Per-rule severity overrides. Key = rule ID (e.g. "column-not-found").
    /// Values: "error", "warning", "info", "off".
    #[serde(default)]
    pub rules: std::collections::HashMap<String, String>,
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
    pub const FILENAME: &'static str = "pgweave.toml";

    pub fn load(path: &Path) -> Result<Self, String> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        toml::from_str(&content).map_err(|e| format!("cannot parse {}: {e}", path.display()))
    }

    /// Walk up from `start` looking for `pgweave.toml`.
    pub fn find_and_load(start: &Path) -> Option<(PathBuf, Self)> {
        let mut dir: &Path = if start.is_file() {
            start.parent()?
        } else {
            start
        };
        loop {
            let candidate = dir.join(Self::FILENAME);
            if candidate.exists() {
                let config = Self::load(&candidate).ok()?;
                return Some((dir.to_path_buf(), config));
            }
            dir = dir.parent()?;
        }
    }

    /// Construit le scope d'indexation du projet.
    ///
    /// Contenu :
    /// 1. `migrations.directory` (récursif), chaque fichier avec sa séquence
    /// 2. `schema.directory` (récursif) **si configuré**, sinon la racine du
    ///    projet — pour coïncider avec [`Self::classify_file`], qui traite tout
    ///    fichier non-migration comme Schema quand `schema.directory` est absent
    /// 3. `schema.include` : répertoires supplémentaires
    ///
    /// Le cas 2 est le point important : sans lui, un fichier pouvait être
    /// *classé* Schema tout en restant hors du scope indexé, donc ses tables
    /// n'existaient pour personne. Les répertoires de build et `node_modules`
    /// sont exclus du parcours.
    pub fn to_source(&self, base_dir: &Path) -> MultiSource {
        let mut sources: Vec<Box<dyn SchemaSource>> = Vec::new();

        // Migration files — each gets sequence from filename
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
            // Pas de répertoire dédié : le projet entier est le schéma, sauf
            // les migrations — déjà indexées ci-dessus avec leur séquence.
            None => {
                sources.push(Box::new(
                    DirSource::new(base_dir.to_path_buf(), FileRole::Schema).excluding(&mig_dir),
                ));
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
            return FileRole::Migration {
                sequence: sequence_from_path(&path_canon),
            };
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
