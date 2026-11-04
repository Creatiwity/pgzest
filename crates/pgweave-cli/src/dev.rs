//! `pgweave dev`: applies the routines of the project's schema files to a
//! development database, built with run logs and snapshots, then again
//! each time a file is saved (PLAN.md P9).
//!
//! Only routines are applied — functions, procedures, triggers, which
//! `CREATE OR REPLACE` swaps in place. Tables and the rest of the schema
//! go through migrations. The routines a file no longer declares are
//! dropped, as long as this session created them: nothing else in the
//! database is touched.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use pgweave_diagnostics::Severity;
use pgweave_macro::{Instrumentation, MacroSet};
use pgweave_schema::source::SchemaSource;
use pgweave_schema::{FileRole, ProjectConfig};
use tokio_postgres::error::{ErrorPosition, SqlState};
use tokio_postgres::{Client, NoTls};

/// How often the files are compared with their last applied content.
const INTERVAL: Duration = Duration::from_millis(300);

pub struct Options {
    pub url: String,
    pub instrumentation: Instrumentation,
    /// Apply once and exit (1 if a file could not be applied).
    pub once: bool,
}

pub fn run(opts: Options) -> i32 {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {e}");
            return 2;
        }
    };
    runtime.block_on(watch(opts))
}

/// What `pgweave.toml` and the macros directory hold: a change reloads the
/// macros and re-applies every file.
#[derive(PartialEq)]
struct Environment {
    config: String,
    macros: BTreeMap<PathBuf, String>,
}

/// What a file created in the database, by this session.
#[derive(Default, Clone)]
struct Created {
    /// `regprocedure` text of each routine.
    routines: BTreeSet<String>,
    /// `(trigger, table)`, both as PostgreSQL prints them.
    triggers: BTreeSet<(String, String)>,
}

struct FileState {
    content: String,
    created: Created,
}

async fn watch(opts: Options) -> i32 {
    let project = match crate::discover_project() {
        Ok(Some(p)) => p,
        Ok(None) => {
            eprintln!(
                "error: no {} found: `pgweave dev` applies a project's routines",
                ProjectConfig::FILENAME
            );
            return 2;
        }
        Err(e) => {
            eprintln!("error: {e}");
            return 2;
        }
    };
    let root = project.0.clone();
    let config: tokio_postgres::Config = match opts.url.parse() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: database URL: {e}");
            return 2;
        }
    };
    let Some(mut client) = connect(&config).await else {
        return 2;
    };
    let i = opts.instrumentation;
    println!(
        "pgweave dev: routines of {} → {} (log {}, {}){}",
        root.display(),
        describe(&config),
        i.log.as_str(),
        i.target.as_str(),
        if opts.once { "" } else { "; Ctrl-C to stop" }
    );

    let mut env: Option<Environment> = None;
    let mut project = project;
    let mut macros = MacroSet::builtin();
    let mut files: BTreeMap<PathBuf, FileState> = BTreeMap::new();
    loop {
        if client.is_closed() {
            match connect(&config).await {
                Some(c) => {
                    println!("reconnected");
                    client = c;
                    env = None;
                }
                None if opts.once => return 1,
                None => {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }
            }
        }
        let mut failed = false;
        let current = read_environment(&project.0, &project.1);
        let reload = env.as_ref() != Some(&current);
        if reload {
            match ProjectConfig::load(&root.join(ProjectConfig::FILENAME)) {
                Ok(c) => project.1 = c,
                Err(e) => eprintln!("error: {e}"),
            }
            let (set, errors) = crate::load_macros(Some(&project));
            for e in &errors {
                eprintln!("warning: {e}");
            }
            macros = set.with_instrumentation(i);
            let setup = i.setup_sql();
            if !setup.is_empty()
                && let Err(e) = client.batch_execute(&setup).await
            {
                eprintln!("error: creating the log tables: {}", message(&e));
                failed = true;
            }
            env = Some(current);
        }

        let sources: BTreeMap<PathBuf, String> = project
            .1
            .to_source(&root)
            .chunks()
            .into_iter()
            .filter(|c| c.role == FileRole::Schema && c.origin.byte_range.start == 0)
            .map(|c| (c.origin.path, c.sql))
            .collect();
        let gone: Vec<PathBuf> = files
            .keys()
            .filter(|p| !sources.contains_key(*p))
            .cloned()
            .collect();
        for path in gone {
            let state = files.remove(&path).expect("listed");
            let shown = shown(&path, &root);
            match drop_stale(&client, &state.created, &Created::default()).await {
                Ok(()) => println!("{shown}: removed, its routines dropped"),
                Err(e) => {
                    eprintln!("{shown}: error: {}", message(&e));
                    failed = true;
                }
            }
        }
        for (path, text) in sources {
            let previous = files.get(&path);
            if !reload && previous.is_some_and(|s| s.content == text) {
                continue;
            }
            let before = previous.map(|s| s.created.clone()).unwrap_or_default();
            let shown = shown(&path, &root);
            let created = match apply_file(&client, &shown, &text, &macros, &before).await {
                Some(created) => created,
                None => {
                    failed = true;
                    before
                }
            };
            files.insert(
                path,
                FileState {
                    content: text,
                    created,
                },
            );
        }
        if opts.once {
            return i32::from(failed);
        }
        tokio::time::sleep(INTERVAL).await;
    }
}

async fn connect(config: &tokio_postgres::Config) -> Option<Client> {
    match config.connect(NoTls).await {
        Ok((client, conn)) => {
            tokio::spawn(async move {
                if let Err(e) = conn.await {
                    eprintln!("error: connection lost: {e}");
                }
            });
            Some(client)
        }
        Err(e) => {
            eprintln!("error: cannot connect to {}: {e}", describe(config));
            None
        }
    }
}

/// `host:port/dbname`, without the credentials.
fn describe(config: &tokio_postgres::Config) -> String {
    let host = match config.get_hosts().first() {
        Some(tokio_postgres::config::Host::Tcp(h)) => h.clone(),
        #[cfg(unix)]
        Some(tokio_postgres::config::Host::Unix(p)) => p.display().to_string(),
        None => "localhost".into(),
    };
    let port = config.get_ports().first().copied().unwrap_or(5432);
    let db = config
        .get_dbname()
        .or(config.get_user())
        .unwrap_or("postgres");
    format!("{host}:{port}/{db}")
}

fn shown(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

fn read_environment(root: &Path, config: &ProjectConfig) -> Environment {
    let mut macros = BTreeMap::new();
    let mut stack = vec![root.join(&config.macros.directory)];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let path = e.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(text) = std::fs::read_to_string(&path) {
                macros.insert(path, text);
            }
        }
    }
    Environment {
        config: std::fs::read_to_string(root.join(ProjectConfig::FILENAME)).unwrap_or_default(),
        macros,
    }
}

// ── A file ────────────────────────────────────────────────────────────────────

/// A routine or trigger statement of the expansion, as sent.
struct Item {
    /// Statement text, `OR REPLACE` added if missing.
    sql: String,
    /// Range of the statement in the expansion.
    range: std::ops::Range<usize>,
    /// Bytes inserted at `inserted.0` of `sql` (`OR REPLACE`).
    inserted: (usize, usize),
    kind: ItemKind,
}

enum ItemKind {
    /// `name(argument types)`, if pgweave could read the signature.
    Routine(Option<String>),
    /// `(trigger, table)`, if pgweave could read them.
    Trigger(Option<(String, String)>),
}

/// Applies the routines of one file in a transaction. `None`: nothing
/// applied, the errors are printed.
async fn apply_file(
    client: &Client,
    shown: &str,
    text: &str,
    macros: &MacroSet,
    before: &Created,
) -> Option<Created> {
    let exp = pgweave_macro::expand_source(text, macros);
    let mut errors = false;
    for d in &exp.diagnostics {
        let sev = match d.severity {
            Severity::Error => {
                errors = true;
                "error"
            }
            Severity::Warning => "warning",
            Severity::Info => continue,
        };
        let (line, col) = d
            .span
            .as_ref()
            .map(|s| crate::offset_to_line_col(text, s.start))
            .unwrap_or((1, 1));
        eprintln!("{shown}:{line}:{col}: {sev}[{}]: {}", d.code, d.message);
    }
    if errors {
        eprintln!("{shown}: not applied");
        return None;
    }
    let (statements, unreadable) = pgweave_parser::split_statements(&exp.sql);
    if let Some((range, msg)) = unreadable {
        let at = source_offset(&exp, range.start).unwrap_or(0);
        let (line, col) = crate::offset_to_line_col(text, at);
        eprintln!("{shown}:{line}:{col}: error: {msg}; not applied");
        return None;
    }
    let items: Vec<Item> = statements
        .into_iter()
        .filter_map(|s| item(&exp.sql, s))
        .collect();
    if items.is_empty() && before.routines.is_empty() && before.triggers.is_empty() {
        return Some(Created::default());
    }

    let result = apply_items(client, &items).await;
    let created = match result {
        Ok(created) => created,
        Err((index, e)) => {
            let _ = client.batch_execute("ROLLBACK").await;
            report(shown, text, &exp, index.map(|i| &items[i]), &e);
            eprintln!("{shown}: not applied");
            return None;
        }
    };
    if let Err(e) = drop_stale(client, before, &created).await {
        let _ = client.batch_execute("ROLLBACK").await;
        eprintln!("{shown}: error: {}", message(&e));
        eprintln!("{shown}: not applied");
        return None;
    }
    if let Err(e) = client.batch_execute("COMMIT").await {
        eprintln!("{shown}: error: {}", message(&e));
        return None;
    }
    let routines = items
        .iter()
        .filter(|i| matches!(i.kind, ItemKind::Routine(_)))
        .count();
    let triggers = items.len() - routines;
    println!("{shown}: applied {routines} routine(s), {triggers} trigger(s)",);
    Some(created)
}

/// The routine or trigger a statement creates, `OR REPLACE` ensured.
fn item(sql: &str, s: pgweave_parser::SplitStatement) -> Option<Item> {
    let words: Vec<&str> = s.keywords.iter().map(String::as_str).collect();
    let (replace, rest) = match words.as_slice() {
        ["CREATE", "OR", "REPLACE", rest @ ..] => (true, rest),
        ["CREATE", rest @ ..] => (false, rest),
        _ => return None,
    };
    let trigger = match rest {
        ["FUNCTION" | "PROCEDURE", ..] => false,
        ["TRIGGER", ..] | ["CONSTRAINT", "TRIGGER", ..] => true,
        _ => return None,
    };
    let text = &sql[s.range.clone()];
    let (stmt, inserted) = if replace {
        (text.to_string(), (0, 0))
    } else {
        // The statement starts with its `CREATE` word.
        let at = "CREATE".len();
        let add = " OR REPLACE";
        (
            format!("{}{add}{}", &text[..at], &text[at..]),
            (at, add.len()),
        )
    };
    let parsed = pgweave_parser::parse(&stmt)
        .ok()
        .and_then(|v| v.into_iter().next());
    let kind = if trigger {
        ItemKind::Trigger(match parsed {
            Some(pgweave_ast::Statement::Ddl(pgweave_ast::DdlStatement::CreateTrigger(t))) => {
                Some((
                    pgweave_ast::emit::emit_ident(&t.name),
                    pgweave_ast::emit::emit_object_name(&t.table),
                ))
            }
            _ => None,
        })
    } else {
        let signature = |name: &pgweave_ast::ObjectName, params: &[pgweave_ast::FunctionParam]| {
            let args: Vec<String> = params
                .iter()
                .filter(|p| !matches!(p.mode, pgweave_ast::ParamMode::Out))
                .map(|p| pgweave_ast::emit::emit_data_type(&p.data_type))
                .collect();
            format!(
                "{}({})",
                pgweave_ast::emit::emit_object_name(name),
                args.join(", ")
            )
        };
        ItemKind::Routine(match parsed {
            Some(pgweave_ast::Statement::Ddl(pgweave_ast::DdlStatement::CreateFunction(f))) => {
                Some(signature(&f.name, &f.params))
            }
            Some(pgweave_ast::Statement::Ddl(pgweave_ast::DdlStatement::CreateProcedure(p))) => {
                Some(signature(&p.name, &p.params))
            }
            _ => None,
        })
    };
    Some(Item {
        sql: stmt,
        range: s.range,
        inserted,
        kind,
    })
}

type ApplyError = (Option<usize>, tokio_postgres::Error);

/// Runs the items in an open transaction (left open on success).
async fn apply_items(client: &Client, items: &[Item]) -> Result<Created, ApplyError> {
    client.batch_execute("BEGIN").await.map_err(|e| (None, e))?;
    let mut created = Created::default();
    for (index, it) in items.iter().enumerate() {
        let fail = |e| (Some(index), e);
        client
            .batch_execute("SAVEPOINT pgweave_dev")
            .await
            .map_err(fail)?;
        match client.batch_execute(&it.sql).await {
            Ok(()) => {}
            // A changed return type or parameter name: PostgreSQL asks for
            // a DROP first, refused if something depends on the routine.
            Err(e)
                if e.code() == Some(&SqlState::INVALID_FUNCTION_DEFINITION)
                    && matches!(it.kind, ItemKind::Routine(Some(_))) =>
            {
                let ItemKind::Routine(Some(sig)) = &it.kind else {
                    unreachable!()
                };
                client
                    .batch_execute("ROLLBACK TO SAVEPOINT pgweave_dev")
                    .await
                    .map_err(fail)?;
                let existing = regprocedure(client, sig).await.map_err(fail)?;
                let Some(existing) = existing else {
                    return Err(fail(e));
                };
                client
                    .batch_execute(&format!("DROP ROUTINE {existing}"))
                    .await
                    .map_err(fail)?;
                client.batch_execute(&it.sql).await.map_err(fail)?;
            }
            Err(e) => return Err(fail(e)),
        }
        match &it.kind {
            ItemKind::Routine(Some(sig)) => {
                if let Some(id) = regprocedure(client, sig).await.map_err(fail)? {
                    created.routines.insert(id);
                }
            }
            ItemKind::Trigger(Some(t)) => {
                created.triggers.insert(t.clone());
            }
            _ => {}
        }
    }
    Ok(created)
}

/// The routine `signature` names, as PostgreSQL prints it; `None` if
/// there is none.
async fn regprocedure(
    client: &Client,
    signature: &str,
) -> Result<Option<String>, tokio_postgres::Error> {
    let row = client
        .query_one("SELECT to_regprocedure($1)::text", &[&signature])
        .await?;
    Ok(row.get(0))
}

/// Drops what `before` created and `now` no longer does.
async fn drop_stale(
    client: &Client,
    before: &Created,
    now: &Created,
) -> Result<(), tokio_postgres::Error> {
    for (name, table) in before.triggers.difference(&now.triggers) {
        client
            .batch_execute(&format!("DROP TRIGGER IF EXISTS {name} ON {table}"))
            .await?;
    }
    for id in before.routines.difference(&now.routines) {
        client
            .batch_execute(&format!("DROP ROUTINE IF EXISTS {id}"))
            .await?;
    }
    Ok(())
}

/// Position in the source of a position of the expansion.
fn source_offset(exp: &pgweave_macro::Expansion, at: usize) -> Option<usize> {
    exp.map.remap(at, at + 1).map(|(s, _)| s)
}

/// Prints a database error at its place in the source.
fn report(
    shown: &str,
    text: &str,
    exp: &pgweave_macro::Expansion,
    item: Option<&Item>,
    e: &tokio_postgres::Error,
) {
    let Some(db) = e.as_db_error() else {
        eprintln!("{shown}: error: {e}");
        return;
    };
    let at = item.map(|it| {
        let in_stmt = match db.position() {
            // 1-based character position in the statement sent.
            Some(ErrorPosition::Original(p)) => it
                .sql
                .char_indices()
                .nth((*p as usize).saturating_sub(1))
                .map_or(0, |(b, _)| b),
            _ => 0,
        };
        let (at, len) = it.inserted;
        let in_stmt = if in_stmt >= at + len {
            in_stmt - len
        } else {
            in_stmt.min(at)
        };
        source_offset(exp, it.range.start + in_stmt)
            .or_else(|| source_offset(exp, it.range.start))
            .unwrap_or(0)
    });
    let (line, col) = crate::offset_to_line_col(text, at.unwrap_or(0));
    eprintln!("{shown}:{line}:{col}: error: {}", db.message());
    if let Some(detail) = db.detail() {
        eprintln!("  detail: {detail}");
    }
    if let Some(hint) = db.hint() {
        eprintln!("  hint: {hint}");
    }
    if let Some(w) = db.where_() {
        eprintln!("  where: {w}");
    }
}

fn message(e: &tokio_postgres::Error) -> String {
    match e.as_db_error() {
        Some(db) => db.message().to_string(),
        None => e.to_string(),
    }
}
