use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use pgzest_analysis::project::Workspace;
use pgzest_catalog::ProjectCatalog;
use pgzest_diagnostics::{Diagnostic, Severity};
use pgzest_lint::config::LintConfig;
use pgzest_plugin::{WasmCheckRule, WasmPlugin};
use pgzest_schema::provider::SchemaProvider;
use pgzest_schema::{FileRole, ProjectConfig, check_ddl_role};
use tokio::sync::Mutex;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, jsonrpc::Result};

use pgzest_ast::emit::emit_data_type;

use super::completion::{analyze_cursor, build_completions};
use super::line_index::LineIndex;
use super::semantic;

// ── Custom request: macro expansion ───────────────────────────────────────────

/// Parameters of `pgzest/expandMacro`.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExpandMacroParams {
    /// Document to desugar.
    pub text_document: TextDocumentIdentifier,
    /// Cursor position. If it falls inside a macro, only that one is
    /// rendered; otherwise the whole file is.
    pub position: Option<Position>,
}

/// Response of `pgzest/expandMacro`.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExpandMacroResult {
    /// Standard SQL produced.
    pub sql: String,
    /// Number of macros desugared in what was rendered.
    pub expanded: usize,
    /// Message to display when there is nothing to show.
    pub note: Option<String>,
}

// ── Backend state ─────────────────────────────────────────────────────────────

pub struct PgsqlBackend {
    client: Client,
    documents: Mutex<HashMap<String, String>>,
    /// Project in an incremental database: files read at load time, replaced
    /// by the editor's text as it changes. The catalog is only recomputed if
    /// a schema file or a migration changes.
    workspace: Mutex<Workspace>,
    /// Project root and config (from pgzest.toml).
    project_root: Mutex<Option<PathBuf>>,
    config: Mutex<Option<ProjectConfig>>,
    /// Fallback schema path from `schemaFile` init option (legacy, no config file).
    legacy_schema_path: Mutex<Option<String>>,
    /// WASM plugins loaded from plugins.wasm in pgzest.toml.
    wasm_plugins: Mutex<Vec<Arc<WasmPlugin>>>,
    /// Known macros: the shipped ones, plus the project's `*.pgmacro` files.
    ///
    /// Rebuilt with the resolver: a modified `.pgmacro` must take effect
    /// without restarting the server.
    macros: Mutex<Arc<pgzest_expand::MacroSet>>,
}

impl PgsqlBackend {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            documents: Mutex::new(HashMap::new()),
            workspace: Mutex::new(Workspace::new(
                &[],
                Arc::new(pgzest_expand::MacroSet::builtin()),
            )),
            project_root: Mutex::new(None),
            config: Mutex::new(None),
            legacy_schema_path: Mutex::new(None),
            wasm_plugins: Mutex::new(Vec::new()),
            macros: Mutex::new(Arc::new(pgzest_expand::MacroSet::builtin())),
        }
    }

    /// Desugars a document, or only the macro under the cursor.
    ///
    /// Serves the extension's "Show expansion" command: the result is shown in
    /// a read-only tab, as Rust Analyzer does for its macros.
    pub async fn expand_macro(&self, params: ExpandMacroParams) -> Result<ExpandMacroResult> {
        let uri = params.text_document.uri.to_string();
        let Some(text) = self.documents.lock().await.get(&uri).cloned() else {
            return Ok(ExpandMacroResult {
                sql: String::new(),
                expanded: 0,
                note: Some("document not open on the server side".into()),
            });
        };

        let macros = self.macros().await;
        let full = pgzest_expand::expand_source(&text, &macros);

        if full.is_passthrough() {
            return Ok(ExpandMacroResult {
                sql: String::new(),
                expanded: 0,
                note: Some("no macro in this file".into()),
            });
        }

        // Without a position, render the whole desugared file.
        let Some(pos) = params.position else {
            return Ok(ExpandMacroResult {
                sql: full.sql,
                expanded: full.expanded,
                note: None,
            });
        };

        // With a position: isolate the macro containing it. Find the generated
        // range whose origin covers the cursor, relying on the same mapping as
        // the diagnostics.
        let index = LineIndex::new(&text);
        let offset = index.offset_at(pos.line as usize, pos.character as usize);

        match full.expansion_at(offset) {
            Some(sql) => Ok(ExpandMacroResult {
                sql: sql.to_string(),
                expanded: 1,
                note: None,
            }),
            // Cursor outside a macro: the whole file, rather than nothing.
            None => Ok(ExpandMacroResult {
                sql: full.sql,
                expanded: full.expanded,
                note: Some("cursor outside a macro — whole file desugared".into()),
            }),
        }
    }

    /// A pgzest project (`pgzest.toml`) is loaded.
    async fn in_project(&self) -> bool {
        self.config.lock().await.is_some()
    }

    /// Current macro set (Arc clone, cheap).
    async fn macros(&self) -> Arc<pgzest_expand::MacroSet> {
        self.macros.lock().await.clone()
    }

    /// Reloads the project's macros on top of the shipped ones.
    async fn rebuild_macros(&self) {
        let mut set = pgzest_expand::MacroSet::builtin();
        if let Some(root) = self.project_root.lock().await.clone() {
            // Invalid config: already reported by `rebuild_resolver` /
            // `initialize`. No fallback directory is guessed.
            let dir = match ProjectConfig::load(&root.join(ProjectConfig::FILENAME)) {
                Ok(cfg) => {
                    set = set.with_instrumentation(cfg.log.instrumentation());
                    Some(root.join(&cfg.macros.directory))
                }
                Err(_) => None,
            };
            if let Some(dir) = dir.filter(|d| d.is_dir()) {
                // An open file wins over the disk, as for the schema.
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
                        .log_message(MessageType::WARNING, format!("pgzest: {e}"))
                        .await;
                }
            }
        }
        let set = Arc::new(set);
        *self.macros.lock().await = set.clone();
        self.workspace.lock().await.set_macros(set);
    }

    /// Module of `file` in its query directory, and the queries of that
    /// directory per module, open documents included.
    async fn query_tree(
        &self,
        file: &Path,
        catalog: &ProjectCatalog,
        macros: &pgzest_expand::MacroSet,
    ) -> (Vec<String>, Vec<pgzest_query::QueryModule>) {
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
        let modules = pgzest_analysis::query_modules(&qroot, catalog, macros, &|p| {
            let canonical = p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
            open.get(&canonical)
                .or_else(|| open.get(p))
                .cloned()
                .or_else(|| std::fs::read_to_string(p).ok())
        });
        (here, modules)
    }

    /// Up-to-date project catalog (memoized replay, Arc clone).
    async fn resolver(&self) -> Arc<ProjectCatalog> {
        self.workspace.lock().await.catalog()
    }

    /// Loads the project formed by `chunks`, then applies the text of the
    /// open documents: the editor wins over the disk.
    async fn load_workspace(&self, chunks: Vec<pgzest_schema::SqlChunk>) {
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
        // Extension packs of the project: one that cannot be used is said,
        // as an invalid config is.
        let root = self.project_root.lock().await.clone();
        let config = self.config.lock().await.clone();
        if let (Some(root), Some(config)) = (root, config) {
            let packs = pgzest_catalog::ProjectPacks::for_config(&root, &config);
            for (path, d) in &packs.diagnostics {
                let error = d.severity == pgzest_diagnostics::Severity::Error;
                let message = format!("pgzest: {}: {}", path.display(), d.message);
                if error {
                    self.client.show_message(MessageType::ERROR, message).await;
                } else {
                    self.client.log_message(MessageType::INFO, message).await;
                }
            }
            ws.set_packs(Arc::new(packs));
        }
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

        // Config-driven classification (authoritative when pgzest.toml present).
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

    /// Reloads the project from disk (config, or legacy path): new file,
    /// modified config. A plain text change goes through
    /// [`Workspace::set_text`], without rereading.
    async fn reload_workspace(&self) {
        let root = self.project_root.lock().await.clone();

        let chunks = if let Some(ref root) = root {
            let config_path = root.join(ProjectConfig::FILENAME);
            match ProjectConfig::load(&config_path) {
                Ok(config) => {
                    // Project macros loaded first: the catalog must see the
                    // objects declared by custom macros, not only by the
                    // shipped ones.
                    self.rebuild_macros().await;
                    let source = config.to_source(root);
                    let chunks = pgzest_schema::SchemaSource::chunks(&source);
                    // The reloaded config replaces the old one: without this,
                    // a change to [rules] or [parse] was only taken into
                    // account on server restart.
                    *self.config.lock().await = Some(config);
                    chunks
                }
                Err(e) => {
                    self.client
                        .show_message(MessageType::ERROR, format!("pgzest: {e}"))
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

    /// Rules of the loaded WASM plugins.
    async fn make_checker(&self) -> pgzest_analysis::Checker {
        let mut checker = pgzest_analysis::Checker::default();
        for plugin in self.wasm_plugins.lock().await.iter() {
            checker.add_rule(Box::new(WasmCheckRule::new(plugin.clone())));
        }
        checker
    }

    /// Type-checks already parsed statements against `schema`.
    async fn check_stmts(
        &self,
        stmts: &[pgzest_ast::Statement],
        schema: &dyn SchemaProvider,
    ) -> Vec<Diagnostic> {
        self.make_checker().await.run(stmts, schema)
    }

    /// Parses, type-checks, lints and publishes the diagnostics of `uri`.
    ///
    /// Must produce exactly what `pgzest check` produces on the same file:
    /// same parser tolerance policy, same comment directives, same severity
    /// overrides. Any divergence here shows up as a file green in the editor
    /// and red in CI.
    async fn run_diagnostics(&self, uri: &str, text: &str) {
        let line_index = LineIndex::new(text);
        if is_rust(uri) {
            // `sql!` / `query!` queries of a pgzest project only: a macro of
            // the same name from another tool is not analyzed outside a project.
            let diags = if self.in_project().await {
                let resolver = self.resolver().await;
                let macros = self.macros().await;
                super::rust_embed::find(text)
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
        // Fragment file: only its format; each fragment is checked where it
        // is inserted.
        if role == FileRole::Fragments {
            let diags = pgzest_expand::fragment_file_diagnostics(text);
            self.publish(uri, &diags, &line_index).await;
            return;
        }
        let resolver = self.resolver().await;
        let suppressions = pgzest_diagnostics::Suppressions::scan(text);

        // Macro desugaring. A source without macros is copied verbatim, so it
        // is free and no file escapes it because of its extension. The spans
        // produced afterwards are already in the edited file's frame — see
        // `PreparedSource::parse`.
        let macros = self.macros().await;
        let prepared = if role == FileRole::Query {
            pgzest_analysis::prepare_query_source(text, &macros)
        } else {
            pgzest_analysis::prepare_source(text, &macros)
        };

        // A single parse for the checker, the lint and the role rules.
        let outcome = match prepared.parse_with_outcome() {
            Ok(o) => o,
            Err(e) => {
                let mut diags = vec![Diagnostic::error(
                    pgzest_diagnostics::codes::PARSE_ERROR,
                    format!("parse error: {e}"),
                )];
                suppressions.apply(&mut diags);
                self.publish(uri, &diags, &line_index).await;
                return;
            }
        };
        let stmts = outcome.statements;

        // Project config: parser tolerance + severity overrides.
        let (parse_cfg, rule_overrides) = {
            let guard = self.config.lock().await;
            match guard.as_ref() {
                Some(c) => (c.parse.clone(), c.rule_overrides()),
                None => (Default::default(), Default::default()),
            }
        };

        // ── Macros and skipped statements ─────────────────────────────────────
        let mut diags = prepared.diagnostics.clone();

        // Named query file: format checked as well, as in the CLI.
        if let (Some(path), Some(root)) = (uri_to_path(uri), self.project_root.lock().await.clone())
        {
            let is_query_file = self
                .config
                .lock()
                .await
                .as_ref()
                .is_some_and(|c| c.is_query_file(&path, &root));
            if is_query_file {
                diags.extend(pgzest_analysis::query_file_diagnostics(
                    &path, text, &macros,
                ));
            }
        }
        diags.extend(pgzest_analysis::skipped_to_diagnostics(
            &outcome.skipped,
            &parse_cfg,
        ));
        diags.extend(pgzest_analysis::lost_to_diagnostics(&outcome.lost));

        // ── Check ─────────────────────────────────────────────────────────────
        // Like `pgzest check`: a schema or migration file of the project is
        // checked during the replay (replay diagnostics, editor text
        // included); a query file, against the final catalog; a file outside
        // the project, by replaying its own DDL.
        let path = uri_to_path(uri);
        let in_project = match &path {
            Some(p) => self.workspace.lock().await.contains(p),
            None => false,
        };
        match (&role, &path) {
            (FileRole::Query, _) => {
                diags.extend(pgzest_analysis::check_statements(&stmts, &resolver));
                let (here, others) = match &path {
                    Some(p) => self.query_tree(p, &resolver, &macros).await,
                    None => (Vec::new(), Vec::new()),
                };
                diags.extend(pgzest_analysis::query_descriptor_diagnostics(
                    text, &resolver, &macros, &here, &others,
                ));
            }
            (_, Some(p)) if in_project => {
                diags.extend(self.workspace.lock().await.replay_diagnostics(p));
            }
            _ => diags.extend(pgzest_analysis::check_script(&stmts)),
        }
        diags.extend(self.check_stmts(&stmts, resolver.as_ref()).await);

        // ── Lint + role rules ─────────────────────────────────────────────────
        diags.extend(pgzest_lint::lint_statements(&stmts, &LintConfig::default()).diagnostics);
        diags.extend(check_ddl_role(&stmts, &role));

        // Overrides apply to everything, checker and lint alike: a rule set to
        // "off" in the config must be silent wherever it comes from.
        pgzest_diagnostics::apply_rule_overrides(&mut diags, &rule_overrides);

        // Comment directives, last: they have the final say.
        suppressions.apply(&mut diags);

        self.publish(uri, &diags, &line_index).await;
    }

    /// The schema changed: every other open document is rechecked (the replay
    /// is memoized, only the checks are redone).
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
            source: Some("pgzest".to_string()),
            // The hint follows the message, like PostgreSQL's HINT.
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
        // 1. Try to discover pgzest.toml from the workspace root.
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
                // Config present but invalid: tell the user, and do not
                // analyze with an invented config.
                self.client
                    .show_message(MessageType::ERROR, format!("pgzest: {e}"))
                    .await;
                None
            }
            None => None,
        };
        if let Some((project_root, config)) = discovered {
            *self.project_root.lock().await = Some(project_root.clone());
            // Config first: loading the project depends on it (declarative
            // mode).
            *self.config.lock().await = Some(config.clone());
            // Project macros first: the catalog must see the objects declared
            // by custom macros, not only by the shipped ones.
            self.rebuild_macros().await;
            let source = config.to_source(&project_root);
            self.load_workspace(pgzest_schema::SchemaSource::chunks(&source))
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
                                    "pgzest: loaded wasm plugin '{}' from {}",
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
                                    "pgzest: failed to load wasm plugin '{}': {e}",
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
                        "pgzest: loaded schema from {}",
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
                            format!("pgzest: could not load schema at {path}: {e}"),
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
                name: "pgzest".to_string(),
                version: Some("0.1.0".to_string()),
            }),
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "pgzest language server started")
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
        // Schema or migration file missing from the loaded project (created
        // since, e.g. by `pgzest migrate create`): without a reload it would
        // be checked alone, outside the chain of migrations before it.
        let unknown = !is_rust(&uri)
            && matches!(
                self.role_of(&uri).await,
                FileRole::Schema | FileRole::Migration { .. }
            )
            && match uri_to_path(&uri) {
                Some(p) => !self.workspace.lock().await.set_text(&p, &text),
                None => false,
            };
        if unknown && self.in_project().await {
            self.reload_workspace().await;
            self.run_diagnostics(&uri, &text).await;
            self.refresh_other_documents(&uri).await;
            return;
        }
        self.run_diagnostics(&uri, &text).await;
    }

    /// Files created, deleted or changed outside the editor: the project
    /// follows the disk, except for open documents (the editor wins).
    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        if !self.in_project().await && self.legacy_schema_path.lock().await.is_none() {
            return;
        }
        let open: std::collections::HashSet<String> =
            self.documents.lock().await.keys().cloned().collect();
        let (mut reload, mut changed) = (false, false);
        for event in params.changes {
            let uri = event.uri.to_string();
            if event.typ == FileChangeType::CHANGED && open.contains(&uri) {
                continue;
            }
            let Some(path) = uri_to_path(&uri) else {
                continue;
            };
            let config_file = path
                .file_name()
                .is_some_and(|n| n == ProjectConfig::FILENAME);
            let macro_file = path.extension().is_some_and(|e| e == "pgmacro");
            let pack_file = match (
                self.project_root.lock().await.as_ref(),
                self.config.lock().await.as_ref(),
            ) {
                (Some(root), Some(config)) => {
                    pgzest_catalog::ProjectPacks::is_pack_file(root, config, &path)
                }
                _ => false,
            };
            let role = self.role_of(&uri).await;
            if config_file || macro_file || pack_file || role == FileRole::Fragments {
                reload = true;
            } else if matches!(role, FileRole::Schema | FileRole::Migration { .. }) {
                // Known file changed: only what depends on it is recomputed.
                // Created or deleted: the project is reloaded.
                let known = event.typ == FileChangeType::CHANGED
                    && match std::fs::read_to_string(&path) {
                        Ok(text) => self.workspace.lock().await.set_text(&path, &text),
                        Err(_) => false,
                    };
                reload |= !known;
                changed = true;
            } else if role == FileRole::Query {
                // A query can be referenced by others (`-> name`).
                changed = true;
            }
        }
        if reload {
            self.reload_workspace().await;
        }
        if reload || changed {
            self.refresh_other_documents("").await;
        }
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
                // Project macro: all files are reread.
                self.rebuild_macros().await;
                schema_changed = true;
            } else if matches!(
                self.role_of(&uri).await,
                FileRole::Schema | FileRole::Migration { .. }
            ) {
                // Known file: only what depends on it is recomputed.
                // New file: the project is reloaded.
                let known = match &path {
                    Some(p) => self.workspace.lock().await.set_text(p, &text),
                    None => false,
                };
                if !known {
                    self.reload_workspace().await;
                }
                schema_changed = true;
            } else if self.role_of(&uri).await == FileRole::Query {
                // A query can be referenced by others (`-> name`).
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
        // Changes discarded: the disk is authoritative again.
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

        // `$name`: a query parameter, never a column of the same name.
        if param_at(&text, offset as usize).is_some() {
            let resolver = self.resolver().await;
            let macros = self.macros().await;
            return Ok(
                hover_param(&text, offset as usize, resolver.as_ref(), &macros).map(|value| {
                    Hover {
                        contents: HoverContents::Markup(MarkupContent {
                            kind: MarkupKind::Markdown,
                            value,
                        }),
                        range: None,
                    }
                }),
            );
        }

        let role = self.role_of(&uri).await;
        let resolver = self.resolver().await;
        let macros = self.macros().await;

        // A single parse, macros and fragments desugared: spans are in the
        // edited file's frame.
        let prepared = if role == FileRole::Query {
            pgzest_analysis::prepare_query_source(&text, &macros)
        } else {
            pgzest_analysis::prepare_source(&text, &macros)
        };
        let stmts = prepared
            .parse_with_outcome()
            .map(|o| o.statements)
            .unwrap_or_default();

        let hover_text: Option<String> =
            // Fragment invocation: its documentation and the inserted SQL.
            hover_fragment(&prepared, &macros, offset as usize)
            // Named query header: its descriptor.
            .or_else(|| hover_named_query(&text, offset as usize, resolver.as_ref(), &macros))
            // PL/pgSQL declared variables / parameters take priority in any file type.
            .or_else(|| hover_plpgsql_var(&stmts, offset as usize, &word))
            .or_else(|| {
                if matches!(role, FileRole::Schema) {
                    hover_schema_file(&text, &stmts, offset as usize, &word, resolver.as_ref())
                } else {
                    hover_query(&text, &stmts, resolver.as_ref(), offset as usize, &word)
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
        // Rust file: only inside the SQL of a `sql!` / `query!`.
        if is_rust(&uri) {
            if !self.in_project().await {
                return Ok(None);
            }
            let found = super::rust_embed::find(&text)
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

/// Hover in a schema file: a column of the table whose definition or
/// column list surrounds the cursor (`CREATE TABLE t (…)`, `REFERENCES t
/// (…)`, `CREATE INDEX … ON t (…)`), otherwise what the name designates by
/// itself. A column name is never looked up across tables: `id` exists in
/// most of them.
fn hover_schema_file(
    text: &str,
    stmts: &[pgzest_ast::Statement],
    offset: usize,
    word: &str,
    global: &ProjectCatalog,
) -> Option<String> {
    if let Some(h) = hover_routine_body(text, stmts, global, offset, word) {
        return Some(h);
    }
    // The file's own definitions first: it may not be saved yet.
    let own = ProjectCatalog::from_sql(text).ok();
    let table = |name: &str| {
        own.as_ref()
            .and_then(|c| c.get_table(name))
            .or_else(|| global.get_table(name))
    };
    let owners = stmts
        .iter()
        .filter(|s| {
            matches!(
                s,
                pgzest_ast::Statement::Ddl(
                    pgzest_ast::DdlStatement::CreateTable(_)
                        | pgzest_ast::DdlStatement::CreateIndex(_)
                )
            )
        })
        .filter_map(pgzest_ast::stmt_span)
        .find(|sp| sp.start as usize <= offset && offset < sp.end as usize)
        .map(|sp| list_owners(text, sp.start as usize, offset))
        .unwrap_or_default();
    // The innermost list naming its table: never a column of another one.
    if let Some(h) = owners
        .first()
        .and_then(|owner| table(owner))
        .and_then(|t| fmt_column_hover(t, word))
    {
        return Some(h);
    }
    if let Some(t) = table(word) {
        return Some(fmt_table_hover(&t.name, word, t));
    }
    hover_function_name(text, offset, word, global)
}

/// Tables owning the parenthesized lists that surround `offset` in the
/// statement starting at `start`, innermost first: `CREATE TABLE t (`,
/// `REFERENCES t (`, `ON t (`, `ON t USING method (`, `INSERT INTO t (`
/// and `COPY t (`. A list not
/// introduced that way (`CHECK (`, `UNIQUE (`) belongs to the enclosing
/// one.
fn list_owners(text: &str, start: usize, offset: usize) -> Vec<String> {
    let b = text.as_bytes();
    let mut open: Vec<usize> = Vec::new();
    let mut i = start;
    while i < offset.min(b.len()) {
        match b[i] {
            b'\'' => {
                i += 1;
                while i < b.len() && b[i] != b'\'' {
                    i += 1;
                }
            }
            b'"' => {
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    i += 1;
                }
            }
            b'-' if b.get(i + 1) == Some(&b'-') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'(' => open.push(i),
            b')' => {
                open.pop();
            }
            _ => {}
        }
        i += 1;
    }
    open.iter()
        .rev()
        .filter_map(|&paren| {
            let mut words = words_before(&text[start..paren]);
            let mut name = words.next()?;
            let mut keyword = words.next()?;
            if keyword.eq_ignore_ascii_case("using") {
                name = words.next()?;
                keyword = words.next()?;
            }
            ["references", "on", "table", "exists", "into", "copy"]
                .iter()
                .any(|k| keyword.eq_ignore_ascii_case(k))
                .then(|| name.replace('"', ""))
        })
        .collect()
}

/// Words of `text` from the end: identifiers, possibly qualified or quoted.
fn words_before(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '.' || c == '"'))
        .filter(|w| !w.is_empty())
        .rev()
}

/// Hover of a function name written as a call (`name(`) that no analysis
/// resolved: every overload visible on the search path, since only the
/// arguments decide between them.
fn hover_function_name(
    text: &str,
    offset: usize,
    word: &str,
    catalog: &ProjectCatalog,
) -> Option<String> {
    let end = offset
        + text
            .get(offset..)?
            .find(|c: char| !(c.is_alphanumeric() || c == '_'))?;
    if !text[end..].trim_start().starts_with('(') {
        return None;
    }
    let ts = pgzest_sema::TypeSystem::new(&catalog.catalog);
    let ty = |r: &pgzest_catalog::TypeRef| {
        ts.of_ref(r)
            .map_or_else(|| "?".to_string(), |t| pgzest_sema::format_type(&t))
    };
    let overloads: Vec<String> = catalog
        .search_path
        .lookup_order()
        .into_iter()
        .filter_map(|schema| {
            catalog
                .catalog
                .routines
                .get(&pgzest_catalog::QName::new(schema, word))
        })
        .flatten()
        .map(|r| {
            use pgzest_catalog::model::ArgMode;
            let args: Vec<String> = r
                .args
                .iter()
                .filter(|a| !matches!(a.mode, ArgMode::Out | ArgMode::Table))
                .map(|a| ty(&a.ty))
                .collect();
            let returns = r.returns.as_ref().map_or_else(|| "procedure".into(), ty);
            format!(
                "{}.**{}**({}) → `{}{}`",
                r.name.schema,
                r.name.name,
                args.join(", "),
                if r.returns_set { "SETOF " } else { "" },
                returns
            )
        })
        .collect();
    match overloads.as_slice() {
        [] => None,
        [one] => Some(format!("(function) {one}")),
        many => Some(format!(
            "(function) **{word}**, {} overloads: the arguments decide\n\n- {}",
            many.len(),
            many.join("\n- ")
        )),
    }
}

/// Hover in a query: what the name under the cursor designates, according to
/// the semantic analysis of the statement containing it — a column, a
/// function call as resolved, a target column; failing that, a table or the
/// overloads of a function of that name. Never a column found by its name
/// alone.
fn hover_query(
    text: &str,
    stmts: &[pgzest_ast::Statement],
    catalog: &ProjectCatalog,
    offset: usize,
    word: &str,
) -> Option<String> {
    use pgzest_ast::Statement;
    let stmt = stmts.iter().find(|s| {
        pgzest_ast::stmt_span(s)
            .is_some_and(|sp| sp.start as usize <= offset && offset <= sp.end as usize)
    });
    if let Some(h) = hover_routine_body(text, stmts, catalog, offset, word) {
        return Some(h);
    }
    if let Some(
        stmt @ (Statement::Select(_)
        | Statement::Insert(_)
        | Statement::Update(_)
        | Statement::Delete(_)),
    ) = stmt
    {
        let mut a = pgzest_sema::Analyzer::new(
            pgzest_sema::TypeSystem::new(&catalog.catalog),
            search_path_at(stmts, stmt, catalog),
        );
        // References resolved before a possible error remain valid.
        let _ = a.describe(stmt);
        if let Some(h) = hover_refs(text, offset, word, &a.column_refs, &a.call_refs) {
            return Some(h);
        }
        if let Some(h) = hover_target_column(stmt, catalog, word) {
            return Some(h);
        }
    }
    // A column list naming its table: `INSERT INTO t (…)`, `COPY t (…)`.
    if let Some(h) = stmt
        .and_then(pgzest_ast::stmt_span)
        .and_then(|sp| {
            list_owners(text, sp.start as usize, offset)
                .into_iter()
                .next()
        })
        .and_then(|owner| catalog.get_table(&owner))
        .and_then(|t| fmt_column_hover(t, word))
    {
        return Some(h);
    }
    if let Some(t) = catalog.get_table(word) {
        return Some(fmt_table_hover(&t.name, word, t));
    }
    hover_function_name(text, offset, word, catalog)
}

/// Search path in effect for `stmt`: the project's, changed by the
/// `SET search_path` that precede it in the file.
fn search_path_at(
    stmts: &[pgzest_ast::Statement],
    stmt: &pgzest_ast::Statement,
    catalog: &ProjectCatalog,
) -> pgzest_catalog::SearchPath {
    let mut path = catalog.search_path.clone();
    for s in stmts {
        if std::ptr::eq(s, stmt) {
            break;
        }
        if let Some(p) = pgzest_sema::check::search_path_change(s) {
            path = p;
        }
    }
    path
}

/// Hover inside the body of a routine (`CREATE FUNCTION`, `CREATE
/// PROCEDURE`, `DO`): what the analysis of the body resolved, its
/// variables in scope. A trigger function shared between several tables
/// is analyzed once per table: each distinct answer is shown.
fn hover_routine_body(
    text: &str,
    stmts: &[pgzest_ast::Statement],
    catalog: &ProjectCatalog,
    offset: usize,
    word: &str,
) -> Option<String> {
    use pgzest_ast::{DdlStatement, Statement, UtilityKind};
    let routine = |s: &Statement| match s {
        Statement::Ddl(DdlStatement::CreateFunction(_) | DdlStatement::CreateProcedure(_)) => true,
        Statement::Utility(u) => matches!(u.kind, UtilityKind::Do { .. }),
        _ => false,
    };
    let stmt = stmts.iter().find(|s| {
        routine(s)
            && pgzest_ast::stmt_span(s)
                .is_some_and(|sp| sp.start as usize <= offset && offset < sp.end as usize)
    })?;
    let path = search_path_at(stmts, stmt, catalog);
    let refs = pgzest_sema::check::body_refs(&catalog.catalog, &path, stmt);
    hover_refs(text, offset, word, &refs.columns, &refs.calls)
}

/// The innermost reference under the cursor among those resolved by an
/// analysis: a column, or a function call when the cursor is on its name.
/// Several analyses of the same text (one per trigger table) may give
/// several answers for the same reference: each distinct one is listed.
fn hover_refs(
    text: &str,
    offset: usize,
    word: &str,
    columns: &[(pgzest_ast::Span, pgzest_sema::Typed)],
    calls: &[pgzest_sema::CallRef],
) -> Option<String> {
    use pgzest_sema::Nullability;
    let null = |n: Nullability| match n {
        Nullability::NonNull => " NOT NULL",
        Nullability::Nullable => " NULL",
        Nullability::Unknown => " (unknown nullability)",
    };
    let contains = |sp: &pgzest_ast::Span| sp.start as usize <= offset && offset < sp.end as usize;
    let len = |sp: &pgzest_ast::Span| sp.end - sp.start;
    let column = columns
        .iter()
        .filter(|(sp, _)| contains(sp))
        .map(|(sp, _)| *sp)
        .min_by_key(len);
    // On the function's name only, not on the keywords of the call
    // (`FILTER (WHERE …)`, `OVER (…)`).
    let on_name = |c: &&pgzest_sema::CallRef| {
        c.name
            .rsplit('.')
            .next()
            .is_some_and(|n| n.eq_ignore_ascii_case(word))
    };
    let call = calls
        .iter()
        .filter(|c| contains(&c.span))
        .min_by_key(|c| len(&c.span))
        .filter(on_name)
        .map(|c| c.span);
    let mut lines: Vec<String> = Vec::new();
    // The innermost: a column inside a call's arguments wins.
    match (column, call) {
        (Some(sp), c) if c.is_none_or(|c| len(&sp) <= len(&c)) => {
            let written = text
                .get(sp.start as usize..sp.end as usize)
                .filter(|w| !w.contains('\n') && !w.contains('@'))
                .unwrap_or(word);
            for (_, t) in columns.iter().filter(|(s, _)| *s == sp) {
                lines.push(format!(
                    "(column) **{written}**: `{}`{}",
                    pgzest_sema::format_type(&t.ty),
                    null(t.nullable)
                ));
            }
        }
        (_, Some(sp)) => {
            for c in calls.iter().filter(|c| c.span == sp) {
                let args: Vec<String> = c.args.iter().map(pgzest_sema::format_type).collect();
                lines.push(format!(
                    "(function) {}({}): `{}`{}",
                    fmt_qualified_bold(&c.name, word),
                    args.join(", "),
                    pgzest_sema::format_type(&c.result.ty),
                    null(c.result.nullable)
                ));
            }
        }
        _ => {}
    }
    let mut seen = std::collections::HashSet::new();
    lines.retain(|l| seen.insert(l.clone()));
    (!lines.is_empty()).then(|| lines.join("\n\n"))
}

/// Column of the target table of an `UPDATE … SET` (or an `INSERT` column
/// list): written as a bare name, it designates a column of that table only.
fn hover_target_column(
    stmt: &pgzest_ast::Statement,
    catalog: &ProjectCatalog,
    word: &str,
) -> Option<String> {
    use pgzest_ast::Statement;
    let (table, columns): (&pgzest_ast::ObjectName, Vec<&pgzest_ast::Ident>) = match stmt {
        Statement::Insert(i) => (&i.table, i.columns.iter().collect()),
        Statement::Update(u) => (
            &u.table.name,
            u.assignments.iter().flat_map(|a| &a.columns).collect(),
        ),
        _ => return None,
    };
    columns
        .iter()
        .find(|c| c.value.eq_ignore_ascii_case(word))?;
    let name = table
        .0
        .iter()
        .map(|i| i.value.as_str())
        .collect::<Vec<_>>()
        .join(".");
    fmt_column_hover(catalog.get_table(&name)?, word)
}

/// Column `word` of the table `t`, as declared.
fn fmt_column_hover(t: &pgzest_schema::provider::TableSchema, word: &str) -> Option<String> {
    let col = t.get_column(word)?;
    let nn = if col.nullable { "" } else { " NOT NULL" };
    Some(format!(
        "(column) {}.{}.**{}**: `{}`{}",
        tbl_schema(&t.name),
        tbl_local(&t.name),
        col.name,
        emit_data_type(&col.data_type),
        nn
    ))
}

/// Name of the `$name` (or `$1`) parameter under the cursor.
fn param_at(text: &str, offset: usize) -> Option<String> {
    let b = text.as_bytes();
    let word = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    if offset > b.len() {
        return None;
    }
    let mut start = offset.min(b.len());
    while start > 0 && word(b[start - 1]) {
        start -= 1;
    }
    let mut end = offset;
    while end < b.len() && word(b[end]) {
        end += 1;
    }
    (start > 0 && b[start - 1] == b'$' && end > start).then(|| text[start..end].to_string())
}

/// Hover of a parameter: the type and nullability established by the
/// description of its query.
fn hover_param(
    text: &str,
    offset: usize,
    catalog: &ProjectCatalog,
    macros: &pgzest_expand::MacroSet,
) -> Option<String> {
    use pgzest_query::descriptor::NullabilityDesc;
    let name = param_at(text, offset)?;
    let file = pgzest_query::parse_query_file_with(text, macros);
    let q = file
        .queries
        .iter()
        .find(|q| q.sql_range.start <= offset && offset <= q.sql_range.end)?;
    let d = match pgzest_query::describe_named(catalog, q) {
        Ok(d) => d,
        Err(_) => {
            return Some(format!(
                "(parameter) **${name}**: type not established, query `{}` is not described",
                q.name
            ));
        }
    };
    let p = d.params.iter().find(|p| p.name == name)?;
    let ty = if p.ty.modifiers.is_empty() {
        p.ty.pg.clone()
    } else {
        format!("{}({})", p.ty.pg, p.ty.modifiers.join(","))
    };
    let null = match p.nullable {
        NullabilityDesc::Nullable => "may be NULL (`Option` in Rust)",
        NullabilityDesc::NonNull => "NOT NULL",
        NullabilityDesc::Unknown => "nullability not established: non-null value expected",
    };
    Some(format!(
        "(parameter) **${name}**: `{ty}`, {null}\n\nfrom query `{}`",
        q.name
    ))
}

/// Rust document (`sql!` / `query!` queries).
fn is_rust(uri: &str) -> bool {
    uri.ends_with(".rs")
}

/// Named query header equivalent to a query written in Rust.
fn embedded_header(e: &super::rust_embed::Embedded) -> String {
    let name = e.name.as_ref().map_or("query", |(n, _)| n.as_str());
    match &e.cardinality {
        Some(c) => format!("-- name: {name} :{c}\n"),
        None => format!("-- name: {name}\n"),
    }
}

/// Diagnostics of a query written in Rust, at file positions: format,
/// fragments, analysis and what prevents describing it, as for a named
/// query.
fn embedded_diagnostics(
    e: &super::rust_embed::Embedded,
    catalog: &ProjectCatalog,
    macros: &pgzest_expand::MacroSet,
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
                pgzest_diagnostics::Span { start, end }
            }
            // In the added header (name, cardinality): on the written name.
            _ => pgzest_diagnostics::Span {
                start: whole.start,
                end: whole.end,
            },
        });
        d
    };
    let file = pgzest_query::parse_query_file_with(&text, macros);
    let mut out: Vec<Diagnostic> = file.diagnostics.into_iter().map(place).collect();
    for q in &file.queries {
        if let Err(ds) = pgzest_query::describe_named(catalog, q) {
            out.extend(ds.into_iter().map(place));
        }
    }
    out
}

/// Hover in a Rust file: the descriptor on the name of a `sql!`, the type or
/// fragment under the cursor in the SQL.
fn hover_rust(
    text: &str,
    offset: usize,
    catalog: &ProjectCatalog,
    macros: &pgzest_expand::MacroSet,
) -> Option<String> {
    let found = super::rust_embed::find(text);
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
    if param_at(&e.sql, at).is_some() {
        let header = embedded_header(e);
        return hover_param(
            &format!("{header}{}", e.sql),
            header.len() + at,
            catalog,
            macros,
        );
    }
    let prepared = pgzest_analysis::prepare_query_source(&e.sql, macros);
    let stmts = prepared
        .parse_with_outcome()
        .map(|o| o.statements)
        .unwrap_or_default();
    hover_fragment(&prepared, macros, at).or_else(|| {
        let word = word_at_offset(&e.sql, at)?;
        hover_query(&e.sql, &stmts, catalog, at, &word)
    })
}

/// Hover of a `@fragment`: where it comes from, its documentation, what it
/// inserts here.
fn hover_fragment(
    prepared: &pgzest_analysis::PreparedSource,
    macros: &pgzest_expand::MacroSet,
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

/// Hover of the `-- name: …` header of a named query: what its descriptor
/// describes (parameters, columns, cardinality), or why there is none.
fn hover_named_query(
    text: &str,
    offset: usize,
    catalog: &ProjectCatalog,
    macros: &pgzest_expand::MacroSet,
) -> Option<String> {
    let file = pgzest_query::parse_query_file_with(text, macros);
    let q = file.queries.iter().find(|q| {
        let line_start = text[..q.name_range.start].rfind('\n').map_or(0, |i| i + 1);
        let line_end = text[q.name_range.end..]
            .find('\n')
            .map_or(text.len(), |i| q.name_range.end + i);
        (line_start..=line_end).contains(&offset)
    })?;
    let d = match pgzest_query::describe_named(catalog, q) {
        Ok(d) => d,
        Err(diags) => {
            let why: Vec<String> = diags
                .iter()
                .map(|d| format!("- `{}` {}", d.code, d.message))
                .collect();
            return Some(format!(
                "(query) **{}**: not described\n\n{}",
                q.name,
                why.join("\n")
            ));
        }
    };
    let null = |n: pgzest_query::descriptor::NullabilityDesc| match n {
        pgzest_query::descriptor::NullabilityDesc::NonNull => " NOT NULL",
        pgzest_query::descriptor::NullabilityDesc::Nullable => " NULL",
        pgzest_query::descriptor::NullabilityDesc::Unknown => "",
    };
    let mut out = format!("(query) **{}** — `{}`", d.name, d.cardinality.as_str());
    if !d.params.is_empty() {
        out.push_str("\n\nParameters:");
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
        out.push_str("\n\nColumns:");
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
    table: &pgzest_schema::provider::TableSchema,
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

fn var_type_to_sql(t: &pgzest_ast::plpgsql::VarType) -> String {
    use pgzest_ast::plpgsql::VarType;
    let path = |n: &pgzest_ast::ObjectName| {
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
    stmts: &[pgzest_ast::Statement],
    cursor_offset: usize,
    word: &str,
) -> Option<String> {
    use pgzest_ast::Statement;
    use pgzest_ast::ddl::FunctionBody;
    let lower = word.to_lowercase();

    for stmt in stmts {
        let (span, params, declare) = match stmt {
            Statement::Ddl(pgzest_ast::DdlStatement::CreateFunction(cf)) => {
                let decls = match &cf.body {
                    FunctionBody::Parsed(b) => Some(b.block.declarations.as_slice()),
                    _ => None,
                };
                (cf.span, cf.params.as_slice(), decls)
            }
            Statement::Ddl(pgzest_ast::DdlStatement::CreateProcedure(cp)) => {
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
                let pgzest_ast::plpgsql::Declaration::Variable(v) = decl else {
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

/// Chunks of the schema designated by `schemaFile`: the file, or the `.sql`
/// files of the directory (non-recursive), sorted by name.
fn legacy_chunks(path: &str) -> std::io::Result<Vec<pgzest_schema::SqlChunk>> {
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
            pgzest_schema::SchemaSource::chunks(&pgzest_schema::FileSource::new(
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

/// Canonical path if it exists, as is otherwise: common key for project
/// files and editor documents.
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
