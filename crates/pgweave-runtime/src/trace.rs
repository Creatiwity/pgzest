//! Runs of the logged PL/pgSQL routines, as traces (PLAN.md P9).
//!
//! Routines built with `[log] target = "notice"` raise a notice
//! `pgweave_log` when a run starts and when it ends, its JSON in `DETAIL`:
//! the routine, its run, the run that caused it (calling routine, or the
//! statement that fired a trigger), its depth, the caller's
//! `pgweave.traceparent`, and at the end its duration and steps. Notices
//! reach the client at once, even when the transaction then fails: a run
//! that never ends — an endless trigger cascade stopped by
//! `pgweave.max_depth` or `statement_timeout` — is still seen starting.
//!
//! [`Tracing`] reads them off the connection ([`Tracing::drive`] replaces
//! `connection.await`) and hands each finished tree of runs to a sink: the
//! outermost run with the runs it caused, nested. [`Tracing::fail_open`]
//! closes the runs a failed statement left open. With the `opentelemetry`
//! feature, [`otel::export`] turns a tree into spans under the caller's
//! span (an OTLP exporter brings them to Sentry, Jaeger, Tempo…).
//!
//! ```ignore
//! let tracing = Tracing::new(|run| otel::export(&tracer, &run, &Context::current()));
//! let (client, connection) = config.connect(NoTls).await?;
//! tokio::spawn({ let t = tracing.clone(); async move { t.drive(connection, |_| {}).await } });
//! trace::set_traceparent(&client, &traceparent, false).await?;
//! if let Err(e) = client.query("SELECT add_line($1, $2)", &[&1, &5]).await {
//!     tracing.fail_open(&e);
//! }
//! ```

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use futures_util::StreamExt;
use tokio_postgres::error::DbError;
use tokio_postgres::{AsyncMessage, Connection, GenericClient};

/// Message of the notices the routines raise (`DETAIL` holds the JSON).
pub const NOTICE: &str = "pgweave_log";
/// Setting the routines read the caller's W3C `traceparent` from.
pub const TRACEPARENT_SETTING: &str = "pgweave.traceparent";

/// Sets the `traceparent` the routines run under: for the current
/// transaction when `local`, otherwise for the session (a pooled connection
/// keeps it for its next user: prefer `local` inside a transaction).
pub async fn set_traceparent<C: GenericClient>(
    client: &C,
    traceparent: &str,
    local: bool,
) -> Result<(), tokio_postgres::Error> {
    client
        .execute(
            "SELECT set_config('pgweave.traceparent', $1, $2)",
            &[&traceparent, &local],
        )
        .await
        .map(|_| ())
}

/// W3C trace context of the caller: `00-<trace id>-<span id>-<flags>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TraceParent {
    pub trace_id: [u8; 16],
    pub span_id: [u8; 8],
    pub flags: u8,
}

impl TraceParent {
    /// Version `00` only; anything else is `None`, never half-read.
    pub fn parse(s: &str) -> Option<Self> {
        let mut parts = s.trim().split('-');
        let (version, trace, span, flags) =
            (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
        if version != "00" || parts.next().is_some() {
            return None;
        }
        let trace_id = hex::<16>(trace)?;
        let span_id = hex::<8>(span)?;
        let [flags] = hex::<1>(flags)?;
        // All-zero ids are invalid (W3C).
        if trace_id == [0; 16] || span_id == [0; 8] {
            return None;
        }
        Some(TraceParent {
            trace_id,
            span_id,
            flags,
        })
    }
}

fn hex<const N: usize>(s: &str) -> Option<[u8; N]> {
    if s.len() != 2 * N || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; N];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

/// One event of a run, read from a notice.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Start(RunInfo),
    End {
        run: RunInfo,
        duration: Duration,
        steps: Vec<Step>,
        /// `SQLSTATE` of an error the routine handled itself.
        handled_error: Option<String>,
    },
}

/// What names a run, in each of its events.
#[derive(Debug, Clone, PartialEq)]
pub struct RunInfo {
    /// Run id (uuid, as text).
    pub run: String,
    pub routine: String,
    /// Run of the logged routine that caused this one.
    pub parent: Option<String>,
    /// 1 for a run no logged routine caused.
    pub depth: u32,
    /// The caller's `pgweave.traceparent`, as set.
    pub traceparent: Option<String>,
    pub started_at: SystemTime,
}

/// An INSERT, UPDATE, DELETE or EXECUTE of the run (`step` level).
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    pub kind: String,
    pub target: Option<String>,
    /// Line in the source file of the routine.
    pub line: Option<u32>,
    pub row_count: i64,
    pub executed_at: SystemTime,
}

impl Event {
    /// The event of a notice: `None` for another notice, an error for a
    /// `pgweave_log` notice that does not read as one.
    pub fn from_notice(notice: &DbError) -> Option<Result<Event, String>> {
        (notice.message() == NOTICE).then(|| {
            let detail = notice.detail().ok_or("pgweave_log notice without DETAIL")?;
            Event::from_json(detail)
        })
    }

    /// The event of a notice's `DETAIL`.
    pub fn from_json(detail: &str) -> Result<Event, String> {
        let v: serde_json::Value =
            serde_json::from_str(detail).map_err(|e| format!("pgweave_log detail: {e}"))?;
        let text = |key: &str| -> Result<Option<String>, String> {
            match &v[key] {
                serde_json::Value::Null => Ok(None),
                serde_json::Value::String(s) => Ok(Some(s.clone())),
                other => Err(format!("pgweave_log detail: `{key}` is {other}")),
            }
        };
        let required = |key: &str| -> Result<String, String> {
            text(key)?.ok_or_else(|| format!("pgweave_log detail: no `{key}`"))
        };
        let depth = v["depth"]
            .as_u64()
            .and_then(|d| u32::try_from(d).ok())
            .ok_or("pgweave_log detail: no `depth`")?;
        let info = RunInfo {
            run: required("run")?,
            routine: required("routine")?,
            parent: text("parent")?,
            depth,
            traceparent: text("traceparent")?,
            started_at: timestamp(&required("started_at")?)?,
        };
        match v["type"].as_str() {
            Some("start") => Ok(Event::Start(info)),
            Some("end") => {
                let ms = v["duration_ms"]
                    .as_f64()
                    .filter(|ms| ms.is_finite() && *ms >= 0.0)
                    .ok_or("pgweave_log detail: no `duration_ms`")?;
                let steps = match &v["steps"] {
                    serde_json::Value::Null => Vec::new(),
                    serde_json::Value::Array(items) => {
                        items.iter().map(step).collect::<Result<_, _>>()?
                    }
                    other => return Err(format!("pgweave_log detail: `steps` is {other}")),
                };
                Ok(Event::End {
                    run: info,
                    duration: Duration::from_secs_f64(ms / 1000.0),
                    steps,
                    handled_error: text("error")?,
                })
            }
            other => Err(format!("pgweave_log detail: unknown type {other:?}")),
        }
    }
}

fn step(v: &serde_json::Value) -> Result<Step, String> {
    Ok(Step {
        kind: v["kind"]
            .as_str()
            .ok_or("pgweave_log step: no `kind`")?
            .to_string(),
        target: v["target"].as_str().map(str::to_string),
        line: v["line"].as_u64().and_then(|l| u32::try_from(l).ok()),
        row_count: v["row_count"]
            .as_i64()
            .ok_or("pgweave_log step: no `row_count`")?,
        executed_at: timestamp(
            v["executed_at"]
                .as_str()
                .ok_or("pgweave_log step: no `executed_at`")?,
        )?,
    })
}

/// A `timestamptz` as JSON renders it (ISO 8601 with offset).
fn timestamp(s: &str) -> Result<SystemTime, String> {
    chrono::DateTime::parse_from_rfc3339(s)
        .map(SystemTime::from)
        .map_err(|e| format!("pgweave_log timestamp {s:?}: {e}"))
}

/// A run and the runs it caused, finished or left open by a failure.
#[derive(Debug, Clone, PartialEq)]
pub struct RoutineRun {
    pub info: RunInfo,
    pub end: SystemTime,
    pub steps: Vec<Step>,
    pub outcome: Outcome,
    /// In start order.
    pub children: Vec<RoutineRun>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Ok,
    /// Ended in its own exception handler, with this `SQLSTATE`.
    Handled(String),
    /// Never ended: the statement failed (message of the failure).
    Failed(String),
}

#[derive(Default)]
struct Node {
    info: Option<RunInfo>,
    end: Option<(SystemTime, Vec<Step>, Option<String>)>,
    children: Vec<String>,
}

/// Runs seen on a connection, assembled into trees.
#[derive(Default)]
pub struct Collector {
    nodes: HashMap<String, Node>,
    /// Outermost runs not yet finished, in start order.
    roots: Vec<String>,
}

impl Collector {
    /// Adds an event; returns the tree it completes, if any.
    pub fn push(&mut self, event: Event) -> Option<RoutineRun> {
        let (info, end) = match event {
            Event::Start(info) => (info, None),
            Event::End {
                run,
                duration,
                steps,
                handled_error,
            } => {
                let end = run.started_at + duration;
                (run, Some((end, steps, handled_error)))
            }
        };
        let id = info.run.clone();
        let known = self.nodes.contains_key(&id);
        if !known {
            // Attached to its parent when that one is open on this
            // connection; otherwise, the root of its own tree.
            match info.parent.as_ref().filter(|p| self.nodes.contains_key(*p)) {
                Some(parent) => self
                    .nodes
                    .get_mut(parent)
                    .expect("checked")
                    .children
                    .push(id.clone()),
                None => self.roots.push(id.clone()),
            }
        }
        let node = self.nodes.entry(id.clone()).or_default();
        node.info = Some(info);
        if end.is_some() {
            node.end = end;
        }
        let root = self.roots.contains(&id);
        (root && self.nodes[&id].end.is_some()).then(|| self.take(&id, None))
    }

    /// Closes every open tree as failed with `message` at `at`: runs left
    /// open by a failed statement.
    pub fn fail_open(&mut self, message: &str, at: SystemTime) -> Vec<RoutineRun> {
        let roots = std::mem::take(&mut self.roots);
        roots
            .iter()
            .map(|r| self.take(r, Some((message, at))))
            .collect()
    }

    fn take(&mut self, id: &str, failed: Option<(&str, SystemTime)>) -> RoutineRun {
        self.roots.retain(|r| r != id);
        let node = self.nodes.remove(id).expect("known run");
        let info = node.info.expect("a run is added with its info");
        let children: Vec<RoutineRun> =
            node.children.iter().map(|c| self.take(c, failed)).collect();
        let (end, steps, outcome) = match (node.end, failed) {
            (Some((end, steps, handled)), _) => {
                (end, steps, handled.map_or(Outcome::Ok, Outcome::Handled))
            }
            (None, Some((message, at))) => (at, Vec::new(), Outcome::Failed(message.to_string())),
            (None, None) => unreachable!("a finished tree has finished runs"),
        };
        RoutineRun {
            info,
            end,
            steps,
            outcome,
            children,
        }
    }
}

type Sink = dyn Fn(RoutineRun) + Send + Sync;

/// A connection's [`Collector`], shared by the task driving the connection
/// and the code running statements on it.
#[derive(Clone)]
pub struct Tracing {
    collector: Arc<Mutex<Collector>>,
    sink: Arc<Sink>,
}

impl Tracing {
    /// `sink` receives each finished tree.
    pub fn new(sink: impl Fn(RoutineRun) + Send + Sync + 'static) -> Self {
        Tracing {
            collector: Arc::default(),
            sink: Arc::new(sink),
        }
    }

    /// Handles a notice: `true` if it was a `pgweave_log` one. A
    /// `pgweave_log` notice that does not read is returned as an error.
    pub fn notice(&self, notice: &DbError) -> Result<bool, String> {
        let Some(event) = Event::from_notice(notice) else {
            return Ok(false);
        };
        let finished = self.lock().push(event?);
        if let Some(run) = finished {
            (self.sink)(run);
        }
        Ok(true)
    }

    /// Closes the runs the statement that failed with `error` left open
    /// (see [`Collector::fail_open`]). The notices of a statement come
    /// before its error on the connection: call it once the error is
    /// received.
    pub fn fail_open(&self, error: &tokio_postgres::Error) {
        let message = match error.as_db_error() {
            Some(db) => format!("{}: {}", db.code().code(), db.message()),
            None => error.to_string(),
        };
        let runs = self.lock().fail_open(&message, SystemTime::now());
        for run in runs {
            (self.sink)(run);
        }
    }

    /// Drives `connection` as `connection.await` does, handling the
    /// `pgweave_log` notices; `other` receives the other notices and the
    /// unreadable ones.
    pub async fn drive<S, T>(
        &self,
        connection: Connection<S, T>,
        mut other: impl FnMut(Result<DbError, String>),
    ) -> Result<(), tokio_postgres::Error>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
        T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        let mut connection = connection;
        let mut messages = futures_util::stream::poll_fn(move |cx| connection.poll_message(cx));
        while let Some(message) = messages.next().await {
            // Notifications have their own listeners.
            if let AsyncMessage::Notice(n) = message? {
                match self.notice(&n) {
                    Ok(true) => {}
                    Ok(false) => other(Ok(n)),
                    Err(e) => other(Err(e)),
                }
            }
        }
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Collector> {
        // A sink that panicked leaves the collector consistent: every
        // change is complete before the sink is called.
        self.collector.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Export to OpenTelemetry.
#[cfg(feature = "opentelemetry")]
pub mod otel {
    use opentelemetry::trace::{
        Span as _, SpanContext, SpanId, SpanKind, Status, TraceContextExt, TraceFlags, TraceId,
        TraceState, Tracer,
    };
    use opentelemetry::{Context, KeyValue};

    use super::{Outcome, RoutineRun, TraceParent};

    /// Exports `run` and the runs it caused as spans, nested as they ran.
    /// The outermost span's parent is the run's `traceparent` when it has
    /// one, `parent` otherwise.
    pub fn export<T: Tracer>(tracer: &T, run: &RoutineRun, parent: &Context)
    where
        T::Span: Send + Sync + 'static,
    {
        let remote = run
            .info
            .traceparent
            .as_deref()
            .and_then(TraceParent::parse)
            .map(|tp| {
                Context::new().with_remote_span_context(SpanContext::new(
                    TraceId::from_bytes(tp.trace_id),
                    SpanId::from_bytes(tp.span_id),
                    TraceFlags::new(tp.flags),
                    true,
                    TraceState::NONE,
                ))
            });
        emit(tracer, run, remote.as_ref().unwrap_or(parent));
    }

    fn emit<T: Tracer>(tracer: &T, run: &RoutineRun, parent: &Context)
    where
        T::Span: Send + Sync + 'static,
    {
        let mut attributes = vec![
            KeyValue::new("db.system.name", "postgresql"),
            KeyValue::new("pgweave.routine", run.info.routine.clone()),
            KeyValue::new("pgweave.run", run.info.run.clone()),
            KeyValue::new("pgweave.depth", i64::from(run.info.depth)),
        ];
        if let Outcome::Handled(state) = &run.outcome {
            attributes.push(KeyValue::new("pgweave.handled_sqlstate", state.clone()));
        }
        let mut span = tracer
            .span_builder(run.info.routine.clone())
            .with_kind(SpanKind::Internal)
            .with_start_time(run.info.started_at)
            .with_attributes(attributes)
            .start_with_context(tracer, parent);
        for s in &run.steps {
            let mut attrs = vec![
                KeyValue::new("pgweave.step.kind", s.kind.clone()),
                KeyValue::new("pgweave.step.row_count", s.row_count),
            ];
            if let Some(t) = &s.target {
                attrs.push(KeyValue::new("pgweave.step.target", t.clone()));
            }
            if let Some(l) = s.line {
                attrs.push(KeyValue::new("pgweave.step.line", i64::from(l)));
            }
            span.add_event_with_timestamp(s.kind.clone(), s.executed_at, attrs);
        }
        if let Outcome::Failed(message) = &run.outcome {
            span.set_status(Status::error(message.clone()));
        }
        let cx = Context::new().with_remote_span_context(span.span_context().clone());
        for child in &run.children {
            emit(tracer, child, &cx);
        }
        span.end_with_timestamp(run.end);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(json: &str) -> Event {
        Event::from_json(json).expect(json)
    }

    fn start(run: &str, routine: &str, parent: Option<&str>, depth: u32) -> String {
        let parent = parent.map_or("null".to_string(), |p| format!("\"{p}\""));
        format!(
            r#"{{"type":"start","run":"{run}","routine":"{routine}","parent":{parent},"depth":{depth},"traceparent":null,"started_at":"2026-11-04T10:00:00.000100+01:00"}}"#
        )
    }

    fn end(run: &str, routine: &str, parent: Option<&str>, depth: u32, ms: f64) -> String {
        let parent = parent.map_or("null".to_string(), |p| format!("\"{p}\""));
        format!(
            r#"{{"type":"end","run":"{run}","routine":"{routine}","parent":{parent},"depth":{depth},"traceparent":"00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01","started_at":"2026-11-04T10:00:00.000100+01:00","duration_ms":{ms},"steps":[{{"kind":"INSERT","target":"lines","line":12,"row_count":1,"executed_at":"2026-11-04T09:00:00.0002+00:00"}}],"error":null}}"#
        )
    }

    #[test]
    fn events_read_from_the_detail() {
        let Event::End {
            run,
            duration,
            steps,
            handled_error,
        } = event(&end("r1", "add_line", None, 1, 2.5))
        else {
            panic!("end");
        };
        assert_eq!(run.routine, "add_line");
        assert_eq!(duration, Duration::from_micros(2500));
        assert_eq!(handled_error, None);
        assert_eq!(steps[0].line, Some(12));
        assert_eq!(
            steps[0].executed_at.duration_since(run.started_at).unwrap(),
            Duration::from_micros(100)
        );
        for bad in [
            "{}",
            r#"{"type":"start"}"#,
            &start("r", "f", None, 1).replace("10:00:00", "10h"),
            &start("r", "f", None, 1).replace("start", "begin"),
        ] {
            assert!(Event::from_json(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_tree_is_finished_with_its_outermost_run() {
        let mut c = Collector::default();
        assert_eq!(c.push(event(&start("a", "add_line", None, 1))), None);
        assert_eq!(c.push(event(&start("b", "sum_fn", Some("a"), 2))), None);
        assert_eq!(c.push(event(&end("b", "sum_fn", Some("a"), 2, 1.0))), None);
        assert_eq!(c.push(event(&start("c", "total_of", Some("a"), 2))), None);
        assert_eq!(
            c.push(event(&end("c", "total_of", Some("a"), 2, 1.0))),
            None
        );
        let tree = c
            .push(event(&end("a", "add_line", None, 1, 3.0)))
            .expect("finished");
        let names: Vec<&str> = tree
            .children
            .iter()
            .map(|r| r.info.routine.as_str())
            .collect();
        assert_eq!(names, ["sum_fn", "total_of"]);
        assert_eq!(tree.outcome, Outcome::Ok);
        assert!(c.nodes.is_empty() && c.roots.is_empty());
    }

    #[test]
    fn runs_left_open_are_failed() {
        let mut c = Collector::default();
        c.push(event(&start("a", "spin", None, 1)));
        c.push(event(&start("b", "spin", Some("a"), 2)));
        let at = SystemTime::now();
        let trees = c.fail_open("stack depth", at);
        assert_eq!(trees.len(), 1);
        assert_eq!(trees[0].outcome, Outcome::Failed("stack depth".into()));
        assert_eq!(trees[0].children[0].end, at);
        assert!(c.fail_open("again", at).is_empty());
    }

    #[test]
    fn traceparents() {
        let tp = TraceParent::parse("00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01")
            .expect("valid");
        assert_eq!(tp.span_id, [0xb7, 0xad, 0x6b, 0x71, 0x69, 0x20, 0x33, 0x31]);
        assert_eq!(tp.flags, 1);
        for bad in [
            "01-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01",
            "00-00000000000000000000000000000000-b7ad6b7169203331-01",
            "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331",
            "00-0af7651916cd43dd8448eb211c80319c-b7ad6b716920333g-01",
        ] {
            assert_eq!(TraceParent::parse(bad), None, "{bad}");
        }
    }
}
