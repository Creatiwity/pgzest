//! La CLI doit voir le même schéma que le LSP.
//!
//! Historiquement `pgweave check` ignorait pgweave.toml et exigeait
//! des `--schema` explicites, alors que le LSP indexait tout le projet. Même
//! fichier, diagnostics différents entre l'éditeur et la CI.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_pgweave");

fn scaffold(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "pgweave-parity-{name}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = fs::remove_dir_all(&root);
    for (rel, content) in files {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, content).unwrap();
    }
    root
}

/// Lance `pgweave check` depuis `root`, renvoie (code, stdout+stderr).
fn check(root: &Path, args: &[&str]) -> (i32, String) {
    let out = Command::new(BIN)
        .arg("check")
        .args(args)
        .current_dir(root)
        .output()
        .expect("le binaire pgweave doit s'exécuter");
    let mut combined = String::from_utf8_lossy(&out.stdout).to_string();
    combined.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.code().unwrap_or(-1), combined)
}

const PROJECT: &[(&str, &str)] = &[
    ("pgweave.toml", "[schema]\ndirectory = \"schema\"\n"),
    (
        "schema/tables/users.sql",
        "CREATE TABLE users (id UUID PRIMARY KEY, email TEXT NOT NULL);",
    ),
    ("queries/ok.sql", "SELECT email FROM users;"),
    ("queries/bad.sql", "SELECT nope FROM users;"),
];

#[test]
fn le_scope_projet_resout_sans_schema_explicite() {
    let root = scaffold("resolve", PROJECT);
    let (code, out) = check(&root, &["queries/ok.sql"]);
    assert_eq!(
        code, 0,
        "users doit se résoudre depuis le scope projet, sortie:\n{out}"
    );
}

#[test]
fn une_colonne_absente_est_toujours_detectee() {
    let root = scaffold("badcol", PROJECT);
    let (code, out) = check(&root, &["queries/bad.sql"]);
    assert_eq!(code, 1, "sortie:\n{out}");
    assert!(
        out.contains("column-not-found"),
        "column-not-found attendu, sortie:\n{out}"
    );
}

#[test]
fn no_project_revient_au_mode_isole() {
    let root = scaffold("noproject", PROJECT);
    let (code, out) = check(&root, &["queries/ok.sql", "--no-project"]);
    assert_eq!(code, 1, "sans index projet, users est inconnue:\n{out}");
    assert!(out.contains("table-not-found"), "sortie:\n{out}");
}

#[test]
fn les_sous_repertoires_du_scope_comptent() {
    // schema/tables/users.sql est dans un sous-répertoire : le scan doit
    // récurser, sinon la table reste invisible.
    let root = scaffold("subdir", PROJECT);
    let (code, out) = check(&root, &["queries/ok.sql"]);
    assert_eq!(code, 0, "sortie:\n{out}");
    assert!(!out.contains("table-not-found"), "sortie:\n{out}");
}

#[test]
fn un_statement_non_indexable_est_signale() {
    // Une table définie dans un statement illisible disparaît de l'index ;
    // la CLI doit prévenir plutôt que laisser croire à un index complet.
    let root = scaffold(
        "skipidx",
        &[
            ("pgweave.toml", "[schema]\ndirectory = \"schema\"\n"),
            ("schema/broken.sql", "CREATE TABL users (id UUID);"),
            ("queries/q.sql", "SELECT id FROM users;"),
        ],
    );
    let (_code, out) = check(&root, &["queries/q.sql"]);
    assert!(
        out.contains("non indexé"),
        "l'écart d'indexation doit être signalé, sortie:\n{out}"
    );
}

#[test]
fn schema_explicite_reste_prioritaire() {
    // Rétrocompatibilité : --schema doit continuer de fonctionner et primer.
    let root = scaffold(
        "explicit",
        &[
            ("pgweave.toml", "[schema]\ndirectory = \"schema\"\n"),
            ("schema/a.sql", "CREATE TABLE t (a INT);"),
            ("other.sql", "CREATE TABLE other_t (b INT);"),
            ("q.sql", "SELECT b FROM other_t;"),
        ],
    );
    let (code, out) = check(&root, &["q.sql", "--schema", "other.sql"]);
    assert_eq!(code, 0, "sortie:\n{out}");
}

#[test]
fn sans_config_le_mode_autonome_est_conserve() {
    // Aucun pgweave.toml : le comportement historique
    // (DDL du fichier lui-même) doit rester intact.
    let root = scaffold(
        "standalone",
        &[("q.sql", "CREATE TABLE t (a INT);\nSELECT a FROM t;")],
    );
    let (code, out) = check(&root, &["q.sql"]);
    assert_eq!(code, 0, "sortie:\n{out}");
}
