//! `pgweave db check` on PostgreSQL: a database compared with the
//! migrations it received, as its migrations table records them; what was
//! changed by hand is reported, the migrations table itself is not.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tokio_postgres::{Client, NoTls};

const BIN: &str = env!("CARGO_BIN_EXE_pgweave");
const URL_ENV: &str = "PGWEAVE_ORACLE_URL";

const INIT: &str = "\
CREATE TABLE items (id INT PRIMARY KEY, n INT NOT NULL);

CREATE FUNCTION bump(k INT) RETURNS INT LANGUAGE plpgsql AS $$
BEGIN
    UPDATE items SET n = n + 1 WHERE id <= k;
    RETURN k;
END;
$$;

CREATE FUNCTION touch() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    NEW.n := NEW.n + 1;
    RETURN NEW;
END;
$$;

CREATE TRIGGER items_touch BEFORE UPDATE ON items FOR EACH ROW EXECUTE FUNCTION touch();
";

const MORE: &str = "CREATE INDEX items_n_idx ON items (n);\n";

/// A project in the Prisma layout: two migrations.
fn scaffold() -> PathBuf {
    let root = std::env::temp_dir().join(format!("pgweave-db-check-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    for (rel, content) in [
        ("pgweave.toml", "[postgres]\nversion = 18\n"),
        ("migrations/20240101000000_init/migration.sql", INIT),
        ("migrations/20240102000000_more/migration.sql", MORE),
    ] {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, content).unwrap();
    }
    root
}

/// A database that received the first migration, recorded by Prisma.
async fn deployed(url: &str, name: &str) -> (Client, String) {
    let connect = |config: tokio_postgres::Config| async move {
        let (client, conn) = config.connect(NoTls).await.expect("connection");
        tokio::spawn(async move {
            let _ = conn.await;
        });
        client
    };
    let mut config: tokio_postgres::Config = url.parse().expect("URL");
    let admin = connect(config.clone()).await;
    for sql in [
        format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"),
        format!("CREATE DATABASE {name}"),
    ] {
        admin.batch_execute(&sql).await.expect("database created");
    }
    config.dbname(name);
    let client = connect(config).await;
    client.batch_execute(INIT).await.expect("first migration");
    client
        .batch_execute(
            "CREATE TABLE _prisma_migrations (
                 id VARCHAR(36) PRIMARY KEY,
                 checksum VARCHAR(64) NOT NULL,
                 finished_at TIMESTAMPTZ,
                 migration_name VARCHAR(255) NOT NULL,
                 logs TEXT,
                 rolled_back_at TIMESTAMPTZ,
                 started_at TIMESTAMPTZ NOT NULL DEFAULT now(),
                 applied_steps_count INTEGER NOT NULL DEFAULT 0
             );
             INSERT INTO _prisma_migrations (id, checksum, finished_at, migration_name)
             VALUES ('1', 'x', now(), '20240101000000_init')",
        )
        .await
        .expect("migrations table");
    let base = url.rsplit_once('/').expect("URL with a database").0;
    (client, format!("{base}/{name}"))
}

/// `pgweave db check`: (exit code, stdout + stderr).
fn check(root: &Path, args: &[&str]) -> (i32, String) {
    let out = Command::new(BIN)
        .args(["db", "check"])
        .args(args)
        .current_dir(root)
        .output()
        .expect("the pgweave binary runs");
    let mut combined = String::from_utf8_lossy(&out.stdout).to_string();
    combined.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.code().unwrap_or(-1), combined)
}

#[tokio::test]
#[ignore = "requires PostgreSQL: make oracle"]
async fn hand_changes_are_found() {
    let url = std::env::var(URL_ENV).expect("PGWEAVE_ORACLE_URL");
    let (client, db) = deployed(&url, "pgweave_db_check").await;
    let root = scaffold();

    // As deployed: the second migration is not applied yet, and the
    // migrations table is not part of the comparison.
    let (code, out) = check(&root, &["--url", &db]);
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains("unchanged: the database matches its 1 migration(s)"),
        "{out}"
    );
    assert!(
        out.contains("1 migration(s) not applied yet, left out: 20240102000000_more"),
        "{out}"
    );

    // A hotfix, an index and a dropped trigger, by hand.
    client
        .batch_execute(
            "CREATE OR REPLACE FUNCTION bump(k INT) RETURNS INT LANGUAGE plpgsql AS $$
             BEGIN
                 UPDATE items SET n = n + 2 WHERE id <= k;
                 RETURN k;
             END;
             $$;
             CREATE INDEX items_hand_idx ON items (id, n);
             DROP TRIGGER items_touch ON items;",
        )
        .await
        .expect("hand changes");
    let (code, out) = check(&root, &["--url", &db]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("3 object(s)"), "{out}");
    assert!(out.contains("changed  routine bump(integer):"), "{out}");
    assert!(
        out.contains("-     UPDATE items SET n = n + 1 WHERE id <= k;"),
        "{out}"
    );
    assert!(
        out.contains("+                  UPDATE items SET n = n + 2 WHERE id <= k;"),
        "{out}"
    );
    assert!(
        out.contains("extra    index items_hand_idx on public.items: only in the database"),
        "{out}"
    );
    assert!(
        out.contains("missing  trigger items_touch on public.items: in the migrations"),
        "{out}"
    );
    assert!(!out.contains("_prisma_migrations"), "{out}");

    // The second migration applied and recorded: compared too.
    client
        .batch_execute(&format!(
            "{MORE} INSERT INTO _prisma_migrations (id, checksum, finished_at, migration_name) \
             VALUES ('2', 'x', now(), '20240102000000_more')"
        ))
        .await
        .expect("second migration");
    let (code, out) = check(&root, &["--url", &db]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("differs from its 2 migration(s)"), "{out}");
    assert!(!out.contains("not applied yet"), "{out}");

    // A migration the repository does not have, one left half-done.
    client
        .batch_execute(
            "INSERT INTO _prisma_migrations (id, checksum, finished_at, migration_name) \
             VALUES ('3', 'x', now(), '20240103000000_elsewhere')",
        )
        .await
        .expect("unknown migration");
    let (code, out) = check(&root, &["--url", &db]);
    assert_eq!(code, 2, "{out}");
    assert!(
        out.contains("missing from migrations: 20240103000000_elsewhere"),
        "{out}"
    );
    client
        .batch_execute(
            "DELETE FROM _prisma_migrations WHERE id = '3'; \
             INSERT INTO _prisma_migrations (id, checksum, migration_name) \
             VALUES ('4', 'x', '20240102000000_more')",
        )
        .await
        .expect("failed migration");
    let (code, out) = check(&root, &["--url", &db]);
    assert_eq!(code, 2, "{out}");
    assert!(out.contains("started and not finished"), "{out}");

    // Without a migrations table: all of them, or up to `--until`.
    client
        .batch_execute(
            "DROP TABLE _prisma_migrations; DROP INDEX items_hand_idx; DROP INDEX items_n_idx; \
             CREATE TRIGGER items_touch BEFORE UPDATE ON items FOR EACH ROW EXECUTE FUNCTION touch(); \
             CREATE OR REPLACE FUNCTION bump(k INT) RETURNS INT LANGUAGE plpgsql AS $$
BEGIN
    UPDATE items SET n = n + 1 WHERE id <= k;
    RETURN k;
END;
$$;",
        )
        .await
        .expect("back to the first migration");
    let (code, out) = check(&root, &["--url", &db, "--until", "20240101000000_init"]);
    assert_eq!(code, 0, "{out}");
    let (code, out) = check(&root, &["--url", &db]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("compared with all 2 migrations"), "{out}");
    assert!(
        out.contains("missing  index items_n_idx on public.items"),
        "{out}"
    );
    let (code, out) = check(&root, &["--url", &db, "--until", "nope"]);
    assert_eq!(code, 2, "{out}");
    assert!(out.contains("`--until nope`: no such migration"), "{out}");
    let _ = fs::remove_dir_all(&root);
}
