//! Runs of the logged routines become spans under the caller's span, on
//! PostgreSQL: a routine, the trigger it fires and the routine its RETURN
//! calls, nested; an endless recursion that fails its statement, still seen.

#![cfg(feature = "opentelemetry")]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use opentelemetry::Context;
use opentelemetry::trace::{SpanId, Status, TraceId, TracerProvider as _};
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SpanData};
use pgzest_macro::{Instrumentation, LogLevel, LogTarget, MacroSet};
use pgzest_runtime::trace::{self, Tracing};
use tokio_postgres::{AsyncMessage, Client, NoTls};

const URL_ENV: &str = "PGZEST_ORACLE_URL";

const SOURCE: &str = r#"
CREATE TABLE orders (id INT PRIMARY KEY, total INT NOT NULL DEFAULT 0);
CREATE TABLE lines (order_id INT NOT NULL, amount INT NOT NULL);
INSERT INTO orders VALUES (1);

@trigger sum_lines after insert on lines for each row {
    UPDATE orders SET total = total + NEW.amount WHERE id = NEW.order_id;
    RETURN NEW;
}

@function add_line(o INT, a INT): INT {
    INSERT INTO lines VALUES (o, a);
    RETURN total_of(o);
}

@function total_of(o INT): INT {
    RETURN (SELECT total FROM orders WHERE id = o);
}

@function spin(n INT): INT {
    RETURN spin(n + 1);
}
"#;

const TRACEPARENT: &str = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";

async fn database(url: &str, name: &str) -> tokio_postgres::Config {
    let mut config: tokio_postgres::Config = url.parse().expect("URL");
    let (admin, conn) = config.connect(NoTls).await.expect("connection");
    tokio::spawn(conn);
    for sql in [
        format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"),
        format!("CREATE DATABASE {name}"),
    ] {
        admin.batch_execute(&sql).await.expect("database created");
    }
    config.dbname(name);
    config
}

/// A client whose `pgzest_log` notices go to `tracing`, and the other
/// messages to the returned list.
async fn traced(
    config: &tokio_postgres::Config,
    tracing: &Tracing,
) -> (Client, Arc<Mutex<Vec<String>>>) {
    let (client, connection) = config.connect(NoTls).await.expect("connection");
    let t = tracing.clone();
    let others = Arc::new(Mutex::new(Vec::new()));
    let seen = others.clone();
    tokio::spawn(async move {
        t.drive(connection, move |m| {
            seen.lock().unwrap().push(match m {
                Ok(AsyncMessage::Notice(n)) => format!("notice {}", n.message()),
                Ok(AsyncMessage::Notification(n)) => {
                    format!("notification {} {}", n.channel(), n.payload())
                }
                Ok(other) => format!("{other:?}"),
                Err(e) => format!("error {e}"),
            })
        })
        .await
    });
    (client, others)
}

fn spans(provider: &SdkTracerProvider, exporter: &InMemorySpanExporter) -> Vec<SpanData> {
    provider.force_flush().expect("flushed");
    let spans = exporter.get_finished_spans().expect("spans");
    exporter.reset();
    spans
}

fn named<'a>(spans: &'a [SpanData], name: &str) -> &'a SpanData {
    spans
        .iter()
        .find(|s| s.name == name)
        .unwrap_or_else(|| panic!("span {name}: {spans:#?}"))
}

#[tokio::test]
#[ignore = "requires PostgreSQL: make oracle"]
async fn runs_become_spans_under_the_callers_span() {
    let url = std::env::var(URL_ENV).expect("PGZEST_ORACLE_URL");
    let config = database(&url, "pgzest_runtime_trace").await;
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let tracer = provider.tracer("pgzest");
    let tracing = Tracing::new(move |run| trace::otel::export(&tracer, &run, &Context::new()));
    let (client, others) = traced(&config, &tracing).await;

    let i = Instrumentation {
        log: LogLevel::Step,
        target: LogTarget::Notice,
    };
    let exp = pgzest_macro::expand_source(SOURCE, &MacroSet::builtin().with_instrumentation(i));
    assert!(exp.diagnostics.is_empty(), "{:?}", exp.diagnostics);
    client.batch_execute(&exp.sql).await.expect("schema");

    trace::set_traceparent(&client, TRACEPARENT, false)
        .await
        .expect("traceparent");
    let total: i32 = client
        .query_one("SELECT add_line(1, 3)", &[])
        .await
        .expect("add_line")
        .get(0);
    assert_eq!(total, 3);

    let got = spans(&provider, &exporter);
    assert_eq!(got.len(), 3, "{got:#?}");
    let caller = TraceId::from_hex("0af7651916cd43dd8448eb211c80319c").unwrap();
    let add = named(&got, "add_line");
    assert_eq!(add.span_context.trace_id(), caller);
    assert_eq!(
        add.parent_span_id,
        SpanId::from_hex("b7ad6b7169203331").unwrap()
    );
    for child in ["sum_lines_fn", "total_of"] {
        let s = named(&got, child);
        assert_eq!(s.span_context.trace_id(), caller);
        assert_eq!(s.parent_span_id, add.span_context.span_id(), "{child}");
        assert!(s.start_time >= add.start_time && s.end_time <= add.end_time);
    }
    let steps: Vec<&str> = add.events.iter().map(|e| e.name.as_ref()).collect();
    assert_eq!(steps, ["INSERT"]);
    assert_eq!(add.status, Status::Unset);

    // An endless recursion fails its statement: its runs never end, they
    // are closed as failed, nested.
    let err = client
        .batch_execute("SET pgzest.max_depth = 3; SELECT spin(0)")
        .await
        .expect_err("depth limit");
    tracing.fail_open(&err);
    let got = spans(&provider, &exporter);
    assert_eq!(got.len(), 3, "{got:#?}");
    let mut parent = SpanId::from_hex("b7ad6b7169203331").unwrap();
    for depth in 1..=3i64 {
        let s = got
            .iter()
            .find(|s| {
                s.attributes
                    .iter()
                    .any(|kv| kv.key.as_str() == "pgzest.depth" && kv.value == depth.into())
            })
            .expect("span at depth");
        assert_eq!(s.parent_span_id, parent, "depth {depth}");
        assert!(
            matches!(&s.status, Status::Error { description }
                if description.starts_with("54001: pgzest: spin called at depth 4")),
            "{:?}",
            s.status
        );
        parent = s.span_context.span_id();
    }
    // Nothing left open: the next statement starts clean.
    let total: i32 = client
        .query_one("SELECT add_line(1, 1)", &[])
        .await
        .expect("add_line")
        .get(0);
    assert_eq!(total, 4);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(spans(&provider, &exporter).len(), 3);

    // The other messages reach the application: notifications included
    // (sent at commit, after the notice).
    assert!(others.lock().unwrap().is_empty(), "{others:?}");
    client
        .batch_execute(
            "LISTEN orders_changed; NOTIFY orders_changed, 'order 1'; \
             DO $$ BEGIN RAISE NOTICE 'plain'; END $$",
        )
        .await
        .expect("listen, notify, notice");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        *others.lock().unwrap(),
        ["notice plain", "notification orders_changed order 1"]
    );
}
