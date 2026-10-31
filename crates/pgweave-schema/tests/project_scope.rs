//! Le scope d'indexation du projet doit être cohérent avec la classification
//! des fichiers, et couvrir les sous-répertoires.

use std::fs;
use std::path::Path;

use pgweave_schema::{FileRole, ProjectConfig, SchemaProvider, SchemaResolver, SchemaSource};

/// Crée une arborescence de projet jetable et renvoie sa racine.
fn scaffold(name: &str, files: &[(&str, &str)]) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!("pgweave-scope-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    for (rel, content) in files {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, content).unwrap();
    }
    root
}

fn load(root: &Path) -> ProjectConfig {
    ProjectConfig::find_and_load(root)
        .map(|(_, c)| c)
        .unwrap_or_default()
}

#[test]
fn les_sous_repertoires_sont_indexes() {
    // Régression : le scan était `read_dir` sans récursion, donc
    // schema/tables/users.sql était invisible.
    let root = scaffold(
        "recursive",
        &[
            ("pgweave.toml", "[schema]\ndirectory = \"schema\"\n"),
            (
                "schema/tables/users.sql",
                "CREATE TABLE users (id UUID, email TEXT);",
            ),
            ("schema/functions/f.sql", "CREATE TABLE helper (a INT);"),
        ],
    );
    let cfg = load(&root);
    let resolver = SchemaResolver::from_source(&cfg.to_source(&root));

    assert!(
        resolver.get_table("users").is_some(),
        "schema/tables/users.sql doit être indexé"
    );
    assert!(
        resolver.get_table("helper").is_some(),
        "schema/functions/f.sql doit être indexé"
    );
}

#[test]
fn sans_schema_directory_la_racine_est_le_scope() {
    // Cohérence avec classify_file, qui traite tout non-migration comme Schema.
    let root = scaffold(
        "rootscope",
        &[
            ("pgweave.toml", "[schema]\n"),
            ("db/users.sql", "CREATE TABLE users (id UUID);"),
            ("elsewhere/deep/t.sql", "CREATE TABLE deep (a INT);"),
        ],
    );
    let cfg = load(&root);
    let resolver = SchemaResolver::from_source(&cfg.to_source(&root));

    assert!(resolver.get_table("users").is_some());
    assert!(
        resolver.get_table("deep").is_some(),
        "sans schema.directory, tout le projet est indexé"
    );
    // Et la classification concorde.
    assert_eq!(
        cfg.classify_file(&root.join("db/users.sql"), &root),
        FileRole::Schema
    );
}

#[test]
fn les_migrations_ne_sont_pas_indexees_deux_fois() {
    // Sans exclusion, le scan de la racine reprendrait migrations/ comme
    // Schema en plus de Migration, rejouant ses ALTER.
    let root = scaffold(
        "nodouble",
        &[
            ("pgweave.toml", "[schema]\n"),
            ("migrations/000001_init.sql", "CREATE TABLE t (a INT);"),
        ],
    );
    let cfg = load(&root);
    let chunks = cfg.to_source(&root).chunks();

    let from_migrations: Vec<_> = chunks
        .iter()
        .filter(|c| c.origin.path.to_string_lossy().contains("migrations"))
        .collect();
    assert_eq!(
        from_migrations.len(),
        1,
        "le fichier de migration doit apparaître une seule fois, obtenu {:?}",
        from_migrations
            .iter()
            .map(|c| &c.origin.path)
            .collect::<Vec<_>>()
    );
    assert!(
        from_migrations[0].role.is_migration(),
        "et avec le rôle Migration"
    );
}

#[test]
fn node_modules_et_target_sont_ignores() {
    let root = scaffold(
        "ignored",
        &[
            ("pgweave.toml", "[schema]\n"),
            ("real.sql", "CREATE TABLE real_t (a INT);"),
            (
                "node_modules/pkg/schema.sql",
                "CREATE TABLE junk_a (a INT);",
            ),
            ("target/debug/x.sql", "CREATE TABLE junk_b (a INT);"),
            (".schema-snapshots/old.sql", "CREATE TABLE junk_c (a INT);"),
        ],
    );
    let cfg = load(&root);
    let resolver = SchemaResolver::from_source(&cfg.to_source(&root));

    assert!(resolver.get_table("real_t").is_some());
    for junk in ["junk_a", "junk_b", "junk_c"] {
        assert!(
            resolver.get_table(junk).is_none(),
            "{junk} ne devrait pas être indexé"
        );
    }
}

#[test]
fn schema_include_ajoute_des_repertoires() {
    let root = scaffold(
        "include",
        &[
            (
                "pgweave.toml",
                "[schema]\ndirectory = \"schema\"\ninclude = [\"extra\"]\n",
            ),
            ("schema/a.sql", "CREATE TABLE a_t (a INT);"),
            ("extra/b.sql", "CREATE TABLE b_t (b INT);"),
            ("ailleurs/c.sql", "CREATE TABLE c_t (c INT);"),
        ],
    );
    let cfg = load(&root);
    let resolver = SchemaResolver::from_source(&cfg.to_source(&root));

    assert!(resolver.get_table("a_t").is_some(), "schema/");
    assert!(resolver.get_table("b_t").is_some(), "extra/ via include");
    assert!(
        resolver.get_table("c_t").is_none(),
        "hors scope quand schema.directory est explicite"
    );
}
