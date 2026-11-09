//! `pgzest migrate status | apply | resolve` against PostgreSQL 18
//! (`make oracle`): each case gets its own database.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tokio_postgres::{Client, NoTls};

const BIN: &str = env!("CARGO_BIN_EXE_pgzest");
const CONFIG: &str = "[postgres]\nversion = 18\n[migrations]\ndirectory = \"migrations\"\n";

fn admin_url() -> String {
    std::env::var("PGZEST_ORACLE_URL").expect("PGZEST_ORACLE_URL")
}

async fn connect(url: &str) -> Client {
    let (client, conn) = url
        .parse::<tokio_postgres::Config>()
        .expect("URL")
        .connect(NoTls)
        .await
        .expect("connection");
    tokio::spawn(async move {
        let _ = conn.await;
    });
    client
}

/// A fresh database: its URL and a client on it.
async fn database(name: &str) -> (String, Client) {
    let admin = connect(&admin_url()).await;
    for sql in [
        format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"),
        format!("CREATE DATABASE {name}"),
    ] {
        admin.batch_execute(&sql).await.expect("database created");
    }
    let mut config: tokio_postgres::Config = admin_url().parse().unwrap();
    config.dbname(name);
    let url = format!(
        "postgres://{}:{}@{}:{}/{name}",
        config.get_user().unwrap(),
        String::from_utf8_lossy(config.get_password().unwrap()),
        match &config.get_hosts()[0] {
            tokio_postgres::config::Host::Tcp(h) => h.clone(),
            #[cfg(unix)]
            tokio_postgres::config::Host::Unix(p) => p.display().to_string(),
        },
        config.get_ports()[0],
    );
    let client = connect(&url).await;
    (url, client)
}

fn project(name: &str, migrations: &[(&str, &str)]) -> PathBuf {
    let root = std::env::temp_dir().join(format!("pgzest-apply-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("migrations")).unwrap();
    fs::write(root.join("pgzest.toml"), CONFIG).unwrap();
    for (file, sql) in migrations {
        fs::write(root.join("migrations").join(file), sql).unwrap();
    }
    root
}

/// `pgzest migrate <args> --url <url>` from `root`: (exit code, output).
fn migrate(root: &Path, url: &str, args: &[&str]) -> (i32, String) {
    let out = Command::new(BIN)
        .arg("migrate")
        .args(args)
        .args(["--url", url])
        .current_dir(root)
        .output()
        .expect("the pgzest binary runs");
    let mut combined = String::from_utf8_lossy(&out.stdout).to_string();
    combined.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.code().unwrap_or(-1), combined)
}

async fn count(client: &Client, sql: &str) -> i64 {
    client.query_one(sql, &[]).await.expect(sql).get(0)
}

#[tokio::test]
#[ignore = "requires PostgreSQL: make oracle"]
async fn pending_migrations_are_applied_once_and_recorded() {
    let (url, client) = database("pgzest_apply_basic").await;
    let root = project(
        "basic",
        &[
            ("0001_init.sql", "CREATE TABLE t (id INT PRIMARY KEY);\n"),
            ("0002_more.sql", "ALTER TABLE t ADD COLUMN n INT;\n"),
        ],
    );
    let (code, out) = migrate(&root, &url, &["status"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("0 applied, 2 pending"), "{out}");
    let (code, out) = migrate(&root, &url, &["apply", "--dry-run"]);
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains("would apply  0001_init (in a transaction)"),
        "{out}"
    );
    assert_eq!(
        count(&client, "SELECT count(*) FROM pg_class WHERE relname = 't'").await,
        0,
        "a dry run applies nothing"
    );

    let (code, out) = migrate(&root, &url, &["apply"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("2 migration(s) applied"), "{out}");
    assert_eq!(
        count(
            &client,
            "SELECT count(*) FROM _pgzest_migrations WHERE finished_at IS NOT NULL"
        )
        .await,
        2
    );
    let (code, out) = migrate(&root, &url, &["apply"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("up to date"), "{out}");
    let (code, out) = migrate(&root, &url, &["status"]);
    assert_eq!(code, 0, "{out}");

    // An applied migration edited: refused until reviewed.
    fs::write(
        root.join("migrations/0002_more.sql"),
        "ALTER TABLE t ADD COLUMN n BIGINT;\n",
    )
    .unwrap();
    let (code, out) = migrate(&root, &url, &["apply"]);
    assert_eq!(code, 1, "{out}");
    assert!(
        out.contains("0002_more was edited after it was applied"),
        "{out}"
    );
    let (code, out) = migrate(&root, &url, &["resolve", "0002_more", "--checksum"]);
    assert_eq!(code, 0, "{out}");
    let (code, out) = migrate(&root, &url, &["status"]);
    assert_eq!(code, 0, "{out}");

    // A pending migration sorting before an applied one: refused.
    fs::write(root.join("migrations/0004_last.sql"), "SELECT 1;\n").unwrap();
    let (code, out) = migrate(&root, &url, &["apply"]);
    assert_eq!(code, 0, "{out}");
    fs::write(root.join("migrations/0003_late.sql"), "SELECT 2;\n").unwrap();
    let (code, out) = migrate(&root, &url, &["apply"]);
    assert_eq!(code, 1, "{out}");
    assert!(
        out.contains("sorting before the last applied one (0004_last): 0003_late"),
        "{out}"
    );

    // A recorded migration the repository lacks: refused.
    fs::remove_file(root.join("migrations/0003_late.sql")).unwrap();
    fs::remove_file(root.join("migrations/0004_last.sql")).unwrap();
    let (code, out) = migrate(&root, &url, &["status"]);
    assert_eq!(code, 1, "{out}");
    assert!(
        out.contains("missing from the repository: 0004_last"),
        "{out}"
    );
    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
#[ignore = "requires PostgreSQL: make oracle"]
async fn a_failing_migration_leaves_nothing() {
    let (url, client) = database("pgzest_apply_failing").await;
    let root = project(
        "failing",
        &[
            ("0001_init.sql", "CREATE TABLE t (id INT PRIMARY KEY);\n"),
            (
                "0002_bad.sql",
                "-- adds a table, then fails\nCREATE TABLE u (id INT);\nINSERT INTO t VALUES (1), (1);\n",
            ),
        ],
    );
    let (code, out) = migrate(&root, &url, &["apply"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("applied  0001_init"), "{out}");
    assert!(out.contains("migration 0002_bad"), "{out}");
    assert!(out.contains("23505"), "{out}");
    assert!(out.contains("rolled back, nothing of it applied"), "{out}");
    assert_eq!(
        count(&client, "SELECT count(*) FROM pg_class WHERE relname = 'u'").await,
        0
    );
    assert_eq!(
        count(&client, "SELECT count(*) FROM _pgzest_migrations").await,
        1
    );
    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
#[ignore = "requires PostgreSQL: make oracle"]
async fn statements_that_cannot_run_in_a_transaction() {
    let (url, client) = database("pgzest_apply_concurrently").await;
    let root = project(
        "concurrently",
        &[
            (
                "0001_init.sql",
                "CREATE TABLE t (id INT PRIMARY KEY, a INT);\n",
            ),
            (
                "0002_index.sql",
                "CREATE INDEX CONCURRENTLY t_a ON t (a);\nCREATE INDEX CONCURRENTLY t_a2 ON t (a, id);\n",
            ),
        ],
    );
    let (code, out) = migrate(&root, &url, &["apply", "--dry-run"]);
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains("0002_index (outside a transaction, statement by statement)"),
        "{out}"
    );
    let (code, out) = migrate(&root, &url, &["apply"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        count(
            &client,
            "SELECT count(*) FROM pg_indexes WHERE indexname IN ('t_a', 't_a2')"
        )
        .await,
        2
    );

    // Mixed with a transactional statement: refused before anything runs.
    fs::write(
        root.join("migrations/0003_mixed.sql"),
        "ALTER TABLE t ADD COLUMN b INT;\nCREATE INDEX CONCURRENTLY t_b ON t (b);\n",
    )
    .unwrap();
    let (code, out) = migrate(&root, &url, &["apply"]);
    assert_eq!(code, 1, "{out}");
    assert!(
        out.contains("0003_mixed: statement(s) that cannot run in a transaction"),
        "{out}"
    );
    assert_eq!(
        count(
            &client,
            "SELECT count(*) FROM information_schema.columns WHERE column_name = 'b'"
        )
        .await,
        0
    );

    // A failure outside a transaction leaves the run started, until resolved.
    // The data decides it: no replay can foresee it.
    client
        .batch_execute("INSERT INTO t VALUES (1, 5), (2, 5)")
        .await
        .unwrap();
    fs::write(
        root.join("migrations/0003_mixed.sql"),
        "CREATE INDEX CONCURRENTLY t_c ON t (a);\nCREATE UNIQUE INDEX CONCURRENTLY t_u ON t (a);\n",
    )
    .unwrap();
    let (code, out) = migrate(&root, &url, &["apply"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("migration 0003_mixed, line 2"), "{out}");
    assert!(out.contains("23505"), "{out}");
    assert!(out.contains("pgzest migrate resolve 0003_mixed"), "{out}");
    let (code, out) = migrate(&root, &url, &["status"]);
    assert_eq!(code, 1, "{out}");
    assert!(
        out.contains("started and never finished: 0003_mixed"),
        "{out}"
    );
    // What it left: one index built, one invalid.
    client
        .batch_execute("DROP INDEX t_c; DROP INDEX t_u")
        .await
        .unwrap();
    let (code, out) = migrate(&root, &url, &["resolve", "0003_mixed", "--rolled-back"]);
    assert_eq!(code, 0, "{out}");
    fs::write(
        root.join("migrations/0003_mixed.sql"),
        "CREATE INDEX CONCURRENTLY t_c ON t (a);\n",
    )
    .unwrap();
    let (code, out) = migrate(&root, &url, &["apply"]);
    assert_eq!(code, 0, "{out}");

    // Transaction control in a migration: refused.
    fs::write(
        root.join("migrations/0004_tx.sql"),
        "BEGIN;\nALTER TABLE t ADD COLUMN d INT;\nCOMMIT;\n",
    )
    .unwrap();
    let (code, out) = migrate(&root, &url, &["apply"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("line 1: `BEGIN`"), "{out}");
    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
#[ignore = "requires PostgreSQL: make oracle"]
async fn the_server_is_checked_against_the_headers() {
    let (url, client) = database("pgzest_apply_headers").await;
    let version: String = client
        .query_one(
            "SELECT default_version FROM pg_available_extensions WHERE name = 'citext'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    let root = project(
        "headers",
        &[(
            "0001_init.sql",
            &format!(
                "-- pgzest: postgres=18 extensions=citext@{version}\n\
                 CREATE EXTENSION IF NOT EXISTS citext SCHEMA public VERSION '{version}';\n\
                 CREATE TABLE t (email citext);\n"
            ),
        )],
    );
    let (code, out) = migrate(&root, &url, &["apply"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        count(
            &client,
            "SELECT count(*) FROM _pgzest_migrations WHERE header LIKE '-- pgzest: postgres=18%'"
        )
        .await,
        1
    );

    // An extension dropped by hand: the next migrations were verified from
    // a state the database is no longer in.
    fs::write(
        root.join("migrations/0002_more.sql"),
        format!("-- pgzest: postgres=18 extensions=citext@{version}\nCREATE TABLE u (id INT);\n"),
    )
    .unwrap();
    client
        .batch_execute("DROP EXTENSION citext CASCADE")
        .await
        .unwrap();
    let (code, out) = migrate(&root, &url, &["apply"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("extension citext is not installed"), "{out}");
    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
#[ignore = "requires PostgreSQL: make oracle"]
async fn concurrent_runs_apply_each_migration_once() {
    let (url, client) = database("pgzest_apply_concurrent").await;
    let root = project(
        "concurrent",
        &[
            (
                "0001_init.sql",
                "CREATE TABLE t (id INT);\nSELECT pg_sleep(1);\n",
            ),
            ("0002_more.sql", "INSERT INTO t VALUES (1);\n"),
        ],
    );
    let runs: Vec<_> = (0..3)
        .map(|_| {
            let (root, url) = (root.clone(), url.clone());
            std::thread::spawn(move || migrate(&root, &url, &["apply"]))
        })
        .collect();
    for r in runs {
        let (code, out) = r.join().unwrap();
        assert_eq!(code, 0, "{out}");
    }
    assert_eq!(count(&client, "SELECT count(*) FROM t").await, 1);
    assert_eq!(
        count(&client, "SELECT count(*) FROM _pgzest_migrations").await,
        2
    );
    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
#[ignore = "requires PostgreSQL: make oracle"]
async fn a_migration_waiting_for_a_lock_gives_up() {
    let (url, client) = database("pgzest_apply_lock_timeout").await;
    let root = project(
        "lock-timeout",
        &[("0001_init.sql", "CREATE TABLE t (id INT);\n")],
    );
    let (code, out) = migrate(&root, &url, &["apply"]);
    assert_eq!(code, 0, "{out}");
    fs::write(
        root.join("pgzest.toml"),
        format!("{CONFIG}lock_timeout = \"200ms\"\n"),
    )
    .unwrap();
    fs::write(
        root.join("migrations/0002_alter.sql"),
        "ALTER TABLE t ADD COLUMN n INT;\n",
    )
    .unwrap();
    // A long transaction holds a lock the ALTER TABLE waits for.
    let holder = connect(&url).await;
    holder
        .batch_execute("BEGIN; SELECT * FROM t;")
        .await
        .unwrap();
    let (code, out) = migrate(&root, &url, &["apply"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("55P03"), "{out}");
    holder.batch_execute("ROLLBACK").await.unwrap();
    let (code, out) = migrate(&root, &url, &["apply"]);
    assert_eq!(code, 0, "{out}");
    drop(client);
    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
#[ignore = "requires PostgreSQL: make oracle"]
async fn db_check_reads_the_registry() {
    let (url, client) = database("pgzest_apply_db_check").await;
    let root = project(
        "db-check",
        &[
            ("0001_init.sql", "CREATE TABLE t (id INT PRIMARY KEY);\n"),
            ("0002_more.sql", "ALTER TABLE t ADD COLUMN n INT;\n"),
        ],
    );
    let (code, out) = migrate(&root, &url, &["apply"]);
    assert_eq!(code, 0, "{out}");
    // A migration not applied yet is left out, the registry itself is not
    // reported as a difference.
    fs::write(
        root.join("migrations/0003_next.sql"),
        "CREATE TABLE u (id INT);\n",
    )
    .unwrap();
    let out = Command::new(BIN)
        .args(["db", "check", "--url", &url])
        .current_dir(&root)
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(
        text.contains("not applied yet, left out: 0003_next"),
        "{text}"
    );
    assert!(text.contains("matches its 2 migration(s)"), "{text}");
    drop(client);
    let _ = fs::remove_dir_all(&root);
}
