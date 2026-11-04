//! `pgzest db check`: a database compared with what its migrations
//! produce (PLAN.md P9), to find what was changed in it by hand — a routine
//! hotfixed in production, an index added, a trigger dropped.
//!
//! The migrations the database received are replayed in an empty database
//! (on the same server by default), and both are described by PostgreSQL
//! itself (`pg_get_functiondef`, `pg_get_triggerdef`, `format_type`…, see
//! [`pgzest_oracle::schema::introspect`]): no pgzest model stands between
//! them, a difference is one PostgreSQL sees.
//!
//! Which migrations the database received is read from the table of the
//! tool that applied them (`_prisma_migrations`, `__diesel_schema_migrations`);
//! without one, all of them, or up to `--until`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use pgzest_schema::source::{MigrationDirSource, SchemaSource};
use pgzest_schema::{FileRole, ProjectConfig};
use tokio_postgres::{Client, NoTls};

pub struct Options {
    pub url: String,
    /// Server the migrations are replayed on (default: `url`'s).
    pub scratch_url: Option<String>,
    /// Last migration the database received, without a migrations table.
    pub until: Option<String>,
}

pub fn check(opts: Options) -> i32 {
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
    match runtime.block_on(run(opts)) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e}");
            2
        }
    }
}

/// A migration of the repository.
struct MigrationFile {
    /// Name the migration tools record: the directory of a `migration.sql`
    /// (Prisma) or `up.sql` (Diesel), the file stem otherwise.
    name: String,
    path: PathBuf,
}

impl MigrationFile {
    /// The version Diesel records: the name up to its first `_`, without
    /// `-` (`2024-01-02-030405_init` → `20240102030405`).
    fn diesel_version(&self) -> String {
        let head = self.name.split('_').next().unwrap_or_default();
        head.replace('-', "")
    }
}

fn migrations(root: &std::path::Path, config: &ProjectConfig) -> Vec<MigrationFile> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for chunk in MigrationDirSource::new(root.join(&config.migrations.directory)).chunks() {
        let FileRole::Migration { .. } = chunk.role else {
            continue;
        };
        let path = chunk.origin.path;
        if !seen.insert(path.clone()) {
            continue;
        }
        let file = path
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or_default();
        let name = if file == "migration.sql" || file == "up.sql" {
            path.parent().and_then(|d| d.file_name())
        } else {
            path.file_stem()
        };
        out.push(MigrationFile {
            name: name.map_or_else(String::new, |n| n.to_string_lossy().into_owned()),
            path,
        });
    }
    out
}

/// The table a migration tool records its runs in.
struct History {
    table: &'static str,
    /// Names (Prisma) or versions (Diesel) of the applied migrations.
    applied: BTreeSet<String>,
    /// Started and neither finished nor rolled back (Prisma).
    failed: Vec<String>,
}

async fn history(client: &Client) -> Result<Option<History>, String> {
    let exists = |table: &str| {
        let sql = format!("SELECT to_regclass('{table}') IS NOT NULL");
        async move {
            client
                .query_one(&sql, &[])
                .await
                .map(|r| r.get::<_, bool>(0))
                .map_err(|e| db_error(&e))
        }
    };
    if exists("_prisma_migrations").await? {
        let rows = client
            .query(
                "SELECT migration_name, finished_at IS NOT NULL, rolled_back_at IS NOT NULL \
                 FROM _prisma_migrations",
                &[],
            )
            .await
            .map_err(|e| db_error(&e))?;
        let mut h = History {
            table: "_prisma_migrations",
            applied: BTreeSet::new(),
            failed: Vec::new(),
        };
        for r in rows {
            let (name, finished, rolled_back): (String, bool, bool) =
                (r.get(0), r.get(1), r.get(2));
            match (finished, rolled_back) {
                (true, false) => {
                    h.applied.insert(name);
                }
                (false, false) => h.failed.push(name),
                _ => {}
            }
        }
        return Ok(Some(h));
    }
    if exists("__diesel_schema_migrations").await? {
        let rows = client
            .query("SELECT version FROM __diesel_schema_migrations", &[])
            .await
            .map_err(|e| db_error(&e))?;
        return Ok(Some(History {
            table: "__diesel_schema_migrations",
            applied: rows.iter().map(|r| r.get(0)).collect(),
            failed: Vec::new(),
        }));
    }
    Ok(None)
}

async fn run(opts: Options) -> Result<i32, String> {
    let (root, config) = crate::discover_project()?.ok_or_else(|| {
        format!(
            "no {} found: `pgzest db check` compares a database with the project's migrations",
            ProjectConfig::FILENAME
        )
    })?;
    let files = migrations(&root, &config);
    if files.is_empty() {
        return Err(format!(
            "no migration in {}",
            root.join(&config.migrations.directory).display()
        ));
    }
    let live_config: tokio_postgres::Config =
        opts.url.parse().map_err(|e| format!("database URL: {e}"))?;
    let live = connect(&live_config).await?;

    // The migrations the database received, in the repository's order.
    let history = history(&live).await?;
    let mut notes = Vec::new();
    let received: Vec<&MigrationFile> = match (&history, &opts.until) {
        (Some(h), Some(_)) => {
            return Err(format!(
                "`--until` is for a database without migrations table: this one has `{}`",
                h.table
            ));
        }
        (Some(h), None) => {
            if !h.failed.is_empty() {
                return Err(format!(
                    "migration(s) started and not finished in `{}`: {} — the database is in \
                     between, resolve them first",
                    h.table,
                    h.failed.join(", ")
                ));
            }
            let recorded = |f: &MigrationFile| {
                if h.table == "__diesel_schema_migrations" {
                    f.diesel_version()
                } else {
                    f.name.clone()
                }
            };
            let known: BTreeSet<String> = files.iter().map(recorded).collect();
            let unknown: Vec<&String> = h.applied.difference(&known).collect();
            if !unknown.is_empty() {
                return Err(format!(
                    "applied to the database, missing from {}: {} — the repository is behind \
                     the database",
                    config.migrations.directory,
                    unknown
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            let (applied, pending): (Vec<_>, Vec<_>) =
                files.iter().partition(|f| h.applied.contains(&recorded(f)));
            if !pending.is_empty() {
                notes.push(format!(
                    "{} migration(s) not applied yet, left out: {}",
                    pending.len(),
                    pending
                        .iter()
                        .map(|f| f.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            applied
        }
        (None, Some(until)) => {
            let end = files
                .iter()
                .position(|f| &f.name == until)
                .ok_or_else(|| format!("`--until {until}`: no such migration"))?;
            files[..=end].iter().collect()
        }
        (None, None) => {
            notes.push(format!(
                "no migrations table (`_prisma_migrations`, `__diesel_schema_migrations`): \
                 compared with all {} migrations (`--until <name>` for an earlier one)",
                files.len()
            ));
            files.iter().collect()
        }
    };

    let scratch_url = opts.scratch_url.as_deref().unwrap_or(&opts.url);
    let (live_version, scratch_version) = (
        major_version(&live).await?,
        major_version(
            &connect(
                &scratch_url
                    .parse()
                    .map_err(|e| format!("scratch URL: {e}"))?,
            )
            .await?,
        )
        .await?,
    );
    if live_version != scratch_version {
        return Err(format!(
            "the database runs PostgreSQL {live_version}, the scratch server {scratch_version}: \
             their descriptions would differ — replay on a server of the same version \
             (`--scratch-url`)"
        ));
    }
    let mut scripts = Vec::new();
    for f in &received {
        let sql =
            std::fs::read_to_string(&f.path).map_err(|e| format!("{}: {e}", f.path.display()))?;
        scripts.push((f.name.clone(), sql));
    }
    let expected = pgzest_oracle::schema::applied(scratch_url, "dbcheck", &scripts)
        .await
        .map_err(|e| {
            format!(
                "replaying the migrations: {e}\nhint: the migrations are replayed in an empty \
                 database created on the server (CREATEDB privilege); `--scratch-url` replays \
                 them on another one"
            )
        })?;
    let mut actual = pgzest_oracle::schema::introspect(&live).await?;
    if let Some(h) = &history {
        actual.retain(|line| !concerns(line, h.table));
    }

    for n in &notes {
        eprintln!("note: {n}");
    }
    let report = Report::new(&expected, &actual);
    if report.is_empty() {
        println!(
            "unchanged: the database matches its {} migration(s) ({} object properties compared)",
            received.len(),
            actual.len()
        );
        return Ok(0);
    }
    eprintln!(
        "error: the database differs from its {} migration(s), {} object(s):",
        received.len(),
        report.entries.len()
    );
    report.print();
    eprintln!(
        "hint: a change made by hand belongs in a migration (`pgzest migrate create`), or is \
         reverted"
    );
    Ok(1)
}

/// A line of the description about `table` (a migrations table).
fn concerns(line: &str, table: &str) -> bool {
    let mut words = line.split_whitespace();
    let kind = words.next().unwrap_or_default();
    let object = words.next().unwrap_or_default();
    let object = object.strip_prefix("public.").unwrap_or(object);
    let on_table = |w: &str| w == table || w.starts_with(&format!("{table}."));
    match kind {
        "relation" | "column" | "constraint" | "sequence" => {
            on_table(object) || object.starts_with(&format!("{table}_"))
        }
        "index" => {
            line.contains(&format!(" ON {table} "))
                || line.contains(&format!(" ON public.{table} "))
        }
        _ => false,
    }
}

/// Differences, by object.
struct Report {
    entries: Vec<(String, Vec<String>, Vec<String>)>,
}

impl Report {
    fn new(expected: &BTreeSet<String>, actual: &BTreeSet<String>) -> Self {
        let mut by_object: BTreeMap<String, (Vec<String>, Vec<String>)> = BTreeMap::new();
        for line in expected.difference(actual) {
            by_object
                .entry(object(line))
                .or_default()
                .0
                .push(line.clone());
        }
        for line in actual.difference(expected) {
            by_object
                .entry(object(line))
                .or_default()
                .1
                .push(line.clone());
        }
        Report {
            entries: by_object.into_iter().map(|(k, (e, a))| (k, e, a)).collect(),
        }
    }

    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn print(&self) {
        for (object, expected, actual) in &self.entries {
            match (expected.as_slice(), actual.as_slice()) {
                ([], _) => eprintln!("  extra    {object}: only in the database"),
                (_, []) => eprintln!("  missing  {object}: in the migrations, not in the database"),
                ([e], [a]) => {
                    eprintln!("  changed  {object}:");
                    let prefix = format!("{object} ");
                    let shown =
                        |l: &'_ str| l.strip_prefix(prefix.as_str()).unwrap_or(l).to_string();
                    let (e, a) = (shown(e), shown(a));
                    let diff = similar::TextDiff::from_lines(e.as_str(), a.as_str());
                    for change in diff.iter_all_changes() {
                        let sign = match change.tag() {
                            similar::ChangeTag::Delete => "-",
                            similar::ChangeTag::Insert => "+",
                            similar::ChangeTag::Equal => " ",
                        };
                        eprint!("      {sign} {}", change.value());
                        if change.missing_newline() {
                            eprintln!();
                        }
                    }
                }
                _ => {
                    eprintln!("  changed  {object}:");
                    for e in expected {
                        eprintln!("      - {e}");
                    }
                    for a in actual {
                        eprintln!("      + {a}");
                    }
                }
            }
        }
        eprintln!("  (- from the migrations, + in the database)");
    }
}

/// The object a description line is about: its kind and identity, for
/// grouping only — the comparison is on whole lines.
fn object(line: &str) -> String {
    let (kind, rest) = line.split_once(' ').unwrap_or((line, ""));
    let words: Vec<&str> = rest.split_whitespace().collect();
    let word = |i: usize| words.get(i).copied().unwrap_or_default();
    let identity = match kind {
        // `name(argument types)`, types possibly with spaces.
        "routine" => match rest.find(')') {
            Some(end) => rest[..=end].to_string(),
            None => word(0).to_string(),
        },
        // `CREATE [CONSTRAINT] TRIGGER name … ON table …`, `CREATE [UNIQUE]
        // INDEX name ON table …`.
        "trigger" | "index" => {
            let keyword = if kind == "trigger" {
                "TRIGGER"
            } else {
                "INDEX"
            };
            let name = words
                .iter()
                .position(|w| *w == keyword)
                .map_or("", |i| word(i + 1));
            let table = words
                .iter()
                .position(|w| *w == "ON")
                .map_or("", |i| word(i + 1));
            format!("{name} on {table}")
        }
        // `table name …`, or `table NOT NULL column` (unnamed).
        "constraint" if word(1) == "NOT" && word(2) == "NULL" => {
            format!("{} NOT NULL {}", word(0), word(3))
        }
        "constraint" | "policy" => format!("{} {}", word(0), word(1)),
        // The description ends the line: the whole of it.
        "comment" => rest.to_string(),
        _ => word(0).to_string(),
    };
    format!("{kind} {identity}")
}

async fn connect(config: &tokio_postgres::Config) -> Result<Client, String> {
    let (client, connection) = config.connect(NoTls).await.map_err(|e| db_error(&e))?;
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("connection: {e}");
        }
    });
    Ok(client)
}

async fn major_version(client: &Client) -> Result<i32, String> {
    client
        .query_one(
            "SELECT current_setting('server_version_num')::int / 10000",
            &[],
        )
        .await
        .map(|r| r.get(0))
        .map_err(|e| db_error(&e))
}

fn db_error(e: &tokio_postgres::Error) -> String {
    match e.as_db_error() {
        Some(db) => format!("{}: {}", db.code().code(), db.message()),
        None => e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diesel_records_the_timestamp_without_dashes() {
        let f = MigrationFile {
            name: "2024-01-02-030405_create_items".into(),
            path: PathBuf::new(),
        };
        assert_eq!(f.diesel_version(), "20240102030405");
    }

    #[test]
    fn lines_are_grouped_by_object() {
        for (line, expected) in [
            (
                "routine f(character varying,integer) CREATE OR REPLACE FUNCTION …",
                "routine f(character varying,integer)",
            ),
            (
                "trigger CREATE TRIGGER t BEFORE UPDATE ON public.items FOR EACH ROW …",
                "trigger t on public.items",
            ),
            (
                "index CREATE UNIQUE INDEX i ON public.items USING btree (n)",
                "index i on public.items",
            ),
            (
                "constraint items NOT NULL n valid=true",
                "constraint items NOT NULL n",
            ),
            (
                "constraint items items_pkey PRIMARY KEY (id) valid=true",
                "constraint items items_pkey",
            ),
            ("column items.n integer notnull=true", "column items.n"),
            (
                "comment table public.items The items.",
                "comment table public.items The items.",
            ),
        ] {
            assert_eq!(object(line), expected, "{line}");
        }
    }

    #[test]
    fn the_migrations_table_is_recognized() {
        let t = "_prisma_migrations";
        for line in [
            "relation _prisma_migrations kind=r persistence=p rls=false force_rls=false",
            "column _prisma_migrations.id character varying(36) notnull=true",
            "constraint _prisma_migrations _prisma_migrations_pkey PRIMARY KEY (id) valid=true",
            "index CREATE INDEX x ON public._prisma_migrations USING btree (migration_name)",
        ] {
            assert!(concerns(line, t), "{line}");
        }
        for line in [
            "relation items kind=r persistence=p rls=false force_rls=false",
            "routine _prisma_migrations_count() CREATE FUNCTION …",
            "index CREATE INDEX x ON public.items USING btree (n)",
        ] {
            assert!(!concerns(line, t), "{line}");
        }
    }
}
