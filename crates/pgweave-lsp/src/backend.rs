use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use pgweave_catalog::ProjectCatalog;
use pgweave_diagnostics::{Diagnostic, Severity};
use pgweave_lint::config::LintConfig;
use pgweave_plugin_wasm::{WasmCheckRule, WasmPlugin};
use pgweave_project::Workspace;
use pgweave_schema::provider::SchemaProvider;
use pgweave_schema::{FileRole, ProjectConfig, check_ddl_role};
use tokio::sync::Mutex;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, jsonrpc::Result};

use pgweave_ast::emit::emit_data_type;

use crate::completion::{analyze_cursor, build_completions};
use crate::line_index::LineIndex;
use crate::semantic;

// ── Requête custom : expansion des macros ─────────────────────────────────────

/// Paramètres de `pgweave/expandMacro`.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExpandMacroParams {
    /// Document à désucrer.
    pub text_document: TextDocumentIdentifier,
    /// Position du curseur. Si elle tombe dans une macro, seule celle-ci est
    /// rendue ; sinon tout le fichier l'est.
    pub position: Option<Position>,
}

/// Réponse de `pgweave/expandMacro`.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExpandMacroResult {
    /// SQL standard produit.
    pub sql: String,
    /// Nombre de macros désucrées dans ce qui a été rendu.
    pub expanded: usize,
    /// Message à afficher quand il n'y a rien à montrer.
    pub note: Option<String>,
}

// ── Backend state ─────────────────────────────────────────────────────────────

pub struct PgsqlBackend {
    client: Client,
    documents: Mutex<HashMap<String, String>>,
    /// Projet en base incrémentale : fichiers lus au chargement, remplacés par
    /// le texte de l'éditeur à mesure qu'il change. Le catalogue n'est
    /// recalculé que si un fichier de schéma ou une migration change.
    workspace: Mutex<Workspace>,
    /// Project root and config (from pgweave.toml).
    project_root: Mutex<Option<PathBuf>>,
    config: Mutex<Option<ProjectConfig>>,
    /// Fallback schema path from `schemaFile` init option (legacy, no config file).
    legacy_schema_path: Mutex<Option<String>>,
    /// WASM plugins loaded from plugins.wasm in pgweave.toml.
    wasm_plugins: Mutex<Vec<Arc<WasmPlugin>>>,
    /// Macros reconnues : celles fournies, plus les `*.pgmacro` du projet.
    ///
    /// Reconstruit avec le resolver : un `.pgmacro` modifié doit prendre effet
    /// sans redémarrer le serveur.
    macros: Mutex<Arc<pgweave_macro::MacroSet>>,
}

impl PgsqlBackend {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            documents: Mutex::new(HashMap::new()),
            workspace: Mutex::new(Workspace::new(
                &[],
                Arc::new(pgweave_macro::MacroSet::builtin()),
            )),
            project_root: Mutex::new(None),
            config: Mutex::new(None),
            legacy_schema_path: Mutex::new(None),
            wasm_plugins: Mutex::new(Vec::new()),
            macros: Mutex::new(Arc::new(pgweave_macro::MacroSet::builtin())),
        }
    }

    /// Désucre un document, ou seulement la macro sous le curseur.
    ///
    /// Sert la commande « Voir l'expansion » de l'extension : le résultat est
    /// affiché dans un onglet en lecture seule, comme le fait Rust Analyzer
    /// pour ses macros.
    pub async fn expand_macro(&self, params: ExpandMacroParams) -> Result<ExpandMacroResult> {
        let uri = params.text_document.uri.to_string();
        let Some(text) = self.documents.lock().await.get(&uri).cloned() else {
            return Ok(ExpandMacroResult {
                sql: String::new(),
                expanded: 0,
                note: Some("document non ouvert côté serveur".into()),
            });
        };

        let macros = self.macros().await;
        let full = pgweave_macro::expand_source(&text, &macros);

        if full.is_passthrough() {
            return Ok(ExpandMacroResult {
                sql: String::new(),
                expanded: 0,
                note: Some("aucune macro dans ce fichier".into()),
            });
        }

        // Sans position, on rend tout le fichier désucré.
        let Some(pos) = params.position else {
            return Ok(ExpandMacroResult {
                sql: full.sql,
                expanded: full.expanded,
                note: None,
            });
        };

        // Avec une position : isoler la macro qui la contient. On retrouve la
        // plage engendrée dont l'antécédent couvre le curseur, en s'appuyant sur
        // la même correspondance que les diagnostics.
        let index = LineIndex::new(&text);
        let offset = index.offset_at(pos.line as usize, pos.character as usize);

        match full.expansion_at(offset) {
            Some(sql) => Ok(ExpandMacroResult {
                sql: sql.to_string(),
                expanded: 1,
                note: None,
            }),
            // Curseur hors d'une macro : le fichier entier, plutôt que rien.
            None => Ok(ExpandMacroResult {
                sql: full.sql,
                expanded: full.expanded,
                note: Some("curseur hors d'une macro — fichier entier désucré".into()),
            }),
        }
    }

    /// Un projet pgweave (`pgweave.toml`) est chargé.
    async fn in_project(&self) -> bool {
        self.config.lock().await.is_some()
    }

    /// Jeu de macros courant (clone d'Arc, bon marché).
    async fn macros(&self) -> Arc<pgweave_macro::MacroSet> {
        self.macros.lock().await.clone()
    }

    /// Recharge les macros du projet par-dessus les macros fournies.
    async fn rebuild_macros(&self) {
        let mut set = pgweave_macro::MacroSet::builtin();
        if let Some(root) = self.project_root.lock().await.clone() {
            // Config invalide : déjà signalée par `rebuild_resolver` /
            // `initialize`. On ne devine pas un répertoire de repli.
            let dir = match ProjectConfig::load(&root.join(ProjectConfig::FILENAME)) {
                Ok(cfg) => Some(root.join(&cfg.macros.directory)),
                Err(_) => None,
            };
            if let Some(dir) = dir.filter(|d| d.is_dir()) {
                // Un fichier ouvert fait foi sur le disque, comme pour le schéma.
                let open: HashMap<std::path::PathBuf, String> = self
                    .documents
                    .lock()
                    .await
                    .iter()
                    .filter_map(|(uri, text)| Some((uri_to_path(uri)?, text.clone())))
                    .collect();
                let errors = set.load_dir_with(&dir, |p| {
                    let canonical = p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
                    open.get(&canonical).or_else(|| open.get(p)).cloned()
                });
                for e in errors {
                    self.client
                        .log_message(MessageType::WARNING, format!("pgweave-lsp: {e}"))
                        .await;
                }
            }
        }
        let set = Arc::new(set);
        *self.macros.lock().await = set.clone();
        self.workspace.lock().await.set_macros(set);
    }

    /// Module de `file` dans son répertoire de requêtes, et les requêtes de
    /// ce répertoire par module, documents ouverts compris.
    async fn query_tree(
        &self,
        file: &Path,
        catalog: &ProjectCatalog,
        macros: &pgweave_macro::MacroSet,
    ) -> (Vec<String>, Vec<pgweave_query::QueryModule>) {
        let (Some(root), Some(config)) = (
            self.project_root.lock().await.clone(),
            self.config.lock().await.clone(),
        ) else {
            return (Vec::new(), Vec::new());
        };
        let Some((qroot, here)) = config.query_module(file, &root) else {
            return (Vec::new(), Vec::new());
        };
        let open: HashMap<std::path::PathBuf, String> = self
            .documents
            .lock()
            .await
            .iter()
            .filter_map(|(uri, text)| Some((uri_to_path(uri)?, text.clone())))
            .collect();
        let modules = pgweave_analysis::query_modules(&qroot, catalog, macros, &|p| {
            let canonical = p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
            open.get(&canonical)
                .or_else(|| open.get(p))
                .cloned()
                .or_else(|| std::fs::read_to_string(p).ok())
        });
        (here, modules)
    }

    /// Catalogue du projet à jour (rejeu mémorisé, clone d'Arc).
    async fn resolver(&self) -> Arc<ProjectCatalog> {
        self.workspace.lock().await.catalog()
    }

    /// Charge le projet formé de `chunks`, puis y reporte le texte des
    /// documents ouverts : l'éditeur fait foi sur le disque.
    async fn load_workspace(&self, chunks: Vec<pgweave_schema::SqlChunk>) {
        let chunks: Vec<_> = chunks
            .into_iter()
            .map(|mut c| {
                c.origin.path = canonical(&c.origin.path);
                c
            })
            .collect();
        let declarative = self
            .config
            .lock()
            .await
            .as_ref()
            .is_some_and(|c| c.schema.declarative);
        let mut ws = Workspace::with_mode(&chunks, self.macros().await, declarative);
        for (uri, text) in self.documents.lock().await.iter() {
            if let Some(path) = uri_to_path(uri) {
                ws.set_text(&path, text);
            }
        }
        *self.workspace.lock().await = ws;
    }

    /// Determine the `FileRole` of a URI.
    async fn role_of(&self, uri: &str) -> FileRole {
        let Some(path) = uri_to_path(uri) else {
            return FileRole::Query;
        };

        // Config-driven classification (authoritative when pgweave.toml present).
        if let (Some(root), Some(config)) = (
            self.project_root.lock().await.clone(),
            self.config.lock().await.clone(),
        ) {
            return config.classify_file(&path, &root);
        }

        // Legacy: schemaFile option → everything inside that path is Schema.
        if let Some(schema_path) = self.legacy_schema_path.lock().await.as_deref()
            && is_inside(&path, schema_path)
        {
            return FileRole::Schema;
        }

        // No config at all → treat everything as Schema (safe default).
        FileRole::Schema
    }

    /// Recharge le projet depuis le disque (config, ou chemin historique) :
    /// nouveau fichier, config modifiée. Une simple modification de texte
    /// passe par [`Workspace::set_text`], sans relecture.
    async fn reload_workspace(&self) {
        let root = self.project_root.lock().await.clone();

        let chunks = if let Some(ref root) = root {
            let config_path = root.join(ProjectConfig::FILENAME);
            match ProjectConfig::load(&config_path) {
                Ok(config) => {
                    // Macros du projet chargées d'abord : le catalogue doit
                    // voir les objets déclarés par les macros maison, pas
                    // seulement par celles fournies.
                    self.rebuild_macros().await;
                    let source = config.to_source(root);
                    let chunks = pgweave_schema::SchemaSource::chunks(&source);
                    // La config rechargée remplace l'ancienne : sans cela une
                    // modification de [rules] ou [parse] n'était prise en compte
                    // qu'au redémarrage du serveur.
                    *self.config.lock().await = Some(config);
                    chunks
                }
                Err(e) => {
                    self.client
                        .show_message(MessageType::ERROR, format!("pgweave : {e}"))
                        .await;
                    return;
                }
            }
        } else if let Some(ref path) = *self.legacy_schema_path.lock().await {
            match legacy_chunks(path) {
                Ok(chunks) => chunks,
                Err(_) => return,
            }
        } else {
            return;
        };

        self.load_workspace(chunks).await;
    }

    /// Règles des plugins WASM chargés.
    async fn make_checker(&self) -> pgweave_checker::Checker {
        let mut checker = pgweave_checker::Checker::default();
        for plugin in self.wasm_plugins.lock().await.iter() {
            checker.add_rule(Box::new(WasmCheckRule::new(plugin.clone())));
        }
        checker
    }

    /// Type-check des statements déjà parsés contre `schema`.
    async fn check_stmts(
        &self,
        stmts: &[pgweave_ast::Statement],
        schema: &dyn SchemaProvider,
    ) -> Vec<Diagnostic> {
        self.make_checker().await.run(stmts, schema)
    }

    /// Parse, type-check, lint et publie les diagnostics de `uri`.
    ///
    /// Doit produire exactement ce que produit `pgweave check` sur le même
    /// fichier : même politique de tolérance du parser, mêmes directives en
    /// commentaire, mêmes surcharges de sévérité. Toute divergence ici se
    /// traduit par un fichier vert dans l'éditeur et rouge en CI.
    async fn run_diagnostics(&self, uri: &str, text: &str) {
        let line_index = LineIndex::new(text);
        if is_rust(uri) {
            // Requêtes `sql!` / `query!` d'un projet pgweave seulement : une
            // macro homonyme d'un autre outil n'est pas analysée hors projet.
            let diags = if self.in_project().await {
                let resolver = self.resolver().await;
                let macros = self.macros().await;
                crate::rust_embed::find(text)
                    .iter()
                    .flat_map(|e| embedded_diagnostics(e, resolver.as_ref(), &macros))
                    .collect()
            } else {
                Vec::new()
            };
            self.publish(uri, &diags, &line_index).await;
            return;
        }
        let role = self.role_of(uri).await;
        // Fichier de fragments : son format seul ; chaque fragment est vérifié
        // là où il est inséré.
        if role == FileRole::Fragments {
            let diags = pgweave_macro::fragment_file_diagnostics(text);
            self.publish(uri, &diags, &line_index).await;
            return;
        }
        let resolver = self.resolver().await;
        let suppressions = pgweave_diagnostics::Suppressions::scan(text);

        // Désucrage des macros. Une source sans macro est recopiée à
        // l'identique, donc c'est gratuit et aucun fichier n'y échappe à cause
        // de son extension. Les spans produits ensuite sont déjà dans le repère
        // du fichier édité — voir `PreparedSource::parse`.
        let macros = self.macros().await;
        let prepared = pgweave_analysis::prepare_source(text, &macros);

        // Un seul parse pour le checker, le lint et les règles de rôle.
        let outcome = match prepared.parse_with_outcome() {
            Ok(o) => o,
            Err(e) => {
                let mut diags = vec![Diagnostic::error(
                    pgweave_diagnostics::codes::PARSE_ERROR,
                    format!("parse error: {e}"),
                )];
                suppressions.apply(&mut diags);
                self.publish(uri, &diags, &line_index).await;
                return;
            }
        };
        let stmts = outcome.statements;

        // Config projet : tolérance du parser + surcharges de sévérité.
        let (parse_cfg, rule_overrides) = {
            let guard = self.config.lock().await;
            match guard.as_ref() {
                Some(c) => (c.parse.clone(), c.rule_overrides()),
                None => (Default::default(), Default::default()),
            }
        };

        // ── Macros et statements écartés ──────────────────────────────────────
        let mut diags = prepared.diagnostics.clone();

        // Fichier de requêtes nommées : format vérifié en plus, comme en CLI.
        if let (Some(path), Some(root)) = (uri_to_path(uri), self.project_root.lock().await.clone())
        {
            let is_query_file = self
                .config
                .lock()
                .await
                .as_ref()
                .is_some_and(|c| c.is_query_file(&path, &root));
            if is_query_file {
                diags.extend(pgweave_analysis::query_file_diagnostics(
                    &path, text, &macros,
                ));
            }
        }
        diags.extend(pgweave_analysis::skipped_to_diagnostics(
            &outcome.skipped,
            &parse_cfg,
        ));

        // ── Vérification ──────────────────────────────────────────────────────
        // Comme `pgweave check` : un fichier de schéma ou de migration du
        // projet est vérifié pendant le rejeu (diagnostics du rejeu, texte de
        // l'éditeur compris) ; un fichier de requêtes, contre le catalogue
        // final ; un fichier hors projet, en rejouant son propre DDL.
        let path = uri_to_path(uri);
        let in_project = match &path {
            Some(p) => self.workspace.lock().await.contains(p),
            None => false,
        };
        match (&role, &path) {
            (FileRole::Query, _) => {
                diags.extend(pgweave_checker::check_statements(&stmts, &resolver));
                let (here, others) = match &path {
                    Some(p) => self.query_tree(p, &resolver, &macros).await,
                    None => (Vec::new(), Vec::new()),
                };
                diags.extend(pgweave_analysis::query_descriptor_diagnostics(
                    text, &resolver, &macros, &here, &others,
                ));
            }
            (_, Some(p)) if in_project => {
                diags.extend(self.workspace.lock().await.replay_diagnostics(p));
            }
            _ => diags.extend(pgweave_checker::check_script(&stmts)),
        }
        diags.extend(self.check_stmts(&stmts, resolver.as_ref()).await);

        // ── Lint + règles de rôle ─────────────────────────────────────────────
        diags.extend(pgweave_lint::lint_statements(&stmts, &LintConfig::default()).diagnostics);
        diags.extend(check_ddl_role(&stmts, &role));

        // Les surcharges s'appliquent à tout, checker comme lint : une règle
        // mise à "off" dans la config doit être muette d'où qu'elle vienne.
        pgweave_diagnostics::apply_rule_overrides(&mut diags, &rule_overrides);

        // Directives en commentaire, en dernier : elles ont le dernier mot.
        suppressions.apply(&mut diags);

        self.publish(uri, &diags, &line_index).await;
    }

    /// Le schéma a changé : chaque autre document ouvert est revérifié (le
    /// rejeu est mémorisé, seules les vérifications sont refaites).
    async fn refresh_other_documents(&self, changed: &str) {
        let others: Vec<(String, String)> = self
            .documents
            .lock()
            .await
            .iter()
            .filter(|(u, _)| u.as_str() != changed)
            .map(|(u, t)| (u.clone(), t.clone()))
            .collect();
        for (uri, text) in others {
            self.run_diagnostics(&uri, &text).await;
        }
    }

    async fn publish(&self, uri: &str, diags: &[Diagnostic], line_index: &LineIndex) {
        let lsp_diags: Vec<tower_lsp::lsp_types::Diagnostic> = diags
            .iter()
            .map(|d| Self::to_lsp_diagnostic(d, line_index))
            .collect();
        if let Ok(url) = uri.parse::<Url>() {
            self.client.publish_diagnostics(url, lsp_diags, None).await;
        }
    }

    fn to_lsp_diagnostic(d: &Diagnostic, idx: &LineIndex) -> tower_lsp::lsp_types::Diagnostic {
        let severity = match d.severity {
            Severity::Error => DiagnosticSeverity::ERROR,
            Severity::Warning => DiagnosticSeverity::WARNING,
            Severity::Info => DiagnosticSeverity::INFORMATION,
        };

        let (start_line, start_char) = match &d.span {
            Some(span) if span.start != 0 || span.end != 0 => {
                idx.offset_to_position(span.start as u32)
            }
            _ => (0, 0),
        };
        let (end_line, end_char) = match &d.span {
            Some(span) if span.start != 0 || span.end != 0 => {
                idx.offset_to_position(span.end as u32)
            }
            _ => (0, 0),
        };

        tower_lsp::lsp_types::Diagnostic {
            range: Range {
                start: Position {
                    line: start_line,
                    character: start_char,
                },
                end: Position {
                    line: end_line,
                    character: end_char,
                },
            },
            severity: Some(severity),
            code: Some(NumberOrString::String(d.code.to_string())),
            source: Some("pgweave-lsp".to_string()),
            // L'indice suit le message, comme le HINT de PostgreSQL.
            message: match &d.hint {
                Some(h) => format!("{}\n{h}", d.message),
                None => d.message.clone(),
            },
            related_information: None,
            tags: None,
            code_description: None,
            data: None,
        }
    }
}

// ── LanguageServer implementation ─────────────────────────────────────────────

#[tower_lsp::async_trait]
impl LanguageServer for PgsqlBackend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        // 1. Try to discover pgweave.toml from the workspace root.
        let workspace_root = params
            .workspace_folders
            .as_deref()
            .and_then(|f| f.first())
            .and_then(|f| url_to_path(f.uri.as_ref()))
            .or_else(|| {
                params
                    .root_uri
                    .as_ref()
                    .and_then(|u| url_to_path(u.as_ref()))
            });

        let discovered = match workspace_root.as_deref().map(ProjectConfig::find_and_load) {
            Some(Ok(found)) => found,
            Some(Err(e)) => {
                // Config présente mais invalide : le dire à l'utilisateur, et
                // ne pas analyser avec une config inventée.
                self.client
                    .show_message(MessageType::ERROR, format!("pgweave : {e}"))
                    .await;
                None
            }
            None => None,
        };
        if let Some((project_root, config)) = discovered {
            *self.project_root.lock().await = Some(project_root.clone());
            // La config d'abord : le chargement du projet en dépend (mode
            // déclaratif).
            *self.config.lock().await = Some(config.clone());
            // Macros du projet d'abord : le catalogue doit voir les objets
            // déclarés par les macros maison, pas seulement par celles fournies.
            self.rebuild_macros().await;
            let source = config.to_source(&project_root);
            self.load_workspace(pgweave_schema::SchemaSource::chunks(&source))
                .await;

            // Load WASM plugins listed in [plugins] wasm = [...]
            let mut loaded: Vec<Arc<WasmPlugin>> = Vec::new();
            for rel_path in &config.plugins.wasm {
                let abs = project_root.join(rel_path);
                match WasmPlugin::from_file(&abs) {
                    Ok(p) => {
                        self.client
                            .log_message(
                                MessageType::INFO,
                                format!(
                                    "pgweave-lsp: loaded wasm plugin '{}' from {}",
                                    p.name(),
                                    abs.display()
                                ),
                            )
                            .await;
                        loaded.push(Arc::new(p));
                    }
                    Err(e) => {
                        self.client
                            .log_message(
                                MessageType::WARNING,
                                format!(
                                    "pgweave-lsp: failed to load wasm plugin '{}': {e}",
                                    abs.display()
                                ),
                            )
                            .await;
                    }
                }
            }
            *self.wasm_plugins.lock().await = loaded;

            self.client
                .log_message(
                    MessageType::INFO,
                    format!(
                        "pgweave-lsp: loaded schema from {}",
                        project_root.join(ProjectConfig::FILENAME).display()
                    ),
                )
                .await;
        }

        // 2. Legacy fallback: schemaFile initialization option.
        if self.project_root.lock().await.is_none()
            && let Some(opts) = &params.initialization_options
            && let Some(path) = opts.get("schemaFile").and_then(|v| v.as_str())
            && !path.is_empty()
        {
            *self.legacy_schema_path.lock().await = Some(path.to_string());
            match legacy_chunks(path) {
                Ok(chunks) => self.load_workspace(chunks).await,
                Err(e) => {
                    self.client
                        .log_message(
                            MessageType::WARNING,
                            format!("pgweave-lsp: could not load schema at {path}: {e}"),
                        )
                        .await;
                }
            }
        }

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![".".to_string(), " ".to_string()]),
                    ..Default::default()
                }),
                semantic_tokens_provider: Some(semantic::server_capability()),
                ..Default::default()
            },
            server_info: Some(ServerInfo {
                name: "pgweave-lsp".to_string(),
                version: Some("0.1.0".to_string()),
            }),
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "pgweave-lsp started")
            .await;
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri.to_string();
        let text = params.text_document.text;
        self.documents
            .lock()
            .await
            .insert(uri.clone(), text.clone());
        self.run_diagnostics(&uri, &text).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        if let Some(change) = params.content_changes.into_iter().last() {
            let uri = params.text_document.uri.to_string();
            let text = change.text;
            self.documents
                .lock()
                .await
                .insert(uri.clone(), text.clone());

            if is_rust(&uri) {
                self.run_diagnostics(&uri, &text).await;
                return;
            }
            let path = uri_to_path(&uri);
            let mut schema_changed = false;
            let macro_file = path
                .as_deref()
                .and_then(Path::extension)
                .is_some_and(|e| e == "pgmacro");
            if macro_file || self.role_of(&uri).await == FileRole::Fragments {
                // Macro du projet : tous les fichiers sont relus.
                self.rebuild_macros().await;
                schema_changed = true;
            } else if matches!(
                self.role_of(&uri).await,
                FileRole::Schema | FileRole::Migration { .. }
            ) {
                // Fichier connu : seul ce qui en dépend est recalculé.
                // Nouveau fichier : le projet est rechargé.
                let known = match &path {
                    Some(p) => self.workspace.lock().await.set_text(p, &text),
                    None => false,
                };
                if !known {
                    self.reload_workspace().await;
                }
                schema_changed = true;
            } else if self.role_of(&uri).await == FileRole::Query {
                // Une requête peut servir de référence à d'autres (`-> nom`).
                schema_changed = true;
            }

            self.run_diagnostics(&uri, &text).await;
            if schema_changed {
                self.refresh_other_documents(&uri).await;
            }
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri.to_string();
        self.documents.lock().await.remove(&uri);
        // Modifications abandonnées : le disque refait foi.
        if self.role_of(&uri).await == FileRole::Fragments {
            self.rebuild_macros().await;
            self.refresh_other_documents(&uri).await;
        } else if let Some(path) = uri_to_path(&uri)
            && let Ok(text) = std::fs::read_to_string(&path)
            && self.workspace.lock().await.set_text(&path, &text)
        {
            self.refresh_other_documents(&uri).await;
        }
        self.client
            .publish_diagnostics(params.text_document.uri, vec![], None)
            .await;
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = params
            .text_document_position_params
            .text_document
            .uri
            .to_string();
        let pos = params.text_document_position_params.position;

        let docs = self.documents.lock().await;
        let text = match docs.get(&uri) {
            Some(t) => t.clone(),
            None => return Ok(None),
        };
        drop(docs);

        let offset = position_to_offset(&text, pos.line, pos.character);
        if is_rust(&uri) {
            if !self.in_project().await {
                return Ok(None);
            }
            let resolver = self.resolver().await;
            let macros = self.macros().await;
            return Ok(
                hover_rust(&text, offset as usize, resolver.as_ref(), &macros).map(|value| Hover {
                    contents: HoverContents::Markup(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value,
                    }),
                    range: None,
                }),
            );
        }
        let word = match word_at_offset(&text, offset as usize) {
            Some(w) => w,
            None => return Ok(None),
        };

        let role = self.role_of(&uri).await;
        let resolver = self.resolver().await;
        let macros = self.macros().await;

        // Un seul parse, macros et fragments désucrés : les spans sont dans le
        // repère du fichier édité.
        let prepared = pgweave_analysis::prepare_source(&text, &macros);
        let stmts = prepared
            .parse_with_outcome()
            .map(|o| o.statements)
            .unwrap_or_default();

        let hover_text: Option<String> =
            // Invocation d'un fragment : sa documentation et le SQL inséré.
            hover_fragment(&prepared, &macros, offset as usize)
            // En-tête d'une requête nommée : son descripteur.
            .or_else(|| hover_named_query(&text, offset as usize, resolver.as_ref(), &macros))
            // PL/pgSQL declared variables / parameters take priority in any file type.
            .or_else(|| hover_plpgsql_var(&stmts, offset as usize, &word))
            .or_else(|| {
                if matches!(role, FileRole::Schema) {
                    hover_schema_file(&text, &word, resolver.as_ref())
                } else {
                    hover_query(&stmts, resolver.as_ref(), offset as usize, &word)
                }
            });

        Ok(hover_text.map(|value| Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value,
            }),
            range: None,
        }))
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let uri = params.text_document_position.text_document.uri.to_string();
        let pos = params.text_document_position.position;

        let docs = self.documents.lock().await;
        let text = match docs.get(&uri) {
            Some(t) => t.clone(),
            None => return Ok(Some(CompletionResponse::Array(vec![]))),
        };
        drop(docs);

        let resolver = self.resolver().await;
        let cursor_offset = position_to_offset(&text, pos.line, pos.character) as usize;
        // Fichier Rust : seulement dans le SQL d'un `sql!` / `query!`.
        if is_rust(&uri) {
            if !self.in_project().await {
                return Ok(None);
            }
            let found = crate::rust_embed::find(&text)
                .into_iter()
                .find_map(|e| e.to_sql(cursor_offset).map(|at| (e, at)));
            return Ok(found.map(|(e, at)| {
                CompletionResponse::Array(build_completions(
                    &analyze_cursor(&e.sql, at),
                    resolver.as_ref(),
                ))
            }));
        }
        let ctx = analyze_cursor(&text, cursor_offset);
        let items = build_completions(&ctx, resolver.as_ref());

        Ok(Some(CompletionResponse::Array(items)))
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        let uri = params.text_document.uri.to_string();
        let docs = self.documents.lock().await;
        let text = match docs.get(&uri) {
            Some(t) => t.clone(),
            None => return Ok(None),
        };
        drop(docs);

        let data = semantic::compute_semantic_tokens(&text);
        Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
            result_id: None,
            data,
        })))
    }
}

// ── Hover helpers ─────────────────────────────────────────────────────────────

fn hover_schema_file(text: &str, word: &str, global: &dyn SchemaProvider) -> Option<String> {
    // Build self-schema first; fall back to global for cross-file references.
    let self_schema = ProjectCatalog::from_sql(text).ok()?;
    let provider: &dyn SchemaProvider = &self_schema;

    lookup_hover(word, provider).or_else(|| lookup_hover(word, global))
}

/// Survol dans une requête : ce que désigne le nom sous le curseur, d'après
/// l'analyse sémantique du statement qui le contient ; à défaut, la table ou
/// la colonne de ce nom dans le catalogue.
fn hover_query(
    stmts: &[pgweave_ast::Statement],
    catalog: &ProjectCatalog,
    offset: usize,
    word: &str,
) -> Option<String> {
    use pgweave_ast::Statement;
    let stmt = stmts.iter().find(|s| {
        pgweave_ast::stmt_span(s)
            .is_some_and(|sp| sp.start as usize <= offset && offset <= sp.end as usize)
    });
    if let Some(
        stmt @ (Statement::Select(_)
        | Statement::Insert(_)
        | Statement::Update(_)
        | Statement::Delete(_)),
    ) = stmt
    {
        let mut a = pgweave_sema::Analyzer::new(
            pgweave_sema::TypeSystem::new(&catalog.catalog),
            catalog.search_path.clone(),
        );
        // Les références résolues avant une éventuelle erreur restent valables.
        let _ = a.describe(stmt);
        let under_cursor = a
            .column_refs
            .iter()
            .filter(|(sp, _)| sp.start as usize <= offset && offset < sp.end as usize)
            .min_by_key(|(sp, _)| sp.end - sp.start);
        if let Some((_, t)) = under_cursor {
            let ty = if t.ty.schema == "pg_catalog" {
                t.ty.name.clone()
            } else {
                format!("{}.{}", t.ty.schema, t.ty.name)
            };
            let nn = match t.nullable {
                pgweave_sema::Nullability::NonNull => " NOT NULL",
                pgweave_sema::Nullability::Nullable => " NULL",
                pgweave_sema::Nullability::Unknown => " (nullabilité inconnue)",
            };
            return Some(format!("(column) **{word}**: `{ty}`{nn}"));
        }
    }
    lookup_hover(word, catalog)
}

/// Survol de l'en-tête `-- name: …` d'une requête nommée : ce que décrit
/// son descripteur (paramètres, colonnes, cardinalité), ou pourquoi il n'y
/// en a pas.
/// Document Rust (requêtes `sql!` / `query!`).
fn is_rust(uri: &str) -> bool {
    uri.ends_with(".rs")
}

/// En-tête de requête nommée équivalent à une requête écrite en Rust.
fn embedded_header(e: &crate::rust_embed::Embedded) -> String {
    let name = e.name.as_ref().map_or("query", |(n, _)| n.as_str());
    match &e.cardinality {
        Some(c) => format!("-- name: {name} :{c}\n"),
        None => format!("-- name: {name}\n"),
    }
}

/// Diagnostics d'une requête écrite en Rust, aux positions du fichier : format,
/// fragments, analyse et ce qui empêche de la décrire, comme une requête
/// nommée.
fn embedded_diagnostics(
    e: &crate::rust_embed::Embedded,
    catalog: &ProjectCatalog,
    macros: &pgweave_macro::MacroSet,
) -> Vec<Diagnostic> {
    let header = embedded_header(e);
    let text = format!("{header}{}", e.sql);
    let whole = e
        .name
        .as_ref()
        .map_or(e.content.clone(), |(_, r)| r.clone());
    let place = |mut d: Diagnostic| {
        d.span = Some(match &d.span {
            Some(s) if s.start >= header.len() => {
                let start = e.to_file(s.start - header.len());
                let end = e.to_file(s.end.max(s.start) - header.len()).max(start + 1);
                pgweave_diagnostics::Span { start, end }
            }
            // Dans l'en-tête ajouté (nom, cardinalité) : sur le nom écrit.
            _ => pgweave_diagnostics::Span {
                start: whole.start,
                end: whole.end,
            },
        });
        d
    };
    let file = pgweave_query::parse_query_file_with(&text, macros);
    let mut out: Vec<Diagnostic> = file.diagnostics.into_iter().map(place).collect();
    for q in &file.queries {
        if let Err(ds) = pgweave_query::describe_named(catalog, q) {
            out.extend(ds.into_iter().map(place));
        }
    }
    out
}

/// Survol dans un fichier Rust : le descripteur sur le nom d'un `sql!`, le
/// type ou le fragment sous le curseur dans le SQL.
fn hover_rust(
    text: &str,
    offset: usize,
    catalog: &ProjectCatalog,
    macros: &pgweave_macro::MacroSet,
) -> Option<String> {
    let found = crate::rust_embed::find(text);
    if let Some(e) = found
        .iter()
        .find(|e| e.name.as_ref().is_some_and(|(_, r)| r.contains(&offset)))
    {
        let header = embedded_header(e);
        return hover_named_query(&format!("{header}{}", e.sql), 9, catalog, macros);
    }
    let (e, at) = found
        .iter()
        .find_map(|e| e.to_sql(offset).map(|at| (e, at)))?;
    let prepared = pgweave_analysis::prepare_source(&e.sql, macros);
    let stmts = prepared
        .parse_with_outcome()
        .map(|o| o.statements)
        .unwrap_or_default();
    hover_fragment(&prepared, macros, at).or_else(|| {
        let word = word_at_offset(&e.sql, at)?;
        hover_query(&stmts, catalog, at, &word)
    })
}

/// Survol d'un `@fragment` : d'où il vient, sa documentation, ce qu'il
/// insère ici.
fn hover_fragment(
    prepared: &pgweave_analysis::PreparedSource,
    macros: &pgweave_macro::MacroSet,
    offset: usize,
) -> Option<String> {
    let (region, inserted) = prepared.region_at(offset)?;
    let def = macros.fragment(&region.macro_name)?;
    let inserted = inserted.trim();
    let mut out = format!("(fragment) **@{}**", def.name);
    if !def.params.is_empty() {
        out.push_str(&format!("({})", def.params.join(", ")));
    }
    let file = std::path::Path::new(&def.origin)
        .file_name()
        .map_or(def.origin.clone(), |f| f.to_string_lossy().into_owned());
    out.push_str(&format!(" — `{file}:{}`\n\n", def.line));
    if !def.doc.is_empty() {
        out.push_str(&def.doc.join("\n"));
        out.push_str("\n\n");
    }
    out.push_str(&format!("```sql\n{inserted}\n```"));
    Some(out)
}

fn hover_named_query(
    text: &str,
    offset: usize,
    catalog: &ProjectCatalog,
    macros: &pgweave_macro::MacroSet,
) -> Option<String> {
    let file = pgweave_query::parse_query_file_with(text, macros);
    let q = file.queries.iter().find(|q| {
        let line_start = text[..q.name_range.start].rfind('\n').map_or(0, |i| i + 1);
        let line_end = text[q.name_range.end..]
            .find('\n')
            .map_or(text.len(), |i| q.name_range.end + i);
        (line_start..=line_end).contains(&offset)
    })?;
    let d = match pgweave_query::describe_named(catalog, q) {
        Ok(d) => d,
        Err(diags) => {
            let why: Vec<String> = diags
                .iter()
                .map(|d| format!("- `{}` {}", d.code, d.message))
                .collect();
            return Some(format!(
                "(requête) **{}** : non décrite\n\n{}",
                q.name,
                why.join("\n")
            ));
        }
    };
    let null = |n: pgweave_query::descriptor::NullabilityDesc| match n {
        pgweave_query::descriptor::NullabilityDesc::NonNull => " NOT NULL",
        pgweave_query::descriptor::NullabilityDesc::Nullable => " NULL",
        pgweave_query::descriptor::NullabilityDesc::Unknown => "",
    };
    let mut out = format!("(requête) **{}** — `{}`", d.name, d.cardinality.as_str());
    if !d.params.is_empty() {
        out.push_str("\n\nParamètres :");
        for p in &d.params {
            out.push_str(&format!(
                "\n- `${}` `{}`{}",
                p.name,
                p.ty.pg,
                null(p.nullable)
            ));
        }
    }
    if !d.columns.is_empty() {
        out.push_str("\n\nColonnes :");
        for c in &d.columns {
            out.push_str(&format!(
                "\n- `{}` `{}`{}",
                c.name,
                c.ty.pg,
                null(c.nullable)
            ));
        }
    }
    Some(out)
}

/// Format a qualified name so the hovered `word` is bold.
/// e.g. word="users", qualified="public.users" → "public.**users**"
fn fmt_qualified_bold(qualified: &str, word: &str) -> String {
    if let Some(dot) = qualified.rfind('.') {
        let (schema, name) = (&qualified[..dot], &qualified[dot + 1..]);
        if name.eq_ignore_ascii_case(word) {
            return format!("{}.**{}**", schema, name);
        }
    }
    if qualified.eq_ignore_ascii_case(word) {
        format!("**{}**", qualified)
    } else {
        qualified.to_string()
    }
}

fn fmt_table_hover(
    qualified_name: &str,
    word: &str,
    table: &pgweave_schema::provider::TableSchema,
) -> String {
    let header = fmt_qualified_bold(qualified_name, word);
    let cols: Vec<String> = table
        .columns
        .iter()
        .map(|c| {
            let nn = if c.nullable { "" } else { " NOT NULL" };
            format!("**{}** `{}`{}", c.name, emit_data_type(&c.data_type), nn)
        })
        .collect();
    if cols.is_empty() {
        format!("(table) {}", header)
    } else {
        format!("(table) {}\n\n{}", header, cols.join("  \n"))
    }
}

fn lookup_hover(word: &str, schema: &dyn SchemaProvider) -> Option<String> {
    // Table name?
    if let Some(table) = schema.get_table(word) {
        return Some(fmt_table_hover(&table.name, word, table));
    }

    let lower = word.to_lowercase();

    // Function signature?
    let func_candidates = [format!("public.{}", lower), lower.clone()];
    for key in &func_candidates {
        if let Some(sig) = schema.get_function(key) {
            let params_str = sig
                .params
                .iter()
                .map(|dt| format!("`{}`", emit_data_type(dt)))
                .collect::<Vec<_>>()
                .join(", ");
            return Some(format!(
                "(function) **{}**({}): `{}`",
                sig.name,
                params_str,
                emit_data_type(&sig.return_type)
            ));
        }
    }

    // Column name anywhere
    for table_name in schema.all_table_names() {
        if let Some(table) = schema.get_table(table_name)
            && let Some(col) = table.get_column(&lower)
        {
            let nn = if col.nullable { "" } else { " NOT NULL" };
            return Some(format!(
                "(column) {}.{}.**{}**: `{}`{}",
                tbl_schema(&table.name),
                tbl_local(&table.name),
                col.name,
                emit_data_type(&col.data_type),
                nn
            ));
        }
    }

    None
}

fn var_type_to_sql(t: &pgweave_ast::plpgsql::VarType) -> String {
    use pgweave_ast::plpgsql::VarType;
    let path = |n: &pgweave_ast::ObjectName| {
        n.0.iter()
            .map(|i| i.value.as_str())
            .collect::<Vec<_>>()
            .join(".")
    };
    match t {
        VarType::Data(dt) => emit_data_type(dt),
        VarType::RowTypeOf(n) => format!("{}%ROWTYPE", path(n)),
        VarType::TypeOf(n) => format!("{}%TYPE", path(n)),
        VarType::Record => "record".into(),
    }
}

/// Check if `word` (lowercased) is a declared variable or parameter in any
/// function/procedure whose span contains `cursor_offset`.
fn hover_plpgsql_var(
    stmts: &[pgweave_ast::Statement],
    cursor_offset: usize,
    word: &str,
) -> Option<String> {
    use pgweave_ast::Statement;
    use pgweave_ast::ddl::FunctionBody;
    let lower = word.to_lowercase();

    for stmt in stmts {
        let (span, params, declare) = match stmt {
            Statement::Ddl(pgweave_ast::DdlStatement::CreateFunction(cf)) => {
                let decls = match &cf.body {
                    FunctionBody::Parsed(b) => Some(b.block.declarations.as_slice()),
                    _ => None,
                };
                (cf.span, cf.params.as_slice(), decls)
            }
            Statement::Ddl(pgweave_ast::DdlStatement::CreateProcedure(cp)) => {
                let decls = match &cp.body {
                    FunctionBody::Parsed(b) => Some(b.block.declarations.as_slice()),
                    _ => None,
                };
                (cp.span, cp.params.as_slice(), decls)
            }
            _ => continue,
        };

        // Only consider functions whose span actually contains the cursor.
        if span.end == 0 {
            continue;
        }
        if !(span.start as usize <= cursor_offset && cursor_offset < span.end as usize) {
            continue;
        }

        // DECLARE variables
        if let Some(decls) = declare {
            for decl in decls {
                let pgweave_ast::plpgsql::Declaration::Variable(v) = decl else {
                    continue;
                };
                if v.name.value.to_lowercase() == lower {
                    return Some(format!(
                        "(variable) **{}**: `{}`",
                        v.name.value,
                        var_type_to_sql(&v.var_type)
                    ));
                }
            }
        }

        // Function / procedure parameters
        for param in params {
            if let Some(name) = &param.name
                && name.value.to_lowercase() == lower
            {
                return Some(format!(
                    "(parameter) **{}**: `{}`",
                    name.value,
                    emit_data_type(&param.data_type)
                ));
            }
        }
    }

    None
}

fn tbl_schema(qualified: &str) -> &str {
    qualified
        .rfind('.')
        .map(|d| &qualified[..d])
        .unwrap_or("public")
}

fn tbl_local(qualified: &str) -> &str {
    qualified
        .rfind('.')
        .map(|d| &qualified[d + 1..])
        .unwrap_or(qualified)
}

// ── Schema loading (legacy schemaFile path) ───────────────────────────────────

/// Fragments du schéma désigné par `schemaFile` : le fichier, ou les `.sql`
/// du répertoire (non récursif), triés par nom.
fn legacy_chunks(path: &str) -> std::io::Result<Vec<pgweave_schema::SqlChunk>> {
    let p = Path::new(path);
    let files = if p.is_file() {
        vec![p.to_path_buf()]
    } else if p.is_dir() {
        let mut files: Vec<PathBuf> = std::fs::read_dir(p)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|f| {
                f.extension()
                    .and_then(|s| s.to_str())
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("sql"))
            })
            .collect();
        files.sort();
        files
    } else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("{path} is neither a file nor a directory"),
        ));
    };
    Ok(files
        .into_iter()
        .flat_map(|f| {
            pgweave_schema::SchemaSource::chunks(&pgweave_schema::FileSource::new(
                f,
                FileRole::Schema,
            ))
        })
        .collect())
}

// ── URI / path helpers ────────────────────────────────────────────────────────

fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let path_str = uri.strip_prefix("file://").unwrap_or(uri);
    Some(canonical(Path::new(path_str)))
}

/// Chemin canonique s'il existe, tel quel sinon : clé commune aux fichiers
/// du projet et aux documents de l'éditeur.
fn canonical(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

fn url_to_path(url: &str) -> Option<PathBuf> {
    uri_to_path(url)
}

fn is_inside(child: &Path, parent_str: &str) -> bool {
    let parent = Path::new(parent_str);
    let parent_canon = parent
        .canonicalize()
        .unwrap_or_else(|_| parent.to_path_buf());
    let child_canon = child.canonicalize().unwrap_or_else(|_| child.to_path_buf());
    child_canon == parent_canon || child_canon.starts_with(format!("{}/", parent_canon.display()))
}

fn position_to_offset(text: &str, line: u32, character: u32) -> u32 {
    let mut current_line = 0u32;
    let mut offset = 0u32;
    for (i, ch) in text.char_indices() {
        if current_line == line {
            return offset + character;
        }
        if ch == '\n' {
            current_line += 1;
            offset = (i + 1) as u32;
        }
    }
    offset + character
}

fn word_at_offset(text: &str, offset: usize) -> Option<String> {
    let bytes = text.as_bytes();
    if offset >= bytes.len() {
        return None;
    }
    let mut start = offset;
    while start > 0 && (bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_') {
        start -= 1;
    }
    let mut end = offset;
    while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
        end += 1;
    }
    if start == end {
        None
    } else {
        Some(text[start..end].to_lowercase())
    }
}
