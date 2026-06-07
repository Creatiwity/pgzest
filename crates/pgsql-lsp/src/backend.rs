use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use pgsql_checker::diagnostic::{Diagnostic, Severity};
use pgsql_checker::schema::SchemaProvider;
use pgsql_lint::config::LintConfig;
use pgsql_schema::{check_ddl_role, FileRole, ProjectConfig, SchemaResolver};
use tokio::sync::Mutex;
use tower_lsp::lsp_types::*;
use tower_lsp::{jsonrpc::Result, Client, LanguageServer};

use pgsql_semantic::data_type_to_sql;

use crate::completion::{analyze_cursor, build_completions};
use crate::line_index::LineIndex;
use crate::semantic;

// ── Backend state ─────────────────────────────────────────────────────────────

pub struct PgsqlBackend {
    client: Client,
    documents: Mutex<HashMap<String, String>>,
    /// Shared schema resolver — rebuilt when schema/migration files change.
    /// Wrapped in Arc so it can be cloned out of the lock quickly.
    resolver: Mutex<Arc<SchemaResolver>>,
    /// Project root where pgsql-tools.toml was found (if any).
    project_root: Mutex<Option<PathBuf>>,
    /// Fallback schema path from `schemaFile` init option (legacy).
    legacy_schema_path: Mutex<Option<String>>,
}

impl PgsqlBackend {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            documents: Mutex::new(HashMap::new()),
            resolver: Mutex::new(Arc::new(SchemaResolver::empty())),
            project_root: Mutex::new(None),
            legacy_schema_path: Mutex::new(None),
        }
    }

    /// Clone the current resolver out of the lock (cheap — Arc clone).
    async fn resolver(&self) -> Arc<SchemaResolver> {
        self.resolver.lock().await.clone()
    }

    /// Determine the `FileRole` of a URI.
    async fn role_of(&self, uri: &str) -> FileRole {
        let path = uri_to_path(uri);
        let resolver = self.resolver().await;

        // Check the index first (files known to the schema source).
        if let Some(role) = path
            .as_deref()
            .and_then(|p| resolver.index.role_of_path(p))
        {
            return role.clone();
        }

        // Fall back to heuristic detection from filename.
        if let Some(p) = &path {
            let detected = FileRole::detect(p);

            // Legacy: if a `schemaFile` path was configured, files inside it are Schema.
            if let Some(schema_path) = self.legacy_schema_path.lock().await.as_deref() {
                if is_inside(p, schema_path) {
                    return FileRole::Schema;
                }
            }

            return detected;
        }

        FileRole::Query
    }

    /// Rebuild the resolver from the project config (or legacy path).
    async fn rebuild_resolver(&self) {
        let root = self.project_root.lock().await.clone();

        let new_resolver = if let Some(ref root) = root {
            let config_path = root.join(ProjectConfig::FILENAME);
            match ProjectConfig::load(&config_path) {
                Ok(config) => {
                    let source = config.to_source(root);
                    SchemaResolver::from_source(&source)
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
                Ok(sql) => SchemaResolver::from_sql(&sql).unwrap_or_else(|_| SchemaResolver::empty()),
                Err(_) => return,
            }
        } else {
            return;
        };

        *self.resolver.lock().await = Arc::new(new_resolver);
    }

    /// Parse, type-check, lint and publish diagnostics for `uri`.
    async fn run_diagnostics(&self, uri: &str, text: &str) {
        let line_index = LineIndex::new(text);
        let role = self.role_of(uri).await;
        let resolver = self.resolver().await;

        // ── Checker diagnostics ───────────────────────────────────────────────
        let checker_diags: Vec<Diagnostic> = match role {
            FileRole::Schema => {
                // Schema file: 2-pass — check against self-schema merged with global.
                let mut composite = pgsql_checker::CompositeSchemaProvider::new();
                if let Ok(self_schema) = SchemaResolver::from_sql(text) {
                    composite.add(Box::new(self_schema));
                }
                composite.add(Box::new(GlobalResolver(resolver.clone())));

                pgsql_checker::check_sql(text, &composite)
                    .into_iter()
                    .filter(|d| !is_suppressable_schema_error(d))
                    .collect()
            }
            FileRole::Migration { .. } | FileRole::Query => {
                pgsql_checker::check_sql(text, resolver.as_ref())
            }
        };

        // ── Lint diagnostics ──────────────────────────────────────────────────
        let (lint_diags, role_diags) = match pgsql_parser::parse(text) {
            Ok(stmts) => {
                let lint = pgsql_lint::lint_statements(&stmts, &LintConfig::default()).diagnostics;
                let role_lint = check_ddl_role(&stmts, &role);
                (lint, role_lint)
            }
            Err(_) => (vec![], vec![]),
        };

        // ── Publish ───────────────────────────────────────────────────────────
        let lsp_diags: Vec<tower_lsp::lsp_types::Diagnostic> = checker_diags
            .iter()
            .chain(lint_diags.iter())
            .chain(role_diags.iter())
            .map(|d| Self::to_lsp_diagnostic(d, &line_index))
            .collect();

        if let Ok(url) = uri.parse::<Url>() {
            self.client.publish_diagnostics(url, lsp_diags, None).await;
        }
    }

    fn to_lsp_diagnostic(d: &Diagnostic, idx: &LineIndex) -> tower_lsp::lsp_types::Diagnostic {
        let severity = match d.severity {
            Severity::Error   => DiagnosticSeverity::ERROR,
            Severity::Warning => DiagnosticSeverity::WARNING,
            Severity::Info    => DiagnosticSeverity::INFORMATION,
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
                start: Position { line: start_line, character: start_char },
                end:   Position { line: end_line,   character: end_char },
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
            .and_then(|f| url_to_path(&f.uri.to_string()))
            .or_else(|| {
                params
                    .root_uri
                    .as_ref()
                    .and_then(|u| url_to_path(&u.to_string()))
            });

        if let Some(ref root) = workspace_root {
            if let Some((project_root, config)) = ProjectConfig::find_and_load(root) {
                let source = config.to_source(&project_root);
                let resolver = SchemaResolver::from_source(&source);
                *self.resolver.lock().await = Arc::new(resolver);
                *self.project_root.lock().await = Some(project_root.clone());
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
        }

        // 2. Legacy fallback: schemaFile initialization option.
        if self.project_root.lock().await.is_none() {
            if let Some(opts) = &params.initialization_options {
                if let Some(path) = opts.get("schemaFile").and_then(|v| v.as_str()) {
                    if !path.is_empty() {
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
        self.documents.lock().await.insert(uri.clone(), text.clone());
        self.run_diagnostics(&uri, &text).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        if let Some(change) = params.content_changes.into_iter().last() {
            let uri = params.text_document.uri.to_string();
            let text = change.text;
            self.documents.lock().await.insert(uri.clone(), text.clone());

            // Rebuild schema if a schema/migration file changed.
            let role = self.role_of(&uri).await;
            if matches!(role, FileRole::Schema | FileRole::Migration { .. }) {
                self.rebuild_resolver().await;
            }

            self.run_diagnostics(&uri, &text).await;
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        self.documents.lock().await.remove(&params.text_document.uri.to_string());
        self.client
            .publish_diagnostics(params.text_document.uri, vec![], None)
            .await;
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = params.text_document_position_params.text_document.uri.to_string();
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

        let hover_text: Option<String> = if matches!(role, FileRole::Schema) {
            hover_schema_file(&text, &word, resolver.as_ref())
        } else {
            let stmts = match pgsql_parser::parse(&text) {
                Ok(s) => s,
                Err(_) => return Ok(None),
            };
            hover_query(&stmts, resolver.as_ref(), &word)
        };

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
    if d.code == "E003" {
        if let Some(start) = d.message.find('\'') {
            let rest = &d.message[start + 1..];
            if let Some(end) = rest.find('\'') {
                let table = &rest[..end];
                if let Some(dot) = table.find('.') {
                    if &table[..dot] != "public" {
                        return true;
                    }
                }
            }
        }
    }
    // E002 inside PL/pgSQL function bodies — text extractor false positives.
    // TODO: remove once proper PlpgsqlContext analysis is in place.
    if d.code == "E002" && d.message.contains("[in ") {
        return true;
    }
    false
}

// ── Hover helpers ─────────────────────────────────────────────────────────────

fn hover_schema_file(text: &str, word: &str, global: &dyn SchemaProvider) -> Option<String> {
    // Build self-schema first; fall back to global for cross-file references.
    let self_schema = SchemaResolver::from_sql(text).ok()?;
    let provider: &dyn SchemaProvider = &self_schema;

    let result = lookup_hover(word, provider).or_else(|| lookup_hover(word, global));
    result
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
        if cols.is_empty() { break; }
        for col in &cols {
            if col.name.to_lowercase() == word {
                return Some(format!(
                    "```sql\n-- {} : {}\n```",
                    col.name,
                    data_type_to_sql(&col.data_type)
                ));
            }
        }
    }

    // Table name via symbols
    if let Some(sym) = semantic.symbols.get_table(word) {
        if let Some(table) = schema.get_table(&sym.name) {
            let cols: Vec<String> = table
                .columns
                .iter()
                .map(|c| format!("  {}: {}", c.name, data_type_to_sql(&c.data_type)))
                .collect();
            return Some(format!(
                "```sql\n-- table: {}\n{}\n```",
                table.name,
                cols.join("\n")
            ));
        }
        return Some(format!("```sql\n-- table: {}\n```", sym.name));
    }

    // Output columns (SELECT aliases)
    for cols in semantic.select_columns.values() {
        for col in cols {
            if col.name.as_deref().map(|n| n.to_lowercase()).as_deref() == Some(word) {
                return Some(format!(
                    "```sql\n-- {} : {}\n```",
                    col.name.as_deref().unwrap_or(word),
                    data_type_to_sql(&col.data_type)
                ));
            }
        }
    }

    // Direct schema lookup (table / column)
    lookup_hover(word, schema)
}

fn lookup_hover(word: &str, schema: &dyn SchemaProvider) -> Option<String> {
    // Table name?
    if let Some(table) = schema.get_table(word) {
        let cols: Vec<String> = table
            .columns
            .iter()
            .map(|c| {
                let nn = if c.nullable { "" } else { " NOT NULL" };
                format!("  {}: {}{}", c.name, data_type_to_sql(&c.data_type), nn)
            })
            .collect();
        return Some(format!(
            "```sql\n-- table: {}\n{}\n```",
            table.name,
            cols.join("\n")
        ));
    }

    // Column name anywhere
    let lower = word.to_lowercase();
    for table_name in schema.all_table_names() {
        if let Some(table) = schema.get_table(table_name) {
            if let Some(col) = table.get_column(&lower) {
                let nn = if col.nullable { "" } else { " NOT NULL" };
                return Some(format!(
                    "```sql\n-- {}.{} : {}{}\n```",
                    table.name, col.name,
                    data_type_to_sql(&col.data_type), nn
                ));
            }
        }
    }

    None
}

// ── Newtype: let Arc<SchemaResolver> work as a SchemaProvider ─────────────────

/// Thin wrapper so we can pass Arc<SchemaResolver> where &dyn SchemaProvider is needed
/// without cloning the entire map.
struct GlobalResolver(Arc<SchemaResolver>);

impl SchemaProvider for GlobalResolver {
    fn get_table(&self, name: &str) -> Option<&pgsql_checker::schema::TableSchema> {
        self.0.get_table(name)
    }
    fn get_function(&self, name: &str) -> Option<&pgsql_checker::schema::FunctionSignature> {
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
    let path_str = uri
        .strip_prefix("file://")
        .unwrap_or(uri);
    let p = PathBuf::from(path_str);
    p.canonicalize().ok().or(Some(p))
}

fn url_to_path(url: &str) -> Option<PathBuf> {
    uri_to_path(url)
}

fn is_inside(child: &Path, parent_str: &str) -> bool {
    let parent = Path::new(parent_str);
    let parent_canon = parent.canonicalize().unwrap_or_else(|_| parent.to_path_buf());
    let child_canon = child.canonicalize().unwrap_or_else(|_| child.to_path_buf());
    child_canon == parent_canon
        || child_canon.starts_with(format!("{}/", parent_canon.display()))
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
    if start == end { None } else { Some(text[start..end].to_lowercase()) }
}
