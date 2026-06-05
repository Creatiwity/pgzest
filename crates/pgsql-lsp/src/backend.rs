use std::collections::HashMap;

use pgsql_checker::diagnostic::{Diagnostic, Severity};
use pgsql_checker::schema::{DdlSchemaProvider, SchemaProvider};
use pgsql_lint::config::LintConfig;
use tokio::sync::Mutex;
use tower_lsp::lsp_types::*;
use tower_lsp::{jsonrpc::Result, Client, LanguageServer};

pub struct PgsqlBackend {
    client: Client,
    /// URI → document text
    documents: Mutex<HashMap<String, String>>,
    /// Optional schema SQL loaded from the workspace (e.g. a schema.sql file)
    schema_sql: Mutex<Option<String>>,
}

impl PgsqlBackend {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            documents: Mutex::new(HashMap::new()),
            schema_sql: Mutex::new(None),
        }
    }

    /// Parse, type-check and lint `text`, then publish LSP diagnostics for `uri`.
    async fn run_diagnostics(&self, uri: &str, text: &str) {
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
            .map(Self::to_lsp_diagnostic)
            .collect();

        if let Ok(url) = uri.parse::<Url>() {
            self.client
                .publish_diagnostics(url, lsp_diags, None)
                .await;
        }
    }

    /// Convert a checker/lint `Diagnostic` to the LSP `Diagnostic` type.
    fn to_lsp_diagnostic(d: &Diagnostic) -> tower_lsp::lsp_types::Diagnostic {
        let severity = match d.severity {
            Severity::Error => DiagnosticSeverity::ERROR,
            Severity::Warning => DiagnosticSeverity::WARNING,
            Severity::Info => DiagnosticSeverity::INFORMATION,
        };

        // Span is a byte-offset range; we map it to a line/character range.
        // Without a full line-index we approximate by placing diagnostics at
        // line 0 when no span is available. A real implementation would
        // pre-build a line index from the document text.
        let range = Range {
            start: Position {
                line: 0,
                character: 0,
            },
            end: Position {
                line: 0,
                character: 0,
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

// ── LanguageServer implementation ─────────────────────────────────────────────

#[tower_lsp::async_trait]
impl LanguageServer for PgsqlBackend {
    async fn initialize(&self, _params: InitializeParams) -> Result<InitializeResult> {
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

    async fn hover(&self, _params: HoverParams) -> Result<Option<Hover>> {
        // Stub — a full implementation would look up the token under the cursor
        // in the document AST and show type information.
        Ok(None)
    }

    async fn completion(&self, _params: CompletionParams) -> Result<Option<CompletionResponse>> {
        // Stub — return empty list for now.
        Ok(Some(CompletionResponse::Array(vec![])))
    }
}
