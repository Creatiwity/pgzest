//! Isolated temp tables and snapshots (PLAN.md P9), run on PostgreSQL: a
//! routine using a temp table, called recursively and from a loop in one
//! session, keeps its rows apart; its snapshots show each run.

use pgweave_macro::{Instrumentation, LogLevel, LogTarget, MacroSet};
use tokio_postgres::{Client, NoTls};

const SOURCE: &str = r#"
CREATE TABLE items (id INT PRIMARY KEY, grp INT NOT NULL);
INSERT INTO items SELECT g, g % 3 FROM generate_series(1, 9) AS g;

-- Each level takes the 3 items of its group (levels 0 and 3 share theirs),
-- then calls the level below: one session, one temp table, nested runs.
@function collect(level INT): BIGINT {
    DECLARE mine BIGINT;
    DECLARE foreign_rows BIGINT;
    DECLARE nested BIGINT := 0;
    @temp_table work (id INT PRIMARY KEY, lvl INT NOT NULL);
    INSERT INTO work (id, lvl) SELECT id, level FROM items WHERE grp = level % 3;
    -- Same keys again in the same run: ON CONFLICT targets this run's rows.
    INSERT INTO work (id, lvl) SELECT id, level FROM items WHERE grp = level % 3
        ON CONFLICT (id) DO UPDATE SET lvl = EXCLUDED.lvl;
    @snapshot filled;
    IF level > 0 THEN
        nested := collect(level - 1);
    END IF;
    UPDATE work SET lvl = lvl + 100;
    SELECT count(*) INTO mine FROM work;
    SELECT count(*) INTO foreign_rows FROM work AS w WHERE w.lvl <> level + 100;
    IF foreign_rows > 0 THEN
        RAISE EXCEPTION 'level %: % row(s) of another run', level, foreign_rows;
    END IF;
    DELETE FROM work WHERE id < 0;
    RETURN mine + nested;
}

CREATE TABLE events (id BIGINT PRIMARY KEY);

@trigger audit after insert on events for each row {
    @temp_table batch (id BIGINT);
    INSERT INTO batch VALUES (NEW.id);
    @snapshot row_seen;
    RETURN NEW;
}

@procedure refresh(p INT) {
    @temp_table seen (id INT);
    INSERT INTO seen VALUES (p);
    @snapshot after;
    DELETE FROM seen;
}

@function many(): BIGINT {
    DECLARE total BIGINT := 0;
    FOR i IN 0..4 LOOP
        total := total + collect(i);
    END LOOP;
    RETURN total;
}
"#;

async fn fresh(url: &str, name: &str) -> Client {
    let connect = |config: tokio_postgres::Config| async move {
        let (client, conn) = config.connect(NoTls).await.expect("connection");
        tokio::spawn(async move {
            let _ = conn.await;
        });
        client
    };
    let config: tokio_postgres::Config = url.parse().expect("URL");
    let admin = connect(config.clone()).await;
    for sql in [
        format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"),
        format!("CREATE DATABASE {name}"),
    ] {
        admin.batch_execute(&sql).await.expect("database created");
    }
    let mut config = config;
    config.dbname(name);
    connect(config).await
}

fn expand(debug: bool) -> String {
    let log = if debug {
        LogLevel::Debug
    } else {
        LogLevel::None
    };
    build(SOURCE, log, LogTarget::Table)
}

/// `source` built as `pgweave build --log … --log-target …` does.
fn build(source: &str, log: LogLevel, target: LogTarget) -> String {
    let i = Instrumentation { log, target };
    let exp = pgweave_macro::expand_source(source, &MacroSet::builtin().with_instrumentation(i));
    assert!(exp.diagnostics.is_empty(), "{:?}", exp.diagnostics);
    format!("{}\n{}", i.setup_sql(), exp.sql)
}

async fn count(client: &Client, sql: &str) -> i64 {
    client.query_one(sql, &[]).await.expect(sql).get(0)
}

/// Rows left in the session's isolated temp tables.
async fn leftover_rows(client: &Client) -> i64 {
    let tables = client
        .query(
            "SELECT relname::text FROM pg_class WHERE relnamespace = pg_my_temp_schema() \
             AND relkind = 'r'",
            &[],
        )
        .await
        .expect("temp tables");
    assert!(!tables.is_empty(), "the temp tables were created");
    let mut rows = 0;
    for t in tables {
        let name: String = t.get(0);
        rows += count(client, &format!("SELECT count(*) FROM pg_temp.{name}")).await;
    }
    rows
}

#[tokio::test]
#[ignore = "requires PostgreSQL: make oracle"]
async fn nested_and_looped_runs_keep_their_rows() {
    let url = std::env::var(pgweave_oracle::URL_ENV).expect("PGWEAVE_ORACLE_URL");
    let client = fresh(&url, "pgweave_instrument").await;
    client
        .batch_execute(&expand(false))
        .await
        .unwrap_or_else(|e| panic!("{e:?}\n{}", expand(false)));

    // 5 nested levels of 3 rows each.
    assert_eq!(count(&client, "SELECT collect(4)").await, 15);
    assert_eq!(leftover_rows(&client).await, 0, "rows deleted on return");
    // Sequential runs from a loop, each nesting the levels below it.
    assert_eq!(count(&client, "SELECT many()").await, 3 + 6 + 9 + 12 + 15);
    assert_eq!(leftover_rows(&client).await, 0);
    // A procedure and a trigger function clean up as well.
    client.batch_execute("CALL refresh(3)").await.expect("CALL");
    client
        .batch_execute("INSERT INTO events VALUES (1), (2)")
        .await
        .expect("trigger");
    assert_eq!(leftover_rows(&client).await, 0);
    // A failing run leaves nothing either: its rows roll back.
    let failed = client.batch_execute("SELECT collect(2) / 0").await;
    assert!(failed.is_err());
    assert_eq!(leftover_rows(&client).await, 0);
}

#[tokio::test]
#[ignore = "requires PostgreSQL: make oracle"]
async fn snapshots_show_each_run_in_a_debug_build() {
    let url = std::env::var(pgweave_oracle::URL_ENV).expect("PGWEAVE_ORACLE_URL");
    let client = fresh(&url, "pgweave_instrument_debug").await;
    client
        .batch_execute(&expand(true))
        .await
        .unwrap_or_else(|e| panic!("{e:?}\n{}", expand(true)));
    assert_eq!(count(&client, "SELECT collect(2)").await, 9);
    client.batch_execute("CALL refresh(5)").await.expect("CALL");
    client
        .batch_execute("INSERT INTO events VALUES (7)")
        .await
        .expect("trigger");
    assert_eq!(
        count(
            &client,
            "SELECT count(*) FROM pgweave_debug.snapshot s JOIN pgweave_debug.snapshot_rows r \
             USING (run, label) WHERE s.routine = 'refresh' AND s.variables->>'p' = '5' \
             AND r.row_data->>'id' = '5'"
        )
        .await,
        1
    );
    assert_eq!(
        count(
            &client,
            "SELECT count(*) FROM pgweave_debug.snapshot WHERE routine = 'audit_fn' \
             AND variables->'NEW'->>'id' = '7' AND variables->>'TG_OP' = 'INSERT'"
        )
        .await,
        1
    );

    // One snapshot per level, with its variables.
    let levels = client
        .query(
            "SELECT (variables->>'level')::int, run FROM pgweave_debug.snapshot \
             WHERE routine = 'collect' AND label = 'filled' ORDER BY id",
            &[],
        )
        .await
        .expect("snapshots");
    let seen: Vec<i32> = levels.iter().map(|r| r.get(0)).collect();
    assert_eq!(seen, vec![2, 1, 0]);
    // Each snapshot holds the 3 rows of its own run, without the hidden column.
    for row in &levels {
        let level: i32 = row.get(0);
        let run: uuid_like::Uuid = row.get(1);
        let rows = client
            .query(
                "SELECT (row_data->>'lvl')::int, row_data ? '_pgweave_run' \
                 FROM pgweave_debug.snapshot_rows WHERE run = $1 AND table_name = 'work'",
                &[&run],
            )
            .await
            .expect("snapshot rows");
        assert_eq!(rows.len(), 3, "level {level}");
        for r in rows {
            assert_eq!(r.get::<_, i32>(0), level);
            assert!(!r.get::<_, bool>(1), "hidden column left out");
        }
    }
}

const LOGGED: &str = r#"
CREATE TABLE items (id INT PRIMARY KEY, n INT NOT NULL);
INSERT INTO items SELECT g, 0 FROM generate_series(1, 5) AS g;

-- Returns the row count the body reads itself, after a logged statement.
@function touch(k INT): INT {
    DECLARE c INT;
    @temp_table picked (id INT);
    INSERT INTO picked SELECT id FROM items WHERE id <= k;
    UPDATE items SET n = n + 1 WHERE id IN (SELECT id FROM picked);
    GET DIAGNOSTICS c = ROW_COUNT;
    IF k = 0 THEN
        RETURN c / k;
    END IF;
    RETURN c;
EXCEPTION WHEN division_by_zero THEN
    RETURN -1;
}

@function seen(k INT): BOOLEAN {
    UPDATE items SET n = n WHERE id = k;
    RETURN FOUND;
}

@procedure reset() {
    UPDATE items SET n = 0;
    DELETE FROM items WHERE id > 100;
}
"#;

/// 1-based line of `needle` in [`LOGGED`].
fn line_of(needle: &str) -> i32 {
    let at = LOGGED.find(needle).expect(needle);
    LOGGED[..at].matches('\n').count() as i32 + 1
}

#[tokio::test]
#[ignore = "requires PostgreSQL: make oracle"]
async fn logged_runs_and_steps_land_in_the_log_tables() {
    let url = std::env::var(pgweave_oracle::URL_ENV).expect("PGWEAVE_ORACLE_URL");
    let client = fresh(&url, "pgweave_log_table").await;
    let sql = build(LOGGED, LogLevel::Step, LogTarget::Table);
    client
        .batch_execute(&sql)
        .await
        .unwrap_or_else(|e| panic!("{e:?}\n{sql}"));

    // The body reads ROW_COUNT and FOUND after the logged statements.
    assert_eq!(count(&client, "SELECT touch(3)::bigint").await, 3);
    let seen: bool = client
        .query_one("SELECT seen(1)", &[])
        .await
        .expect("seen")
        .get(0);
    assert!(seen, "FOUND of the UPDATE, kept across the log");
    let unseen: bool = client
        .query_one("SELECT seen(99)", &[])
        .await
        .expect("seen")
        .get(0);
    assert!(!unseen);

    let steps = client
        .query(
            "SELECT s.step, s.kind, s.target, s.line, s.row_count FROM pgweave_log.step s \
             JOIN pgweave_log.run r USING (run) WHERE r.routine = 'touch' ORDER BY s.step",
            &[],
        )
        .await
        .expect("steps");
    let steps: Vec<(i32, String, String, i32, i64)> = steps
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2), r.get(3), r.get(4)))
        .collect();
    assert_eq!(
        steps,
        vec![
            (
                1,
                "INSERT".into(),
                "picked".into(),
                line_of("INSERT INTO picked"),
                3
            ),
            (
                2,
                "UPDATE".into(),
                "items".into(),
                line_of("UPDATE items SET n = n + 1"),
                3
            ),
        ]
    );
    let run = client
        .query_one(
            "SELECT steps, error, duration_ms >= 0 FROM pgweave_log.run WHERE routine = 'touch'",
            &[],
        )
        .await
        .expect("run");
    assert_eq!(run.get::<_, Option<i32>>(0), Some(2));
    assert_eq!(run.get::<_, Option<String>>(1), None);
    assert!(run.get::<_, bool>(2));

    // An error handled by the routine: logged with its SQLSTATE.
    assert_eq!(count(&client, "SELECT touch(0)::bigint").await, -1);
    assert_eq!(
        count(
            &client,
            "SELECT count(*) FROM pgweave_log.run WHERE routine = 'touch' AND error = '22012'"
        )
        .await,
        1
    );
    // A run in a statement that fails is rolled back, its log with it.
    let before = count(&client, "SELECT count(*) FROM pgweave_log.run").await;
    assert!(client.batch_execute("SELECT touch(2) / 0").await.is_err());
    assert_eq!(
        count(&client, "SELECT count(*) FROM pgweave_log.run").await,
        before
    );
    // A procedure ends without RETURN; the caller's traceparent is kept.
    let trace = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";
    client
        .batch_execute(&format!(
            "SELECT set_config('pgweave.traceparent', '{trace}', false); CALL reset()"
        ))
        .await
        .expect("CALL");
    let reset = client
        .query_one(
            "SELECT steps, traceparent FROM pgweave_log.run WHERE routine = 'reset'",
            &[],
        )
        .await
        .expect("reset logged");
    assert_eq!(reset.get::<_, Option<i32>>(0), Some(2));
    assert_eq!(reset.get::<_, Option<String>>(1).as_deref(), Some(trace));
    // Temp tables are still cleaned up.
    assert_eq!(leftover_rows(&client).await, 0);
}

#[tokio::test]
#[ignore = "requires PostgreSQL: make oracle"]
async fn logged_runs_are_notified() {
    let url = std::env::var(pgweave_oracle::URL_ENV).expect("PGWEAVE_ORACLE_URL");
    drop(fresh(&url, "pgweave_log_notify").await);
    let mut config: tokio_postgres::Config = url.parse().expect("URL");
    config.dbname("pgweave_log_notify");
    let (client, mut conn) = config.connect(NoTls).await.expect("connection");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(Ok(m)) = std::future::poll_fn(|cx| conn.poll_message(cx)).await {
            if let tokio_postgres::AsyncMessage::Notification(n) = m {
                let _ = tx.send((n.channel().to_string(), n.payload().to_string()));
            }
        }
    });
    let sql = build(LOGGED, LogLevel::Step, LogTarget::Notify);
    assert!(!sql.contains("pgweave_log.run"), "no table with notify");
    client
        .batch_execute(&sql)
        .await
        .unwrap_or_else(|e| panic!("{e:?}\n{sql}"));
    client
        .batch_execute("LISTEN pgweave_log; SELECT touch(2)")
        .await
        .expect("touch");

    // Two steps, then the run.
    let mut got = Vec::new();
    while got.len() < 3 {
        let next = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("notification in time")
            .expect("connection open");
        assert_eq!(next.0, "pgweave_log");
        got.push(serde_json::from_str::<serde_json::Value>(&next.1).expect("JSON payload"));
    }
    let kinds: Vec<(&str, &str)> = got
        .iter()
        .map(|v| (v["type"].as_str().unwrap(), v["routine"].as_str().unwrap()))
        .collect();
    assert_eq!(
        kinds,
        vec![("step", "touch"), ("step", "touch"), ("run", "touch")]
    );
    assert_eq!(got[0]["kind"], "INSERT");
    assert_eq!(got[0]["row_count"], 2);
    assert_eq!(got[1]["step"], 2);
    assert_eq!(got[2]["steps"], 2);
    assert_eq!(got[0]["run"], got[2]["run"], "one run");
}

const TRACED: &str = r#"
CREATE TABLE orders (id INT PRIMARY KEY, total INT NOT NULL DEFAULT 0);
CREATE TABLE lines (order_id INT NOT NULL, amount INT NOT NULL);
INSERT INTO orders VALUES (1), (2);

-- Each line adds to its order's total, through a trigger.
@trigger sum_lines after insert on lines for each row {
    UPDATE orders SET total = total + NEW.amount WHERE id = NEW.order_id;
    RETURN NEW;
}

-- The calls of a RETURN are runs of this routine.
@function add_line(o INT, a INT): INT {
    INSERT INTO lines VALUES (o, a);
    RETURN total_of(o);
}

@function total_of(o INT): INT {
    RETURN (SELECT total FROM orders WHERE id = o);
}

@function fails(): INT {
    RETURN 1 / 0;
}

-- A nested run fails, handled: the next one still has the right parent.
@function guarded(o INT): INT {
    DECLARE r INT;
    BEGIN
        r := fails();
    EXCEPTION WHEN division_by_zero THEN
        r := 0;
    END;
    RETURN add_line(o, 5);
}

-- Never ends by itself.
@function spin(n INT): INT {
    RETURN spin(n + 1);
}
"#;

#[tokio::test]
#[ignore = "requires PostgreSQL: make oracle"]
async fn runs_know_the_run_that_caused_them() {
    let url = std::env::var(pgweave_oracle::URL_ENV).expect("PGWEAVE_ORACLE_URL");
    let client = fresh(&url, "pgweave_log_tree").await;
    let sql = build(TRACED, LogLevel::Step, LogTarget::Table);
    client
        .batch_execute(&sql)
        .await
        .unwrap_or_else(|e| panic!("{e:?}\n{sql}"));

    assert_eq!(count(&client, "SELECT guarded(1)::bigint").await, 5);
    let runs = client
        .query(
            "SELECT r.routine, r.depth, p.routine FROM pgweave_log.run r \
             LEFT JOIN pgweave_log.run p ON p.run = r.parent ORDER BY r.depth, r.routine",
            &[],
        )
        .await
        .expect("runs");
    let runs: Vec<(String, i32, Option<String>)> = runs
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2)))
        .collect();
    // `fails` left with its error: rolled back with its log.
    assert_eq!(
        runs,
        vec![
            ("guarded".into(), 1, None),
            ("add_line".into(), 2, Some("guarded".into())),
            ("sum_lines_fn".into(), 3, Some("add_line".into())),
            ("total_of".into(), 3, Some("add_line".into())),
        ]
    );
    // A child runs within its parent.
    assert_eq!(
        count(
            &client,
            "SELECT count(*) FROM pgweave_log.run c JOIN pgweave_log.run p ON p.run = c.parent \
             WHERE c.started_at < p.started_at OR c.started_at + c.duration_ms * interval '1 ms' \
             > p.started_at + p.duration_ms * interval '1 ms' + interval '1 ms'"
        )
        .await,
        0
    );
    // Once the outermost run ends, the next statement starts a new tree.
    let after = client
        .query_one(
            "SELECT guarded(2), current_setting('pgweave.depth'), \
             current_setting('pgweave.span')",
            &[],
        )
        .await
        .expect("settings");
    assert_eq!(after.get::<_, String>(1), "0");
    assert_eq!(after.get::<_, String>(2), "");

    // A run beyond `pgweave.max_depth` fails, naming itself.
    let err = client
        .batch_execute("SET pgweave.max_depth = 20; SELECT spin(0)")
        .await
        .expect_err("depth limit");
    let db = err.as_db_error().expect("database error");
    assert_eq!(db.code().code(), "54001");
    assert!(
        db.message().contains("spin called at depth 21"),
        "{}",
        db.message()
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL: make oracle"]
async fn noticed_runs_survive_their_failure() {
    let url = std::env::var(pgweave_oracle::URL_ENV).expect("PGWEAVE_ORACLE_URL");
    drop(fresh(&url, "pgweave_log_notice").await);
    let mut config: tokio_postgres::Config = url.parse().expect("URL");
    config.dbname("pgweave_log_notice");
    let (client, mut conn) = config.connect(NoTls).await.expect("connection");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(Ok(m)) = std::future::poll_fn(|cx| conn.poll_message(cx)).await {
            if let tokio_postgres::AsyncMessage::Notice(n) = m
                && n.message() == "pgweave_log"
            {
                let _ = tx.send(n.detail().unwrap_or_default().to_string());
            }
        }
    });
    let sql = build(TRACED, LogLevel::Step, LogTarget::Notice);
    assert!(!sql.contains("pgweave_log.run"), "no table with notice");
    client
        .batch_execute(&sql)
        .await
        .unwrap_or_else(|e| panic!("{e:?}\n{sql}"));
    // Runs emit only under a sampled trace: none without a traceparent, nor
    // with one not sampled or malformed — which never fails the routine.
    for (total, traceparent) in (1..).zip([
        None,
        Some("00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-00"),
        Some("not a traceparent"),
    ]) {
        if let Some(tp) = traceparent {
            client
                .batch_execute(&format!("SET pgweave.traceparent = '{tp}'"))
                .await
                .expect("traceparent");
        }
        assert_eq!(count(&client, "SELECT add_line(1, 1)::bigint").await, total);
        client.batch_execute("SELECT 1").await.expect("round trip");
        assert!(rx.try_recv().is_err(), "unsampled: {traceparent:?}");
    }
    // Not sampled, no limit: the depth is not even kept.
    let depth: Option<String> = client
        .query_one("SELECT current_setting('pgweave.depth', true)", &[])
        .await
        .expect("depth")
        .get(0);
    assert!(depth.as_ref().is_none_or(|d| d.is_empty()), "{depth:?}");
    // Not sampled, with a limit: an endless recursion still stops there.
    let err = client
        .batch_execute("SET pgweave.max_depth = 4; SELECT spin(0)")
        .await
        .expect_err("depth limit");
    let db = err.as_db_error().expect("db error");
    assert_eq!(
        (db.code().code(), db.message()),
        (
            "54001",
            "pgweave: spin called at depth 5, beyond pgweave.max_depth"
        )
    );
    client
        .batch_execute("RESET pgweave.max_depth; SELECT 1")
        .await
        .expect("reset");
    assert!(rx.try_recv().is_err(), "unsampled recursion: no notice");
    client
        .batch_execute(
            "SET pgweave.traceparent = '00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01'",
        )
        .await
        .expect("traceparent");

    let mut next = async || {
        let detail = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("notice in time")
            .expect("connection open");
        serde_json::from_str::<serde_json::Value>(&detail).expect("JSON detail")
    };

    assert_eq!(count(&client, "SELECT add_line(2, 3)::bigint").await, 3);
    let mut events = Vec::new();
    for _ in 0..6 {
        events.push(next().await);
    }
    // An end event names only its run: the start said the rest.
    let start_of = |run: &serde_json::Value| {
        events
            .iter()
            .find(|e| e["type"] == "start" && &e["run"] == run)
            .expect("start before end")
    };
    let seen: Vec<(&str, &str, i64)> = events
        .iter()
        .map(|e| {
            let s = start_of(&e["run"]);
            (
                e["type"].as_str().unwrap(),
                s["routine"].as_str().unwrap(),
                s["depth"].as_i64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        seen,
        vec![
            ("start", "add_line", 1),
            ("start", "sum_lines_fn", 2),
            ("end", "sum_lines_fn", 2),
            ("start", "total_of", 2),
            ("end", "total_of", 2),
            ("end", "add_line", 1),
        ]
    );
    assert_eq!(events[1]["parent"], events[0]["run"]);
    assert_eq!(events[0]["parent"], serde_json::Value::Null);
    // The traceparent on the outermost run only; the end refers to the start.
    assert_eq!(
        events[0]["traceparent"],
        "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01"
    );
    assert_eq!(events[1]["traceparent"], serde_json::Value::Null);
    assert_eq!(events[5].as_object().unwrap().len(), 5, "{}", events[5]);
    let steps = events[5]["steps"].as_array().expect("steps");
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0]["kind"], "INSERT");
    assert_eq!(steps[0]["target"], "lines");

    // An endless recursion: the transaction fails, the starts are seen.
    let err = client
        .batch_execute("SET pgweave.max_depth = 5; SELECT spin(0)")
        .await
        .expect_err("depth limit");
    assert_eq!(err.as_db_error().expect("db error").code().code(), "54001");
    let mut chain = Vec::new();
    for depth in 1..=5 {
        let e = next().await;
        assert_eq!(
            (e["type"].as_str(), e["depth"].as_i64()),
            (Some("start"), Some(depth))
        );
        if let Some(parent) = chain.last() {
            assert_eq!(&e["parent"], parent);
        }
        chain.push(e["run"].clone());
    }
    assert!(rx.try_recv().is_err(), "no end: the runs failed");
}

/// `uuid` read as text: the oracle does not depend on a uuid crate.
mod uuid_like {
    use tokio_postgres::types::{FromSql, IsNull, ToSql, Type, to_sql_checked};

    #[derive(Debug)]
    pub struct Uuid([u8; 16]);

    impl<'a> FromSql<'a> for Uuid {
        fn from_sql(
            _: &Type,
            raw: &'a [u8],
        ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
            Ok(Uuid(raw.try_into()?))
        }
        fn accepts(ty: &Type) -> bool {
            *ty == Type::UUID
        }
    }

    impl ToSql for Uuid {
        fn to_sql(
            &self,
            _: &Type,
            out: &mut tokio_postgres::types::private::BytesMut,
        ) -> Result<IsNull, Box<dyn std::error::Error + Sync + Send>> {
            out.extend_from_slice(&self.0);
            Ok(IsNull::No)
        }
        fn accepts(ty: &Type) -> bool {
            *ty == Type::UUID
        }
        to_sql_checked!();
    }
}
