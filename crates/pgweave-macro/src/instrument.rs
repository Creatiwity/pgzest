//! Instrumentation of PL/pgSQL macro bodies (PLAN.md, P9): isolated temp
//! tables (`@temp_table`), debug snapshots (`@snapshot`) and run logs
//! (build option, [`Instrumentation`]).
//!
//! Runs on the expansion, after the macros are rendered: each body between
//! `$pgweave_macro$` tags that holds a directive (every PL/pgSQL one in a
//! logged build) is parsed with its routine,
//! and the accesses to its temp tables are rewritten from the AST, as text
//! edits whose positions come from the parser. Nothing is guessed: an
//! access that cannot be rewritten is an error, dynamic SQL a warning.
//!
//! ```text
//! @function close_month(m DATE): BIGINT {
//!     @temp_table work (order_id BIGINT PRIMARY KEY, amount INT);
//!     INSERT INTO work SELECT id, total FROM orders WHERE …;
//!     @snapshot filled;
//!     …
//! }
//! ```
//!
//! A session-scoped temp table is shared by every call of the session:
//! each run gets an id, stored in a hidden last column of every row, and
//! every access is restricted to the rows of the run. See PLAN.md P9 for
//! the generated shapes.

use std::ops::Range;

use pgweave_ast::emit::{emit_create_table, emit_ident};
use pgweave_ast::plpgsql::{Block, Declaration, PlStatement, PlStatementKind, VarType};
use pgweave_ast::stmt::{
    Cte, CteQuery, DeleteStmt, InsertSource, InsertStmt, JoinConstraint, OnConflictAction,
    OnConflictTarget, SelectItem, SelectStmt, Statement, TableRef, TableSource, TargetTable,
    UpdateStmt,
};
use pgweave_ast::{
    ColumnConstraint, ColumnDef, CreateTable, DataType, DdlStatement, Expr, FunctionBody, Ident,
    MapEdit, ObjectName, ReturnType, UtilityKind,
};
use pgweave_diagnostics::{Diagnostic, Span};

use crate::expand::{Expansion, MACRO_ERROR};
use crate::lex::{Kind, Token, tokenize};

/// Dollar quote of the bodies rendered by the PL/pgSQL macros.
pub const BODY_TAG: &str = "$pgweave_macro$";
/// Variable holding the id of the current run.
pub const RUN_VAR: &str = "_pgweave_run_id";
/// Hidden last column of an isolated temp table: the run owning the row.
pub const RUN_COLUMN: &str = "_pgweave_run";
/// Setting read by the hidden column's default, set before each insert.
pub const RUN_SETTING: &str = "pgweave.run";
/// Body directives, at the start of a line of a PL/pgSQL macro body.
pub const DIRECTIVES: [&str; 2] = ["temp_table", "snapshot"];
/// Diagnostic code: access to an isolated temp table pgweave cannot see.
pub const NOT_ISOLATED: &str = "not-isolated";
/// Diagnostic code: a routine a logged build leaves unlogged.
pub const NOT_LOGGED: &str = "not-logged";

/// Variables of a logged run.
const STARTED_VAR: &str = "_pgweave_started_at";
const TRACEPARENT_VAR: &str = "_pgweave_traceparent";
const ROWS_VAR: &str = "_pgweave_rows";
const STEPS_VAR: &str = "_pgweave_steps";
/// `FOUND` kept across the exit code, for a `RETURN` that reads it.
const FOUND_VAR: &str = "_pgweave_found";
/// Run of the logged routine this one was called from (directly, or by a
/// statement firing a trigger), and the call depth.
const PARENT_VAR: &str = "_pgweave_parent";
const DEPTH_VAR: &str = "_pgweave_depth";
/// Receives `set_config`, called in assignments: unlike `PERFORM`, they
/// leave `FOUND` alone.
const CONFIG_VAR: &str = "_pgweave_config";
/// Value of a `RETURN`, evaluated before the exit code.
const RESULT_VAR: &str = "_pgweave_result";

/// Tables written by `@snapshot` in a debug build. Created by
/// `pgweave build --log debug` before the routines.
pub const DEBUG_SETUP: &str = "\
CREATE SCHEMA IF NOT EXISTS pgweave_debug;
CREATE TABLE IF NOT EXISTS pgweave_debug.snapshot (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    run UUID NOT NULL,
    routine TEXT NOT NULL,
    label TEXT NOT NULL,
    taken_at TIMESTAMPTZ NOT NULL,
    variables JSONB NOT NULL
);
CREATE TABLE IF NOT EXISTS pgweave_debug.snapshot_rows (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    run UUID NOT NULL,
    label TEXT NOT NULL,
    table_name TEXT NOT NULL,
    row_data JSONB NOT NULL
);
";

/// Tables written by the `table` log target. Unlogged: a log is not worth
/// WAL, and it does not survive a crash.
pub const LOG_SETUP: &str = "\
CREATE SCHEMA IF NOT EXISTS pgweave_log;
CREATE UNLOGGED TABLE IF NOT EXISTS pgweave_log.run (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    run UUID NOT NULL,
    routine TEXT NOT NULL,
    parent UUID,
    depth INT NOT NULL,
    traceparent TEXT,
    started_at TIMESTAMPTZ NOT NULL,
    duration_ms DOUBLE PRECISION NOT NULL,
    steps INT,
    error TEXT
);
CREATE UNLOGGED TABLE IF NOT EXISTS pgweave_log.step (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    run UUID NOT NULL,
    step INT NOT NULL,
    kind TEXT NOT NULL,
    target TEXT,
    line INT,
    row_count BIGINT NOT NULL,
    executed_at TIMESTAMPTZ NOT NULL
);
";
/// `pg_notify` channel of the `notify` log target, and message of the
/// `notice` one.
pub const LOG_CHANNEL: &str = "pgweave_log";
/// Setting holding the run of the logged routine in progress, read as the
/// parent of the runs it causes (transaction-local).
pub const SPAN_SETTING: &str = "pgweave.span";
/// Setting holding the call depth of the logged routines (transaction-local).
pub const DEPTH_SETTING: &str = "pgweave.depth";
/// Optional limit of that depth: beyond it, a logged routine raises
/// `54001` (`statement_too_complex`) instead of running, naming itself.
pub const MAX_DEPTH_SETTING: &str = "pgweave.max_depth";
/// Setting read at the start of a logged run: the W3C `traceparent` of the
/// caller, if it set one (`set_config('pgweave.traceparent', …, true)`).
pub const TRACEPARENT_SETTING: &str = "pgweave.traceparent";

/// What a run of a PL/pgSQL macro routine records. Fixed at build time:
/// a level below emits no code at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum LogLevel {
    /// Nothing (analysis, migrations, deployment by default).
    #[default]
    None,
    /// One entry per run: routine, start, duration, handled error.
    Info,
    /// `Info`, plus one entry per INSERT, UPDATE, DELETE or EXECUTE, with
    /// its row count.
    Step,
    /// `Step`, plus the `@snapshot` copies.
    Debug,
}

impl LogLevel {
    /// The level as `--log` spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Info => "info",
            Self::Step => "step",
            Self::Debug => "debug",
        }
    }

    /// The level spelled as [`Self::as_str`] does.
    pub fn parse(s: &str) -> Option<Self> {
        [Self::None, Self::Info, Self::Step, Self::Debug]
            .into_iter()
            .find(|l| l.as_str() == s)
    }
}

/// Where the log entries go.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogTarget {
    /// `pgweave_log.run` / `pgweave_log.step` ([`LOG_SETUP`]).
    #[default]
    Table,
    /// `pg_notify('pgweave_log', json)`, one notification per entry.
    Notify,
    /// `RAISE NOTICE 'pgweave_log'` with the JSON entry as `DETAIL`: one
    /// when a run starts, one when it ends, steps included. Sent to the
    /// client at once, so a run whose error rolls back the transaction is
    /// still seen starting (`pgweave_runtime::trace`).
    Notice,
}

impl LogTarget {
    /// The target as `--log-target` spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Table => "table",
            Self::Notify => "notify",
            Self::Notice => "notice",
        }
    }

    /// The target spelled as [`Self::as_str`] does.
    pub fn parse(s: &str) -> Option<Self> {
        [Self::Table, Self::Notify, Self::Notice]
            .into_iter()
            .find(|l| l.as_str() == s)
    }
}

/// Build options of the instrumentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Instrumentation {
    pub log: LogLevel,
    pub target: LogTarget,
}

impl Instrumentation {
    /// SQL creating what the instrumented routines write to, to run before
    /// them (idempotent). Empty when they write nothing.
    pub fn setup_sql(&self) -> String {
        let mut out = String::new();
        if self.log >= LogLevel::Info && self.target == LogTarget::Table {
            out.push_str(LOG_SETUP);
        }
        if self.log >= LogLevel::Debug {
            out.push_str(DEBUG_SETUP);
        }
        out
    }

    fn snapshots(&self) -> bool {
        self.log >= LogLevel::Debug
    }
}

/// Run log level a routine definition was generated at, read from the code
/// the instrumentation emits; `None` when it holds none (not a generated
/// routine, or one generated unlogged). A `debug` routine without
/// `@snapshot` reads as `step`: the two emit the same code.
pub fn instrumentation_of(definition: &str) -> Option<Instrumentation> {
    if !definition.contains(&format!("{STARTED_VAR} timestamptz")) {
        return None;
    }
    let log = if definition.contains("INSERT INTO pgweave_debug.snapshot ") {
        LogLevel::Debug
    } else if definition.contains(&format!("{STEPS_VAR} jsonb[]")) {
        LogLevel::Step
    } else {
        LogLevel::Info
    };
    let target = if definition.contains(&format!("pg_notify('{LOG_CHANNEL}'")) {
        LogTarget::Notify
    } else if definition.contains(&format!("RAISE NOTICE '{LOG_CHANNEL}'")) {
        LogTarget::Notice
    } else {
        LogTarget::Table
    };
    Some(Instrumentation { log, target })
}

/// Instruments the PL/pgSQL bodies of `exp`: those holding a directive,
/// and every one of them when runs are logged. `source` is the text `exp`
/// was expanded from, at `base` in its document.
pub(crate) fn instrument(
    exp: Expansion,
    source: &str,
    base: usize,
    opts: Instrumentation,
) -> Expansion {
    if opts.log == LogLevel::None
        && !exp.sql.contains("@temp_table")
        && !exp.sql.contains("@snapshot")
    {
        return exp;
    }
    let mut out = Out::default();
    let bodies = bodies(&exp.sql);
    let lines = Lines {
        exp: &exp,
        source,
        base,
    };
    for body in &bodies {
        instrument_body(&exp.sql, body, opts, &lines, &mut out);
    }
    stray_directives(&exp.sql, &bodies, &mut out);

    let mut edits = out.edits;
    edits.sort_by_key(|e| (e.range.start, e.range.end));
    if let Some(w) = edits.windows(2).find(|w| w[0].range.end > w[1].range.start) {
        // Two rewrites of the same text: a defect of this module, never
        // applied half-way.
        let mut exp = exp;
        exp.diagnostics.push(
            Diagnostic::error(
                MACRO_ERROR,
                "internal error: overlapping rewrites, the temp tables are not isolated",
            )
            .with_span(source_span(&exp, w[1].range.clone())),
        );
        return exp;
    }
    let (sql, map) = exp.map.apply_edits(&exp.sql, &edits);
    let mut diagnostics = exp.diagnostics.clone();
    for (range, d) in out.diags {
        diagnostics.push(d.with_span(source_span(&exp, range)));
    }
    let regions = exp
        .regions
        .iter()
        .map(|r| crate::expand::ExpandedRegion {
            macro_name: r.macro_name.clone(),
            source: r.source.clone(),
            generated: MapEdit::shift(&edits, r.generated.start)
                ..MapEdit::shift(&edits, r.generated.end),
        })
        .collect();
    Expansion {
        sql,
        map,
        diagnostics,
        expanded: exp.expanded,
        regions,
    }
}

fn source_span(exp: &Expansion, range: Range<usize>) -> Span {
    let (start, end) = exp
        .map
        .remap(range.start, range.end.max(range.start + 1))
        .unwrap_or((range.start, range.end));
    Span { start, end }
}

/// Source line of a position of the expansion, for the step log.
struct Lines<'a> {
    exp: &'a Expansion,
    source: &'a str,
    base: usize,
}

impl Lines<'_> {
    /// 1-based line in `source`, or `None` for generated text.
    fn line(&self, at: usize) -> Option<usize> {
        let (start, _) = self.exp.map.remap(at, at + 1)?;
        let offset = start.checked_sub(self.base)?;
        let before = self.source.get(..offset)?;
        Some(before.bytes().filter(|b| *b == b'\n').count() + 1)
    }
}

#[derive(Default)]
struct Out {
    edits: Vec<MapEdit>,
    /// Diagnostics located in the generated text.
    diags: Vec<(Range<usize>, Diagnostic)>,
}

impl Out {
    fn edit(&mut self, range: Range<usize>, text: impl Into<String>, anchor: Range<usize>) {
        self.edits.push(MapEdit {
            range,
            text: text.into(),
            anchor,
        });
    }

    fn error(&mut self, range: Range<usize>, message: impl Into<String>) {
        self.diags
            .push((range, Diagnostic::error(MACRO_ERROR, message)));
    }
}

// ── Bodies and directives ─────────────────────────────────────────────────────

/// A body rendered by a macro, and the routine statement around it.
struct Body {
    /// Text between the two tags.
    body: Range<usize>,
    /// `CREATE … $tag$ … $tag$;`, or `None` if the statement could not be
    /// delimited (template of a project macro).
    statement: Option<Range<usize>>,
}

fn bodies(sql: &str) -> Vec<Body> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(open) = sql[from..].find(BODY_TAG).map(|k| from + k) {
        let start = open + BODY_TAG.len();
        let Some(close) = sql[start..].find(BODY_TAG).map(|k| start + k) else {
            break;
        };
        let after = close + BODY_TAG.len();
        let end = if sql[after..].trim_start().starts_with(';') {
            after + sql[after..].find(';').unwrap_or(0) + 1
        } else {
            after
        };
        let statement = sql[..open].rfind("CREATE ").map(|s| s..end);
        out.push(Body {
            body: start..close,
            statement,
        });
        from = after;
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DirectiveKind {
    TempTable,
    Snapshot,
}

/// `@temp_table …;` or `@snapshot …;`, at the start of a line.
struct Directive {
    kind: DirectiveKind,
    /// From `@` to `;` included.
    range: Range<usize>,
    /// After the keyword, up to the `;` excluded.
    rest: Range<usize>,
}

/// Directives of `sql[range]`; a directive without its `;` is reported.
fn find_directives(sql: &str, range: Range<usize>, out: &mut Out) -> Vec<Directive> {
    let tokens = offset_tokens(sql, range);
    let mut found = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let t = &tokens[i];
        let kind = (t.kind == Kind::Punct && &sql[t.range.clone()] == "@")
            .then(|| tokens.get(i + 1))
            .flatten()
            .filter(|n| n.kind == Kind::Word && n.range.start == t.range.end)
            .and_then(|n| match &sql[n.range.clone()] {
                "temp_table" => Some(DirectiveKind::TempTable),
                "snapshot" => Some(DirectiveKind::Snapshot),
                _ => None,
            });
        let line_start = sql[..t.range.start].rfind('\n').map_or(0, |k| k + 1);
        let Some(kind) = kind.filter(|_| sql[line_start..t.range.start].trim().is_empty()) else {
            i += 1;
            continue;
        };
        let rest_start = tokens[i + 1].range.end;
        let mut depth = 0usize;
        let mut end = None;
        for (j, u) in tokens.iter().enumerate().skip(i + 2) {
            match (&sql[u.range.clone()], u.kind) {
                ("(", Kind::Punct) => depth += 1,
                (")", Kind::Punct) => depth = depth.saturating_sub(1),
                (";", Kind::Punct) if depth == 0 => {
                    end = Some((j, u.range.start));
                    break;
                }
                _ => {}
            }
        }
        let Some((j, semi)) = end else {
            out.error(
                t.range.start..tokens[i + 1].range.end,
                "directive without its `;`",
            );
            break;
        };
        found.push(Directive {
            kind,
            range: t.range.start..semi + 1,
            rest: rest_start..semi,
        });
        i = j + 1;
    }
    found
}

/// Tokens of `sql[range]`, in `sql`'s frame.
fn offset_tokens(sql: &str, range: Range<usize>) -> Vec<Token> {
    let base = range.start;
    tokenize(&sql[range])
        .into_iter()
        .map(|t| Token {
            kind: t.kind,
            range: t.range.start + base..t.range.end + base,
        })
        .collect()
}

fn significant(t: &Token) -> bool {
    !matches!(
        t.kind,
        Kind::Whitespace | Kind::LineComment | Kind::BlockComment
    )
}

/// Directives outside any macro body: reported and removed.
fn stray_directives(sql: &str, bodies: &[Body], out: &mut Out) {
    let mut at = 0;
    let mut outside = Vec::new();
    for b in bodies {
        outside.push(at..b.body.start);
        at = b.body.end;
    }
    outside.push(at..sql.len());
    for range in outside {
        // Bodies are skipped above; the rest is tokenized as is (a body of a
        // hand-written routine is a string token, never searched).
        let mut scratch = Out::default();
        for d in find_directives(sql, range, &mut scratch) {
            out.error(
                d.range.clone(),
                "`@temp_table` and `@snapshot` are only allowed in the body of a PL/pgSQL \
                 macro (@function, @procedure, @trigger)",
            );
            out.edit(d.range.clone(), "", d.range);
        }
    }
}

// ── A body ────────────────────────────────────────────────────────────────────

/// An isolated temp table.
struct TempTable {
    /// Name as written (folded if unquoted).
    name: String,
    /// Name of the table in `pg_temp`.
    physical: String,
    /// Declared columns, ready to emit.
    columns: Vec<String>,
    /// `CREATE TEMPORARY TABLE …`, hidden column included.
    create: String,
    /// The directive declaring it.
    directive: Range<usize>,
}

/// The routine around a body, as far as the snapshots need it.
struct Routine {
    name: String,
    /// `(name, captured)`: parameters are always captured.
    params: Vec<String>,
    trigger: bool,
    /// Type of a variable holding a `RETURN` value, if one can be declared.
    result: Option<String>,
}

/// Type of a variable holding what a function returns: the declared type
/// without modifiers, which PostgreSQL does not keep for a result (a
/// `varchar(10)` function may return a longer text). `None` when no
/// variable can hold it: polymorphic type, or no value to return.
fn result_type(rt: &ReturnType) -> Option<String> {
    fn bare(dt: &DataType) -> Option<String> {
        Some(match dt {
            DataType::Numeric { .. } => "numeric".into(),
            DataType::Varchar(_) => "varchar".into(),
            // `char` alone is `char(1)`: `bpchar` has no length.
            DataType::Char(_) => "bpchar".into(),
            DataType::Time(_) => "time".into(),
            DataType::TimeTz(_) => "timetz".into(),
            DataType::Timestamp(_) => "timestamp".into(),
            DataType::TimestampTz(_) => "timestamptz".into(),
            DataType::Array(inner) => format!("{}[]", bare(inner)?),
            DataType::Modified { name, .. } => bare(&DataType::Custom(name.clone()))?,
            DataType::Custom(name) => {
                let last = name.rsplit('.').next().unwrap_or(name).to_ascii_lowercase();
                if last.trim_matches('"').starts_with("any") {
                    return None;
                }
                name.clone()
            }
            other => pgweave_ast::emit::emit_data_type(other),
        })
    }
    match rt {
        ReturnType::Scalar(dt) => bare(dt),
        ReturnType::Trigger => Some("record".into()),
        ReturnType::OutParams | ReturnType::Table(_) | ReturnType::SetOf(_) | ReturnType::Void => {
            None
        }
    }
}

fn instrument_body(sql: &str, b: &Body, opts: Instrumentation, lines: &Lines<'_>, out: &mut Out) {
    let directives = find_directives(sql, b.body.clone(), out);
    let logged = opts.log >= LogLevel::Info;
    if directives.is_empty() && !logged {
        return;
    }
    let anchor = match (directives.first(), &b.statement) {
        (Some(d), _) => d.range.clone(),
        (None, Some(s)) => s.start..b.body.start,
        (None, None) => b.body.start..b.body.start,
    };
    // With directives, a body pgweave cannot instrument is an error (its
    // temp tables would not be isolated); logged alone, it runs unlogged.
    let fail = |out: &mut Out, msg: String| {
        if directives.is_empty() {
            out.diags.push((
                anchor.clone(),
                Diagnostic::warning(NOT_LOGGED, format!("{msg}: this routine is not logged")),
            ));
        } else {
            out.error(anchor.clone(), format!("{msg}: directives not applied"));
            fallback(sql, &directives, out);
        }
    };
    let Some(stmt_range) = b.statement.clone() else {
        return fail(
            out,
            "the routine around this body could not be delimited".into(),
        );
    };
    let header = sql[stmt_range.start..b.body.start].to_ascii_lowercase();
    if !header.contains("language plpgsql") {
        if directives.is_empty() {
            // Only PL/pgSQL routines are logged (documented).
            return;
        }
        return fail(
            out,
            "`@temp_table` and `@snapshot` need a PL/pgSQL body (`language plpgsql`)".into(),
        );
    }

    // The routine, directives blanked (same length: positions unchanged).
    let mut text = sql[stmt_range.clone()].to_string();
    for d in &directives {
        let (s, e) = (
            d.range.start - stmt_range.start,
            d.range.end - stmt_range.start,
        );
        let blank: String = text[s..e]
            .chars()
            .map(|c| if c == '\n' { '\n' } else { ' ' })
            .collect();
        text.replace_range(s..e, &blank);
    }
    let parsed = match pgweave_parser::parse_at_with_outcome(&text, stmt_range.start) {
        Ok(p) if p.statements.len() == 1 => p.statements.into_iter().next(),
        Ok(p) => {
            let why = p
                .skipped
                .first()
                .map(|s| format!(" ({:?})", s.reason))
                .unwrap_or_default();
            return fail(out, format!("routine not analyzed{why}"));
        }
        Err(e) => return fail(out, format!("routine not analyzed ({e})")),
    };
    let (routine, body) = match parsed {
        Some(Statement::Ddl(DdlStatement::CreateFunction(f))) => (
            Routine {
                name: pgweave_ast::emit::emit_object_name(&f.name),
                params: f
                    .params
                    .iter()
                    .filter_map(|p| p.name.as_ref().map(|n| n.value.clone()))
                    .collect(),
                trigger: matches!(f.return_type, ReturnType::Trigger),
                result: result_type(&f.return_type),
            },
            f.body,
        ),
        Some(Statement::Ddl(DdlStatement::CreateProcedure(p))) => (
            Routine {
                name: pgweave_ast::emit::emit_object_name(&p.name),
                params: p
                    .params
                    .iter()
                    .filter_map(|p| p.name.as_ref().map(|n| n.value.clone()))
                    .collect(),
                trigger: false,
                result: None,
            },
            p.body,
        ),
        _ => return fail(out, "not a function or a procedure".into()),
    };
    let FunctionBody::Parsed(body) = body else {
        return fail(out, "body not analyzed (see its parse error)".into());
    };

    // Declared temp tables.
    let mut tables = Vec::new();
    let mut ok = true;
    for d in directives
        .iter()
        .filter(|d| d.kind == DirectiveKind::TempTable)
    {
        match temp_table(sql, d) {
            Ok(t) => {
                if tables.iter().any(|x: &TempTable| x.name == t.name) {
                    out.error(
                        d.range.clone(),
                        format!("temp table `{}` declared twice", t.name),
                    );
                    ok = false;
                }
                tables.push(t);
            }
            Err(msg) => {
                out.error(d.range.clone(), msg);
                ok = false;
            }
        }
    }
    if !ok {
        return fallback(sql, &directives, out);
    }

    let exit = Exit {
        result: routine.result.clone(),
        cleanup: cleanup_sql(&tables),
        log: logged.then(|| RunLog {
            routine: routine.name.clone(),
            target: opts.target,
            steps: opts.log >= LogLevel::Step,
        }),
    };
    let mut w = Walker {
        sql,
        tables: &tables,
        out: Out::default(),
        inserts: false,
        exit: &exit,
        handlers: 0,
        lines,
        scopes: Vec::new(),
    };
    w.block(&body.block);
    if w.out
        .diags
        .iter()
        .any(|(_, d)| d.severity == pgweave_diagnostics::Severity::Error)
    {
        if directives.is_empty() {
            return fail(out, "a statement of the body has no position".into());
        }
        // A rewrite is refused: nothing half-isolated is emitted.
        out.diags.extend(w.out.diags);
        return fallback(sql, &directives, out);
    }
    let walked = w.out;
    let scopes = w.scopes;

    // Run id, tables, and the end of the body.
    let tokens = offset_tokens(sql, b.body.clone());
    let begin = tokens
        .iter()
        .find(|t| t.kind == Kind::Word && sql[t.range.clone()].eq_ignore_ascii_case("begin"));
    let Some(begin) = begin else {
        return fail(out, "no `BEGIN` in the body".into());
    };
    let has_declare = tokens
        .iter()
        .take_while(|t| t.range.start < begin.range.start)
        .any(|t| t.kind == Kind::Word && sql[t.range.clone()].eq_ignore_ascii_case("declare"));
    let snapshots = directives.iter().any(|d| d.kind == DirectiveKind::Snapshot);
    let mut decls = Vec::new();
    if !tables.is_empty() || (opts.snapshots() && snapshots) || logged {
        decls.push(format!("{RUN_VAR} uuid := gen_random_uuid();"));
    }
    if logged {
        decls.push(format!("{STARTED_VAR} timestamptz := clock_timestamp();"));
        decls.push(format!(
            "{TRACEPARENT_VAR} text := current_setting('{TRACEPARENT_SETTING}', true);"
        ));
        // Set in the declarations, before any statement and outside the
        // subtransaction of an exception block: a handler still restores
        // them, an error leaving the routine rolls them back.
        decls.push(format!(
            "{PARENT_VAR} uuid := nullif(current_setting('{SPAN_SETTING}', true), '')::uuid;"
        ));
        decls.push(format!(
            "{DEPTH_VAR} int := set_config('{DEPTH_SETTING}', (coalesce(nullif(current_setting(\
             '{DEPTH_SETTING}', true), '')::int, 0) + 1)::text, true)::int;"
        ));
        decls.push(format!(
            "{CONFIG_VAR} text := set_config('{SPAN_SETTING}', {RUN_VAR}::text, true);"
        ));
    }
    if opts.log >= LogLevel::Step {
        decls.push(format!("{ROWS_VAR} bigint;"));
        decls.push(format!("{STEPS_VAR} jsonb[] := ARRAY[]::jsonb[];"));
    }
    if !exit.is_empty() {
        decls.push(format!("{FOUND_VAR} boolean;"));
    }
    if !decls.is_empty() {
        let decl: String = decls.iter().map(|d| format!("    {d}\n")).collect();
        let text = if has_declare {
            decl
        } else {
            format!("DECLARE\n{decl}")
        };
        out.edit(begin.range.start..begin.range.start, text, anchor.clone());
    }
    if let Some(log) = &exit.log {
        out.edit(
            begin.range.end..begin.range.end,
            log.start_sql(),
            anchor.clone(),
        );
    }
    for t in &tables {
        out.edit(
            begin.range.end..begin.range.end,
            format!(
                "\n    IF to_regclass('pg_temp.{}') IS NULL THEN\n        {};\n    END IF;",
                t.physical, t.create
            ),
            t.directive.clone(),
        );
    }
    // The rewrites of the statements come before the end of the body: a
    // step recorded after the last statement precedes the exit.
    out.edits.extend(walked.edits);
    out.diags.extend(walked.diags);
    if !exit.is_empty() {
        let mut ends = vec![(body.block.statements.last(), false)];
        if let Some(ex) = &body.block.exception {
            for h in &ex.handlers {
                ends.push((h.statements.last(), true));
            }
        }
        for (last, in_handler) in ends {
            // After a final RETURN, the end of the list is never reached.
            if last.is_some_and(|s| matches!(s.kind, PlStatementKind::Return(_))) {
                continue;
            }
            let (at, indent) = match last {
                Some(s) => (
                    after_semicolon(sql, s.span.end as usize),
                    indent_at(sql, s.span.start as usize),
                ),
                None => (begin.range.end, "    ".to_string()),
            };
            out.edit(
                at..at,
                format!("\n{indent}{}", exit.sql(in_handler, false, &indent)),
                anchor.clone(),
            );
        }
    }

    // Directives.
    for d in &directives {
        let text = match d.kind {
            DirectiveKind::TempTable => String::new(),
            DirectiveKind::Snapshot if opts.snapshots() => {
                match snapshot_sql(sql, d, &routine, &body.block, &scopes, &tables) {
                    Ok(s) => s,
                    Err(msg) => {
                        out.error(d.range.clone(), msg);
                        String::new()
                    }
                }
            }
            DirectiveKind::Snapshot => {
                if let Err(msg) = snapshot_label(sql, d) {
                    out.error(d.range.clone(), msg);
                }
                String::new()
            }
        };
        out.edit(d.range.clone(), text, d.range.clone());
    }
}

/// Directives applied without isolation, after an error: the body stays
/// analyzable (the tables exist under their written name).
fn fallback(sql: &str, directives: &[Directive], out: &mut Out) {
    for d in directives {
        let text = match d.kind {
            DirectiveKind::TempTable => {
                format!("CREATE TEMP TABLE IF NOT EXISTS{};", &sql[d.rest.clone()])
            }
            DirectiveKind::Snapshot => String::new(),
        };
        out.edit(d.range.clone(), text, d.range.clone());
    }
}

/// Code run when a run ends — before each `RETURN`, at the end of the
/// body and of its exception handlers: the cleanup of its temp tables,
/// then its log. An error that leaves the routine runs none of it, and
/// rolls back what the run wrote, logs included.
struct Exit {
    /// See [`Routine::result`].
    result: Option<String>,
    cleanup: Vec<String>,
    log: Option<RunLog>,
}

struct RunLog {
    routine: String,
    target: LogTarget,
    steps: bool,
}

impl Exit {
    fn is_empty(&self) -> bool {
        self.cleanup.is_empty() && self.log.is_none()
    }

    /// The statements, one per line at `indent`. `in_handler`: the run
    /// ends in an exception handler, whose `SQLSTATE` the log records.
    /// `keep_found`: the code is followed by a `RETURN` reading `FOUND`.
    fn sql(&self, in_handler: bool, keep_found: bool, indent: &str) -> String {
        let mut lines = Vec::new();
        if keep_found {
            lines.push(format!("{FOUND_VAR} := FOUND;"));
        }
        lines.extend(self.cleanup.iter().cloned());
        if let Some(log) = &self.log {
            lines.extend(log.flush(in_handler));
        }
        if keep_found {
            lines.push(format!("FOUND := {FOUND_VAR};"));
        }
        lines.join(&format!("\n{indent}"))
    }
}

impl RunLog {
    /// Right after `BEGIN`: the depth guard, and the start event of the
    /// `notice` target.
    fn start_sql(&self) -> String {
        let routine = literal(&self.routine);
        let mut out = format!(
            "\n    IF {DEPTH_VAR} > coalesce(nullif(current_setting('{MAX_DEPTH_SETTING}', true), \
             '')::int, {DEPTH_VAR}) THEN\n        RAISE EXCEPTION 'pgweave: % called at depth %, \
             beyond {MAX_DEPTH_SETTING}', {routine}, {DEPTH_VAR} USING ERRCODE = '54001';\n    \
             END IF;"
        );
        if self.target == LogTarget::Notice {
            out.push_str(&format!(
                "\n    RAISE NOTICE '{LOG_CHANNEL}' USING DETAIL = jsonb_build_object('type', \
                 'start', {})::text;",
                self.identity()
            ));
        }
        out
    }

    /// Fields naming the run in an event.
    fn identity(&self) -> String {
        format!(
            "'run', {RUN_VAR}, 'routine', {}, 'parent', {PARENT_VAR}, 'depth', {DEPTH_VAR}, \
             'traceparent', left({TRACEPARENT_VAR}, 512), 'started_at', {STARTED_VAR}",
            literal(&self.routine)
        )
    }

    fn flush(&self, in_handler: bool) -> Vec<String> {
        let routine = literal(&self.routine);
        let error = if in_handler { "SQLSTATE" } else { "NULL::text" };
        let steps = if self.steps {
            format!("cardinality({STEPS_VAR})")
        } else {
            "NULL::int".into()
        };
        let duration =
            format!("1000 * extract(epoch FROM clock_timestamp() - {STARTED_VAR})::float8");
        let entries = format!("unnest({STEPS_VAR}) WITH ORDINALITY AS _pgweave_s(entry, ord)");
        let mut out = Vec::new();
        match self.target {
            LogTarget::Table => {
                if self.steps {
                    out.push(format!(
                        "INSERT INTO pgweave_log.step (run, step, kind, target, line, row_count, \
                         executed_at) SELECT {RUN_VAR}, _pgweave_s.ord, \
                         _pgweave_s.entry->>'kind', _pgweave_s.entry->>'target', \
                         (_pgweave_s.entry->>'line')::int, \
                         (_pgweave_s.entry->>'row_count')::bigint, \
                         (_pgweave_s.entry->>'executed_at')::timestamptz FROM {entries};"
                    ));
                }
                out.push(format!(
                    "INSERT INTO pgweave_log.run (run, routine, parent, depth, traceparent, \
                     started_at, duration_ms, steps, error) VALUES ({RUN_VAR}, {routine}, \
                     {PARENT_VAR}, {DEPTH_VAR}, {TRACEPARENT_VAR}, {STARTED_VAR}, {duration}, \
                     {steps}, {error});"
                ));
            }
            LogTarget::Notify => {
                // A notification holds at most 8000 bytes: one per entry,
                // and the caller's setting is bounded.
                if self.steps {
                    out.push(format!(
                        "PERFORM pg_notify('{LOG_CHANNEL}', (_pgweave_s.entry || \
                         jsonb_build_object('type', 'step', 'run', {RUN_VAR}, 'routine', \
                         {routine}, 'step', _pgweave_s.ord))::text) FROM {entries};"
                    ));
                }
                out.push(format!(
                    "PERFORM pg_notify('{LOG_CHANNEL}', jsonb_build_object('type', 'run', {}, \
                     'duration_ms', {duration}, 'steps', {steps}, 'error', {error})::text);",
                    self.identity()
                ));
            }
            LogTarget::Notice => {
                let steps = if self.steps {
                    format!("to_jsonb({STEPS_VAR})")
                } else {
                    "NULL::jsonb".into()
                };
                out.push(format!(
                    "RAISE NOTICE '{LOG_CHANNEL}' USING DETAIL = jsonb_build_object('type', \
                     'end', {}, 'duration_ms', {duration}, 'steps', {steps}, 'error', \
                     {error})::text;",
                    self.identity()
                ));
            }
        }
        // The caller's run and depth again, for what it runs next.
        out.push(format!(
            "{CONFIG_VAR} := set_config('{SPAN_SETTING}', coalesce({PARENT_VAR}::text, ''), true);"
        ));
        out.push(format!(
            "{CONFIG_VAR} := set_config('{DEPTH_SETTING}', ({DEPTH_VAR} - 1)::text, true);"
        ));
        out
    }
}

/// Records the statement that just ran as a step: `GET DIAGNOSTICS` and a
/// simple assignment, which change neither `ROW_COUNT` nor `FOUND`.
fn step_sql(kind: &str, target: Option<&str>, line: Option<usize>, indent: &str) -> String {
    let target = target.map_or("NULL::text".to_string(), literal);
    let line = line.map_or("NULL::int".to_string(), |l| l.to_string());
    format!(
        "GET DIAGNOSTICS {ROWS_VAR} = ROW_COUNT;\n{indent}{STEPS_VAR} := array_append({STEPS_VAR}, \
         jsonb_build_object('kind', '{kind}', 'target', {target}, 'line', {line}, 'row_count', \
         {ROWS_VAR}, 'executed_at', clock_timestamp()));"
    )
}

fn cleanup_sql(tables: &[TempTable]) -> Vec<String> {
    tables
        .iter()
        .map(|t| {
            format!(
                "DELETE FROM pg_temp.{p} WHERE {p}.{RUN_COLUMN} = {RUN_VAR};",
                p = t.physical
            )
        })
        .collect()
}

/// Whitespace before `pos` on its line, if only whitespace precedes it.
fn indent_at(sql: &str, pos: usize) -> String {
    let line_start = sql[..pos].rfind('\n').map_or(0, |k| k + 1);
    let lead = &sql[line_start..pos];
    if lead.trim().is_empty() {
        lead.to_string()
    } else {
        String::new()
    }
}

/// The text at `range` holds a call (`name(`).
fn calls(sql: &str, range: &Range<usize>) -> bool {
    let tokens: Vec<Token> = offset_tokens(sql, range.clone())
        .into_iter()
        .filter(significant)
        .collect();
    tokens
        .windows(2)
        .any(|w| w[0].kind == Kind::Word && &sql[w[1].range.clone()] == "(")
}

/// The `RETURN` at `range` returns a variable, a field or a constant
/// (`NEW`, `NULL`, `v.total`, `0`): evaluating it before or after the exit
/// code gives the same value, and nothing runs. Not `FOUND`, which the exit
/// code changes.
fn plain_value(sql: &str, range: &Range<usize>) -> bool {
    let tokens: Vec<Token> = offset_tokens(sql, range.clone())
        .into_iter()
        .filter(significant)
        .collect();
    let mut value = tokens.get(1..).unwrap_or_default();
    if let Some((last, rest)) = value.split_last()
        && &sql[last.range.clone()] == ";"
    {
        value = rest;
    }
    match value {
        [t] => {
            matches!(
                t.kind,
                Kind::Word | Kind::QuotedIdent | Kind::Number | Kind::String | Kind::Param
            ) && !sql[t.range.clone()].eq_ignore_ascii_case("found")
        }
        [_, ..] => {
            value.iter().enumerate().all(|(i, t)| {
                if i % 2 == 0 {
                    matches!(t.kind, Kind::Word | Kind::QuotedIdent)
                } else {
                    &sql[t.range.clone()] == "."
                }
            }) && value.len() % 2 == 1
        }
        [] => false,
    }
}

/// Position after the `;` ending a statement whose span ends at `end`
/// (the span may stop before it).
fn after_semicolon(sql: &str, end: usize) -> usize {
    if sql[..end].ends_with(';') {
        return end;
    }
    let rest = &sql[end..];
    let tokens = tokenize(rest);
    match tokens.iter().find(|t| significant(t)) {
        Some(t) if &rest[t.range.clone()] == ";" => end + t.range.end,
        _ => end,
    }
}

// ── @temp_table ───────────────────────────────────────────────────────────────

fn fold(i: &Ident) -> String {
    if i.quoted {
        i.value.clone()
    } else {
        i.value.to_lowercase()
    }
}

/// FNV-1a: stable across builds and platforms, unlike `DefaultHasher`.
fn fnv1a(text: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in text.bytes() {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

fn temp_table(sql: &str, d: &Directive) -> Result<TempTable, String> {
    let text = format!("CREATE TEMP TABLE {};", &sql[d.rest.clone()]);
    let stmts =
        pgweave_parser::parse_with_outcome(&text).map_err(|e| format!("@temp_table: {e}"))?;
    if let Some(s) = stmts.skipped.first() {
        return Err(format!("@temp_table: not read ({:?})", s.reason));
    }
    let Some(Statement::Ddl(DdlStatement::CreateTable(ct))) = stmts.statements.into_iter().next()
    else {
        return Err("@temp_table: expected `@temp_table name (column definitions);`".into());
    };
    let CreateTable {
        name,
        columns,
        constraints,
        likes,
        as_query,
        on_commit,
        unlogged,
        ..
    } = ct;
    if name.0.len() != 1 {
        return Err("@temp_table: the name is not qualified (the table lives in pg_temp)".into());
    }
    if !likes.is_empty() || as_query.is_some() {
        return Err("@temp_table: declare the columns (no LIKE, no AS query)".into());
    }
    if on_commit.is_some() || unlogged {
        return Err("@temp_table: ON COMMIT and UNLOGGED are managed by pgweave".into());
    }
    if columns.is_empty() {
        return Err("@temp_table: at least one column".into());
    }
    let table_name = fold(&name.0[0]);
    let mut columns = columns;
    let mut constraints = constraints;
    let run = Ident::new(RUN_COLUMN);
    for c in &mut columns {
        if fold(&c.name) == RUN_COLUMN {
            return Err(format!(
                "@temp_table: `{RUN_COLUMN}` is reserved by pgweave"
            ));
        }
        let mut kept = Vec::new();
        for k in std::mem::take(&mut c.constraints) {
            match k {
                // Uniqueness per run: the constraint includes the run.
                ColumnConstraint::PrimaryKey => {
                    constraints.push(pgweave_ast::TableConstraint::PrimaryKey {
                        name: None,
                        columns: vec![c.name.clone(), run.clone()],
                    })
                }
                ColumnConstraint::Unique => {
                    constraints.push(pgweave_ast::TableConstraint::Unique {
                        name: None,
                        columns: vec![c.name.clone(), run.clone()],
                    })
                }
                ColumnConstraint::References { .. } => {
                    return Err("@temp_table: no foreign key on an isolated temp table".to_string());
                }
                other => kept.push(other),
            }
        }
        c.constraints = kept;
    }
    for k in &mut constraints {
        match k {
            pgweave_ast::TableConstraint::PrimaryKey { columns, .. }
            | pgweave_ast::TableConstraint::Unique { columns, .. } => {
                if !columns.iter().any(|c| fold(c) == RUN_COLUMN) {
                    columns.push(run.clone());
                }
            }
            pgweave_ast::TableConstraint::Check { .. } => {}
            _ => {
                return Err(
                    "@temp_table: only PRIMARY KEY, UNIQUE and CHECK constraints".to_string(),
                );
            }
        }
    }
    let declared: Vec<String> = columns.iter().map(|c| emit_ident(&c.name)).collect();
    let signature = columns
        .iter()
        .map(pgweave_ast::emit::emit_column_def)
        .chain(
            constraints
                .iter()
                .map(pgweave_ast::emit::emit_table_constraint),
        )
        .collect::<Vec<_>>()
        .join(", ");
    let base: String = table_name.chars().take(54).collect();
    let physical = format!("{base}_{:08x}", fnv1a(&signature));
    columns.push(ColumnDef {
        name: run.clone(),
        data_type: DataType::Uuid,
        nullable: false,
        default: None,
        generated: None,
        identity: None,
        constraints: vec![ColumnConstraint::Default(run_default())],
    });
    let create = emit_create_table(&CreateTable {
        id: Default::default(),
        span: Default::default(),
        name: ObjectName(vec![Ident::new(physical.clone())]),
        if_not_exists: false,
        temporary: true,
        unlogged: false,
        on_commit: None,
        columns,
        constraints,
        likes: Vec::new(),
        as_query: None,
    });
    Ok(TempTable {
        name: table_name,
        physical,
        columns: declared,
        // One line: it sits in the body of the routine.
        create: create
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .replace("( ", "(")
            .replace(" )", ")")
            .trim_end_matches(';')
            .to_string(),
        directive: d.range.clone(),
    })
}

/// `current_setting('pgweave.run')::uuid`, as an expression of the AST.
fn run_default() -> Expr {
    let sql = format!("SELECT current_setting('{RUN_SETTING}')::uuid");
    match pgweave_parser::parse(&sql)
        .ok()
        .and_then(|s| s.into_iter().next())
    {
        Some(Statement::Select(s)) => match s.columns.into_iter().next() {
            Some(SelectItem::Expr { expr, .. }) => expr,
            _ => unreachable!("constant expression"),
        },
        _ => unreachable!("constant expression"),
    }
}

// ── Rewriting the accesses ────────────────────────────────────────────────────

/// Variables declared by a nested block or a loop, visible in `range`.
struct Scope {
    range: Range<usize>,
    vars: Vec<(String, bool)>,
}

struct Walker<'a> {
    sql: &'a str,
    tables: &'a [TempTable],
    out: Out,
    /// The statement being walked inserts into a temp table.
    inserts: bool,
    exit: &'a Exit,
    /// Depth of exception handlers around the statement being walked.
    handlers: usize,
    lines: &'a Lines<'a>,
    scopes: Vec<Scope>,
}

impl Walker<'_> {
    fn table(&self, name: &ObjectName) -> Option<&TempTable> {
        let parts = &name.0;
        let last = match parts.as_slice() {
            [n] => n,
            [schema, n] if fold(schema) == "pg_temp" => n,
            _ => return None,
        };
        let key = fold(last);
        self.tables.iter().find(|t| t.name == key)
    }

    fn real(span: pgweave_ast::Span) -> Option<Range<usize>> {
        (span != pgweave_ast::Span::DUMMY).then_some(span.start as usize..span.end as usize)
    }

    // ── PL/pgSQL ──────────────────────────────────────────────────────────

    fn block(&mut self, b: &Block) {
        let Block {
            label: _,
            declarations,
            statements,
            exception,
        } = b;
        for d in declarations {
            self.declaration(d);
        }
        self.statements(statements);
        if let Some(ex) = exception {
            self.handlers += 1;
            for h in &ex.handlers {
                self.statements(&h.statements);
            }
            self.handlers -= 1;
        }
    }

    fn declaration(&mut self, d: &Declaration) {
        match d {
            Declaration::Variable(v) => {
                if let VarType::RowTypeOf(n) | VarType::TypeOf(n) = &v.var_type {
                    let table = match &v.var_type {
                        VarType::TypeOf(_) if n.0.len() >= 2 => {
                            ObjectName(n.0[..n.0.len() - 1].to_vec())
                        }
                        _ => n.clone(),
                    };
                    if let Some(t) = self.table(&table) {
                        let range = Self::real(v.span).unwrap_or(t.directive.clone());
                        self.out.error(
                            range,
                            format!(
                                "%ROWTYPE / %TYPE of isolated temp table `{}`: not supported, \
                                 declare the type",
                                t.name
                            ),
                        );
                    }
                }
                if let Some(e) = &v.default {
                    self.expr(e);
                }
            }
            Declaration::Cursor { query, .. } => self.select(query),
            Declaration::Alias { .. } => {}
        }
    }

    fn statements(&mut self, stmts: &[PlStatement]) {
        for s in stmts {
            self.statement(s);
        }
    }

    fn statement(&mut self, s: &PlStatement) {
        let Some(range) = Self::real(s.span) else {
            if !self.tables.is_empty() || self.exit.log.is_some() {
                self.out.error(
                    self.tables[0].directive.clone(),
                    "a statement of the body has no position: temp tables not isolated",
                );
            }
            return;
        };
        let outer = std::mem::replace(&mut self.inserts, false);
        let limit = range.end;
        match &s.kind {
            PlStatementKind::Assign { value, .. } => self.expr(value),
            PlStatementKind::Sql { statement, .. } => {
                self.sql_statement(statement, &range, limit);
                let step = match statement {
                    Statement::Insert(i) => Some(("INSERT", &i.table)),
                    Statement::Update(u) => Some(("UPDATE", &u.table.name)),
                    Statement::Delete(d) => Some(("DELETE", &d.table.name)),
                    _ => None,
                };
                if let Some((kind, table)) = step {
                    let target = pgweave_ast::emit::emit_object_name(table);
                    self.step(&range, kind, Some(&target));
                }
            }
            PlStatementKind::Perform(q) | PlStatementKind::ReturnQuery(q) => self.select(q),
            PlStatementKind::Execute { sql, using, .. } => {
                self.dynamic(&range);
                self.expr(sql);
                using.iter().for_each(|e| self.expr(e));
                self.step(&range, "EXECUTE", None);
            }
            PlStatementKind::ReturnQueryExecute { sql, using } => {
                self.dynamic(&range);
                self.expr(sql);
                using.iter().for_each(|e| self.expr(e));
            }
            PlStatementKind::Return(value) => {
                let before = self.out.edits.len();
                if let Some(e) = value {
                    self.expr(e);
                }
                let reads_temp = self.out.edits.len() > before;
                let keyword = offset_tokens(self.sql, range.clone())
                    .into_iter()
                    .find(|t| t.kind == Kind::Word)
                    .filter(|t| self.sql[t.range.clone()].eq_ignore_ascii_case("return"));
                let indent = indent_at(self.sql, range.start);
                match (value, &self.exit.result, keyword) {
                    _ if self.exit.is_empty() => {}
                    // The value first — its calls are runs of this one, it
                    // may read `FOUND` or a temp table — then the exit:
                    // `DECLARE r t := <value>; BEGIN <exit> RETURN r; END;`.
                    // A variable or a constant (`RETURN NEW;`) needs no
                    // such block.
                    (Some(_), Some(ty), Some(keyword)) if !plain_value(self.sql, &range) => {
                        let anchor = range.clone();
                        self.out.edit(
                            keyword.range.clone(),
                            format!("DECLARE {RESULT_VAR} {ty} :="),
                            anchor.clone(),
                        );
                        let inner = format!("{indent}    ");
                        let exit = self.exit.sql(self.handlers > 0, false, &inner);
                        let at = after_semicolon(self.sql, range.end);
                        self.out.edit(
                            at..at,
                            format!(
                                "\n{indent}BEGIN\n{inner}{exit}\n{inner}RETURN {RESULT_VAR};\n\
                                 {indent}END;"
                            ),
                            anchor,
                        );
                    }
                    _ => {
                        if reads_temp {
                            self.out.error(
                                range.clone(),
                                "RETURN reads an isolated temp table, whose rows are deleted \
                                 on return: assign the value to a variable first",
                            );
                        }
                        if value.is_some() && self.exit.log.is_some() && calls(self.sql, &range) {
                            self.out.diags.push((
                                range.clone(),
                                Diagnostic::warning(
                                    NOT_LOGGED,
                                    "the routines this RETURN calls run after the run is \
                                     recorded (polymorphic result): they are logged as \
                                     called by the caller",
                                )
                                .with_hint("assign the value to a variable first"),
                            ));
                        }
                        let exit =
                            self.exit
                                .sql(self.handlers > 0, self.reads_found(&range), &indent);
                        let text = format!("{exit}\n{indent}");
                        let anchor = range.clone();
                        self.out.edit(range.start..range.start, text, anchor);
                    }
                }
            }
            PlStatementKind::ReturnNext(value) => {
                if let Some(e) = value {
                    self.expr(e);
                }
            }
            PlStatementKind::Raise { args, options, .. } => {
                args.iter().for_each(|e| self.expr(e));
                options.iter().for_each(|o| self.expr(&o.value));
            }
            PlStatementKind::Assert { condition, message } => {
                self.expr(condition);
                if let Some(m) = message {
                    self.expr(m);
                }
            }
            PlStatementKind::If {
                condition,
                then_branch,
                elsif_branches,
                else_branch,
            } => {
                self.expr(condition);
                self.with_inserts_reset(|w| w.statements(then_branch));
                for (c, body) in elsif_branches {
                    self.expr(c);
                    self.with_inserts_reset(|w| w.statements(body));
                }
                if let Some(e) = else_branch {
                    self.with_inserts_reset(|w| w.statements(e));
                }
            }
            PlStatementKind::Case {
                operand,
                whens,
                else_branch,
            } => {
                if let Some(o) = operand {
                    self.expr(o);
                }
                for (values, body) in whens {
                    values.iter().for_each(|e| self.expr(e));
                    self.with_inserts_reset(|w| w.statements(body));
                }
                if let Some(e) = else_branch {
                    self.with_inserts_reset(|w| w.statements(e));
                }
            }
            PlStatementKind::Loop { body, .. } => self.with_inserts_reset(|w| w.statements(body)),
            PlStatementKind::While {
                condition, body, ..
            } => {
                self.expr(condition);
                self.with_inserts_reset(|w| w.statements(body));
            }
            PlStatementKind::ForRange {
                var,
                from,
                to,
                by,
                body,
                ..
            } => {
                self.expr(from);
                self.expr(to);
                if let Some(b) = by {
                    self.expr(b);
                }
                self.scopes.push(Scope {
                    range: range.clone(),
                    vars: vec![(var.value.clone(), true)],
                });
                self.with_inserts_reset(|w| w.statements(body));
            }
            PlStatementKind::ForQuery {
                targets,
                query,
                body,
                ..
            } => {
                self.select(query);
                // An undeclared loop target is an implicit record.
                self.scopes.push(Scope {
                    range: range.clone(),
                    vars: targets.iter().map(|t| (t.value.clone(), false)).collect(),
                });
                self.with_inserts_reset(|w| w.statements(body));
            }
            PlStatementKind::ForExecute {
                sql, using, body, ..
            } => {
                self.dynamic(&range);
                self.expr(sql);
                using.iter().for_each(|e| self.expr(e));
                self.with_inserts_reset(|w| w.statements(body));
            }
            PlStatementKind::ForCursor { args, body, .. } => {
                args.iter().for_each(|e| self.expr(e));
                self.with_inserts_reset(|w| w.statements(body));
            }
            PlStatementKind::Foreach { array, body, .. } => {
                self.expr(array);
                self.with_inserts_reset(|w| w.statements(body));
            }
            PlStatementKind::Exit { when, .. } | PlStatementKind::Continue { when, .. } => {
                if let Some(w) = when {
                    self.expr(w);
                }
            }
            PlStatementKind::Open { args, query, .. } => {
                args.iter().for_each(|e| self.expr(e));
                if let Some(q) = query {
                    self.select(q);
                }
            }
            PlStatementKind::Block(b) => {
                let vars = b
                    .declarations
                    .iter()
                    .filter_map(|d| match d {
                        Declaration::Variable(v) => {
                            Some((v.name.value.clone(), !matches!(v.var_type, VarType::Record)))
                        }
                        _ => None,
                    })
                    .collect();
                self.scopes.push(Scope {
                    range: range.clone(),
                    vars,
                });
                self.with_inserts_reset(|w| w.block(b));
            }
            PlStatementKind::GetDiagnostics { .. }
            | PlStatementKind::Fetch { .. }
            | PlStatementKind::Close(_)
            | PlStatementKind::Null => {}
        }
        if self.inserts {
            let indent = indent_at(self.sql, range.start);
            let text =
                format!("PERFORM set_config('{RUN_SETTING}', {RUN_VAR}::text, true);\n{indent}");
            self.out.edit(range.start..range.start, text, range.clone());
        }
        self.inserts = outer;
    }

    /// Records the statement at `range` as a step, in a build logging steps.
    fn step(&mut self, range: &Range<usize>, kind: &str, target: Option<&str>) {
        if !self.exit.log.as_ref().is_some_and(|l| l.steps) {
            return;
        }
        let at = after_semicolon(self.sql, range.end);
        let indent = indent_at(self.sql, range.start);
        let text = format!(
            "\n{indent}{}",
            step_sql(kind, target, self.lines.line(range.start), &indent)
        );
        self.out.edit(at..at, text, range.clone());
    }

    /// The statement at `range` names `FOUND`, which the exit code changes.
    fn reads_found(&self, range: &Range<usize>) -> bool {
        offset_tokens(self.sql, range.clone()).iter().any(|t| {
            t.kind == Kind::Word && self.sql[t.range.clone()].eq_ignore_ascii_case("found")
        })
    }

    /// Walks a nested statement list: its inserts are prefixed inside it,
    /// not before the enclosing statement.
    fn with_inserts_reset(&mut self, f: impl FnOnce(&mut Self)) {
        let saved = std::mem::replace(&mut self.inserts, false);
        f(self);
        self.inserts = saved;
    }

    fn dynamic(&mut self, range: &Range<usize>) {
        if self.tables.is_empty() {
            return;
        }
        self.out.diags.push((
            range.clone(),
            Diagnostic::warning(
                NOT_ISOLATED,
                "dynamic SQL is not rewritten: an isolated temp table it reads or writes sees \
                 the rows of every run",
            )
            .with_hint(format!(
                "filter on `{RUN_COLUMN} = {RUN_VAR}` in the dynamic statement, or avoid the \
                 temp table there"
            )),
        ));
    }

    // ── SQL ───────────────────────────────────────────────────────────────

    fn sql_statement(&mut self, stmt: &Statement, range: &Range<usize>, limit: usize) {
        match stmt {
            Statement::Select(s) => self.select(s),
            Statement::Insert(i) => self.insert(i, limit),
            Statement::Update(u) => self.update(u, limit),
            Statement::Delete(d) => self.delete(d, limit),
            Statement::Utility(u) if matches!(u.kind, UtilityKind::Call { .. }) => {
                if let UtilityKind::Call { args, .. } = &u.kind {
                    args.iter().for_each(|e| self.expr(e));
                }
            }
            Statement::Ddl(_) | Statement::Utility(_) => self.unsupported(range),
        }
    }

    /// A statement pgweave does not rewrite: refused if it names a temp table.
    fn unsupported(&mut self, range: &Range<usize>) {
        for t in offset_tokens(self.sql, range.clone()) {
            if t.kind != Kind::Word && t.kind != Kind::QuotedIdent {
                continue;
            }
            let word = &self.sql[t.range.clone()];
            let name = if t.kind == Kind::QuotedIdent {
                word.trim_matches('"').replace("\"\"", "\"")
            } else {
                word.to_lowercase()
            };
            if let Some(tt) = self.tables.iter().find(|x| x.name == name) {
                let msg = format!(
                    "isolated temp table `{}` in a statement pgweave does not rewrite: only \
                     SELECT, INSERT, UPDATE and DELETE (TRUNCATE → DELETE)",
                    tt.name
                );
                self.out.error(range.clone(), msg);
                return;
            }
        }
    }

    fn ctes(&mut self, with: &[Cte]) {
        for c in with {
            let Cte {
                name,
                columns: _,
                query,
            } = c;
            if let Some(t) = self.tables.iter().find(|t| t.name == fold(name)) {
                let msg = format!(
                    "the CTE `{}` shadows the isolated temp table of the same name: rename it",
                    t.name
                );
                self.out.error(t.directive.clone(), msg);
            }
            match query {
                CteQuery::Select(s) => self.select(s),
                CteQuery::Modify(m) => {
                    let limit = self.sql.len();
                    match m.as_ref() {
                        Statement::Insert(i) => self.insert(i, limit),
                        Statement::Update(u) => self.update(u, limit),
                        Statement::Delete(d) => self.delete(d, limit),
                        Statement::Select(s) => self.select(s),
                        _ => {}
                    }
                }
            }
        }
    }

    fn select(&mut self, s: &SelectStmt) {
        let SelectStmt {
            id: _,
            span: _,
            with,
            with_recursive: _,
            distinct: _,
            distinct_on,
            columns,
            from,
            joins,
            where_clause,
            group_by,
            having,
            windows,
            order_by,
            limit,
            offset,
            set_ops,
            values,
        } = s;
        self.ctes(with);
        distinct_on.iter().for_each(|e| self.expr(e));
        self.items(columns);
        from.iter().for_each(|t| self.table_ref(t));
        for j in joins {
            self.table_ref(&j.table);
            if let JoinConstraint::On(e) = &j.constraint {
                self.expr(e);
            }
        }
        if let Some(e) = where_clause {
            self.expr(e);
        }
        group_by.iter().for_each(|e| self.expr(e));
        if let Some(e) = having {
            self.expr(e);
        }
        for w in windows {
            w.spec.exprs().into_iter().for_each(|e| self.expr(e));
        }
        order_by.iter().for_each(|o| self.expr(&o.expr));
        if let Some(e) = limit {
            self.expr(e);
        }
        if let Some(e) = offset {
            self.expr(e);
        }
        set_ops.iter().for_each(|op| self.select(&op.select));
        values.iter().flatten().for_each(|e| self.expr(e));
    }

    fn items(&mut self, items: &[SelectItem]) {
        for i in items {
            if let SelectItem::Expr { expr, .. } = i {
                self.expr(expr);
            }
        }
    }

    fn expr(&mut self, e: &Expr) {
        for s in e.subqueries() {
            self.select(s);
        }
        for c in e.children() {
            self.expr(c);
        }
    }

    fn table_ref(&mut self, t: &TableRef) {
        match &t.source {
            TableSource::Table(name) => {
                let Some(tt) = self.table(name) else {
                    return;
                };
                let Some(range) = Self::real(t.span) else {
                    let anchor = tt.directive.clone();
                    let msg = format!(
                        "a read of `{}` has no position: temp tables not isolated",
                        tt.name
                    );
                    self.out.error(anchor, msg);
                    return;
                };
                let alias = if t.alias.is_some() {
                    String::new()
                } else {
                    format!(" AS {}", quote(&tt.name))
                };
                let text = format!(
                    "(SELECT {cols} FROM pg_temp.{p} WHERE {p}.{RUN_COLUMN} = {RUN_VAR}){alias}",
                    cols = tt.columns.join(", "),
                    p = tt.physical,
                );
                self.out.edit(range.clone(), text, range);
            }
            TableSource::Subquery { query, .. } => self.select(query),
            TableSource::Function { args, .. } => args.iter().for_each(|e| self.expr(e)),
        }
    }

    /// Renames a target to its physical table, aliased with the written name.
    fn retarget(&mut self, tt_name: &str, span: pgweave_ast::Span, alias: bool) -> bool {
        let Some(tt) = self.tables.iter().find(|t| t.name == tt_name) else {
            return false;
        };
        let Some(range) = Self::real(span) else {
            let msg = format!("a write to `{}` has no position: not isolated", tt.name);
            self.out.error(tt.directive.clone(), msg);
            return false;
        };
        let suffix = if alias {
            String::new()
        } else {
            format!(" AS {}", quote(&tt.name))
        };
        let text = format!("pg_temp.{}{suffix}", tt.physical);
        self.out.edit(range.clone(), text, range);
        true
    }

    fn no_star(&mut self, returning: &[SelectItem], range: Range<usize>, name: &str) {
        if returning
            .iter()
            .any(|i| matches!(i, SelectItem::Wildcard | SelectItem::QualifiedWildcard(_)))
        {
            self.out.error(
                range,
                format!(
                    "RETURNING * on isolated temp table `{name}` would return its hidden \
                     column: list the columns"
                ),
            );
        }
    }

    fn insert(&mut self, i: &InsertStmt, limit: usize) {
        let InsertStmt {
            id: _,
            span: _,
            with,
            with_recursive: _,
            table,
            table_span,
            alias,
            columns: _,
            source,
            on_conflict,
            returning,
            returning_aliases: _,
        } = i;
        self.ctes(with);
        let target = self.table(table).map(|t| t.name.clone());
        if let Some(name) = &target
            && self.retarget(name, *table_span, alias.is_some())
        {
            self.inserts = true;
            let at = table_span.start as usize..table_span.end as usize;
            self.no_star(returning, at.clone(), name);
            if let Some(oc) = on_conflict {
                match &oc.target {
                    OnConflictTarget::Column(_) => self.conflict_columns(at.end, limit, name),
                    OnConflictTarget::Constraint(_) => self.out.error(
                        at,
                        format!(
                            "ON CONFLICT ON CONSTRAINT on isolated temp table `{name}`: name \
                             the columns instead"
                        ),
                    ),
                    OnConflictTarget::None => {}
                }
            }
        }
        match source {
            InsertSource::Values(rows) => rows.iter().flatten().for_each(|e| self.expr(e)),
            InsertSource::Select(s) => self.select(s),
            InsertSource::DefaultValues => {}
        }
        if let Some(oc) = on_conflict
            && let OnConflictAction::Update {
                assignments,
                where_clause,
            } = &oc.action
        {
            assignments.iter().for_each(|a| self.expr(&a.value));
            if let Some(w) = where_clause {
                self.expr(w);
            }
        }
        self.items(returning);
    }

    /// `ON CONFLICT (cols)` of an isolated table: the unique index includes
    /// the run.
    fn conflict_columns(&mut self, from: usize, limit: usize, name: &str) {
        let tokens: Vec<Token> = offset_tokens(self.sql, from..limit.max(from))
            .into_iter()
            .filter(significant)
            .collect();
        let word = |t: &Token, w: &str| {
            t.kind == Kind::Word && self.sql[t.range.clone()].eq_ignore_ascii_case(w)
        };
        let mut depth = 0usize;
        for (k, t) in tokens.iter().enumerate() {
            match &self.sql[t.range.clone()] {
                "(" => depth += 1,
                ")" => depth = depth.saturating_sub(1),
                _ if depth == 0
                    && word(t, "on")
                    && tokens.get(k + 1).is_some_and(|n| word(n, "conflict")) =>
                {
                    if tokens
                        .get(k + 2)
                        .is_none_or(|p| &self.sql[p.range.clone()] != "(")
                    {
                        break;
                    }
                    let mut d = 0usize;
                    for u in &tokens[k + 2..] {
                        match &self.sql[u.range.clone()] {
                            "(" => d += 1,
                            ")" => {
                                d -= 1;
                                if d == 0 {
                                    let at = u.range.start;
                                    self.out.edit(
                                        at..at,
                                        format!(", {RUN_COLUMN}"),
                                        u.range.clone(),
                                    );
                                    return;
                                }
                            }
                            _ => {}
                        }
                    }
                    break;
                }
                _ => {}
            }
        }
        self.out.error(
            from..from,
            format!("ON CONFLICT of isolated temp table `{name}` not located: not isolated"),
        );
    }

    fn target(&mut self, t: &TargetTable, limit: usize, returning: &[SelectItem]) {
        let TargetTable { name, alias, span } = t;
        let Some(tt_name) = self.table(name).map(|t| t.name.clone()) else {
            return;
        };
        if !self.retarget(&tt_name, *span, alias.is_some()) {
            return;
        }
        let at = span.start as usize..span.end as usize;
        self.no_star(returning, at.clone(), &tt_name);
        let visible = alias
            .as_ref()
            .map(emit_ident)
            .unwrap_or_else(|| quote(&tt_name));
        self.run_predicate(at.end, limit, &visible, &tt_name);
    }

    /// Restricts an `UPDATE` / `DELETE` to the rows of the run: the
    /// predicate is added to its `WHERE`, found at the statement's top level
    /// after the target.
    fn run_predicate(&mut self, from: usize, limit: usize, visible: &str, name: &str) {
        let tokens: Vec<Token> = offset_tokens(self.sql, from..limit.max(from))
            .into_iter()
            .filter(significant)
            .collect();
        let word = |t: &Token, w: &str| {
            t.kind == Kind::Word && self.sql[t.range.clone()].eq_ignore_ascii_case(w)
        };
        let mut depth = 0usize;
        let mut where_at = None;
        let mut clause_end = None;
        let mut last_end = from;
        for (k, t) in tokens.iter().enumerate() {
            let s = &self.sql[t.range.clone()];
            if s == "(" {
                depth += 1;
            } else if s == ")" {
                if depth == 0 {
                    break;
                }
                depth -= 1;
            } else if depth == 0 && s == ";" {
                break;
            } else if depth == 0 && word(t, "where") && where_at.is_none() {
                if tokens.get(k + 1).is_some_and(|n| word(n, "current")) {
                    self.out.error(
                        t.range.clone(),
                        format!("WHERE CURRENT OF on isolated temp table `{name}`: not supported"),
                    );
                    return;
                }
                where_at = Some(k);
            } else if depth == 0 && word(t, "returning") {
                clause_end = Some(last_end);
                break;
            }
            last_end = t.range.end;
        }
        let end = clause_end.unwrap_or(last_end);
        let predicate = format!("{visible}.{RUN_COLUMN} = {RUN_VAR}");
        match where_at.and_then(|k| tokens.get(k + 1).map(|n| (k, n))) {
            Some((k, first)) => {
                let anchor = tokens[k].range.clone();
                self.out
                    .edit(first.range.start..first.range.start, "(", anchor.clone());
                self.out
                    .edit(end..end, format!(") AND {predicate}"), anchor);
            }
            None => self
                .out
                .edit(end..end, format!(" WHERE {predicate}"), from..end),
        }
    }

    fn update(&mut self, u: &UpdateStmt, limit: usize) {
        let UpdateStmt {
            id: _,
            span: _,
            with,
            with_recursive: _,
            table,
            assignments,
            from,
            joins,
            where_clause,
            returning,
            returning_aliases: _,
        } = u;
        self.ctes(with);
        self.target(table, limit, returning);
        assignments.iter().for_each(|a| self.expr(&a.value));
        from.iter().for_each(|t| self.table_ref(t));
        for j in joins {
            self.table_ref(&j.table);
            if let JoinConstraint::On(e) = &j.constraint {
                self.expr(e);
            }
        }
        if let Some(w) = where_clause {
            self.expr(w);
        }
        self.items(returning);
    }

    fn delete(&mut self, d: &DeleteStmt, limit: usize) {
        let DeleteStmt {
            id: _,
            span: _,
            with,
            with_recursive: _,
            table,
            using,
            joins,
            where_clause,
            returning,
            returning_aliases: _,
        } = d;
        self.ctes(with);
        self.target(table, limit, returning);
        using.iter().for_each(|t| self.table_ref(t));
        for j in joins {
            self.table_ref(&j.table);
            if let JoinConstraint::On(e) = &j.constraint {
                self.expr(e);
            }
        }
        if let Some(w) = where_clause {
            self.expr(w);
        }
        self.items(returning);
    }
}

fn quote(name: &str) -> String {
    emit_ident(&Ident {
        value: name.to_string(),
        quoted: !pgweave_ast::emit::is_plain_identifier(name),
    })
}

// ── @snapshot ─────────────────────────────────────────────────────────────────

fn snapshot_label(sql: &str, d: &Directive) -> Result<String, String> {
    let label = sql[d.rest.clone()].trim();
    let plain = label
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if plain {
        Ok(label.to_string())
    } else {
        Err("@snapshot: expected `@snapshot label;` (letters, digits, `_`)".into())
    }
}

/// Literal SQL string.
fn literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn snapshot_sql(
    sql: &str,
    d: &Directive,
    routine: &Routine,
    top: &Block,
    scopes: &[Scope],
    tables: &[TempTable],
) -> Result<String, String> {
    let label = snapshot_label(sql, d)?;
    // Variables visible here: parameters, top-level declarations, then the
    // blocks and loops around the directive.
    let mut vars: Vec<(String, bool)> = routine.params.iter().map(|p| (p.clone(), true)).collect();
    for decl in &top.declarations {
        if let Declaration::Variable(v) = decl {
            vars.push((v.name.value.clone(), !matches!(v.var_type, VarType::Record)));
        }
    }
    for s in scopes.iter().filter(|s| s.range.contains(&d.range.start)) {
        vars.extend(s.vars.iter().cloned());
    }
    if routine.trigger {
        vars.extend(
            ["TG_OP", "TG_TABLE_NAME", "NEW", "OLD"]
                .into_iter()
                .map(|v| (v.to_string(), true)),
        );
    }
    let pairs: Vec<String> = vars
        .iter()
        .map(|(name, captured)| {
            let key = literal(name);
            if *captured {
                format!("{key}, to_jsonb({name})")
            } else {
                format!("{key}, to_jsonb('not captured: record'::text)")
            }
        })
        .collect();
    // jsonb_build_object takes at most 100 arguments.
    let variables = if pairs.is_empty() {
        "'{}'::jsonb".to_string()
    } else {
        pairs
            .chunks(50)
            .map(|c| format!("jsonb_build_object({})", c.join(", ")))
            .collect::<Vec<_>>()
            .join(" || ")
    };
    let indent = indent_at(sql, d.range.start);
    let mut out = format!(
        "INSERT INTO pgweave_debug.snapshot (run, routine, label, taken_at, variables) \
         VALUES ({RUN_VAR}, {}, {}, clock_timestamp(), {variables});",
        literal(&routine.name),
        literal(&label),
    );
    for t in tables {
        out.push_str(&format!(
            "\n{indent}INSERT INTO pgweave_debug.snapshot_rows (run, label, table_name, row_data) \
             SELECT {RUN_VAR}, {}, {}, to_jsonb(_pgweave_t) - '{RUN_COLUMN}' FROM pg_temp.{} \
             AS _pgweave_t WHERE _pgweave_t.{RUN_COLUMN} = {RUN_VAR};",
            literal(&label),
            literal(&t.name),
            t.physical,
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{Instrumentation, LogLevel, LogTarget};
    use crate::{MacroSet, expand_source};
    use pgweave_diagnostics::Severity;

    fn body(stmts: &str) -> String {
        format!(
            "@function f(p INT): INT {{\n    DECLARE n INT;\n    \
             @temp_table work (id INT PRIMARY KEY, v INT);\n{stmts}\n}}\n"
        )
    }

    fn expand(src: &str, debug: bool) -> crate::Expansion {
        let log = if debug {
            LogLevel::Debug
        } else {
            LogLevel::None
        };
        logged(src, log, LogTarget::Table)
    }

    fn logged(src: &str, log: LogLevel, target: LogTarget) -> crate::Expansion {
        let i = Instrumentation { log, target };
        expand_source(src, &MacroSet::builtin().with_instrumentation(i))
    }

    fn sql(stmts: &str) -> String {
        let e = expand(&body(stmts), false);
        assert!(e.diagnostics.is_empty(), "{:?}\n{}", e.diagnostics, e.sql);
        e.sql
    }

    /// Error messages, with the source text each one points at.
    fn errors(src: &str) -> Vec<(String, String)> {
        expand(src, false)
            .diagnostics
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .map(|d| {
                let s = d.span.as_ref().expect("located");
                (d.message.clone(), src[s.start..s.end].to_string())
            })
            .collect()
    }

    fn physical(sql: &str) -> String {
        let at = sql.find("pg_temp.work_").expect("physical name") + "pg_temp.".len();
        sql[at..at + "work_".len() + 8].to_string()
    }

    #[test]
    fn reads_and_writes_are_restricted_to_the_run() {
        let s = sql("    INSERT INTO work VALUES (p, p);\n    \
             UPDATE work SET v = v + 1;\n    \
             DELETE FROM work AS w WHERE w.v > 3 RETURNING w.id INTO n;\n    \
             SELECT count(*) INTO n FROM work JOIN work AS other USING (id);\n    \
             RETURN n;");
        let p = physical(&s);
        let read =
            format!("(SELECT id, v FROM pg_temp.{p} WHERE {p}._pgweave_run = _pgweave_run_id)");
        assert!(
            s.contains(&format!("{read} AS work JOIN {read} AS other")),
            "{s}"
        );
        assert!(
            s.contains(&format!(
                "PERFORM set_config('pgweave.run', _pgweave_run_id::text, true);\n    \
                 INSERT INTO pg_temp.{p} AS work VALUES"
            )),
            "{s}"
        );
        assert!(
            s.contains(&format!(
                "UPDATE pg_temp.{p} AS work SET v = v + 1 WHERE work._pgweave_run = \
                 _pgweave_run_id;"
            )),
            "{s}"
        );
        assert!(
            s.contains(&format!(
                "DELETE FROM pg_temp.{p} AS w WHERE (w.v > 3) AND w._pgweave_run = \
                 _pgweave_run_id RETURNING w.id INTO n;"
            )),
            "{s}"
        );
        // The cleanup, then the RETURN of a variable; none after it.
        assert!(
            s.contains(&format!(
                "DELETE FROM pg_temp.{p} WHERE {p}._pgweave_run = _pgweave_run_id;\n    \
                 RETURN n;\n\nEND;"
            )),
            "{s}"
        );
        assert!(
            s.contains("_pgweave_run_id uuid := gen_random_uuid();"),
            "{s}"
        );
    }

    #[test]
    fn the_physical_name_follows_the_definition() {
        let a = physical(&sql("    RETURN 1;"));
        let again = physical(&sql("    PERFORM 1;\n    RETURN 1;"));
        assert_eq!(a, again, "stable across builds and bodies");
        let other = expand(
            "@function f(): INT {\n    @temp_table work (id BIGINT);\n    RETURN 1;\n}\n",
            false,
        );
        assert_ne!(a, physical(&other.sql), "another definition, another table");
    }

    #[test]
    fn primary_keys_and_conflicts_include_the_run() {
        let s =
            sql("    INSERT INTO work VALUES (1, 1) ON CONFLICT (id) DO NOTHING;\n    RETURN 1;");
        assert!(s.contains("PRIMARY KEY (id, _pgweave_run)"), "{s}");
        assert!(
            s.contains("ON CONFLICT (id, _pgweave_run) DO NOTHING"),
            "{s}"
        );
    }

    #[test]
    fn statements_pgweave_does_not_rewrite_are_refused() {
        let cases = [
            (
                "    TRUNCATE work;\n    RETURN 1;",
                "TRUNCATE work;",
                "TRUNCATE → DELETE",
            ),
            (
                "    INSERT INTO work VALUES (1, 1) RETURNING *;\n    RETURN 1;",
                "work",
                "RETURNING *",
            ),
            (
                "    WITH work AS (SELECT 1 AS id) SELECT id INTO n FROM work;\n    RETURN n;",
                "@temp_table work (id INT PRIMARY KEY, v INT);",
                "shadows",
            ),
            (
                "    INSERT INTO work VALUES (1, 1) ON CONFLICT ON CONSTRAINT work_pkey DO \
                 NOTHING;\n    RETURN 1;",
                "work",
                "ON CONSTRAINT",
            ),
        ];
        for (stmts, at, msg) in cases {
            let src = body(stmts);
            let errs = errors(&src);
            assert!(
                errs.iter().any(|(m, t)| m.contains(msg) && t == at),
                "{stmts}: {errs:?}"
            );
            // The body stays analyzable: the table exists under its name.
            let e = expand(&src, false);
            assert!(
                e.sql.contains("CREATE TEMP TABLE IF NOT EXISTS work"),
                "{}",
                e.sql
            );
            assert!(!e.sql.contains("@temp_table"), "{}", e.sql);
        }
    }

    #[test]
    fn dynamic_sql_is_a_warning() {
        let e = expand(
            &body("    EXECUTE 'DELETE FROM work';\n    RETURN 1;"),
            false,
        );
        let w: Vec<_> = e
            .diagnostics
            .iter()
            .filter(|d| d.code == super::NOT_ISOLATED)
            .collect();
        assert_eq!(w.len(), 1, "{:?}", e.diagnostics);
        assert_eq!(w[0].severity, Severity::Warning);
        assert!(e.sql.contains("_pgweave_run_id"), "the rest is isolated");
    }

    #[test]
    fn directives_need_a_plpgsql_macro_body() {
        let src = "@temp_table work (id INT);\nSELECT 1;\n";
        let errs = errors(src);
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert_eq!(errs[0].1, "@temp_table work (id INT);");
        let src = "@function f(): INT language sql {\n    @snapshot here;\n    SELECT 1;\n}\n";
        assert!(
            errors(src)[0].0.contains("PL/pgSQL body"),
            "{:?}",
            errors(src)
        );
    }

    #[test]
    fn snapshots_emit_nothing_outside_a_debug_build() {
        let src = "@function f(p INT): INT {\n    DECLARE n INT := 1;\n    @snapshot start;\n    \
                   RETURN n;\n}\n";
        let normal = expand(src, false);
        assert!(normal.diagnostics.is_empty(), "{:?}", normal.diagnostics);
        assert!(!normal.sql.contains("pgweave_debug"), "{}", normal.sql);
        assert!(
            !normal.sql.contains("_pgweave_run_id"),
            "no run without a temp table"
        );
        let debug = expand(src, true);
        assert!(debug.diagnostics.is_empty(), "{:?}", debug.diagnostics);
        assert!(
            debug.sql.contains(
                "VALUES (_pgweave_run_id, 'f', 'start', clock_timestamp(), \
                 jsonb_build_object('p', to_jsonb(p), 'n', to_jsonb(n)));"
            ),
            "{}",
            debug.sql
        );
        assert!(
            errors("@function f(): INT {\n    @snapshot 'x y';\n    RETURN 1;\n}\n")[0]
                .0
                .contains("@snapshot label;")
        );
    }

    #[test]
    fn logs_follow_the_level_and_the_target() {
        let src = "@function f(k INT): INT {\n    UPDATE t SET a = 1;\n    RETURN k;\n}\n";
        let at = |log, target| {
            let e = logged(src, log, target);
            assert!(e.diagnostics.is_empty(), "{:?}", e.diagnostics);
            e.sql
        };
        let none = at(LogLevel::None, LogTarget::Table);
        assert!(!none.contains("_pgweave"), "{none}");

        let info = at(LogLevel::Info, LogTarget::Table);
        assert!(info.contains("INSERT INTO pgweave_log.run"), "{info}");
        assert!(!info.contains("GET DIAGNOSTICS"), "no step at info: {info}");
        assert!(!info.contains("pg_notify"), "{info}");

        let step = at(LogLevel::Step, LogTarget::Table);
        assert!(
            step.contains(
                "    UPDATE t SET a = 1;\n    GET DIAGNOSTICS _pgweave_rows = ROW_COUNT;\n    \
                 _pgweave_steps := array_append(_pgweave_steps, jsonb_build_object('kind', \
                 'UPDATE', 'target', 't', 'line', 2,"
            ),
            "{step}"
        );
        // The log is written right before the RETURN.
        let flush = step.find("INSERT INTO pgweave_log.run").expect("flush");
        let ret = step[flush..].find("RETURN ").expect("RETURN");
        assert!(Some(ret) < step[flush..].find("END;"), "{step}");

        let notify = at(LogLevel::Step, LogTarget::Notify);
        assert!(notify.contains("pg_notify('pgweave_log'"), "{notify}");
        assert!(!notify.contains("pgweave_log.run"), "{notify}");
    }

    #[test]
    fn a_return_value_is_evaluated_before_the_exit_code() {
        // It may read FOUND, a temp table, or call routines: runs of this one.
        let e = logged(
            "@function f(k INT): BOOLEAN {\n    UPDATE t SET a = k;\n    RETURN FOUND;\n}\n",
            LogLevel::Info,
            LogTarget::Table,
        );
        let value = e
            .sql
            .find("DECLARE _pgweave_result BOOLEAN := FOUND;")
            .expect(&e.sql);
        let log = e.sql.find("INSERT INTO pgweave_log.run").unwrap();
        let ret = e.sql.find("RETURN _pgweave_result;").unwrap();
        assert!(value < log && log < ret, "{}", e.sql);
        let s = sql("    RETURN (SELECT count(*) FROM work);");
        let p = physical(&s);
        let value = s
            .find("DECLARE _pgweave_result INTEGER := (SELECT count(*) FROM (SELECT")
            .expect(&s);
        let cleanup = s.find(&format!("DELETE FROM pg_temp.{p}")).unwrap();
        assert!(value < cleanup, "{s}");
        // Modifiers are not part of a result type; a trigger returns a row.
        let e = logged(
            "@function f(): VARCHAR(3) {\n    RETURN 'long' || 'er';\n}\n",
            LogLevel::Info,
            LogTarget::Table,
        );
        assert!(
            e.sql
                .contains("DECLARE _pgweave_result varchar := 'long' || 'er';"),
            "{}",
            e.sql
        );
        let e = logged(
            "@trigger t before insert on x for each row {\n    RETURN f(NEW);\n}\n",
            LogLevel::Info,
            LogTarget::Table,
        );
        assert!(
            e.sql.contains("DECLARE _pgweave_result record := f(NEW);"),
            "{}",
            e.sql
        );
        // A variable, a field or a constant: returned after the exit code,
        // no block.
        for value in ["NEW", "NULL", "NEW.a", "0", "'x'"] {
            let e = logged(
                &format!(
                    "@trigger t before insert on x for each row {{\n    RETURN {value};\n}}\n"
                ),
                LogLevel::Info,
                LogTarget::Table,
            );
            assert!(!e.sql.contains("_pgweave_result"), "{}", e.sql);
            let log = e.sql.find("INSERT INTO pgweave_log.run").expect(&e.sql);
            assert!(
                log < e.sql.find(&format!("RETURN {value};")).unwrap(),
                "{}",
                e.sql
            );
        }
    }

    #[test]
    fn a_polymorphic_result_is_returned_after_the_exit_code() {
        let e = logged(
            "@function f(x ANYELEMENT): ANYELEMENT {\n    RETURN g(x);\n}\n",
            LogLevel::Info,
            LogTarget::Table,
        );
        let log = e.sql.find("INSERT INTO pgweave_log.run").expect(&e.sql);
        assert!(log < e.sql.find("RETURN g(x);").unwrap(), "{}", e.sql);
        assert!(
            e.diagnostics
                .iter()
                .any(|d| d.code == super::NOT_LOGGED && d.message.contains("polymorphic")),
            "{:?}",
            e.diagnostics
        );
    }

    #[test]
    fn routines_not_logged_are_left_as_written() {
        // An SQL-language routine: nothing to log in it.
        let src = "@function f(k INT): INT language sql {\n    SELECT k;\n}\n";
        let plain = logged(src, LogLevel::None, LogTarget::Table);
        let info = logged(src, LogLevel::Step, LogTarget::Table);
        assert!(info.diagnostics.is_empty(), "{:?}", info.diagnostics);
        assert_eq!(plain.sql, info.sql);
    }

    #[test]
    fn diagnostics_of_the_body_keep_their_position() {
        // The rewrite does not move what the user wrote: a diagnostic on `v`
        // still points at it.
        let src = body("    UPDATE work SET v = 1 WHERE id = p;\n    RETURN 1;");
        let e = expand(&src, false);
        let at = e.sql.find("v = 1").unwrap();
        let (s, _) = e.map.remap(at, at + 1).unwrap();
        assert_eq!(&src[s..s + 5], "v = 1");
    }
}
