//! Applying migrations (`docs/decisions/0002-migrate-apply.md`): the part
//! that talks to the database, shared by `pgzest migrate apply` and an
//! application applying its migrations at startup.
//!
//! What can be known without a database is decided before: each
//! [`Migration`] arrives with its checksum, its header, the statements to
//! run outside a transaction, and the reasons it must not be applied
//! ([`Migration::blockers`]). Here, under an advisory lock:
//!
//! 1. the registry is read and checked against the migrations: a recorded
//!    migration that is missing or was edited, a run left unfinished, or a
//!    pending migration sorting before an applied one stops everything;
//! 2. the server is checked against the headers: PostgreSQL major version,
//!    extension versions available for the pending migrations, extensions
//!    installed as the last applied migration left them;
//! 3. each pending migration runs in its own transaction, its registry row
//!    written in the same one. A migration of statements that cannot run in
//!    a transaction (`CREATE INDEX CONCURRENTLY`…) runs statement by
//!    statement, its row written as started first and finished last.
//!
//! Nothing runs until every check passed.

use std::fmt;
use std::time::{Duration, Instant};

use tokio_postgres::{Client, GenericClient};

/// Registry of the flat layout, in the connection's `current_schema()`.
pub const DEFAULT_TABLE: &str = "_pgzest_migrations";
/// Default `lock_timeout` of a migration: one waiting for an `ACCESS
/// EXCLUSIVE` lock queues every query behind it.
pub const DEFAULT_LOCK_TIMEOUT: &str = "5s";
/// Advisory lock held for a whole run (`pgzest:mg`).
const LOCK_KEY: i64 = 0x7067_7a65_7374_6d67;

/// A migration of the repository, in repository order.
#[derive(Debug, Clone)]
pub struct Migration {
    /// File stem (`0007_add_loyalty`).
    pub name: String,
    /// The file, applied as written.
    pub sql: String,
    /// SHA-256 of the file.
    pub checksum: [u8; 32],
    /// Its `-- pgzest:` header.
    pub header: Option<Header>,
    /// Statements that cannot run in a transaction, when the migration is
    /// made only of them: run one by one, in order. Empty: the whole file
    /// runs in one transaction.
    pub separate: Vec<String>,
    /// Why it must not be applied (a change left to write, statements that
    /// cannot share a transaction mixed with others…): a pending migration
    /// with blockers stops the run before anything is applied.
    pub blockers: Vec<String>,
}

/// The context a migration was generated in: the state after it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    /// The line, as written (recorded with the migration).
    pub line: String,
    pub postgres: u32,
    /// `(name, version)` of the active extensions.
    pub extensions: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
pub struct Options {
    /// Registry table, optionally qualified (`app._pgzest_migrations`).
    pub table: String,
    /// `lock_timeout` of each migration; `None` leaves the server's.
    pub lock_timeout: Option<String>,
    /// PostgreSQL major version of the project.
    pub postgres: u32,
}

impl Options {
    pub fn new(postgres: u32) -> Self {
        Options {
            table: DEFAULT_TABLE.into(),
            lock_timeout: Some(DEFAULT_LOCK_TIMEOUT.into()),
            postgres,
        }
    }
}

#[derive(Debug)]
pub enum Error {
    /// The database or the repository is not in a state migrations can be
    /// applied from; nothing was applied.
    Refused(String),
    /// A query of pgzest's own failed.
    Db {
        during: String,
        source: tokio_postgres::Error,
    },
    /// A migration failed. Rolled back with its registry row, unless it
    /// ran outside a transaction (`separate`): then its row stays started
    /// and [`resolve`] closes it.
    Migration {
        name: String,
        /// Line in the file: of the error when PostgreSQL gave a position,
        /// of the failed statement when it ran alone.
        line: Option<usize>,
        /// Column, when PostgreSQL gave a position.
        column: Option<usize>,
        transactional: bool,
        source: tokio_postgres::Error,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Refused(m) => write!(f, "{m}"),
            Error::Db { during, source } => write!(f, "{during}: {}", db_message(source)),
            Error::Migration {
                name,
                line,
                column,
                transactional,
                source,
            } => {
                write!(f, "migration {name}")?;
                if let Some(line) = line {
                    write!(f, ", line {line}")?;
                }
                if let Some(column) = column {
                    write!(f, ", column {column}")?;
                }
                write!(f, ": {}", db_message(source))?;
                if *transactional {
                    write!(f, " (rolled back, nothing of it applied)")
                } else {
                    write!(
                        f,
                        " (it runs outside a transaction: what ran before stays; check the \
                         database, then record the outcome with `pgzest migrate resolve {name} \
                         --applied` or `--rolled-back`)"
                    )
                }
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Refused(_) => None,
            Error::Db { source, .. } | Error::Migration { source, .. } => Some(source),
        }
    }
}

fn db_message(e: &tokio_postgres::Error) -> String {
    match e.as_db_error() {
        Some(db) => {
            let mut m = format!("{}: {}", db.code().code(), db.message());
            if let Some(d) = db.detail() {
                m.push_str(&format!(" ({d})"));
            }
            m
        }
        None => e.to_string(),
    }
}

fn db(during: impl Into<String>) -> impl FnOnce(tokio_postgres::Error) -> Error {
    let during = during.into();
    move |source| Error::Db { during, source }
}

/// What the database received, and what it has not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// Indexes of the applied migrations.
    pub applied: Vec<usize>,
    /// Indexes of the pending ones, in order.
    pub pending: Vec<usize>,
    /// The registry does not exist yet.
    pub new_registry: bool,
}

/// A row of the registry.
struct Recorded {
    name: String,
    checksum: Vec<u8>,
    finished: bool,
}

/// Quoted table name: each part quoted, so a name is never read as SQL.
fn quoted_table(table: &str) -> Result<String, Error> {
    let parts: Vec<&str> = table.split('.').collect();
    if parts.len() > 2 || parts.iter().any(|p| p.is_empty()) {
        return Err(Error::Refused(format!(
            "registry table `{table}`: expected `name` or `schema.name`"
        )));
    }
    Ok(parts
        .iter()
        .map(|p| format!("\"{}\"", p.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join("."))
}

async fn registry<C: GenericClient>(
    client: &C,
    table: &str,
) -> Result<Option<Vec<Recorded>>, Error> {
    let exists: bool = client
        .query_one("SELECT to_regclass($1) IS NOT NULL", &[&table])
        .await
        .map_err(db("looking for the registry"))?
        .get(0);
    if !exists {
        return Ok(None);
    }
    let rows = client
        .query(
            &format!("SELECT name, checksum, finished_at IS NOT NULL FROM {table}"),
            &[],
        )
        .await
        .map_err(db("reading the registry"))?;
    Ok(Some(
        rows.iter()
            .map(|r| Recorded {
                name: r.get(0),
                checksum: r.get(1),
                finished: r.get(2),
            })
            .collect(),
    ))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Reads the registry and checks it, and the server, against `migrations`.
/// Read-only: nothing is created or locked.
pub async fn plan<C: GenericClient>(
    client: &C,
    migrations: &[Migration],
    opts: &Options,
) -> Result<Plan, Error> {
    let table = quoted_table(&opts.table)?;
    let recorded = registry(client, &table).await?;
    let new_registry = recorded.is_none();
    let recorded = recorded.unwrap_or_default();

    let unfinished: Vec<&str> = recorded
        .iter()
        .filter(|r| !r.finished)
        .map(|r| r.name.as_str())
        .collect();
    if !unfinished.is_empty() {
        return Err(Error::Refused(format!(
            "migration(s) started and never finished: {} — they ran outside a transaction and \
             failed or were interrupted; check the database, then record the outcome with \
             `pgzest migrate resolve <name> --applied` or `--rolled-back`",
            unfinished.join(", ")
        )));
    }
    let unknown: Vec<&str> = recorded
        .iter()
        .filter(|r| !migrations.iter().any(|m| m.name == r.name))
        .map(|r| r.name.as_str())
        .collect();
    if !unknown.is_empty() {
        return Err(Error::Refused(format!(
            "applied to the database, missing from the repository: {} — the repository is \
             behind the database",
            unknown.join(", ")
        )));
    }
    let mut applied = Vec::new();
    let mut pending = Vec::new();
    for (i, m) in migrations.iter().enumerate() {
        match recorded.iter().find(|r| r.name == m.name) {
            Some(r) if r.checksum != m.checksum => {
                return Err(Error::Refused(format!(
                    "migration {} was edited after it was applied (checksum {} recorded, {} \
                     now): an applied migration is history — revert the edit and write a new \
                     migration, or record the new content with `pgzest migrate resolve {} \
                     --checksum` after review",
                    m.name,
                    hex(&r.checksum),
                    hex(&m.checksum),
                    m.name
                )));
            }
            Some(_) => applied.push(i),
            None => pending.push(i),
        }
    }
    if let (Some(&last), Some(&first)) = (applied.last(), pending.first())
        && first < last
    {
        let late: Vec<&str> = pending
            .iter()
            .filter(|&&p| p < last)
            .map(|&p| migrations[p].name.as_str())
            .collect();
        return Err(Error::Refused(format!(
            "pending migration(s) sorting before the last applied one ({}): {} — the replay \
             verified the repository's order, the database would receive another one; rename \
             them after {}",
            migrations[last].name,
            late.join(", "),
            migrations[last].name
        )));
    }
    let blocked: Vec<String> = pending
        .iter()
        .flat_map(|&p| {
            migrations[p]
                .blockers
                .iter()
                .map(move |b| format!("  {}: {b}", migrations[p].name))
        })
        .collect();
    if !blocked.is_empty() {
        return Err(Error::Refused(format!(
            "pending migration(s) that cannot be applied:\n{}",
            blocked.join("\n")
        )));
    }

    check_server(client, migrations, &applied, &pending, opts).await?;
    Ok(Plan {
        applied,
        pending,
        new_registry,
    })
}

async fn check_server<C: GenericClient>(
    client: &C,
    migrations: &[Migration],
    applied: &[usize],
    pending: &[usize],
    opts: &Options,
) -> Result<(), Error> {
    let server: i32 = client
        .query_one(
            "SELECT current_setting('server_version_num')::int / 10000",
            &[],
        )
        .await
        .map_err(db("reading the server version"))?
        .get(0);
    let server = server as u32;
    if server != opts.postgres {
        return Err(Error::Refused(format!(
            "the server runs PostgreSQL {server}, the project targets {} (`[postgres] version`)",
            opts.postgres
        )));
    }
    for &p in pending {
        if let Some(h) = &migrations[p].header
            && h.postgres != server
        {
            return Err(Error::Refused(format!(
                "migration {} was generated for PostgreSQL {}, the server runs {server}: \
                 its analysis was made for {}",
                migrations[p].name, h.postgres, h.postgres
            )));
        }
    }

    // The extension versions the pending migrations reach can be installed.
    let mut wanted: Vec<(&str, &str, &str)> = Vec::new();
    for &p in pending {
        if let Some(h) = &migrations[p].header {
            for (name, version) in &h.extensions {
                if !wanted.iter().any(|(n, v, _)| n == name && v == version) {
                    wanted.push((name, version, &migrations[p].name));
                }
            }
        }
    }
    for (name, version, migration) in wanted {
        let available = client
            .query_opt(
                "SELECT 1 FROM pg_available_extension_versions WHERE name = $1 AND version = $2",
                &[&name, &version],
            )
            .await
            .map_err(db("reading the available extensions"))?
            .is_some();
        if !available {
            return Err(Error::Refused(format!(
                "migration {migration} needs extension {name} {version}, not available on the \
                 server (`pg_available_extension_versions`)"
            )));
        }
    }

    // The extensions are as the last applied migration left them: the
    // pending ones were verified from that state.
    if let Some(h) = applied.last().and_then(|&a| migrations[a].header.as_ref()) {
        let last = &migrations[*applied.last().expect("non-empty")].name;
        for (name, version) in &h.extensions {
            let installed: Option<String> = client
                .query_opt(
                    "SELECT extversion FROM pg_extension WHERE extname = $1",
                    &[name],
                )
                .await
                .map_err(db("reading the installed extensions"))?
                .map(|r| r.get(0));
            match installed {
                Some(v) if &v == version => {}
                Some(v) => {
                    return Err(Error::Refused(format!(
                        "extension {name} is at version {v}, the last applied migration ({last}) \
                         left it at {version}: it was changed outside the migrations"
                    )));
                }
                None => {
                    return Err(Error::Refused(format!(
                        "extension {name} is not installed, the last applied migration ({last}) \
                         left it at {version}: it was dropped outside the migrations"
                    )));
                }
            }
        }
    }
    Ok(())
}

/// What [`apply`] reports while it runs.
#[derive(Debug)]
pub enum Event<'a> {
    /// Another run holds the lock: waiting for it.
    Waiting,
    Applied {
        name: &'a str,
        duration: Duration,
    },
}

/// Applies the pending migrations, in order: the names applied.
pub async fn apply(
    client: &mut Client,
    migrations: &[Migration],
    opts: &Options,
    mut progress: impl FnMut(Event<'_>),
) -> Result<Vec<String>, Error> {
    let table = quoted_table(&opts.table)?;
    let locked: bool = client
        .query_one("SELECT pg_try_advisory_lock($1)", &[&LOCK_KEY])
        .await
        .map_err(db("taking the migration lock"))?
        .get(0);
    if !locked {
        progress(Event::Waiting);
        client
            .execute("SELECT pg_advisory_lock($1)", &[&LOCK_KEY])
            .await
            .map_err(db("waiting for the migration lock"))?;
    }
    let result = apply_locked(client, migrations, opts, &table, &mut progress).await;
    let unlocked = client
        .execute("SELECT pg_advisory_unlock($1)", &[&LOCK_KEY])
        .await
        .map_err(db("releasing the migration lock"));
    let applied = result?;
    unlocked?;
    Ok(applied)
}

async fn apply_locked(
    client: &mut Client,
    migrations: &[Migration],
    opts: &Options,
    table: &str,
    progress: &mut impl FnMut(Event<'_>),
) -> Result<Vec<String>, Error> {
    // Checked under the lock: another run may have applied some meanwhile.
    let plan = plan(&*client, migrations, opts).await?;
    if plan.pending.is_empty() {
        return Ok(Vec::new());
    }
    client
        .batch_execute(&format!(
            "CREATE TABLE IF NOT EXISTS {table} (
                name text PRIMARY KEY,
                checksum bytea NOT NULL,
                header text,
                started_at timestamptz NOT NULL,
                finished_at timestamptz,
                applied_by text NOT NULL DEFAULT current_user,
                pgzest text NOT NULL
            )"
        ))
        .await
        .map_err(db(format!("creating the registry {table}")))?;

    let mut done = Vec::new();
    for &p in &plan.pending {
        let m = &migrations[p];
        let start = Instant::now();
        if m.separate.is_empty() {
            run_in_transaction(client, m, opts, table).await?;
        } else {
            run_separately(client, m, opts, table).await?;
        }
        progress(Event::Applied {
            name: &m.name,
            duration: start.elapsed(),
        });
        done.push(m.name.clone());
    }
    Ok(done)
}

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Line and column (1-based) of a 1-based character position.
fn line_column(text: &str, position: usize) -> (usize, usize) {
    let mut line = 1;
    let mut column = 1;
    for c in text.chars().take(position.saturating_sub(1)) {
        if c == '\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    (line, column)
}

/// A failure of `text`, which starts at `base_line` of the migration; `alone`:
/// it is one statement, so its line locates the error even without a
/// position.
fn failed(
    m: &Migration,
    text: &str,
    base_line: usize,
    alone: bool,
) -> impl FnOnce(tokio_postgres::Error) -> Error {
    let name = m.name.clone();
    let text = text.to_string();
    move |source| {
        let at = source.as_db_error().and_then(|d| match d.position() {
            Some(tokio_postgres::error::ErrorPosition::Original(p)) => {
                Some(line_column(&text, *p as usize))
            }
            _ => None,
        });
        let line = match at {
            Some((line, _)) => Some(base_line + line - 1),
            None => alone.then_some(base_line),
        };
        Error::Migration {
            name,
            line,
            column: at.map(|(_, c)| c),
            transactional: !alone,
            source,
        }
    }
}

async fn run_in_transaction(
    client: &mut Client,
    m: &Migration,
    opts: &Options,
    table: &str,
) -> Result<(), Error> {
    let tx = client
        .transaction()
        .await
        .map_err(db(format!("starting migration {}", m.name)))?;
    if let Some(t) = &opts.lock_timeout {
        tx.execute("SELECT set_config('lock_timeout', $1, true)", &[t])
            .await
            .map_err(db(format!("setting lock_timeout = {t}")))?;
    }
    tx.batch_execute(&m.sql)
        .await
        .map_err(failed(m, &m.sql, 1, false))?;
    tx.execute(
        &format!(
            "INSERT INTO {table} (name, checksum, header, started_at, finished_at, pgzest) \
             VALUES ($1, $2, $3, now(), clock_timestamp(), $4)"
        ),
        &[
            &m.name,
            &m.checksum.as_slice(),
            &m.header.as_ref().map(|h| h.line.as_str()),
            &VERSION,
        ],
    )
    .await
    .map_err(db(format!("recording migration {}", m.name)))?;
    tx.commit().await.map_err(failed(m, &m.sql, 1, false))
}

async fn run_separately(
    client: &mut Client,
    m: &Migration,
    opts: &Options,
    table: &str,
) -> Result<(), Error> {
    client
        .execute(
            &format!(
                "INSERT INTO {table} (name, checksum, header, started_at, pgzest) \
                 VALUES ($1, $2, $3, clock_timestamp(), $4)"
            ),
            &[
                &m.name,
                &m.checksum.as_slice(),
                &m.header.as_ref().map(|h| h.line.as_str()),
                &VERSION,
            ],
        )
        .await
        .map_err(db(format!("recording the start of migration {}", m.name)))?;
    if let Some(t) = &opts.lock_timeout {
        client
            .execute("SELECT set_config('lock_timeout', $1, false)", &[t])
            .await
            .map_err(db(format!("setting lock_timeout = {t}")))?;
    }
    let mut result = Ok(());
    for statement in &m.separate {
        // The statement's first line in the file, for the error position.
        let base_line = m
            .sql
            .find(statement.as_str())
            .map_or(1, |at| m.sql[..at].matches('\n').count() + 1);
        if let Err(e) = client.batch_execute(statement).await {
            result = Err(failed(m, statement, base_line, true)(e));
            break;
        }
    }
    if opts.lock_timeout.is_some() {
        client
            .batch_execute("RESET lock_timeout")
            .await
            .map_err(db("resetting lock_timeout"))?;
    }
    result?;
    client
        .execute(
            &format!("UPDATE {table} SET finished_at = clock_timestamp() WHERE name = $1"),
            &[&m.name],
        )
        .await
        .map_err(db(format!("recording the end of migration {}", m.name)))?;
    Ok(())
}

/// How a migration left started ends, as checked by hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    /// It took full effect: recorded as finished.
    Applied,
    /// It left nothing (or what it left was removed): its row is deleted,
    /// and the next run applies it again.
    RolledBack,
    /// An applied migration was edited and the new content reviewed: its
    /// checksum is replaced.
    Checksum,
}

/// Records the outcome of `name`, decided by whoever checked the database.
pub async fn resolve(
    client: &Client,
    migrations: &[Migration],
    opts: &Options,
    name: &str,
    resolution: Resolution,
) -> Result<(), Error> {
    let table = quoted_table(&opts.table)?;
    let m = migrations
        .iter()
        .find(|m| m.name == name)
        .ok_or_else(|| Error::Refused(format!("no migration {name} in the repository")))?;
    let recorded = registry(client, &table)
        .await?
        .unwrap_or_default()
        .into_iter()
        .find(|r| r.name == name)
        .ok_or_else(|| Error::Refused(format!("migration {name} is not in the registry")))?;
    let checksum = m.checksum.as_slice();
    let (sql, params): (String, Vec<&(dyn tokio_postgres::types::ToSql + Sync)>) = match resolution
    {
        Resolution::Applied | Resolution::RolledBack if recorded.finished => {
            return Err(Error::Refused(format!(
                "migration {name} finished: there is nothing to resolve"
            )));
        }
        Resolution::Applied => (
            format!("UPDATE {table} SET finished_at = clock_timestamp() WHERE name = $1"),
            vec![&m.name],
        ),
        Resolution::RolledBack => (
            format!("DELETE FROM {table} WHERE name = $1"),
            vec![&m.name],
        ),
        Resolution::Checksum if !recorded.finished => {
            return Err(Error::Refused(format!(
                "migration {name} never finished: resolve it with `--applied` or \
                     `--rolled-back` first"
            )));
        }
        Resolution::Checksum if recorded.checksum == m.checksum => {
            return Err(Error::Refused(format!(
                "migration {name} is unchanged: its checksum is already recorded"
            )));
        }
        Resolution::Checksum => (
            format!("UPDATE {table} SET checksum = $2 WHERE name = $1"),
            vec![&m.name, &checksum],
        ),
    };
    client
        .execute(&sql, &params)
        .await
        .map_err(db(format!("resolving migration {name}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_names_are_quoted() {
        assert_eq!(
            quoted_table("_pgzest_migrations").unwrap(),
            "\"_pgzest_migrations\""
        );
        assert_eq!(quoted_table("app.m").unwrap(), "\"app\".\"m\"");
        assert_eq!(quoted_table("a\"b").unwrap(), "\"a\"\"b\"");
        for bad in ["", "a.", "a.b.c"] {
            assert!(quoted_table(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn positions_become_lines_and_columns() {
        let text = "-- header\nCREATE TABLE t (\n  x nope\n);";
        let at = text.find("nope").unwrap() + 1;
        assert_eq!(line_column(text, at), (3, 5));
    }
}
