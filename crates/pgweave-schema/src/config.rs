//! Configuration projet : `pgweave.toml`, format v1.
//!
//! Spécification de référence : `docs/config.md`. Toute évolution de ce
//! fichier doit y être reportée.
//!
//! Règles de conception :
//! - **Strict** : une clé inconnue est une erreur (`deny_unknown_fields`). Une
//!   faute de frappe dans une clé ne doit jamais se traduire par un réglage
//!   silencieusement ignoré.
//! - **Rien d'accepté sans effet** : une clé n'existe dans le format que si
//!   l'outil l'applique. Les évolutions prévues sont décrites dans
//!   `docs/config.md` mais refusées tant qu'elles ne sont pas implémentées.
//! - **Erreurs remontées** : un fichier illisible ou invalide est une erreur
//!   pour l'appelant, jamais un repli sur la config par défaut.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::role::{FileRole, sequence_from_path};
use crate::source::{DirSource, MigrationDirSource, MultiSource, SchemaSource};

/// Versions majeures de PostgreSQL pour lesquelles l'analyse est disponible.
///
/// Une version absente de cette liste est refusée plutôt qu'analysée avec les
/// règles d'une autre : ce serait deviner (PLAN.md, D9).
pub const SUPPORTED_POSTGRES_VERSIONS: &[u32] = &[18];

// ── Top-level config ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectConfig {
    /// Contexte PostgreSQL cible. Obligatoire.
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
    /// Surcharges de sévérité par code de diagnostic, checker comme lint.
    #[serde(default)]
    pub rules: BTreeMap<String, RuleLevel>,
    #[serde(default)]
    pub plugins: PluginsConfig,
    #[serde(default)]
    pub rust: RustConfig,
}

// ── Rust ──────────────────────────────────────────────────────────────────────

/// Code Rust généré (`pgweave-build`, `pgweave-macros`).
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct RustConfig {
    /// Module où `pgweave_macros::types!()` génère les enums du projet
    /// (`"crate::db"`). Présent, le code des requêtes y renvoie au lieu de
    /// définir les siens ; requis dès qu'une requête `sql!` utilise un enum.
    pub types: Option<String>,
}

// ── PostgreSQL ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PostgresConfig {
    /// Version majeure ciblée (ex. `18`). Doit figurer dans
    /// [`SUPPORTED_POSTGRES_VERSIONS`].
    pub version: u32,
}

// ── Schema ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct SchemaConfig {
    /// Répertoire des fichiers de schéma déclaratifs, parcouru récursivement.
    ///
    /// Absent : tout fichier hors migrations et requêtes est un fichier de
    /// schéma, et le scope d'indexation est la racine du projet (voir
    /// [`ProjectConfig::to_source`]). Présent : seuls les fichiers de ce
    /// répertoire (et de `include`) sont des fichiers de schéma.
    pub directory: Option<String>,
    /// Répertoires supplémentaires à indexer, en plus de `directory`.
    ///
    /// Utile quand le DDL est éclaté (`db/tables/`, `db/functions/`) sans
    /// répertoire parent commun dédié.
    #[serde(default)]
    pub include: Vec<String>,
    /// Le schéma décrit seul l'état cible (PLAN.md, D10) : l'analyse ne
    /// rejoue que lui, les migrations sont rejouées à part et comparées au
    /// schéma (`schema-drift`), et `pgweave migrate create` génère la
    /// suivante. Sinon, le schéma complète l'état atteint par les migrations.
    #[serde(default)]
    pub declarative: bool,
}

// ── Migrations ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MigrationsConfig {
    /// Répertoire des migrations. Chaque fichier y est indexé avec la
    /// séquence tirée de son nom.
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

/// Fichiers de requêtes nommées (format : `docs/named-queries.md`).
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct QueriesConfig {
    /// Répertoire des fichiers de requêtes nommées, parcouru récursivement.
    /// Absent : le projet n'a pas de requêtes nommées.
    pub directory: Option<String>,
    /// Répertoires de requêtes supplémentaires, près du code qui les
    /// utilise (`src/order/queries`). Chacun est la racine de son propre
    /// arbre de modules (`pgweave_macros::queries!`).
    #[serde(default)]
    pub include: Vec<String>,
}

// ── Macros ────────────────────────────────────────────────────────────────────

/// Où trouver les macros du projet.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct ParseConfig {
    /// Statement que sqlparser-rs n'a pas su lire. Peut être du PostgreSQL
    /// valide hors périmètre *ou* une faute de syntaxe — les deux sont
    /// indiscernables à ce niveau, d'où le défaut `warn` plutôt que `error`.
    #[serde(default = "default_unparsable")]
    pub unparsable: SkipLevel,
    /// Statement valide mais volontairement pas modélisé dans l'AST.
    ///
    /// Défaut `warn` et non `ignore` : « on sait que le SQL est bon » ne
    /// justifie pas le silence, puisque le statement disparaît quand même de
    /// l'analyse. Un `CREATE VIEW` ignoré, et la vue n'existe pour aucun autre
    /// fichier — sans que rien ne le dise.
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

/// Sévérité imposée à un code de diagnostic. Même vocabulaire que `[parse]`
/// (`warn`, pas `warning`) : un seul jeu de mots pour toute la config.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleLevel {
    Error,
    #[serde(rename = "warn")]
    Warning,
    Info,
    /// Le diagnostic est retiré.
    Off,
}

impl RuleLevel {
    /// Forme textuelle attendue par `pgweave_diagnostics::apply_rule_overrides`.
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
    /// Plugins fournis à activer, avec leurs options : `name = { … }`.
    /// Un nom inconnu est une erreur, signalée par le chargeur de plugins.
    #[serde(default)]
    pub builtin: BTreeMap<String, serde_json::Value>,
    /// Plugins WASM externes, chemins relatifs au fichier de config.
    #[serde(default)]
    pub wasm: Vec<String>,
}

// ── Erreurs ───────────────────────────────────────────────────────────────────

/// Config illisible ou invalide. Le message cite le fichier.
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

    /// Charge et valide un fichier de config.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| ConfigError(format!("cannot read {}: {e}", path.display())))?;
        Self::parse(&content).map_err(|e| ConfigError(format!("{}: {e}", path.display())))
    }

    /// Parse et valide un contenu de config (sans fichier, pour les tests).
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
                "[postgres] version = {} : version non prise en charge (disponibles : {})",
                self.postgres.version,
                supported.join(", ")
            )));
        }
        Ok(())
    }

    /// Remonte depuis `start` jusqu'à trouver `pgweave.toml`.
    ///
    /// `Ok(None)` : aucun fichier trouvé. `Err` : un fichier existe mais est
    /// invalide — l'appelant doit le signaler, pas l'ignorer.
    pub fn find_and_load(start: &Path) -> Result<Option<(PathBuf, Self)>, ConfigError> {
        let Some(root) = Self::find_root(start) else {
            return Ok(None);
        };
        let config = Self::load(&root.join(Self::FILENAME))?;
        Ok(Some((root, config)))
    }

    /// Répertoire contenant le `pgweave.toml` le plus proche de `start`.
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

    /// Surcharges de sévérité sous la forme attendue par
    /// `pgweave_diagnostics::apply_rule_overrides`.
    pub fn rule_overrides(&self) -> std::collections::HashMap<String, String> {
        self.rules
            .iter()
            .map(|(code, level)| (code.clone(), level.as_str().to_string()))
            .collect()
    }

    /// Plugins fournis activés, sous la forme attendue par le registre.
    pub fn builtin_plugins(&self) -> std::collections::HashMap<String, serde_json::Value> {
        self.plugins
            .builtin
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// Construit le scope d'indexation du projet.
    ///
    /// Contenu :
    /// 1. `migrations.directory` (récursif), chaque fichier avec sa séquence
    /// 2. `schema.directory` (récursif) **si configuré**, sinon la racine du
    ///    projet hors migrations et requêtes — pour coïncider avec
    ///    [`Self::classify_file`]
    /// 3. `schema.include` : répertoires supplémentaires
    ///
    /// Les requêtes nommées ne sont jamais indexées : elles ne déclarent pas
    /// d'objets de schéma.
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
            // Pas de répertoire dédié : le projet entier est le schéma, sauf
            // les migrations — déjà indexées ci-dessus avec leur séquence — et
            // les requêtes nommées.
            None => {
                // Les fragments du répertoire de macros ne sont pas du schéma.
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

    /// Répertoires de requêtes : `directory` puis `include`.
    pub fn query_roots(&self, base_dir: &Path) -> Vec<std::path::PathBuf> {
        self.queries
            .directory
            .iter()
            .chain(&self.queries.include)
            .map(|d| base_dir.join(d))
            .collect()
    }

    /// Vrai si `path` est dans un répertoire de requêtes.
    pub fn is_query_file(&self, path: &Path, base_dir: &Path) -> bool {
        self.query_roots(base_dir)
            .iter()
            .any(|d| is_within(path, d))
    }

    /// Répertoire de requêtes qui contient `path`, et le chemin de module du
    /// fichier dans l'arbre de ce répertoire (`order/book.sql` →
    /// `["order", "book"]`). Un fichier de requêtes hors de ces répertoires
    /// (brouillon, bac à sable) est rattaché à la racine de `directory`.
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

    /// Classe un fichier selon sa position dans le projet.
    ///
    /// Règles (pilotées par la config, aucune heuristique sur le nom) :
    /// 1. Dans `migrations.directory` → Migration (séquence tirée du nom)
    /// 2. Dans `queries.directory` ou `queries.include` → Query
    /// 3. Dans `schema.directory` ou `schema.include` → Schema
    /// 4. Sinon : Schema si `schema.directory` est absent (tout le projet est
    ///    schéma), Query s'il est présent
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

/// `path` est-il sous `dir` ? Compare les chemins canoniques quand ils
/// existent, pour ne pas dépendre de `./` ou des liens symboliques.
fn is_within(path: &Path, dir: &Path) -> bool {
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    path.starts_with(&dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn la_config_minimale_ne_demande_que_la_version() {
        let c = ProjectConfig::parse("[postgres]\nversion = 18\n").unwrap();
        assert_eq!(c.postgres.version, 18);
        assert_eq!(c.migrations.directory, "migrations");
        assert_eq!(c.macros.directory, "macros");
        assert!(c.queries.directory.is_none());
        assert_eq!(c.parse.unparsable, SkipLevel::Warn);
    }

    #[test]
    fn la_version_postgres_est_obligatoire() {
        let err = ProjectConfig::parse("[schema]\ndirectory = \"schema\"\n").unwrap_err();
        assert!(err.0.contains("postgres"), "{err}");
    }

    #[test]
    fn une_version_non_prise_en_charge_est_refusee() {
        let err = ProjectConfig::parse("[postgres]\nversion = 15\n").unwrap_err();
        assert!(err.0.contains("non prise en charge"), "{err}");
    }

    #[test]
    fn une_cle_inconnue_est_une_erreur() {
        for content in [
            "[postgres]\nversion = 18\nsearch_paht = [\"public\"]\n",
            "[postgres]\nversion = 18\n[schema]\ndirectroy = \"schema\"\n",
            "[postgres]\nversion = 18\n[instrumentation]\nenabled = true\n",
        ] {
            assert!(
                ProjectConfig::parse(content).is_err(),
                "aurait dû être refusé :\n{content}"
            );
        }
    }

    #[test]
    fn les_regles_et_plugins_sont_lus() {
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
    fn un_niveau_de_regle_inconnu_est_refuse() {
        for level in ["warning", "fatal"] {
            let content = format!("[postgres]\nversion = 18\n[rules]\nL001 = \"{level}\"\n");
            assert!(ProjectConfig::parse(&content).is_err(), "{level}");
        }
    }

    #[test]
    fn repertoires_de_requetes_et_modules() {
        let c = ProjectConfig::parse(
            "[postgres]\nversion = 18\n[schema]\ndirectory = \"schema\"\n\
             [queries]\ndirectory = \"queries\"\ninclude = [\"src/order/queries\"]\n",
        )
        .unwrap();
        let base = Path::new("/projet");
        let m = |p: &str| {
            c.query_module(Path::new(p), base)
                .map(|(r, s)| (r, s.join("::")))
        };
        assert_eq!(
            m("/projet/queries/order/book.sql"),
            Some((base.join("queries"), "order::book".into()))
        );
        assert_eq!(
            m("/projet/src/order/queries/book.sql"),
            Some((base.join("src/order/queries"), "book".into()))
        );
        // Hors des répertoires de requêtes : rattaché à la racine principale.
        assert_eq!(
            m("/projet/sandbox/x.sql"),
            Some((base.join("queries"), String::new()))
        );
        assert_eq!(
            c.classify_file(Path::new("/projet/src/order/queries/book.sql"), base),
            FileRole::Query
        );
        assert_eq!(
            c.classify_file(Path::new("/projet/macros/f.sql"), base),
            FileRole::Fragments
        );
    }
}
