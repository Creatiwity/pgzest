//! Tests du serveur LSP à travers le protocole, en pilotant le binaire.
//!
//! Le LSP est ce que l'utilisateur voit dans son éditeur, et il n'avait aucun
//! test : toutes les vérifications passaient par la CLI, dont le chemin est
//! différent. Ces tests parlent JSON-RPC sur stdio, comme VS Code.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_pgweave-lsp");

struct Lsp {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
}

impl Lsp {
    fn spawn() -> Self {
        let mut child = Command::new(BIN)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("le binaire pgweave-lsp doit démarrer");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        Self {
            child,
            stdin,
            stdout,
            next_id: 0,
        }
    }

    fn send(&mut self, msg: &serde_json::Value) {
        let body = serde_json::to_vec(msg).expect("sérialisation");
        write!(self.stdin, "Content-Length: {}\r\n\r\n", body.len()).expect("en-tête");
        self.stdin.write_all(&body).expect("corps");
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

    /// Lit un message, ou `None` si le flux est clos.
    fn read(&mut self) -> Option<serde_json::Value> {
        let mut len = 0usize;
        loop {
            let mut line = String::new();
            if self.stdout.read_line(&mut line).ok()? == 0 {
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
        self.stdout.read_exact(&mut buf).ok()?;
        serde_json::from_slice(&buf).ok()
    }

    /// Lit jusqu'à la réponse portant `id`, en collectant les diagnostics vus.
    fn await_response(&mut self, id: i64) -> (serde_json::Value, Vec<serde_json::Value>) {
        let mut diags = Vec::new();
        for _ in 0..200 {
            let Some(msg) = self.read() else { break };
            if msg.get("id").and_then(|v| v.as_i64()) == Some(id) {
                return (msg, diags);
            }
            if msg.get("method").and_then(|v| v.as_str()) == Some("textDocument/publishDiagnostics")
                && let Some(d) = msg.pointer("/params/diagnostics")
            {
                diags.push(d.clone());
            }
        }
        panic!("aucune réponse pour la requête {id}");
    }

    /// initialize + initialized, avec la racine donnée.
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
        // tower-lsp refuse tout avant d'avoir répondu à initialize.
        let (resp, _) = self.await_response(id);
        assert!(resp.get("result").is_some(), "initialize a échoué : {resp}");
        self.notify("initialized", serde_json::json!({}));
    }

    fn open(&mut self, path: &std::path::Path, language: &str) -> String {
        let uri = format!("file://{}", path.display());
        let text = std::fs::read_to_string(path).expect("lecture");
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

impl Drop for Lsp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Projet jetable avec une macro maison et un schéma en `.pgsql`.
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

const CONFIG: &str = "[schema]\ndirectory = \"schema\"\n[macros]\ndirectory = \"macros\"\n";
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
fn un_pgsql_propre_ne_produit_aucun_diagnostic() {
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

    let id = lsp.request(
        "pgweave/expandMacro",
        serde_json::json!({ "textDocument": { "uri": uri } }),
    );
    let (_, diags) = lsp.await_response(id);

    for d in &diags {
        assert_eq!(
            d.as_array().map(Vec::len),
            Some(0),
            "aucun diagnostic attendu sur un .pgsql valide, obtenu : {d}"
        );
    }
}

#[test]
fn une_erreur_dans_un_corps_de_macro_est_signalee_a_la_bonne_ligne() {
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

    let id = lsp.request(
        "pgweave/expandMacro",
        serde_json::json!({ "textDocument": { "uri": uri } }),
    );
    let (_, diags) = lsp.await_response(id);

    let found = diags
        .iter()
        .flat_map(|d| d.as_array().unwrap())
        .find(|d| d.get("code").and_then(|c| c.as_str()) == Some("column-not-found"));
    let d = found.unwrap_or_else(|| panic!("column-not-found attendu, obtenu : {diags:?}"));

    // Ligne 2 en 0-indexé = la 3e ligne du fichier, celle du SELECT.
    assert_eq!(
        d.pointer("/range/start/line").and_then(|v| v.as_i64()),
        Some(2),
        "le diagnostic doit viser la ligne du SELECT dans le .pgsql : {d}"
    );
}

#[test]
fn expand_macro_rend_le_sql_desucre() {
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

    // Curseur dans le corps de la macro.
    let id = lsp.request(
        "pgweave/expandMacro",
        serde_json::json!({
            "textDocument": { "uri": uri },
            "position": { "line": 1, "character": 6 }
        }),
    );
    let (resp, _) = lsp.await_response(id);
    let sql = resp
        .pointer("/result/sql")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("sql attendu dans la réponse : {resp}"));

    assert!(sql.contains("CREATE TABLE widgets"), "obtenu :\n{sql}");
    assert!(
        sql.contains("created_at"),
        "la colonne injectée par la macro doit apparaître :\n{sql}"
    );
    assert_eq!(
        resp.pointer("/result/expanded").and_then(|v| v.as_i64()),
        Some(1)
    );
}

#[test]
fn expand_macro_le_dit_quand_il_ny_a_pas_de_macro() {
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
    let (resp, _) = lsp.await_response(id);

    assert_eq!(
        resp.pointer("/result/sql").and_then(|v| v.as_str()),
        Some(""),
        "rien à développer"
    );
    assert!(
        resp.pointer("/result/note")
            .and_then(|v| v.as_str())
            .is_some_and(|n| n.contains("aucune macro")),
        "une note explicative est attendue : {resp}"
    );
}
