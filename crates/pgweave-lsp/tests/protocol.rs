//! Tests du serveur LSP à travers le protocole, en pilotant le binaire.
//!
//! Le LSP est ce que l'utilisateur voit dans son éditeur, et il n'avait aucun
//! test : toutes les vérifications passaient par la CLI, dont le chemin est
//! différent. Ces tests parlent JSON-RPC sur stdio, comme VS Code.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_pgweave-lsp");

/// Délai maximal d'attente d'un message. Large : la CI exécute les tests en
/// parallèle et le serveur peut être lent à démarrer. Ce n'est qu'un garde-fou
/// contre un blocage, pas une hypothèse de timing.
const TIMEOUT: Duration = Duration::from_secs(30);

struct Lsp {
    child: Child,
    stdin: ChildStdin,
    /// Messages lus par un thread dédié : une lecture bloquante sur stdout
    /// gèlerait le test pour toujours si le serveur ne répondait jamais.
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
            .expect("le binaire pgweave-lsp doit démarrer");
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

    /// Prochain message du serveur, ou panique si rien n'arrive avant
    /// `deadline` ou si le serveur a fermé son flux.
    fn next_message(&self, deadline: Instant, waiting_for: &str) -> serde_json::Value {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match self.messages.recv_timeout(remaining) {
            Ok(msg) => msg,
            Err(RecvTimeoutError::Timeout) => panic!("délai dépassé en attendant {waiting_for}"),
            Err(RecvTimeoutError::Disconnected) => {
                panic!("le serveur a fermé son flux en attendant {waiting_for}")
            }
        }
    }

    /// Lit jusqu'à la réponse portant `id`. Les autres messages sont ignorés.
    fn await_response(&mut self, id: i64) -> serde_json::Value {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let msg = self.next_message(deadline, &format!("la réponse à la requête {id}"));
            if msg.get("id").and_then(|v| v.as_i64()) == Some(id) {
                return msg;
            }
        }
    }

    /// Lit jusqu'à une publication de diagnostics pour `uri`.
    ///
    /// Le serveur traite `didOpen` de façon asynchrone : rien ne garantit
    /// l'ordre entre cette publication et la réponse à une requête envoyée
    /// ensuite. Il faut donc attendre la publication elle-même.
    fn await_diagnostics(&mut self, uri: &str) -> Vec<serde_json::Value> {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let msg = self.next_message(deadline, &format!("des diagnostics pour {uri}"));
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
        let resp = self.await_response(id);
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

/// Lit un message LSP (en-têtes + corps JSON), ou `None` si le flux est clos.
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

    let diags = lsp.await_diagnostics(&uri);
    assert!(
        diags.is_empty(),
        "aucun diagnostic attendu sur un .pgsql valide, obtenu : {diags:?}"
    );
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

    let diags = lsp.await_diagnostics(&uri);
    let found = diags
        .iter()
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
    let resp = lsp.await_response(id);
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
    let resp = lsp.await_response(id);

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

#[test]
fn le_format_des_requetes_nommees_est_verifie_comme_en_cli() {
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
        "query-mixed-parameters attendu, obtenu : {diags:?}"
    );
}

#[test]
fn le_schema_non_sauvegarde_fait_foi_jusqu_a_sa_fermeture() {
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

    // La colonne est ajoutée dans l'éditeur, sans sauvegarde.
    let t = lsp.open(&root.join("schema/t.sql"), "sql");
    lsp.await_diagnostics(&t);
    // Les autres documents ouverts sont revérifiés sans y toucher.
    change(&mut lsp, &t, 2, "CREATE TABLE t (a INT, b INT);\n");
    lsp.await_diagnostics(&t);
    let diags = lsp.await_diagnostics(&q);
    assert!(codes(&diags).is_empty(), "{diags:?}");

    // Fermé sans sauvegarde : le disque refait foi.
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
fn le_survol_donne_le_type_du_nom_sous_le_curseur() {
    // `id` existe dans les deux tables avec des types différents : seul le
    // statement analysé dit lequel est désigné.
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
fn le_survol_d_un_en_tete_donne_le_descripteur() {
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
        "(requête) **page_by_id** — `optional`\n\nParamètres :\n- `$id` `pg_catalog.int8`\n\n\
         Colonnes :\n- `title` `pg_catalog.text` NOT NULL\n- `note` `pg_catalog.text` NULL",
        "{response:?}"
    );
}

#[test]
fn modifier_le_schema_revérifie_les_autres_documents() {
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
    assert_eq!(lsp.await_diagnostics(&q).len(), 1, "colonne b absente");
    let t = lsp.open(&root.join("schema/t.sql"), "sql");
    lsp.await_diagnostics(&t);
    lsp.notify(
        "textDocument/didChange",
        serde_json::json!({
            "textDocument": { "uri": t, "version": 2 },
            "contentChanges": [{ "text": "CREATE TABLE t (a INT, b INT);\n" }]
        }),
    );
    // La requête n'a pas été touchée : ses diagnostics sont republiés.
    lsp.await_diagnostics(&t);
    let diags = lsp.await_diagnostics(&q);
    assert!(diags.is_empty(), "{diags:?}");
}

#[test]
fn mode_declaratif_schema_et_migrations_rejoues_a_part() {
    let root = scaffold(
        "declaratif",
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
    // Le schéma n'entre pas en conflit avec la migration qui crée la même table.
    let schema = lsp.open(&root.join("schema/t.sql"), "sql");
    let diags = lsp.await_diagnostics(&schema);
    assert!(codes(&diags).is_empty(), "{diags:?}");
    // L'écart (colonne `b`) est signalé sur la dernière migration.
    let m = lsp.open(&root.join("migrations/0001_init.sql"), "sql");
    let diags = lsp.await_diagnostics(&m);
    assert!(
        codes(&diags).contains(&"schema-drift".to_string()),
        "{diags:?}"
    );
}

#[test]
fn migration_creee_apres_le_demarrage() {
    let root = scaffold(
        "migration-nouvelle",
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

    // Écrite sur le disque (`pgweave migrate create`), hors de l'éditeur :
    // le projet la prend en compte, l'écart disparaît de 0001.
    let path = root.join("migrations/0002_b.sql");
    std::fs::write(&path, "ALTER TABLE t ADD COLUMN b INT;\n").unwrap();
    let uri = format!("file://{}", path.display());
    lsp.notify(
        "workspace/didChangeWatchedFiles",
        serde_json::json!({ "changes": [{ "uri": uri, "type": 1 }] }),
    );
    let diags = lsp.await_diagnostics(&init);
    assert!(!has_drift(&diags), "{diags:?}");

    // Ouverte ensuite : rejouée après 0001, pas seule (la table existe).
    let opened = lsp.open(&path, "sql");
    let diags = lsp.await_diagnostics(&opened);
    assert!(diags.is_empty(), "{diags:?}");
}

#[test]
fn migration_ouverte_avant_tout_rechargement() {
    let root = scaffold(
        "migration-ouverte",
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
    // Créée après le démarrage, sans notification du client : l'ouverture
    // suffit à la placer dans la chaîne.
    let path = root.join("migrations/0002_b.sql");
    std::fs::write(&path, "ALTER TABLE t ADD COLUMN b INT;\n").unwrap();
    let opened = lsp.open(&path, "sql");
    let diags = lsp.await_diagnostics(&opened);
    assert!(diags.is_empty(), "{diags:?}");
}

#[test]
fn requete_sql_dans_un_fichier_rust() {
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
                "pgweave_macros::sql! {\n    pub par_id: optional,\n    r#\"SELECT a, nope FROM t WHERE id = $id\"#\n}\n\
                 fn f() { let _ = \"SELECT nope FROM rien\"; }\n",
            ),
        ],
    );
    let mut lsp = Lsp::spawn();
    lsp.init(&root);
    let uri = lsp.open(&root.join("src/q.rs"), "rust");
    let diags = lsp.await_diagnostics(&uri);
    // Une seule erreur : celle de la requête, sur `nope` dans le littéral ;
    // la chaîne ordinaire hors macro n'est pas analysée.
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0]["code"], "column-not-found");
    assert_eq!(diags[0]["range"]["start"]["line"], 2);
    assert_eq!(diags[0]["range"]["start"]["character"], 17);
}

#[test]
fn le_survol_d_un_parametre_donne_son_type_pas_une_colonne() {
    // `restaurant` est aussi le nom d'une colonne d'une autre relation : le
    // survol de `$restaurant` doit donner le paramètre.
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
        value.starts_with("(paramètre) **$restaurant** : `pg_catalog.int8`"),
        "{value}"
    );
}
