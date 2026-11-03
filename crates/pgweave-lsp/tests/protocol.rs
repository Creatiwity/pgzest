//! Tests of the LSP server through the protocol, driving the binary.
//!
//! The LSP is what the user sees in their editor, and it had no tests: all
//! checks went through the CLI, whose path is different. These tests speak
//! JSON-RPC over stdio, like VS Code.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_pgweave-lsp");

/// Maximum wait for a message. Generous: CI runs tests in parallel and the
/// server may be slow to start. It is only a guard against a hang, not a
/// timing assumption.
const TIMEOUT: Duration = Duration::from_secs(30);

struct Lsp {
    child: Child,
    stdin: ChildStdin,
    /// Messages read by a dedicated thread: a blocking read on stdout would
    /// freeze the test forever if the server never answered.
    messages: Receiver<serde_json::Value>,
    next_id: i64,
}

impl Lsp {
    fn spawn() -> Self {
        let mut child = Command::new(BIN)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("the pgweave-lsp binary must start");
        let stdin = child.stdin.take().expect("stdin");
        let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));
        let (tx, messages) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            while let Some(msg) = read_message(&mut stdout) {
                if tx.send(msg).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            stdin,
            messages,
            next_id: 0,
        }
    }

    fn send(&mut self, msg: &serde_json::Value) {
        let body = serde_json::to_vec(msg).expect("serialization");
        write!(self.stdin, "Content-Length: {}\r\n\r\n", body.len()).expect("header");
        self.stdin.write_all(&body).expect("body");
        self.stdin.flush().expect("flush");
    }

    fn request(&mut self, method: &str, params: serde_json::Value) -> i64 {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&serde_json::json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params
        }));
        id
    }

    fn notify(&mut self, method: &str, params: serde_json::Value) {
        self.send(&serde_json::json!({
            "jsonrpc": "2.0", "method": method, "params": params
        }));
    }

    /// Next message from the server, or panics if nothing arrives before
    /// `deadline` or if the server closed its stream.
    fn next_message(&self, deadline: Instant, waiting_for: &str) -> serde_json::Value {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match self.messages.recv_timeout(remaining) {
            Ok(msg) => msg,
            Err(RecvTimeoutError::Timeout) => panic!("timed out waiting for {waiting_for}"),
            Err(RecvTimeoutError::Disconnected) => {
                panic!("the server closed its stream while waiting for {waiting_for}")
            }
        }
    }

    /// Reads up to the response carrying `id`. Other messages are ignored.
    fn await_response(&mut self, id: i64) -> serde_json::Value {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let msg = self.next_message(deadline, &format!("the response to request {id}"));
            if msg.get("id").and_then(|v| v.as_i64()) == Some(id) {
                return msg;
            }
        }
    }

    /// Reads up to a diagnostics publication for `uri`.
    ///
    /// The server handles `didOpen` asynchronously: nothing guarantees the
    /// order between this publication and the response to a request sent
    /// afterwards. So the publication itself must be awaited.
    fn await_diagnostics(&mut self, uri: &str) -> Vec<serde_json::Value> {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let msg = self.next_message(deadline, &format!("diagnostics for {uri}"));
            if msg.get("method").and_then(|v| v.as_str()) == Some("textDocument/publishDiagnostics")
                && msg.pointer("/params/uri").and_then(|v| v.as_str()) == Some(uri)
            {
                return msg
                    .pointer("/params/diagnostics")
                    .and_then(|d| d.as_array())
                    .cloned()
                    .unwrap_or_default();
            }
        }
    }

    /// initialize + initialized, with the given root.
    fn init(&mut self, root: &std::path::Path) {
        let uri = format!("file://{}", root.display());
        let id = self.request(
            "initialize",
            serde_json::json!({
                "processId": null,
                "rootUri": uri,
                "capabilities": {},
                "workspaceFolders": [{ "uri": uri, "name": "test" }],
            }),
        );
        // tower-lsp refuses everything before answering initialize.
        let resp = self.await_response(id);
        assert!(resp.get("result").is_some(), "initialize failed: {resp}");
        self.notify("initialized", serde_json::json!({}));
    }

    fn open(&mut self, path: &std::path::Path, language: &str) -> String {
        let uri = format!("file://{}", path.display());
        let text = std::fs::read_to_string(path).expect("read");
        self.notify(
            "textDocument/didOpen",
            serde_json::json!({
                "textDocument": {
                    "uri": uri, "languageId": language, "version": 1, "text": text
                }
            }),
        );
        uri
    }
}

/// Reads an LSP message (headers + JSON body), or `None` if the stream is closed.
fn read_message(stdout: &mut impl BufRead) -> Option<serde_json::Value> {
    let mut len = 0usize;
    loop {
        let mut line = String::new();
        if stdout.read_line(&mut line).ok()? == 0 {
            return None;
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some(v) = line.strip_prefix("Content-Length:") {
            len = v.trim().parse().ok()?;
        }
    }
    let mut buf = vec![0u8; len];
    stdout.read_exact(&mut buf).ok()?;
    serde_json::from_slice(&buf).ok()
}

impl Drop for Lsp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Throwaway project with a custom macro and a `.pgsql` schema.
fn scaffold(name: &str, files: &[(&str, &str)]) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!("pgweave-lsp-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    for (rel, content) in files {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
    }
    root
}

const CONFIG: &str = "[postgres]\nversion = 18\n[schema]\ndirectory = \"schema\"\n[macros]\ndirectory = \"macros\"\n";
const AUDITED: &str = "\
macro audited
header {name:ident}
template
CREATE TABLE {name} (
{body},
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
end
";

#[test]
fn a_clean_pgsql_produces_no_diagnostic() {
    let root = scaffold(
        "clean",
        &[
            ("pgweave.toml", CONFIG),
            ("macros/audited.pgmacro", AUDITED),
            (
                "schema/t.pgsql",
                "@audited widgets {\n    id UUID PRIMARY KEY,\n    label TEXT\n}\n",
            ),
        ],
    );
    let mut lsp = Lsp::spawn();
    lsp.init(&root);
    let uri = lsp.open(&root.join("schema/t.pgsql"), "pgsql");

    let diags = lsp.await_diagnostics(&uri);
    assert!(
        diags.is_empty(),
        "no diagnostic expected on a valid .pgsql, got: {diags:?}"
    );
}

#[test]
fn an_error_in_a_macro_body_is_reported_at_the_right_line() {
    let root = scaffold(
        "diag",
        &[
            ("pgweave.toml", CONFIG),
            ("macros/audited.pgmacro", AUDITED),
            (
                "schema/t.pgsql",
                "@audited widgets {\n    id UUID PRIMARY KEY,\n    label TEXT\n}\n",
            ),
            (
                "schema/f.pgsql",
                "@function bad(): INTEGER {\n\
                 \x20   DECLARE n INTEGER;\n\
                 \x20   SELECT count(*) INTO n FROM widgets WHERE nope = 1;\n\
                 \x20   RETURN n;\n\
                 }\n",
            ),
        ],
    );
    let mut lsp = Lsp::spawn();
    lsp.init(&root);
    let uri = lsp.open(&root.join("schema/f.pgsql"), "pgsql");

    let diags = lsp.await_diagnostics(&uri);
    let found = diags
        .iter()
        .find(|d| d.get("code").and_then(|c| c.as_str()) == Some("column-not-found"));
    let d = found.unwrap_or_else(|| panic!("column-not-found expected, got: {diags:?}"));

    // Line 2 zero-based = the 3rd line of the file, the SELECT's.
    assert_eq!(
        d.pointer("/range/start/line").and_then(|v| v.as_i64()),
        Some(2),
        "the diagnostic must target the SELECT line in the .pgsql: {d}"
    );
}

#[test]
fn expand_macro_returns_the_desugared_sql() {
    let root = scaffold(
        "expand",
        &[
            ("pgweave.toml", CONFIG),
            ("macros/audited.pgmacro", AUDITED),
            (
                "schema/t.pgsql",
                "@audited widgets {\n    id UUID PRIMARY KEY\n}\n",
            ),
        ],
    );
    let mut lsp = Lsp::spawn();
    lsp.init(&root);
    let uri = lsp.open(&root.join("schema/t.pgsql"), "pgsql");

    // Cursor in the macro body.
    let id = lsp.request(
        "pgweave/expandMacro",
        serde_json::json!({
            "textDocument": { "uri": uri },
            "position": { "line": 1, "character": 6 }
        }),
    );
    let resp = lsp.await_response(id);
    let sql = resp
        .pointer("/result/sql")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("sql expected in the response: {resp}"));

    assert!(sql.contains("CREATE TABLE widgets"), "got:\n{sql}");
    assert!(
        sql.contains("created_at"),
        "the column injected by the macro must appear:\n{sql}"
    );
    assert_eq!(
        resp.pointer("/result/expanded").and_then(|v| v.as_i64()),
        Some(1)
    );
}

#[test]
fn expand_macro_says_so_when_there_is_no_macro() {
    let root = scaffold(
        "nomacro",
        &[
            ("pgweave.toml", CONFIG),
            ("schema/t.sql", "CREATE TABLE t (a INT);\n"),
        ],
    );
    let mut lsp = Lsp::spawn();
    lsp.init(&root);
    let uri = lsp.open(&root.join("schema/t.sql"), "sql");

    let id = lsp.request(
        "pgweave/expandMacro",
        serde_json::json!({ "textDocument": { "uri": uri } }),
    );
    let resp = lsp.await_response(id);

    assert_eq!(
        resp.pointer("/result/sql").and_then(|v| v.as_str()),
        Some(""),
        "nothing to expand"
    );
    assert!(
        resp.pointer("/result/note")
            .and_then(|v| v.as_str())
            .is_some_and(|n| n.contains("no macro")),
        "an explanatory note is expected: {resp}"
    );
}

#[test]
fn named_query_format_is_checked_as_in_the_cli() {
    let root = scaffold(
        "queries",
        &[
            (
                "pgweave.toml",
                "[postgres]\nversion = 18\n[schema]\ndirectory = \"schema\"\n\
                 [queries]\ndirectory = \"queries\"\n",
            ),
            (
                "schema/t.sql",
                "CREATE TABLE pages (id BIGINT PRIMARY KEY);\n",
            ),
            (
                "queries/q.sql",
                "-- name: mixed\nSELECT id FROM pages WHERE id = $id OR id = $1;\n",
            ),
        ],
    );
    let mut lsp = Lsp::spawn();
    lsp.init(&root);
    let uri = lsp.open(&root.join("queries/q.sql"), "sql");

    let diags = lsp.await_diagnostics(&uri);
    assert!(
        diags
            .iter()
            .any(|d| d.get("code").and_then(|c| c.as_str()) == Some("query-mixed-parameters")),
        "query-mixed-parameters expected, got: {diags:?}"
    );
}

#[test]
fn the_unsaved_schema_is_authoritative_until_closed() {
    let root = scaffold(
        "buffer",
        &[
            (
                "pgweave.toml",
                "[postgres]\nversion = 18\n[schema]\ndirectory = \"schema\"\n\
                 [queries]\ndirectory = \"queries\"\n",
            ),
            ("schema/t.sql", "CREATE TABLE t (a INT);\n"),
            ("queries/q.sql", "-- name: q\nSELECT b FROM t;\n"),
        ],
    );
    let codes = |diags: &[serde_json::Value]| -> Vec<String> {
        diags
            .iter()
            .filter_map(|d| d.get("code").and_then(|c| c.as_str()).map(str::to_string))
            .collect()
    };
    let change = |lsp: &mut Lsp, uri: &str, version: i64, text: &str| {
        lsp.notify(
            "textDocument/didChange",
            serde_json::json!({
                "textDocument": { "uri": uri, "version": version },
                "contentChanges": [{ "text": text }]
            }),
        );
    };
    let mut lsp = Lsp::spawn();
    lsp.init(&root);
    let q = lsp.open(&root.join("queries/q.sql"), "sql");
    let diags = lsp.await_diagnostics(&q);
    assert!(
        codes(&diags).contains(&"column-not-found".to_string()),
        "{diags:?}"
    );

    // The column is added in the editor, without saving.
    let t = lsp.open(&root.join("schema/t.sql"), "sql");
    lsp.await_diagnostics(&t);
    // The other open documents are rechecked without touching them.
    change(&mut lsp, &t, 2, "CREATE TABLE t (a INT, b INT);\n");
    lsp.await_diagnostics(&t);
    let diags = lsp.await_diagnostics(&q);
    assert!(codes(&diags).is_empty(), "{diags:?}");

    // Closed without saving: the disk is authoritative again.
    lsp.notify(
        "textDocument/didClose",
        serde_json::json!({ "textDocument": { "uri": t } }),
    );
    let diags = lsp.await_diagnostics(&q);
    assert!(
        codes(&diags).contains(&"column-not-found".to_string()),
        "{diags:?}"
    );
}

#[test]
fn hover_gives_the_type_of_the_name_under_the_cursor() {
    // `id` exists in both tables with different types: only the analyzed
    // statement tells which one is designated.
    let root = scaffold(
        "hover",
        &[
            (
                "pgweave.toml",
                "[postgres]\nversion = 18\n[schema]\ndirectory = \"schema\"\n\
                 [queries]\ndirectory = \"queries\"\n",
            ),
            (
                "schema/t.sql",
                "CREATE TABLE a (id BIGINT PRIMARY KEY);\nCREATE TABLE b (id TEXT);\n",
            ),
            ("queries/q.sql", "-- name: q\nSELECT b.id FROM b;\n"),
        ],
    );
    let mut lsp = Lsp::spawn();
    lsp.init(&root);
    let uri = lsp.open(&root.join("queries/q.sql"), "sql");
    lsp.await_diagnostics(&uri);
    let id = lsp.request(
        "textDocument/hover",
        serde_json::json!({
            "textDocument": { "uri": uri },
            "position": { "line": 1, "character": 10 }
        }),
    );
    let response = lsp.await_response(id);
    let value = response["result"]["contents"]["value"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert_eq!(value, "(column) **id**: `text` NULL", "{response:?}");
}

#[test]
fn hovering_a_header_gives_the_descriptor() {
    let root = scaffold(
        "hover-query",
        &[
            (
                "pgweave.toml",
                "[postgres]\nversion = 18\n[schema]\ndirectory = \"schema\"\n\
                 [queries]\ndirectory = \"queries\"\n",
            ),
            (
                "schema/t.sql",
                "CREATE TABLE pages (id BIGINT PRIMARY KEY, title TEXT NOT NULL, note TEXT);\n",
            ),
            (
                "queries/q.sql",
                "-- name: page_by_id\nSELECT title, note FROM pages WHERE id = $id;\n",
            ),
        ],
    );
    let mut lsp = Lsp::spawn();
    lsp.init(&root);
    let uri = lsp.open(&root.join("queries/q.sql"), "sql");
    lsp.await_diagnostics(&uri);
    let id = lsp.request(
        "textDocument/hover",
        serde_json::json!({
            "textDocument": { "uri": uri },
            "position": { "line": 0, "character": 12 }
        }),
    );
    let response = lsp.await_response(id);
    let value = response["result"]["contents"]["value"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert_eq!(
        value,
        "(query) **page_by_id** — `optional`\n\nParameters:\n- `$id` `pg_catalog.int8`\n\n\
         Columns:\n- `title` `pg_catalog.text` NOT NULL\n- `note` `pg_catalog.text` NULL",
        "{response:?}"
    );
}

#[test]
fn changing_the_schema_rechecks_the_other_documents() {
    let root = scaffold(
        "refresh",
        &[
            (
                "pgweave.toml",
                "[postgres]\nversion = 18\n[schema]\ndirectory = \"schema\"\n\
                 [queries]\ndirectory = \"queries\"\n",
            ),
            ("schema/t.sql", "CREATE TABLE t (a INT);\n"),
            ("queries/q.sql", "-- name: q\nSELECT b FROM t;\n"),
        ],
    );
    let mut lsp = Lsp::spawn();
    lsp.init(&root);
    let q = lsp.open(&root.join("queries/q.sql"), "sql");
    assert_eq!(lsp.await_diagnostics(&q).len(), 1, "column b missing");
    let t = lsp.open(&root.join("schema/t.sql"), "sql");
    lsp.await_diagnostics(&t);
    lsp.notify(
        "textDocument/didChange",
        serde_json::json!({
            "textDocument": { "uri": t, "version": 2 },
            "contentChanges": [{ "text": "CREATE TABLE t (a INT, b INT);\n" }]
        }),
    );
    // The query was not touched: its diagnostics are republished.
    lsp.await_diagnostics(&t);
    let diags = lsp.await_diagnostics(&q);
    assert!(diags.is_empty(), "{diags:?}");
}

#[test]
fn declarative_mode_replays_schema_and_migrations_separately() {
    let root = scaffold(
        "declarative",
        &[
            (
                "pgweave.toml",
                "[postgres]\nversion = 18\n[migrations]\ndirectory = \"migrations\"\n\
                 [schema]\ndirectory = \"schema\"\ndeclarative = true\n",
            ),
            (
                "migrations/0001_init.sql",
                "CREATE TABLE t (a INT PRIMARY KEY);\n",
            ),
            (
                "schema/t.sql",
                "CREATE TABLE t (a INT PRIMARY KEY, b INT);\n",
            ),
        ],
    );
    let codes = |diags: &[serde_json::Value]| -> Vec<String> {
        diags
            .iter()
            .filter_map(|d| d.get("code").and_then(|c| c.as_str()).map(str::to_string))
            .collect()
    };
    let mut lsp = Lsp::spawn();
    lsp.init(&root);
    // The schema does not conflict with the migration creating the same table.
    let schema = lsp.open(&root.join("schema/t.sql"), "sql");
    let diags = lsp.await_diagnostics(&schema);
    assert!(codes(&diags).is_empty(), "{diags:?}");
    // The drift (column `b`) is reported on the last migration.
    let m = lsp.open(&root.join("migrations/0001_init.sql"), "sql");
    let diags = lsp.await_diagnostics(&m);
    assert!(
        codes(&diags).contains(&"schema-drift".to_string()),
        "{diags:?}"
    );
}

#[test]
fn migration_created_after_startup() {
    let root = scaffold(
        "migration-new",
        &[
            (
                "pgweave.toml",
                "[postgres]\nversion = 18\n[migrations]\ndirectory = \"migrations\"\n\
                 [schema]\ndirectory = \"schema\"\ndeclarative = true\n",
            ),
            (
                "migrations/0001_init.sql",
                "CREATE TABLE t (a INT PRIMARY KEY);\n",
            ),
            (
                "schema/t.sql",
                "CREATE TABLE t (a INT PRIMARY KEY, b INT);\n",
            ),
        ],
    );
    let has_drift = |diags: &[serde_json::Value]| {
        diags
            .iter()
            .any(|d| d.get("code").and_then(|c| c.as_str()) == Some("schema-drift"))
    };
    let mut lsp = Lsp::spawn();
    lsp.init(&root);
    let init = lsp.open(&root.join("migrations/0001_init.sql"), "sql");
    assert!(has_drift(&lsp.await_diagnostics(&init)));

    // Written to disk (`pgweave migrate create`), outside the editor: the
    // project picks it up and the drift leaves 0001.
    let path = root.join("migrations/0002_b.sql");
    std::fs::write(&path, "ALTER TABLE t ADD COLUMN b INT;\n").unwrap();
    let uri = format!("file://{}", path.display());
    lsp.notify(
        "workspace/didChangeWatchedFiles",
        serde_json::json!({ "changes": [{ "uri": uri, "type": 1 }] }),
    );
    let diags = lsp.await_diagnostics(&init);
    assert!(!has_drift(&diags), "{diags:?}");

    // Opened afterwards: replayed after 0001, not alone (the table exists).
    let opened = lsp.open(&path, "sql");
    let diags = lsp.await_diagnostics(&opened);
    assert!(diags.is_empty(), "{diags:?}");
}

#[test]
fn migration_opened_before_any_reload() {
    let root = scaffold(
        "migration-opened",
        &[
            (
                "pgweave.toml",
                "[postgres]\nversion = 18\n[migrations]\ndirectory = \"migrations\"\n",
            ),
            (
                "migrations/0001_init.sql",
                "CREATE TABLE t (a INT PRIMARY KEY);\n",
            ),
        ],
    );
    let mut lsp = Lsp::spawn();
    lsp.init(&root);
    // Created after startup, without a client notification: opening it is
    // enough to place it in the chain.
    let path = root.join("migrations/0002_b.sql");
    std::fs::write(&path, "ALTER TABLE t ADD COLUMN b INT;\n").unwrap();
    let opened = lsp.open(&path, "sql");
    let diags = lsp.await_diagnostics(&opened);
    assert!(diags.is_empty(), "{diags:?}");
}

#[test]
fn sql_query_in_a_rust_file() {
    let root = scaffold(
        "rust",
        &[
            (
                "pgweave.toml",
                "[postgres]\nversion = 18\n[schema]\ndirectory = \"schema\"\n",
            ),
            (
                "schema/t.sql",
                "CREATE TABLE t (id INT PRIMARY KEY, a TEXT);\n",
            ),
            (
                "src/q.rs",
                "pgweave_macros::sql! {\n    pub by_id: optional,\n    r#\"SELECT a, nope FROM t WHERE id = $id\"#\n}\n\
                 fn f() { let _ = \"SELECT nope FROM nothing\"; }\n",
            ),
        ],
    );
    let mut lsp = Lsp::spawn();
    lsp.init(&root);
    let uri = lsp.open(&root.join("src/q.rs"), "rust");
    let diags = lsp.await_diagnostics(&uri);
    // A single error: the query's, on `nope` in the literal; the ordinary
    // string outside the macro is not analyzed.
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0]["code"], "column-not-found");
    assert_eq!(diags[0]["range"]["start"]["line"], 2);
    assert_eq!(diags[0]["range"]["start"]["character"], 17);
}

#[test]
fn hovering_a_parameter_gives_its_type_not_a_column() {
    // `restaurant` is also the name of a column of another relation: hovering
    // `$restaurant` must give the parameter.
    let root = scaffold(
        "param",
        &[
            (
                "pgweave.toml",
                "[postgres]\nversion = 18\n[schema]\ndirectory = \"schema\"\n\
                 [queries]\ndirectory = \"queries\"\n",
            ),
            (
                "schema/t.sql",
                "CREATE TABLE d (id BIGINT PRIMARY KEY, restaurant_id BIGINT NOT NULL);\n\
                 CREATE VIEW v AS SELECT 'x'::text AS restaurant;\n",
            ),
            (
                "queries/q.sql",
                "-- name: q\nSELECT id FROM d WHERE restaurant_id = $restaurant;\n",
            ),
        ],
    );
    let mut lsp = Lsp::spawn();
    lsp.init(&root);
    let uri = lsp.open(&root.join("queries/q.sql"), "sql");
    lsp.await_diagnostics(&uri);
    let id = lsp.request(
        "textDocument/hover",
        serde_json::json!({
            "textDocument": { "uri": uri },
            "position": { "line": 1, "character": 42 }
        }),
    );
    let response = lsp.await_response(id);
    let value = response["result"]["contents"]["value"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        value.starts_with("(parameter) **$restaurant**: `pg_catalog.int8`"),
        "{value}"
    );
}
