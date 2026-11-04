//! `cargo xtask corpus`: pgweave's analysis rate on an external corpus.
//!
//! The report is meant to be committed (`docs/baselines/`) while the corpus
//! itself is private. Strict rule: **no corpus identifier** in the report.
//! Only counters, diagnostic codes and statement "shapes", where every name
//! is replaced by `…`, are written to it. Error messages (from sqlparser,
//! pgweave or PostgreSQL) quote names: they only go to standard error with
//! `--details`, or are masked.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use pgweave_ast::Statement;
use pgweave_ast::ddl::{DdlStatement, FunctionBody};
use pgweave_diagnostics::Severity;
use pgweave_parser::{SkipReason, SkippedStatement};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::keywords::Keyword;
use sqlparser::tokenizer::{Token, Tokenizer};

/// Layout of a migration directory. Explicit: guessing it would mistake a
/// rollback script for a migration.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum Layout {
    /// Every `.sql`, recursively, sorted by path.
    Flat,
    /// Prisma: `<timestamp>_<name>/migration.sql`, the only applied file
    /// (the others, like a `down.sql`, are ignored).
    Prisma,
    /// Diesel: `<timestamp>_<name>/up.sql`, the only applied file (the
    /// `down.sql` is ignored).
    Diesel,
}

impl Layout {
    fn describe(self) -> &'static str {
        match self {
            Layout::Flat => "every `.sql`, sorted by path",
            Layout::Prisma => "Prisma: `*/migration.sql`, sorted by timestamped directory",
            Layout::Diesel => "Diesel: `*/up.sql`, sorted by timestamped directory",
        }
    }

    fn accepts(self, path: &Path) -> bool {
        match self {
            Layout::Flat => path.extension().is_some_and(|e| e == "sql"),
            Layout::Prisma => path.file_name().is_some_and(|n| n == "migration.sql"),
            Layout::Diesel => path.file_name().is_some_and(|n| n == "up.sql"),
        }
    }
}

pub fn run(
    name: &str,
    path: Option<PathBuf>,
    layout: Layout,
    pg_url: Option<String>,
    out: Option<PathBuf>,
    details: bool,
) -> Result<(), String> {
    let env_var = format!("PGWEAVE_CORPUS_{}", name.to_ascii_uppercase());
    let dir = path
        .or_else(|| std::env::var_os(&env_var).map(PathBuf::from))
        .ok_or_else(|| format!("corpus not found: pass --path or set {env_var}"))?;

    let files = collect_sql(&dir, layout)?;
    if files.is_empty() {
        return Err(format!("no .sql file under {}", dir.display()));
    }
    let sources: Vec<(PathBuf, String)> = files
        .into_iter()
        .map(|p| {
            std::fs::read_to_string(&p)
                .map(|s| (p.clone(), s))
                .map_err(|e| format!("{}: {e}", p.display()))
        })
        .collect::<Result<_, _>>()?;

    let mut stats = Stats::default();
    let parse_start = Instant::now();
    let mut parsed_files = Vec::new();
    for (path, sql) in &sources {
        match pgweave_parser::parse_with_outcome(sql) {
            Ok(outcome) => {
                stats.record_parse(&outcome.statements, &outcome.skipped);
                if details {
                    for sk in &outcome.skipped {
                        eprintln!("{}: {:?}", path.display(), sk.reason);
                    }
                }
                parsed_files.push((path, sql, outcome.statements));
            }
            Err(e) => {
                stats.files_unreadable += 1;
                if details {
                    eprintln!("{}: unreadable file: {e}", path.display());
                }
            }
        }
    }
    stats.parse_time = parse_start.elapsed();

    // Statements skipped at parse time: missing from the replayed catalog.
    stats.index_skipped = stats.statements - stats.analyzed;

    // Replay by pgweave and `check` diagnostics: each migration is checked
    // against the catalog as it is right after it, i.e. the real database
    // state at the time it applies (P2), not the final state.
    let mut replay = pgweave_catalog::Replay::new(
        pgweave_catalog::Catalog::for_postgres(18).map_err(|e| e.to_string())?,
    );
    replay.typer = Some(pgweave_sema::ViewTyper::shared());
    // Each statement is checked when it applies, against the state at that
    // moment; a PL/pgSQL body, run later, is checked against the final state
    // if it is still in force (`pgweave_sema::check`).
    let mut check_diags: Vec<(usize, pgweave_diagnostics::Diagnostic)> = Vec::new();
    let mut deferred = Vec::new();
    let mut routines = Vec::new();
    for (i, (path, sql, stmts)) in parsed_files.iter().enumerate() {
        let mut replay_diags = Vec::new();
        for st in stmts {
            let replay_start = Instant::now();
            replay.apply(st, &mut replay_diags);
            stats.replay_time += replay_start.elapsed();
            // Routine body as written, for the export.
            pgweave_catalog::project::attach_source(&mut replay.catalog, st, sql);
            let check_start = Instant::now();
            let mut d = Vec::new();
            pgweave_sema::check::check_applied(&replay.catalog, &replay.search_path, st, &mut d);
            check_diags.extend(d.into_iter().map(|d| (i, d)));
            if pgweave_sema::check::is_deferred(st) {
                deferred.push((i, replay.search_path.clone(), st));
            }
            if matches!(
                st,
                Statement::Ddl(
                    pgweave_ast::DdlStatement::CreateFunction(_)
                        | pgweave_ast::DdlStatement::CreateProcedure(_)
                )
            ) {
                routines.push((replay.search_path.clone(), st));
            }
            stats.check_time += check_start.elapsed();
        }
        for d in &replay_diags {
            *stats.replay_diagnostics.entry(d.code).or_default() += 1;
            if details {
                eprintln!("{}: replay: [{}] {}", path.display(), d.code, d.message);
            }
        }
    }
    let check_start = Instant::now();
    for (i, path, st) in deferred {
        if pgweave_catalog::project::routine_in_force(&replay.catalog, st) {
            let mut d = Vec::new();
            pgweave_sema::check::check_deferred(&replay.catalog, &path, st, &mut d);
            check_diags.extend(d.into_iter().map(|d| (i, d)));
        }
    }
    stats.check_time += check_start.elapsed();
    let mut files_with_errors = std::collections::BTreeSet::new();
    for (i, d) in &check_diags {
        let sev = match d.severity {
            Severity::Error => {
                files_with_errors.insert(*i);
                "error"
            }
            Severity::Warning => "warning",
            Severity::Info => "info",
        };
        *stats.diagnostics.entry((sev, d.code)).or_default() += 1;
        if details {
            let (file, sql, _) = &parsed_files[*i];
            let line = d
                .span
                .as_ref()
                .map_or(0, |s| sql[..s.start.min(sql.len())].lines().count());
            eprintln!("{}:{line}: {sev}[{}] {}", file.display(), d.code, d.message);
        }
    }
    stats.files_with_errors = files_with_errors.len();

    let pg_url_for_import = pg_url.or_else(|| std::env::var("PGWEAVE_ORACLE_URL").ok());
    let mut probes = crate::probes::generate(&replay.catalog);
    // Queries of the routine bodies in force, analyzed in their scope.
    let mut queries = Vec::new();
    for (path, st) in &routines {
        if pgweave_catalog::project::routine_in_force(&replay.catalog, st) {
            queries.extend(pgweave_sema::check::body_queries(&replay.catalog, path, st));
        }
    }
    let (body_probes, bodies_skipped) = crate::probes::body_probes(&replay.catalog, queries);
    probes.extend(body_probes);
    let pg = match &pg_url_for_import {
        Some(url) => Some(replay_on_postgres(url, &sources, &probes, details)?),
        None => None,
    };
    let probe_stats = pg.as_ref().filter(|p| p.catalog.is_some()).map(|p| {
        let mut stats = crate::probes::compare(
            &probes,
            &p.probes,
            &replay.catalog,
            &replay.search_path,
            details,
        );
        stats.bodies_skipped = bodies_skipped;
        stats
    });

    // Import: the rebuilt catalog, exported as SQL and applied to an empty
    // database, must give back the real catalog.
    let import_comparison = match (
        pg_url_for_import.as_deref(),
        pg.as_ref().and_then(|p| p.catalog.as_ref()),
    ) {
        (Some(url), Some(real)) => {
            let dump = pgweave_catalog::dump::dump(&replay.catalog, 18);
            if let Some(path) = std::env::var_os("PGWEAVE_IMPORT_DUMP") {
                let _ = std::fs::write(path, &dump.baseline);
            }
            Some(match apply_on_fresh_db(url, &dump.baseline) {
                Ok(imported) => Ok(crate::catalog_compare::compare(real, &imported)),
                Err(e) => Err(redact(&e)),
            })
        }
        _ => None,
    };
    // Same check, down to the text (migration oracle, P7): what PostgreSQL
    // says about each object of both databases.
    let fidelity = match (pg_url_for_import.as_deref(), &import_comparison) {
        (Some(url), Some(Ok(_))) => {
            let dump = pgweave_catalog::dump::dump(&replay.catalog, 18);
            let migrations: Vec<(String, String)> = sources
                .iter()
                .map(|(p, sql)| (p.display().to_string(), sql.clone()))
                .collect();
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())?;
            let r = rt.block_on(pgweave_oracle::schema::compare(
                url,
                "corpus",
                &migrations,
                &dump.baseline,
            ));
            if details && let Ok(c) = &r {
                for l in &c.only_migrations {
                    eprintln!("fidelity, migrations: {l}");
                }
                for l in &c.only_schema {
                    eprintln!("fidelity, import: {l}");
                }
            }
            Some(r.map_err(|e| redact(&e)))
        }
        _ => None,
    };
    if details && let Some(Ok(c)) = &import_comparison {
        for d in &c.details {
            eprintln!("import: {d}");
        }
    }
    if let Some(Err(e)) = &import_comparison
        && details
    {
        eprintln!("import rejected by PostgreSQL: {e}");
    }

    let comparison = pg
        .as_ref()
        .and_then(|p| p.catalog.as_ref())
        .map(|real| crate::catalog_compare::compare(real, &replay.catalog));
    if details && let Some(c) = &comparison {
        for d in &c.details {
            eprintln!("catalog: {d}");
        }
    }

    let report = render(
        name,
        layout,
        sources.len(),
        &stats,
        Oracle {
            pg: pg.as_ref(),
            comparison: comparison.as_ref(),
            import: import_comparison.as_ref(),
            fidelity: fidelity.as_ref(),
            probes: probe_stats.as_ref(),
        },
    );
    match out {
        Some(path) => std::fs::write(&path, report).map_err(|e| format!("{}: {e}", path.display())),
        None => {
            print!("{report}");
            Ok(())
        }
    }
}

// ── Collection ────────────────────────────────────────────────────────────────

fn collect_sql(dir: &Path, layout: Layout) -> Result<Vec<PathBuf>, String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).map_err(|e| format!("{}: {e}", d.display()))? {
            let path = entry.map_err(|e| e.to_string())?.path();
            if path.is_dir() {
                stack.push(path);
            } else if layout.accepts(&path) {
                out.push(path);
            }
        }
    }
    // Sort by path: the application order of timestamped migrations.
    out.sort();
    Ok(out)
}

// ── Statistics ────────────────────────────────────────────────────────────────

#[derive(Default)]
struct Stats {
    files_unreadable: usize,
    statements: usize,
    analyzed: usize,
    unparsable: BTreeMap<String, usize>,
    unsupported: BTreeMap<String, usize>,
    functions: usize,
    bodies_parsed: usize,
    bodies_sql: usize,
    bodies_unparsed: usize,
    index_skipped: usize,
    diagnostics: BTreeMap<(&'static str, &'static str), usize>,
    files_with_errors: usize,
    parse_time: Duration,
    check_time: Duration,
    /// pgweave replay diagnostics, by code.
    replay_diagnostics: BTreeMap<&'static str, usize>,
    replay_time: Duration,
}

impl Stats {
    fn record_parse(&mut self, stmts: &[Statement], skipped: &[SkippedStatement]) {
        self.analyzed += stmts.len();
        self.statements += stmts.len();
        for stmt in stmts {
            let body = match stmt {
                Statement::Ddl(DdlStatement::CreateFunction(f)) => Some(&f.body),
                Statement::Ddl(DdlStatement::CreateProcedure(p)) => Some(&p.body),
                _ => None,
            };
            if let Some(body) = body {
                self.functions += 1;
                match body {
                    FunctionBody::Parsed(_) => self.bodies_parsed += 1,
                    FunctionBody::SqlQuery(_)
                    | FunctionBody::Sql(_)
                    | FunctionBody::SqlReturn(_) => self.bodies_sql += 1,
                    FunctionBody::Raw(_) => {}
                }
            }
        }
        for sk in skipped {
            match &sk.reason {
                SkipReason::Unparsable(_) => {
                    self.statements += 1;
                    *self.unparsable.entry(shape(&sk.sql)).or_default() += 1;
                }
                SkipReason::Unsupported(_) => {
                    self.statements += 1;
                    *self.unsupported.entry(shape(&sk.sql)).or_default() += 1;
                }
                // The statement is counted as analyzed; only its body is
                // not.
                SkipReason::UnparsedBody { .. } => self.bodies_unparsed += 1,
            }
        }
    }
}

/// Shape of a statement without any name: unquoted SQL keywords are kept,
/// everything else (identifiers, literals, numbers) becomes `…`.
/// Limited to the first 8 elements, enough to recognize the construct.
fn shape(sql: &str) -> String {
    let Ok(tokens) = Tokenizer::new(&PostgreSqlDialect {}, sql).tokenize() else {
        return "(not tokenizable)".into();
    };
    let mut parts: Vec<String> = Vec::new();
    for tok in tokens {
        let part = match tok {
            Token::Whitespace(_) | Token::Comma | Token::Period | Token::SemiColon => continue,
            Token::LParen | Token::RParen => continue,
            Token::Word(w) if w.quote_style.is_none() && w.keyword != Keyword::NoKeyword => {
                w.value.to_ascii_uppercase()
            }
            _ => "…".to_string(),
        };
        if part == "…" && parts.last().is_some_and(|p| p == "…") {
            continue;
        }
        parts.push(part);
        if parts.len() == 8 {
            parts.push("…".into());
            break;
        }
    }
    parts.dedup();
    parts.join(" ")
}

// ── Replay on PostgreSQL ──────────────────────────────────────────────────────

struct PgReplay {
    major: u32,
    applied: usize,
    /// Project catalog in PostgreSQL, if every migration was applied.
    catalog: Option<pgweave_catalog::Catalog>,
    /// Rank (1-based) of the first rejected migration and masked message.
    first_failure: Option<(usize, String)>,
    /// PostgreSQL's description of each probe, in order.
    probes: Vec<Result<crate::probes::Described, String>>,
}

fn replay_on_postgres(
    url: &str,
    sources: &[(PathBuf, String)],
    probes: &[crate::probes::Probe],
    details: bool,
) -> Result<PgReplay, String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    rt.block_on(async {
        let admin = connect(url).await?;
        let major: i32 = admin
            .query_one(
                "SELECT current_setting('server_version_num')::int / 10000",
                &[],
            )
            .await
            .map_err(|e| db_message(&e))?
            .get(0);
        let db = format!("pgweave_corpus_{}", std::process::id());
        admin
            .batch_execute(&format!("DROP DATABASE IF EXISTS \"{db}\" WITH (FORCE)"))
            .await
            .map_err(|e| db_message(&e))?;
        admin
            .batch_execute(&format!("CREATE DATABASE \"{db}\""))
            .await
            .map_err(|e| db_message(&e))?;

        let mut config: tokio_postgres::Config = url.parse().map_err(|e| format!("URL: {e}"))?;
        config.dbname(&db);
        let result = async {
            let client = connect_config(&config).await?;
            let mut applied = 0;
            for (i, (path, sql)) in sources.iter().enumerate() {
                if let Err(e) = client.batch_execute(sql).await {
                    let msg = db_message(&e);
                    if details {
                        eprintln!("{}: rejected by PostgreSQL: {msg}", path.display());
                    }
                    return Ok::<_, String>((
                        applied,
                        Some((i + 1, redact(&msg))),
                        None,
                        Vec::new(),
                    ));
                }
                applied += 1;
            }
            let catalog = crate::catalog_compare::real_catalog(&client).await?;
            let mut described = Vec::with_capacity(probes.len());
            for p in probes {
                described.push(match &p.typed {
                    Some(t) => {
                        crate::probes::theirs_typed(&client, &p.sql, &t.params, &t.setup).await
                    }
                    None => crate::probes::theirs(&client, &p.sql).await,
                });
            }
            Ok((applied, None, Some(catalog), described))
        }
        .await;

        admin
            .batch_execute(&format!("DROP DATABASE IF EXISTS \"{db}\" WITH (FORCE)"))
            .await
            .map_err(|e| db_message(&e))?;
        let (applied, first_failure, catalog, probes) = result?;
        Ok(PgReplay {
            major: major as u32,
            applied,
            catalog,
            first_failure,
            probes,
        })
    })
}

/// Applies a script to an empty database and returns its project catalog.
fn apply_on_fresh_db(url: &str, sql: &str) -> Result<pgweave_catalog::Catalog, String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    rt.block_on(async {
        let admin = connect(url).await?;
        let db = format!("pgweave_import_{}", std::process::id());
        admin
            .batch_execute(&format!("DROP DATABASE IF EXISTS \"{db}\" WITH (FORCE)"))
            .await
            .map_err(|e| db_message(&e))?;
        admin
            .batch_execute(&format!("CREATE DATABASE \"{db}\""))
            .await
            .map_err(|e| db_message(&e))?;
        let mut config: tokio_postgres::Config = url.parse().map_err(|e| format!("URL: {e}"))?;
        config.dbname(&db);
        let result = async {
            let client = connect_config(&config).await?;
            client
                .batch_execute(sql)
                .await
                .map_err(|e| db_message(&e))?;
            crate::catalog_compare::real_catalog(&client).await
        }
        .await;
        admin
            .batch_execute(&format!("DROP DATABASE IF EXISTS \"{db}\" WITH (FORCE)"))
            .await
            .map_err(|e| db_message(&e))?;
        result
    })
}

/// Masks the identifiers PostgreSQL quotes between double quotes.
fn redact(msg: &str) -> String {
    let mut out = String::new();
    let mut in_quote = false;
    for c in msg.chars() {
        if c == '"' {
            if in_quote {
                out.push_str("…\"");
            } else {
                out.push('"');
            }
            in_quote = !in_quote;
        } else if !in_quote {
            out.push(c);
        }
    }
    out
}

async fn connect(url: &str) -> Result<tokio_postgres::Client, String> {
    let config: tokio_postgres::Config = url.parse().map_err(|e| format!("URL: {e}"))?;
    connect_config(&config).await
}

async fn connect_config(config: &tokio_postgres::Config) -> Result<tokio_postgres::Client, String> {
    let (client, connection) = config
        .connect(tokio_postgres::NoTls)
        .await
        .map_err(|e| format!("connecting to PostgreSQL: {e}"))?;
    tokio::spawn(async move {
        let _ = connection.await;
    });
    Ok(client)
}

fn db_message(e: &tokio_postgres::Error) -> String {
    e.as_db_error()
        .map(|d| d.message().to_string())
        .unwrap_or_else(|| e.to_string())
}

// ── Rendering ─────────────────────────────────────────────────────────────────

/// What the replay on PostgreSQL established.
struct Oracle<'a> {
    pg: Option<&'a PgReplay>,
    /// Rebuilt catalog compared to the real one.
    comparison: Option<&'a crate::catalog_compare::Comparison>,
    /// Import: schema exported then applied, compared to the real one.
    import: Option<&'a Result<crate::catalog_compare::Comparison, String>>,
    fidelity: Option<&'a Result<pgweave_oracle::schema::SchemaComparison, String>>,
    probes: Option<&'a crate::probes::ProbeStats>,
}

fn render(name: &str, layout: Layout, files: usize, s: &Stats, oracle: Oracle<'_>) -> String {
    let Oracle {
        pg,
        comparison,
        import,
        fidelity,
        probes,
    } = oracle;
    let mut o = String::new();
    let skipped_unparsable: usize = s.unparsable.values().sum();
    let skipped_unsupported: usize = s.unsupported.values().sum();
    let date = chrono::Local::now().format("%Y-%m-%d");
    let commit = git_head().unwrap_or_else(|| "unknown".into());

    let _ = writeln!(o, "# Corpus `{name}` — analysis rate\n");
    let _ = writeln!(
        o,
        "> Generated on {date} by `cargo xtask corpus {name}` (pgweave `{commit}`).\n\
         > Files kept: {}.\n\
         > Aggregates only: no corpus identifier appears here.",
        layout.describe()
    );

    let _ = writeln!(o, "\n## Reading\n");
    let _ = writeln!(o, "| Measure | Value |\n|---|--:|");
    let _ = writeln!(o, "| Files | {files} |");
    let _ = writeln!(o, "| Unreadable files | {} |", s.files_unreadable);
    let _ = writeln!(o, "| Statements | {} |", s.statements);
    let _ = writeln!(
        o,
        "| Analyzed (in the AST) | {} ({} %) |",
        s.analyzed,
        pct(s.analyzed, s.statements)
    );
    let _ = writeln!(
        o,
        "| Skipped: unreadable for sqlparser | {skipped_unparsable} |"
    );
    let _ = writeln!(o, "| Skipped: not modeled | {skipped_unsupported} |");
    let _ = writeln!(o, "| Functions and procedures | {} |", s.functions);
    let _ = writeln!(o, "| PL/pgSQL bodies analyzed | {} |", s.bodies_parsed);
    let _ = writeln!(o, "| `LANGUAGE sql` bodies analyzed | {} |", s.bodies_sql);
    let _ = writeln!(o, "| Bodies not analyzed | {} |", s.bodies_unparsed);

    for (title, map) in [
        ("Unreadable statements, by shape", &s.unparsable),
        ("Statements not modeled, by shape", &s.unsupported),
    ] {
        if map.is_empty() {
            continue;
        }
        let _ = writeln!(o, "\n## {title}\n");
        let _ = writeln!(o, "| Shape | Count |\n|---|--:|");
        let mut rows: Vec<_> = map.iter().collect();
        rows.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        for (shape, n) in rows {
            let _ = writeln!(o, "| `{shape}` | {n} |");
        }
    }

    let _ = writeln!(o, "\n## `check` diagnostics\n");
    let _ = writeln!(
        o,
        "Each migration is checked against the catalog replayed up to it \
         (inclusive), `postgres@18` catalog and extensions included. Since the migrations \
         were applied in production, an error is presumably a false positive.\n"
    );
    let _ = writeln!(
        o,
        "Files with at least one error: {}/{files}. Statements missing from \
         the index: {}.\n",
        s.files_with_errors, s.index_skipped
    );
    if s.diagnostics.is_empty() {
        let _ = writeln!(o, "No diagnostics.");
    } else {
        let _ = writeln!(o, "| Severity | Code | Count |\n|---|---|--:|");
        for ((sev, code), n) in &s.diagnostics {
            let _ = writeln!(o, "| {sev} | `{code}` | {n} |");
        }
    }

    let _ = writeln!(o, "\n## Replay on PostgreSQL\n");
    match pg {
        None => {
            let _ = writeln!(o, "Not run (neither `--pg-url` nor `PGWEAVE_ORACLE_URL`).");
        }
        Some(r) => {
            let _ = writeln!(
                o,
                "PostgreSQL {}: {}/{files} migrations applied to an empty database.",
                r.major, r.applied
            );
            if let Some((rank, msg)) = &r.first_failure {
                let _ = writeln!(
                    o,
                    "\nFirst rejected migration: #{rank}: \"{msg}\" (names masked)."
                );
            }
        }
    }

    let _ = writeln!(o, "\n## Catalog rebuilt by pgweave\n");
    let _ = writeln!(
        o,
        "Replay of the migrations in order on the `postgres@18` catalog (extension packs included)."
    );
    if s.replay_diagnostics.is_empty() {
        let _ = writeln!(o, "\nNo replay diagnostics.");
    } else {
        let _ = writeln!(o, "\n| Replay diagnostic | Count |\n|---|--:|");
        for (code, n) in &s.replay_diagnostics {
            let _ = writeln!(o, "| `{code}` | {n} |");
        }
    }
    match comparison {
        None => {
            let _ = writeln!(
                o,
                "\nComparison with the real catalog: not run (PostgreSQL replay missing or incomplete)."
            );
        }
        Some(c) => {
            let _ = writeln!(
                o,
                "\nComparison with the real catalog (project objects, excluding extension members; \
                 expressions compared by presence):\n"
            );
            let _ = writeln!(
                o,
                "| Objects | Real | Identical | Not established (P3) | Different | Missing | Extra |\n|---|--:|--:|--:|--:|--:|--:|"
            );
            for (cat, r) in &c.categories {
                let _ = writeln!(
                    o,
                    "| {cat} | {} | {} | {} | {} | {} | {} |",
                    r.real, r.matched, r.unknown, r.different, r.missing, r.extra
                );
            }
        }
    }

    let _ = writeln!(o, "\n## Import (declarative schema)\n");
    match import {
        None => {
            let _ = writeln!(o, "Not verified (PostgreSQL replay missing or incomplete).");
        }
        Some(Err(e)) => {
            let _ = writeln!(
                o,
                "The exported SQL was rejected by PostgreSQL: \"{e}\" (names masked)."
            );
        }
        Some(Ok(c)) => {
            let _ = writeln!(
                o,
                "The rebuilt catalog, exported as SQL (`pgweave_catalog::dump`) and applied to an \
                 empty database, compared with the real catalog of the migrations:\n"
            );
            let _ = writeln!(
                o,
                "| Objects | Real | Identical | Different | Missing | Extra |\n|---|--:|--:|--:|--:|--:|"
            );
            for (cat, r) in &c.categories {
                let _ = writeln!(
                    o,
                    "| {cat} | {} | {} | {} | {} | {} |",
                    r.real,
                    r.matched,
                    r.different + r.unknown,
                    r.missing,
                    r.extra
                );
            }
        }
    }
    match fidelity {
        None => {}
        Some(Err(e)) => {
            let _ = writeln!(
                o,
                "\nText-level comparison not run: \"{e}\" (names masked)."
            );
        }
        Some(Ok(c)) => {
            let _ = writeln!(
                o,
                "\nDown to the text, according to PostgreSQL (`format_type`, `pg_get_*def`: defaults, \
                 indexes, views, routine bodies; P7 migration oracle, except NOT NULL \
                 constraint names), {} description line(s) compared: {}.",
                c.objects,
                if c.is_identical() {
                    "identical databases".to_string()
                } else {
                    format!(
                        "{} only in migrations, {} only in import",
                        c.only_migrations.len(),
                        c.only_schema.len()
                    )
                }
            );
        }
    }

    crate::probes::render(&mut o, probes);

    let _ = writeln!(o, "\n## Time (indicative, machine-dependent)\n");
    let _ = writeln!(o, "| Step | Duration |\n|---|--:|");
    let _ = writeln!(
        o,
        "| Parse of all files | {} ms |",
        s.parse_time.as_millis()
    );
    let _ = writeln!(
        o,
        "| Check of all files | {} ms |",
        s.check_time.as_millis()
    );
    let _ = writeln!(
        o,
        "| Replay on the catalog (`pgweave-catalog`) | {} ms |",
        s.replay_time.as_millis()
    );
    o
}

fn pct(n: usize, total: usize) -> usize {
    (n * 100).checked_div(total).unwrap_or(100)
}

fn git_head() -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shape_contains_no_name() {
        let s = shape(
            r#"ALTER TABLE "OrderLine" ADD CONSTRAINT "fk_x" FOREIGN KEY ("a") REFERENCES "Other"("id") ON DELETE CASCADE"#,
        );
        assert!(
            !s.contains("Menu") && !s.contains("Other") && !s.contains("fk_x"),
            "{s}"
        );
        assert!(s.starts_with("ALTER TABLE … ADD CONSTRAINT …"), "{s}");
    }

    #[test]
    fn redact_masks_identifiers() {
        assert_eq!(
            redact(r#"relation "OrderLine" does not exist"#),
            r#"relation "…" does not exist"#
        );
    }
}
