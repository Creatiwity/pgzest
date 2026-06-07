use std::collections::HashMap;

use pgsql_checker::diagnostic::{Diagnostic, Severity};
use pgsql_checker::schema::{DdlSchemaProvider, SchemaProvider};
use pgsql_lint::config::LintConfig;
use tokio::sync::Mutex;
use tower_lsp::lsp_types::*;
use tower_lsp::{jsonrpc::Result, Client, LanguageServer};

use crate::completion::{analyze_cursor, build_completions};
use crate::line_index::LineIndex;

pub struct PgsqlBackend {
    client: Client,
    /// URI → document text
    documents: Mutex<HashMap<String, String>>,
    /// Optional schema SQL loaded from the workspace (e.g. a schema.sql file)
    schema_sql: Mutex<Option<String>>,
    /// Canonical path of the configured schemaFile (file or directory).
    /// Files inside this path are schema definitions and are not type-checked
    /// as query files — opening them produces no diagnostics.
    schema_path: Mutex<Option<String>>,
}

impl PgsqlBackend {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            documents: Mutex::new(HashMap::new()),
            schema_sql: Mutex::new(None),
            schema_path: Mutex::new(None),
        }
    }

    /// Returns true if `file_uri` is the configured schema file / directory,
    /// or lives inside it. Such files are pure DDL definitions and should not
    /// be type-checked as query files.
    async fn is_schema_file(&self, file_uri: &str) -> bool {
        let guard = self.schema_path.lock().await;
        let schema_path = match guard.as_deref() {
            Some(p) if !p.is_empty() => p,
            _ => return false,
        };
        // Strip the "file://" prefix that LSP URIs use
        let file_path = file_uri.strip_prefix("file://").unwrap_or(file_uri);
        let schema_canon = std::path::Path::new(schema_path)
            .canonicalize()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| schema_path.to_string());
        let file_canon = std::path::Path::new(file_path)
            .canonicalize()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| file_path.to_string());
        file_canon == schema_canon
            || file_canon.starts_with(&format!("{}/", schema_canon))
    }

    /// Parse, type-check and lint `text`, then publish LSP diagnostics for `uri`.
    async fn run_diagnostics(&self, uri: &str, text: &str) {
        // Schema definition files are not query files — skip diagnostics entirely.
        if self.is_schema_file(uri).await {
            if let Ok(url) = uri.parse::<Url>() {
                self.client.publish_diagnostics(url, vec![], None).await;
            }
            return;
        }

        let line_index = LineIndex::new(text);

        let schema_guard = self.schema_sql.lock().await;
        let schema: Box<dyn SchemaProvider> = match schema_guard.as_deref() {
            Some(sql) => match DdlSchemaProvider::from_sql(sql) {
                Ok(p) => Box::new(p),
                Err(_) => Box::new(EmptySchemaProvider),
            },
            None => Box::new(EmptySchemaProvider),
        };
        drop(schema_guard);

        // --- checker diagnostics ---
        let checker_diags = pgsql_checker::check_sql(text, schema.as_ref());

        // --- lint diagnostics (parse once, then lint) ---
        let lint_diags: Vec<Diagnostic> = match pgsql_parser::parse(text) {
            Ok(stmts) => {
                let config = LintConfig::default();
                pgsql_lint::lint_statements(&stmts, &config).diagnostics
            }
            Err(_) => vec![],
        };

        // Combine and convert to LSP type
        let lsp_diags: Vec<tower_lsp::lsp_types::Diagnostic> = checker_diags
            .iter()
            .chain(lint_diags.iter())
            .map(|d| Self::to_lsp_diagnostic_with_index(d, &line_index))
            .collect();

        if let Ok(url) = uri.parse::<Url>() {
            self.client
                .publish_diagnostics(url, lsp_diags, None)
                .await;
        }
    }

    /// Convert a checker/lint `Diagnostic` to the LSP `Diagnostic` type using a LineIndex.
    fn to_lsp_diagnostic_with_index(
        d: &Diagnostic,
        idx: &LineIndex,
    ) -> tower_lsp::lsp_types::Diagnostic {
        let severity = match d.severity {
            Severity::Error => DiagnosticSeverity::ERROR,
            Severity::Warning => DiagnosticSeverity::WARNING,
            Severity::Info => DiagnosticSeverity::INFORMATION,
        };

        // Use span if present and non-zero; otherwise emit at (0,0).
        // Infrastructure is ready for Phase E when parser emits real spans.
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

        let range = Range {
            start: Position {
                line: start_line,
                character: start_char,
            },
            end: Position {
                line: end_line,
                character: end_char,
            },
        };

        tower_lsp::lsp_types::Diagnostic {
            range,
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

// ── Schema loading ────────────────────────────────────────────────────────────

/// Load schema SQL from a file path or a directory.
/// If the path is a directory, all `.sql` files are read in lexicographic order
/// and concatenated.
fn load_schema_from_path(path: &str) -> std::io::Result<String> {
    let p = std::path::Path::new(path);
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
        let mut parts = Vec::with_capacity(entries.len());
        for entry in entries {
            parts.push(std::fs::read_to_string(entry.path())?);
        }
        return Ok(parts.join("\n"));
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!("{path} is neither a file nor a directory"),
    ))
}

// ── Empty schema provider (no schema loaded) ─────────────────────────────────

struct EmptySchemaProvider;

impl SchemaProvider for EmptySchemaProvider {
    fn get_table(&self, _name: &str) -> Option<&pgsql_checker::schema::TableSchema> {
        None
    }

    fn get_function(&self, _name: &str) -> Option<&pgsql_checker::schema::FunctionSignature> {
        None
    }

    fn all_table_names(&self) -> Vec<&str> {
        vec![]
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Convert LSP (line, character) position to a byte offset in `text`.
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

/// Extract the SQL identifier (word) at a byte offset in `text`.
/// Returns the word lowercased, or None if cursor is on whitespace/punctuation.
fn word_at_offset(text: &str, offset: usize) -> Option<String> {
    let bytes = text.as_bytes();
    if offset >= bytes.len() {
        return None;
    }

    // Find start of word (scan backward while alphanumeric or underscore)
    let mut start = offset;
    while start > 0 {
        let b = bytes[start - 1];
        if b.is_ascii_alphanumeric() || b == b'_' {
            start -= 1;
        } else {
            break;
        }
    }
    // Find end of word (scan forward)
    let mut end = offset;
    while end < bytes.len() {
        let b = bytes[end];
        if b.is_ascii_alphanumeric() || b == b'_' {
            end += 1;
        } else {
            break;
        }
    }
    if start == end {
        return None;
    }
    Some(text[start..end].to_lowercase())
}

// ── LanguageServer implementation ─────────────────────────────────────────────

#[tower_lsp::async_trait]
impl LanguageServer for PgsqlBackend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        if let Some(opts) = params.initialization_options {
            if let Some(path) = opts.get("schemaFile").and_then(|v| v.as_str()) {
                if !path.is_empty() {
                    // Remember the path so we can skip diagnostics on schema files.
                    *self.schema_path.lock().await = Some(path.to_string());
                    match load_schema_from_path(path) {
                        Ok(sql) => *self.schema_sql.lock().await = Some(sql),
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
            self.run_diagnostics(&uri, &text).await;
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri.to_string();
        self.documents.lock().await.remove(&uri);
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

        // Convert LSP (line, char) back to byte offset
        let offset = position_to_offset(&text, pos.line, pos.character);
        let word = match word_at_offset(&text, offset as usize) {
            Some(w) => w,
            None => return Ok(None),
        };

        // Run semantic analysis to get type info
        let schema_guard = self.schema_sql.lock().await;
        let schema: Box<dyn SchemaProvider> = match schema_guard.as_deref() {
            Some(sql) => match DdlSchemaProvider::from_sql(sql) {
                Ok(p) => Box::new(p),
                Err(_) => Box::new(EmptySchemaProvider),
            },
            None => Box::new(EmptySchemaProvider),
        };
        drop(schema_guard);

        let stmts = match pgsql_parser::parse(&text) {
            Ok(s) => s,
            Err(_) => return Ok(None),
        };

        let semantic = pgsql_semantic::analyze(&stmts, schema.as_ref());

        let mut hover_text: Option<String> = None;

        // Search in all scopes for a matching column
        for scope_id in 0..100u32 {
            let sid = pgsql_semantic::ScopeId(scope_id);
            let cols = semantic.scopes.available_columns(sid);
            if cols.is_empty() {
                break;
            }
            for col in cols {
                if col.name.to_lowercase() == word {
                    let ts_type = pgsql_semantic::data_type_to_ts(&col.data_type);
                    hover_text = Some(format!(
                        "```sql\n-- {} : {}\n```",
                        col.name, ts_type
                    ));
                    break;
                }
            }
            if hover_text.is_some() {
                break;
            }
        }

        // If not a column, check if it's a table name
        if hover_text.is_none() {
            if let Some(sym) = semantic.symbols.get_table(&word) {
                hover_text = Some(format!("```sql\n-- table: {}\n```", sym.name));
            }
        }

        // If not found in semantic, check output columns
        if hover_text.is_none() {
            'outer: for cols in semantic.select_columns.values() {
                for col in cols {
                    if col.name.as_deref().map(|n| n.to_lowercase()).as_deref() == Some(&word as &str) {
                        let ts_type = col.ts_type();
                        hover_text = Some(format!(
                            "```sql\n-- {} : {}\n```",
                            col.name.as_deref().unwrap_or(&word),
                            ts_type
                        ));
                        break 'outer;
                    }
                }
            }
        }

        Ok(hover_text.map(|text| Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: text,
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

        let schema_guard = self.schema_sql.lock().await;
        let schema: Box<dyn SchemaProvider> = match schema_guard.as_deref() {
            Some(sql) => match DdlSchemaProvider::from_sql(sql) {
                Ok(p) => Box::new(p),
                Err(_) => Box::new(EmptySchemaProvider),
            },
            None => Box::new(EmptySchemaProvider),
        };
        drop(schema_guard);

        let cursor_offset = position_to_offset(&text, pos.line, pos.character) as usize;
        let ctx = analyze_cursor(&text, cursor_offset);
        let items = build_completions(&ctx, schema.as_ref());

        Ok(Some(CompletionResponse::Array(items)))
    }
}
