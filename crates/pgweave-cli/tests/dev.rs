//! `pgweave dev` on PostgreSQL: the routines of the schema files reach the
//! database instrumented, once or on each save; a routine a file no longer
//! declares is dropped; a database error points into the source.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tokio_postgres::{Client, NoTls};

const BIN: &str = env!("CARGO_BIN_EXE_pgweave");
const URL_ENV: &str = "PGWEAVE_ORACLE_URL";

const CONFIG: &str = "[postgres]\nversion = 18\n[schema]\ndirectory = \"schema\"\n";

const ROUTINES: &str = "\
CREATE TABLE items (id INT PRIMARY KEY, n INT NOT NULL);

@function bump(k INT): INT {
    DECLARE c INT;
    UPDATE items SET n = n + 1 WHERE id <= k;
    GET DIAGNOSTICS c = ROW_COUNT;
    @snapshot done;
    RETURN c;
}

@trigger audit after insert on items for each row {
    RETURN NEW;
}
";

fn scaffold(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let root = std::env::temp_dir().join(format!("pgweave-dev-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    for (rel, content) in files {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, content).unwrap();
    }
    root
}

/// A new database with the `items` table (`pgweave dev` applies no table).
async fn fresh(url: &str, name: &str) -> (Client, String) {
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
    client
        .batch_execute(
            "CREATE TABLE items (id INT PRIMARY KEY, n INT NOT NULL); \
             INSERT INTO items SELECT g, 0 FROM generate_series(1, 5) AS g",
        )
        .await
        .expect("items");
    let base = url.rsplit_once('/').expect("URL with a database").0;
    (client, format!("{base}/{name}"))
}

/// `pgweave dev --once`: (exit code, stdout + stderr).
fn once(root: &Path, url: &str) -> (i32, String) {
    let out = Command::new(BIN)
        .args(["dev", "--once", "--url", url])
        .current_dir(root)
        .output()
        .expect("the pgweave binary runs");
    let mut combined = String::from_utf8_lossy(&out.stdout).to_string();
    combined.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.code().unwrap_or(-1), combined)
}

async fn routine(client: &Client, signature: &str) -> Option<String> {
    client
        .query_one(
            "SELECT p.prorettype::regtype::text FROM pg_proc p \
             WHERE p.oid = to_regprocedure($1)",
            &[&signature],
        )
        .await
        .ok()
        .map(|r| r.get(0))
}

#[tokio::test]
#[ignore = "requires PostgreSQL: make oracle"]
async fn routines_are_applied_instrumented() {
    let url = std::env::var(URL_ENV).expect("PGWEAVE_ORACLE_URL");
    let (client, db) = fresh(&url, "pgweave_dev_once").await;
    let root = scaffold(
        "once",
        &[
            ("pgweave.toml", CONFIG),
            ("schema/a.sql", ROUTINES),
            (
                "schema/b.sql",
                "CREATE FUNCTION plain() RETURNS INT LANGUAGE sql AS 'SELECT 1';\n",
            ),
        ],
    );
    let (code, out) = once(&root, &db);
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains("schema/a.sql: applied 2 routine(s), 1 trigger(s)"),
        "{out}"
    );
    // Applied twice: `CREATE OR REPLACE`, even for `CREATE FUNCTION`.
    let (code, out) = once(&root, &db);
    assert_eq!(code, 0, "{out}");

    // Built at the debug level: the run is logged, its snapshot taken.
    let n: i32 = client
        .query_one("SELECT bump(2)", &[])
        .await
        .expect("bump")
        .get(0);
    assert_eq!(n, 2);
    let logged: i64 = client
        .query_one(
            "SELECT count(*) FROM pgweave_log.run r JOIN pgweave_debug.snapshot s USING (run) \
             WHERE r.routine = 'bump' AND s.variables->>'c' = '2'",
            &[],
        )
        .await
        .expect("log")
        .get(0);
    assert_eq!(logged, 1);
    let triggers: i64 = client
        .query_one(
            "SELECT count(*) FROM pg_trigger WHERE tgname = 'audit' AND NOT tgisinternal",
            &[],
        )
        .await
        .expect("trigger")
        .get(0);
    assert_eq!(triggers, 1);

    // A database error is located in the source; the other files apply.
    fs::write(
        root.join("schema/c.sql"),
        "@function broken(): INT {\n    DECLARE z nosuchtype;\n    RETURN 1;\n}\n",
    )
    .unwrap();
    let (code, out) = once(&root, &db);
    assert_eq!(code, 1, "{out}");
    assert!(
        out.contains("schema/c.sql:2:15: error: type \"nosuchtype\" does not exist"),
        "{out}"
    );
    assert!(out.contains("schema/a.sql: applied"), "{out}");
    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
#[ignore = "requires PostgreSQL: make oracle"]
async fn a_saved_file_is_applied_again() {
    let url = std::env::var(URL_ENV).expect("PGWEAVE_ORACLE_URL");
    let (client, db) = fresh(&url, "pgweave_dev_watch").await;
    let root = scaffold(
        "watch",
        &[("pgweave.toml", CONFIG), ("schema/a.sql", ROUTINES)],
    );
    let mut child = Command::new(BIN)
        .args(["dev", "--url", &db])
        .current_dir(&root)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("pgweave dev starts");

    let wait = async |what: &str, ok: &dyn Fn(Option<String>, Option<String>) -> bool| {
        let start = Instant::now();
        loop {
            let old = routine(&client, "bump(integer)").await;
            let new = routine(&client, "bump2(integer)").await;
            if ok(old, new) {
                return;
            }
            assert!(start.elapsed() < Duration::from_secs(15), "{what}");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    };
    wait("first apply", &|old, _| old.as_deref() == Some("integer")).await;

    // Another return type: the routine is dropped and created again.
    fs::write(
        root.join("schema/a.sql"),
        ROUTINES.replace("bump(k INT): INT", "bump(k INT): BIGINT"),
    )
    .unwrap();
    wait("return type changed", &|old, _| {
        old.as_deref() == Some("bigint")
    })
    .await;

    // Renamed: the routine of the previous save is dropped.
    fs::write(
        root.join("schema/a.sql"),
        ROUTINES.replace("bump(k INT): INT", "bump2(k INT): INT"),
    )
    .unwrap();
    wait("renamed", &|old, new| old.is_none() && new.is_some()).await;

    child.kill().expect("stopped");
    let _ = child.wait();
    let _ = fs::remove_dir_all(&root);
}
