use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use pgsql_diagnostics::{Diagnostic, Severity};
use pgsql_lint::config::LintConfig;
use pgsql_plugin_wasm::{WasmCheckRule, WasmPlugin};
use pgsql_schema::provider::SchemaProvider;
use pgsql_schema::{FileRole, ProjectConfig, SchemaResolver, check_ddl_role};
use tokio::sync::Mutex;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, jsonrpc::Result};

use pgsql_semantic::data_type_to_sql;

use crate::completion::{analyze_cursor, build_completions};
use crate::line_index::LineIndex;
use crate::semantic;

// ── Backend state ─────────────────────────────────────────────────────────────

pub struct PgsqlBackend {
    client: Client,
    documents: Mutex<HashMap<String, String>>,
    /// Shared schema resolver — rebuilt when schema/migration files change.
    resolver: Mutex<Arc<SchemaResolver>>,
    /// Project root and config (from pgsql-tools.config.toml).
    project_root: Mutex<Option<PathBuf>>,
    config: Mutex<Option<ProjectConfig>>,
    /// Fallback schema path from `schemaFile` init option (legacy, no config file).
    legacy_schema_path: Mutex<Option<String>>,
    /// WASM plugins loaded from plugins.wasm in pgsql-tools.config.toml.
    wasm_plugins: Mutex<Vec<Arc<WasmPlugin>>>,
    /// Macros reconnues : celles fournies, plus les `*.pgmacro` du projet.
    ///
    /// Reconstruit avec le resolver : un `.pgmacro` modifié doit prendre effet
    /// sans redémarrer le serveur.
    macros: Mutex<Arc<pgsql_macro::MacroSet>>,
}

impl PgsqlBackend {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            documents: Mutex::new(HashMap::new()),
            resolver: Mutex::new(Arc::new(SchemaResolver::empty())),
            project_root: Mutex::new(None),
            config: Mutex::new(None),
            legacy_schema_path: Mutex::new(None),
            wasm_plugins: Mutex::new(Vec::new()),
            macros: Mutex::new(Arc::new(pgsql_macro::MacroSet::builtin())),
        }
    }

    /// Jeu de macros courant (clone d'Arc, bon marché).
    async fn macros(&self) -> Arc<pgsql_macro::MacroSet> {
        self.macros.lock().await.clone()
    }

    /// Recharge les macros du projet par-dessus les macros fournies.
    async fn rebuild_macros(&self) {
        let mut set = pgsql_macro::MacroSet::builtin();
        if let Some(root) = self.project_root.lock().await.clone() {
            let dir = match ProjectConfig::load(&root.join(ProjectConfig::FILENAME)) {
                Ok(cfg) => root.join(&cfg.macros.directory),
                Err(_) => root.join("macros"),
            };
            if dir.is_dir() {
                for e in set.load_dir(&dir) {
                    self.client
                        .log_message(MessageType::WARNING, format!("pgsql-lsp: {e}"))
                        .await;
                }
            }
        }
        *self.macros.lock().await = Arc::new(set);
    }

    /// Clone the current resolver out of the lock (cheap — Arc clone).
    async fn resolver(&self) -> Arc<SchemaResolver> {
        self.resolver.lock().await.clone()
    }

    /// Determine the `FileRole` of a URI.
    async fn role_of(&self, uri: &str) -> FileRole {
        let Some(path) = uri_to_path(uri) else {
            return FileRole::Query;
        };

        // Config-driven classification (authoritative when pgsql-tools.config.toml present).
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

    /// Rebuild the resolver from the project config (or legacy path).
    async fn rebuild_resolver(&self) {
        let root = self.project_root.lock().await.clone();

        let new_resolver = if let Some(ref root) = root {
            let config_path = root.join(ProjectConfig::FILENAME);
            match ProjectConfig::load(&config_path) {
                Ok(config) => {
                    // Macros du projet chargées d'abord : l'index doit voir les
                    // objets déclarés par les macros maison, pas seulement par
                    // celles fournies.
                    self.rebuild_macros().await;
                    let macros = self.macros().await;
                    let source = config.to_source(root);
                    SchemaResolver::from_chunks_with_macros(
                        &pgsql_schema::SchemaSource::chunks(&source),
                        &macros,
                    )
                }
                Err(e) => {
                    self.client
                        .log_message(
                            MessageType::WARNING,
                            format!("pgsql-lsp: failed to reload config: {e}"),
                        )
                        .await;
                    return;
                }
            }
        } else if let Some(ref path) = *self.legacy_schema_path.lock().await {
            match load_schema_sql(path) {
                Ok(sql) => {
                    SchemaResolver::from_sql(&sql).unwrap_or_else(|_| SchemaResolver::empty())
                }
                Err(_) => return,
            }
        } else {
            return;
        };

        *self.resolver.lock().await = Arc::new(new_resolver);
        self.rebuild_macros().await;
    }

    /// Build a Checker with built-in rules + any loaded WASM plugins.
    async fn make_checker(&self) -> pgsql_checker::Checker {
        let mut checker = pgsql_checker::Checker::with_builtin_rules();
        for plugin in self.wasm_plugins.lock().await.iter() {
            checker.add_rule(Box::new(WasmCheckRule::new(plugin.clone())));
        }
        checker
    }

    /// Type-check des statements déjà parsés contre `schema`.
    async fn check_stmts(
        &self,
        stmts: &[pgsql_ast::Statement],
        schema: &dyn SchemaProvider,
    ) -> Vec<Diagnostic> {
        self.make_checker().await.run(stmts, schema)
    }

    /// Parse, type-check, lint et publie les diagnostics de `uri`.
    ///
    /// Doit produire exactement ce que produit `pgsql check` sur le même
    /// fichier : même politique de tolérance du parser, mêmes directives en
    /// commentaire, mêmes surcharges de sévérité. Toute divergence ici se
    /// traduit par un fichier vert dans l'éditeur et rouge en CI.
    async fn run_diagnostics(&self, uri: &str, text: &str) {
        let line_index = LineIndex::new(text);
        let role = self.role_of(uri).await;
        let resolver = self.resolver().await;
        let suppressions = pgsql_diagnostics::Suppressions::scan(text);

        // Désucrage des macros. Une source sans macro est recopiée à
        // l'identique, donc c'est gratuit et aucun fichier n'y échappe à cause
        // de son extension. Les spans produits ensuite sont déjà dans le repère
        // du fichier édité — voir `PreparedSource::parse`.
        let macros = self.macros().await;
        let prepared = pgsql_analysis::prepare_source(text, &macros);

        // Un seul parse pour le checker, le lint et les règles de rôle.
        let outcome = match prepared.parse_with_outcome() {
            Ok(o) => o,
            Err(e) => {
                let mut diags = vec![Diagnostic::error(
                    pgsql_diagnostics::codes::PARSE_ERROR,
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
                Some(c) => (c.parse.clone(), c.lint.rules.clone()),
                None => (Default::default(), Default::default()),
            }
        };

        // ── Macros et statements écartés ──────────────────────────────────────
        let mut diags = prepared.diagnostics.clone();
        diags.extend(pgsql_analysis::skipped_to_diagnostics(
            &outcome.skipped,
            &parse_cfg,
        ));

        // ── Checker ───────────────────────────────────────────────────────────
        let checker_diags: Vec<Diagnostic> = match role {
            FileRole::Schema => {
                // Schema file: 2-pass — check against self-schema merged with global.
                let mut composite = pgsql_schema::provider::CompositeSchemaProvider::new();
                if let Ok(self_schema) = SchemaResolver::from_sql(&prepared.sql) {
                    composite.add(Box::new(self_schema));
                }
                composite.add(Box::new(GlobalResolver(resolver.clone())));

                self.check_stmts(&stmts, &composite)
                    .await
                    .into_iter()
                    .filter(|d| !is_suppressable_schema_error(d))
                    .collect()
            }
            FileRole::Migration { .. } | FileRole::Query => {
                self.check_stmts(&stmts, resolver.as_ref()).await
            }
        };
        diags.extend(checker_diags);

        // ── Lint + règles de rôle ─────────────────────────────────────────────
        diags.extend(pgsql_lint::lint_statements(&stmts, &LintConfig::default()).diagnostics);
        diags.extend(check_ddl_role(&stmts, &role));

        // Les surcharges s'appliquent à tout, checker comme lint : une règle
        // mise à "off" dans la config doit être muette d'où qu'elle vienne.
        pgsql_diagnostics::apply_rule_overrides(&mut diags, &rule_overrides);

        // Directives en commentaire, en dernier : elles ont le dernier mot.
        suppressions.apply(&mut diags);

        self.publish(uri, &diags, &line_index).await;
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
            source: Some("pgsql-lsp".to_string()),
            message: d.message.clone(),
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
        // 1. Try to discover pgsql-tools.toml from the workspace root.
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

        if let Some(ref root) = workspace_root
            && let Some((project_root, config)) = ProjectConfig::find_and_load(root)
        {
            *self.project_root.lock().await = Some(project_root.clone());
            // Macros du projet d'abord : l'index doit voir les objets déclarés
            // par les macros maison, pas seulement par celles fournies.
            self.rebuild_macros().await;
            let macros = self.macros().await;
            let source = config.to_source(&project_root);
            let resolver = SchemaResolver::from_chunks_with_macros(
                &pgsql_schema::SchemaSource::chunks(&source),
                &macros,
            );
            *self.resolver.lock().await = Arc::new(resolver);

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
                                    "pgsql-lsp: loaded wasm plugin '{}' from {}",
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
                                    "pgsql-lsp: failed to load wasm plugin '{}': {e}",
                                    abs.display()
                                ),
                            )
                            .await;
                    }
                }
            }
            *self.wasm_plugins.lock().await = loaded;

            *self.config.lock().await = Some(config);
            self.client
                .log_message(
                    MessageType::INFO,
                    format!(
                        "pgsql-lsp: loaded schema from {}",
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
            match load_schema_sql(path) {
                Ok(sql) => {
                    if let Ok(r) = SchemaResolver::from_sql(&sql) {
                        *self.resolver.lock().await = Arc::new(r);
                    }
                }
                Err(e) => {
                    self.client
                        .log_message(
                            MessageType::WARNING,
                            format!("pgsql-lsp: could not load schema at {path}: {e}"),
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
                name: "pgsql-lsp".to_string(),
                version: Some("0.1.0".to_string()),
            }),
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "pgsql-lsp started")
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

            // Rebuild schema if a schema/migration file changed.
            let role = self.role_of(&uri).await;
            if matches!(role, FileRole::Schema | FileRole::Migration { .. }) {
                self.rebuild_resolver().await;
            }

            self.run_diagnostics(&uri, &text).await;
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        self.documents
            .lock()
            .await
            .remove(&params.text_document.uri.to_string());
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
        let word = match word_at_offset(&text, offset as usize) {
            Some(w) => w,
            None => return Ok(None),
        };

        let role = self.role_of(&uri).await;
        let resolver = self.resolver().await;

        // Parse once — needed for both PL/pgSQL var lookup and query hover.
        let stmts = pgsql_parser::parse(&text).unwrap_or_default();

        let hover_text: Option<String> =
            // PL/pgSQL declared variables / parameters take priority in any file type.
            hover_plpgsql_var(&stmts, offset as usize, &word)
            .or_else(|| {
                if matches!(role, FileRole::Schema) {
                    hover_schema_file(&text, &word, resolver.as_ref())
                } else {
                    hover_query(&stmts, resolver.as_ref(), &word)
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

// ── Cross-schema error filter ─────────────────────────────────────────────────

fn is_suppressable_schema_error(d: &Diagnostic) -> bool {
    if d.code == pgsql_diagnostics::codes::TABLE_NOT_FOUND
        && let Some(start) = d.message.find('\'')
    {
        let rest = &d.message[start + 1..];
        if let Some(end) = rest.find('\'') {
            let table = &rest[..end];
            if let Some(dot) = table.find('.')
                && &table[..dot] != "public"
            {
                return true;
            }
        }
    }
    false
}

// ── Hover helpers ─────────────────────────────────────────────────────────────

fn hover_schema_file(text: &str, word: &str, global: &dyn SchemaProvider) -> Option<String> {
    // Build self-schema first; fall back to global for cross-file references.
    let self_schema = SchemaResolver::from_sql(text).ok()?;
    let provider: &dyn SchemaProvider = &self_schema;

    lookup_hover(word, provider).or_else(|| lookup_hover(word, global))
}

fn hover_query(
    stmts: &[pgsql_ast::Statement],
    schema: &dyn SchemaProvider,
    word: &str,
) -> Option<String> {
    let semantic = pgsql_semantic::analyze(stmts, schema);

    // Column in any scope
    for scope_id in 0..100u32 {
        let sid = pgsql_semantic::ScopeId(scope_id);
        let cols = semantic.scopes.available_columns(sid);
        if cols.is_empty() {
            break;
        }
        for col in &cols {
            if col.name.to_lowercase() == word {
                return Some(format!(
                    "(column) **{}**: `{}`",
                    col.name,
                    data_type_to_sql(&col.data_type)
                ));
            }
        }
    }

    // Table name via symbols
    if let Some(sym) = semantic.symbols.get_table(word) {
        if let Some(table) = schema.get_table(&sym.name) {
            return Some(fmt_table_hover(&table.name, word, table));
        }
        return Some(format!("(table) **{}**", sym.name));
    }

    // Output columns (SELECT aliases)
    for cols in semantic.select_columns.values() {
        for col in cols {
            if col.name.as_deref().map(|n| n.to_lowercase()).as_deref() == Some(word) {
                return Some(format!(
                    "(column) **{}**: `{}`",
                    col.name.as_deref().unwrap_or(word),
                    data_type_to_sql(&col.data_type)
                ));
            }
        }
    }

    // Direct schema lookup (table / column)
    lookup_hover(word, schema)
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
    table: &pgsql_schema::provider::TableSchema,
) -> String {
    let header = fmt_qualified_bold(qualified_name, word);
    let cols: Vec<String> = table
        .columns
        .iter()
        .map(|c| {
            let nn = if c.nullable { "" } else { " NOT NULL" };
            format!("**{}** `{}`{}", c.name, data_type_to_sql(&c.data_type), nn)
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
                .map(|dt| format!("`{}`", data_type_to_sql(dt)))
                .collect::<Vec<_>>()
                .join(", ");
            return Some(format!(
                "(function) **{}**({}): `{}`",
                sig.name,
                params_str,
                data_type_to_sql(&sig.return_type)
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
                data_type_to_sql(&col.data_type),
                nn
            ));
        }
    }

    None
}

/// Check if `word` (lowercased) is a declared variable or parameter in any
/// function/procedure whose span contains `cursor_offset`.
fn hover_plpgsql_var(
    stmts: &[pgsql_ast::Statement],
    cursor_offset: usize,
    word: &str,
) -> Option<String> {
    use pgsql_ast::Statement;
    use pgsql_ast::ddl::FunctionBody;
    let lower = word.to_lowercase();

    for stmt in stmts {
        let (span, params, declare) = match stmt {
            Statement::Ddl(pgsql_ast::DdlStatement::CreateFunction(cf)) => {
                let decls = match &cf.body {
                    FunctionBody::Parsed(b) => Some(b.declare.as_slice()),
                    _ => None,
                };
                (cf.span, cf.params.as_slice(), decls)
            }
            Statement::Ddl(pgsql_ast::DdlStatement::CreateProcedure(cp)) => {
                let decls = match &cp.body {
                    FunctionBody::Parsed(b) => Some(b.declare.as_slice()),
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
                if decl.name.value.to_lowercase() == lower {
                    return Some(format!(
                        "(variable) **{}**: `{}`",
                        decl.name.value,
                        data_type_to_sql(&decl.data_type)
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
                    data_type_to_sql(&param.data_type)
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

// ── Newtype: let Arc<SchemaResolver> work as a SchemaProvider ─────────────────

/// Thin wrapper so we can pass Arc<SchemaResolver> where &dyn SchemaProvider is needed
/// without cloning the entire map.
struct GlobalResolver(Arc<SchemaResolver>);

impl SchemaProvider for GlobalResolver {
    fn get_table(&self, name: &str) -> Option<&pgsql_schema::provider::TableSchema> {
        self.0.get_table(name)
    }
    fn get_function(&self, name: &str) -> Option<&pgsql_schema::provider::FunctionSignature> {
        self.0.get_function(name)
    }
    fn all_table_names(&self) -> Vec<&str> {
        self.0.all_table_names()
    }
}

// ── Schema loading (legacy schemaFile path) ───────────────────────────────────

fn load_schema_sql(path: &str) -> std::io::Result<String> {
    let p = Path::new(path);
    if p.is_file() {
        return std::fs::read_to_string(p);
    }
    if p.is_dir() {
        let mut entries: Vec<_> = std::fs::read_dir(p)?
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path()
                    .extension()
                    .and_then(|s| s.to_str())
                    .map(|ext| ext.eq_ignore_ascii_case("sql"))
                    .unwrap_or(false)
            })
            .collect();
        entries.sort_by_key(|e| e.file_name());
        let parts: Vec<String> = entries
            .into_iter()
            .filter_map(|e| std::fs::read_to_string(e.path()).ok())
            .collect();
        return Ok(parts.join("\n"));
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!("{path} is neither a file nor a directory"),
    ))
}

// ── URI / path helpers ────────────────────────────────────────────────────────

fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let path_str = uri.strip_prefix("file://").unwrap_or(uri);
    let p = PathBuf::from(path_str);
    p.canonicalize().ok().or(Some(p))
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
