//! `pgzest migrate status | apply | resolve`: the project's migrations
//! applied to a database (`docs/decisions/0002-migrate-apply.md`).
//!
//! What needs no database is decided here, before connecting: the
//! migrations replay without error, and each one is read for what
//! `pgzest_runtime::migrate` must know — checksum, header, statements that
//! cannot run in a transaction, and what forbids applying it. The registry,
//! the server checks and the runs are `pgzest_runtime::migrate`'s.

use std::path::{Path, PathBuf};

use pgzest_ast::DdlStatement;
use pgzest_ast::stmt::Statement;
use pgzest_ast::utility::{MaintenanceCommand, TransactionCommand, UtilityKind};
use pgzest_runtime::migrate::{self, Event, Header, Migration, Options, Resolution};
use pgzest_schema::{FileRole, MigrationHeader, ProjectConfig, SchemaSource, SqlChunk};
use sha2::{Digest, Sha256};
use tokio_postgres::{Client, NoTls};

pub enum Action {
    Status,
    Apply {
        dry_run: bool,
    },
    Resolve {
        name: String,
        resolution: Resolution,
    },
}

pub fn run(url: &str, action: Action) -> i32 {
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
    match runtime.block_on(run_async(url, action)) {
        Ok(code) => code,
        Err((code, e)) => {
            eprintln!("error: {e}");
            code
        }
    }
}

async fn run_async(url: &str, action: Action) -> Result<i32, (i32, String)> {
    let (root, config) = crate::discover_project()
        .map_err(|e| (2, e))?
        .ok_or_else(|| (2, format!("no {} found", ProjectConfig::FILENAME)))?;
    let project = (root.clone(), config.clone());
    let (macros, macro_errors) = crate::load_macros(Some(&project));
    if !macro_errors.is_empty() {
        return Err((2, macro_errors.join("\n")));
    }
    let packs = crate::load_packs(Some(&project)).map_err(|e| (2, e))?;
    let chunks: Vec<SqlChunk> = config
        .to_source(&root)
        .chunks()
        .into_iter()
        .filter(|c| c.role != FileRole::Query)
        .collect();

    let files = migration_files(&chunks);
    if files.is_empty() {
        return Err((
            2,
            format!(
                "no migration in {}",
                root.join(&config.migrations.directory).display()
            ),
        ));
    }
    if let Some(f) = files.iter().find(|f| {
        f.file_name()
            .is_some_and(|n| n == "migration.sql" || n == "up.sql")
    }) {
        return Err((
            2,
            format!(
                "{}: Prisma and Diesel layouts keep their own registry \
                 (`_prisma_migrations`, `__diesel_schema_migrations`), which `pgzest migrate \
                 apply` does not write yet; apply them with their tool",
                f.display()
            ),
        ));
    }

    // The migrations replay without error: a migration PostgreSQL would
    // reject, or that pgzest cannot read, never reaches a database.
    let replayed = crate::migrate::build(&chunks, &macros, &packs, config.schema.declarative);
    let errors: Vec<String> = crate::migrate::blocking(&replayed)
        .into_iter()
        .filter(|e| {
            files
                .iter()
                .any(|f| e.starts_with(&f.display().to_string()))
        })
        .collect();
    if !errors.is_empty() {
        return Err((
            1,
            format!(
                "the migrations do not replay, nothing is applied:\n{}",
                errors.join("\n")
            ),
        ));
    }

    let mut migrations = Vec::new();
    for path in &files {
        migrations.push(read(path, &macros).map_err(|e| (2, e))?);
    }
    let opts = Options {
        table: config
            .migrations
            .table
            .clone()
            .unwrap_or_else(|| migrate::DEFAULT_TABLE.into()),
        lock_timeout: Some(
            config
                .migrations
                .lock_timeout
                .clone()
                .unwrap_or_else(|| migrate::DEFAULT_LOCK_TIMEOUT.into()),
        ),
        postgres: config.postgres.version,
    };

    let mut client = connect(url).await.map_err(|e| (2, e))?;
    let refused = |e: migrate::Error| match e {
        migrate::Error::Refused(_) => (1, e.to_string()),
        migrate::Error::Migration { .. } => (1, e.to_string()),
        migrate::Error::Db { .. } => (2, e.to_string()),
    };
    match action {
        Action::Status => {
            let plan = migrate::plan(&client, &migrations, &opts)
                .await
                .map_err(refused)?;
            println!(
                "{} applied, {} pending (registry {}{})",
                plan.applied.len(),
                plan.pending.len(),
                opts.table,
                if plan.new_registry {
                    ", not created yet"
                } else {
                    ""
                }
            );
            for &p in &plan.pending {
                println!("  pending  {}", migrations[p].name);
            }
            Ok(if plan.pending.is_empty() { 0 } else { 1 })
        }
        Action::Apply { dry_run: true } => {
            let plan = migrate::plan(&client, &migrations, &opts)
                .await
                .map_err(refused)?;
            if plan.pending.is_empty() {
                println!("up to date: {} migration(s) applied", plan.applied.len());
            }
            for &p in &plan.pending {
                let m = &migrations[p];
                let how = if m.separate.is_empty() {
                    "in a transaction"
                } else {
                    "outside a transaction, statement by statement"
                };
                println!("would apply  {} ({how})", m.name);
            }
            Ok(0)
        }
        Action::Apply { dry_run: false } => {
            let applied = migrate::apply(&mut client, &migrations, &opts, |event| match event {
                Event::Waiting => {
                    eprintln!("note: another run holds the migration lock, waiting for it")
                }
                Event::Applied { name, duration } => {
                    println!("applied  {name} ({} ms)", duration.as_millis())
                }
            })
            .await
            .map_err(refused)?;
            if applied.is_empty() {
                println!("up to date: nothing to apply");
            } else {
                println!("{} migration(s) applied", applied.len());
            }
            Ok(0)
        }
        Action::Resolve { name, resolution } => {
            migrate::resolve(&client, &migrations, &opts, &name, resolution)
                .await
                .map_err(refused)?;
            let what = match resolution {
                Resolution::Applied => "recorded as applied",
                Resolution::RolledBack => "removed from the registry, applied again next time",
                Resolution::Checksum => "recorded with its new checksum",
            };
            println!("{name}: {what}");
            Ok(0)
        }
    }
}

/// Migration files, in replay order (each once: a file may hold several
/// chunks).
fn migration_files(chunks: &[SqlChunk]) -> Vec<PathBuf> {
    let mut found: Vec<(u64, &Path)> = chunks
        .iter()
        .filter_map(|c| match c.role {
            FileRole::Migration { sequence } => Some((sequence, c.origin.path.as_path())),
            _ => None,
        })
        .collect();
    found.sort_by_key(|(s, _)| *s);
    let mut out: Vec<PathBuf> = Vec::new();
    for (_, p) in found {
        if !out.iter().any(|o| o == p) {
            out.push(p.to_path_buf());
        }
    }
    out
}

/// A migration file, as `pgzest_runtime::migrate` needs it.
fn read(path: &Path, macros: &pgzest_expand::MacroSet) -> Result<Migration, String> {
    let sql = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let name = path
        .file_stem()
        .map_or_else(String::new, |s| s.to_string_lossy().into_owned());
    let mut blockers = Vec::new();
    let header = match MigrationHeader::find(&sql) {
        None => None,
        Some(Ok(h)) => Some(Header {
            line: sql[h.span.clone()].trim().to_string(),
            postgres: h.postgres,
            extensions: h
                .extensions
                .into_iter()
                .map(|e| (e.name, e.version))
                .collect(),
        }),
        Some(Err(e)) => {
            blockers.push(format!("malformed header: {}", e.message));
            None
        }
    };
    blockers.extend(left_to_write(&sql));
    let separate = if pgzest_expand::expand_source(&sql, macros).is_passthrough() {
        separate_statements(&sql, &mut blockers)
    } else {
        blockers.push(
            "it uses pgzest macros, and a migration is applied as written: write the \
             expansion (`pgzest build`) in it"
                .into(),
        );
        Vec::new()
    };
    Ok(Migration {
        name,
        checksum: Sha256::digest(sql.as_bytes()).into(),
        sql,
        header,
        separate,
        blockers,
    })
}

/// The `TODO` lines `pgzest migrate create` writes for a change it cannot
/// generate: the migration is not finished while they remain.
fn left_to_write(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_block = false;
    for (i, line) in sql.lines().enumerate() {
        let line = line.trim();
        if line.starts_with("-- TO WRITE:") {
            in_block = true;
        } else if in_block && let Some(todo) = line.strip_prefix("-- TODO ") {
            out.push(format!(
                "line {}: change left to write ({todo}); write it, then delete the `TODO` line",
                i + 1
            ));
        } else if !line.starts_with("--") {
            in_block = false;
        }
    }
    out
}

/// What a statement requires of the transaction it runs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Needs {
    /// Runs in the migration's transaction.
    Transaction,
    /// Cannot run in a transaction block.
    NoTransaction,
    /// Ends or opens a transaction: it would commit or roll back the
    /// migration's own, registry row included.
    TransactionControl,
}

fn needs(stmt: &Statement) -> Needs {
    match stmt {
        Statement::Ddl(DdlStatement::CreateIndex(ci)) if ci.concurrently => Needs::NoTransaction,
        Statement::Utility(u) => match &u.kind {
            UtilityKind::Maintenance {
                command: MaintenanceCommand::Vacuum,
                ..
            } => Needs::NoTransaction,
            UtilityKind::Transaction(
                TransactionCommand::Begin
                | TransactionCommand::Commit
                | TransactionCommand::Rollback,
            ) => Needs::TransactionControl,
            _ => Needs::Transaction,
        },
        _ => Needs::Transaction,
    }
}

/// The statements of `sql` to run outside a transaction, when it is made
/// only of them; empty when it runs in one. Mixed forms are blockers.
fn separate_statements(sql: &str, blockers: &mut Vec<String>) -> Vec<String> {
    let (split, _) = pgzest_parser::split_statements(sql);
    let line = |at: usize| sql[..at].matches('\n').count() + 1;
    let mut outside = Vec::new();
    let mut inside = Vec::new();
    for s in &split {
        let text = &sql[s.range.clone()];
        // A statement the parser skips stops the replay before this point.
        let Ok(parsed) = pgzest_parser::parse_at_with_outcome(text, s.range.start) else {
            continue;
        };
        for stmt in &parsed.statements {
            match needs(stmt) {
                Needs::Transaction => inside.push(line(s.range.start)),
                Needs::NoTransaction => outside.push((line(s.range.start), text.to_string())),
                Needs::TransactionControl => blockers.push(format!(
                    "line {}: `{}` — each migration runs in its own transaction, recorded \
                     with it; remove it",
                    line(s.range.start),
                    s.keywords.join(" ")
                )),
            }
        }
    }
    if outside.is_empty() {
        return Vec::new();
    }
    if !inside.is_empty() {
        blockers.push(format!(
            "statement(s) that cannot run in a transaction (line(s) {}) mixed with others \
             (line(s) {}): move them to a migration of their own",
            outside
                .iter()
                .map(|(l, _)| l.to_string())
                .collect::<Vec<_>>()
                .join(", "),
            inside
                .iter()
                .map(usize::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ));
        return Vec::new();
    }
    outside.into_iter().map(|(_, text)| text).collect()
}

async fn connect(url: &str) -> Result<Client, String> {
    let config: tokio_postgres::Config = url.parse().map_err(|e| format!("database URL: {e}"))?;
    let (client, connection) = config.connect(NoTls).await.map_err(|e| e.to_string())?;
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("connection: {e}");
        }
    });
    Ok(client)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statements_outside_a_transaction() {
        let mut b = Vec::new();
        let s = separate_statements(
            "-- pgzest: postgres=18\nCREATE INDEX CONCURRENTLY i ON t (a);\nVACUUM t;\n",
            &mut b,
        );
        assert!(b.is_empty(), "{b:?}");
        assert_eq!(s, vec!["CREATE INDEX CONCURRENTLY i ON t (a)", "VACUUM t"]);

        let mut b = Vec::new();
        let s = separate_statements(
            "CREATE TABLE t (a INT);\nCREATE INDEX i ON t (a);\n",
            &mut b,
        );
        assert!(b.is_empty() && s.is_empty(), "{b:?} {s:?}");
    }

    #[test]
    fn mixed_and_transaction_control_are_blockers() {
        let mut b = Vec::new();
        let s = separate_statements(
            "CREATE TABLE t (a INT);\nCREATE INDEX CONCURRENTLY i ON t (a);\n",
            &mut b,
        );
        assert!(s.is_empty());
        assert!(
            b[0].contains("line(s) 2) mixed with others (line(s) 1)"),
            "{b:?}"
        );

        let mut b = Vec::new();
        separate_statements("BEGIN;\nCREATE TABLE t (a INT);\nCOMMIT;\n", &mut b);
        assert_eq!(b.len(), 2, "{b:?}");
        assert!(b[0].starts_with("line 1: `BEGIN`"), "{b:?}");
        assert!(b[1].starts_with("line 3: `COMMIT`"), "{b:?}");
    }

    #[test]
    fn todo_lines_left_by_migrate_create() {
        let sql = "-- note\n\n-- TO WRITE: changes pgzest does not generate.\n\
                   -- TODO type public.k: value removed\n\nALTER TABLE t ADD COLUMN n int;\n\
                   -- TODO unrelated comment\n";
        let found = left_to_write(sql);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].starts_with("line 4: change left to write (type public.k"));
    }
}
