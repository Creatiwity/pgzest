//! Isolated temp tables and snapshots (PLAN.md P9), run on PostgreSQL: a
//! routine using a temp table, called recursively and from a loop in one
//! session, keeps its rows apart; its snapshots show each run.

use pgweave_macro::MacroSet;
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
    let exp = pgweave_macro::expand_source(SOURCE, &MacroSet::builtin().with_debug(debug));
    assert!(exp.diagnostics.is_empty(), "{:?}", exp.diagnostics);
    if debug {
        format!("{}\n{}", pgweave_macro::instrument::DEBUG_SETUP, exp.sql)
    } else {
        exp.sql
    }
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
