use std::fs;
use std::path::Path;
use std::process;

use clap::{Parser, Subcommand};
use pgzest_ast::Statement;

mod db;
mod describe;
mod dev;
mod import;
mod lsp;
mod migrate;
mod pack;

#[derive(Parser)]
#[command(name = "pgzest", about = "PostgreSQL language toolchain", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// clap mirror of `pgzest_schema::SkipLevel`, to expose the levels in the CLI.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum CliSkipLevel {
    /// Report nothing.
    Ignore,
    /// Report without failing.
    Warn,
    /// Report and fail.
    Error,
}

impl From<CliSkipLevel> for pgzest_schema::SkipLevel {
    fn from(v: CliSkipLevel) -> Self {
        match v {
            CliSkipLevel::Ignore => pgzest_schema::SkipLevel::Ignore,
            CliSkipLevel::Warn => pgzest_schema::SkipLevel::Warn,
            CliSkipLevel::Error => pgzest_schema::SkipLevel::Error,
        }
    }
}

/// clap mirror of `pgzest_expand::LogLevel`.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum CliLogLevel {
    /// Nothing.
    None,
    /// One entry per run: routine, start, duration, handled error.
    Info,
    /// Plus one entry per INSERT, UPDATE, DELETE or EXECUTE.
    Step,
    /// Plus the `@snapshot` copies.
    Debug,
}

impl From<CliLogLevel> for pgzest_expand::LogLevel {
    fn from(v: CliLogLevel) -> Self {
        match v {
            CliLogLevel::None => Self::None,
            CliLogLevel::Info => Self::Info,
            CliLogLevel::Step => Self::Step,
            CliLogLevel::Debug => Self::Debug,
        }
    }
}

/// clap mirror of `pgzest_expand::LogTarget`.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum CliLogTarget {
    /// `pgzest_log.run` and `pgzest_log.step`.
    Table,
    /// `pg_notify('pgzest_log', …)`.
    Notify,
    /// `RAISE NOTICE 'pgzest_log'`, sent to the client at once (tracing).
    Notice,
}

impl From<CliLogTarget> for pgzest_expand::LogTarget {
    fn from(v: CliLogTarget) -> Self {
        match v {
            CliLogTarget::Table => Self::Table,
            CliLogTarget::Notify => Self::Notify,
            CliLogTarget::Notice => Self::Notice,
        }
    }
}

#[derive(Subcommand)]
enum Command {
    /// Type-check SQL files, printing diagnostics. Exits 1 if any errors are found.
    ///
    /// All positional files are checked. Any `--schema` files contribute to the
    /// schema context and trigger map without themselves being checked.
    /// If no `--schema` is given and a single file is passed, it is analysed
    /// in self-contained mode (DDL inside the file seeds its own schema).
    Check {
        /// SQL files to check (at least one required)
        #[arg(required = true)]
        files: Vec<String>,
        /// Extra DDL/schema files that provide context (can be repeated)
        #[arg(long = "schema")]
        schemas: Vec<String>,
        /// How to report statements the parser could not read.
        /// Overrides `[parse] unparsable` from pgzest.toml.
        #[arg(long = "on-unparsable", value_enum)]
        on_unparsable: Option<CliSkipLevel>,
        /// How to report valid statements the AST does not model.
        /// Overrides `[parse] unsupported`.
        #[arg(long = "on-unsupported", value_enum)]
        on_unsupported: Option<CliSkipLevel>,
        /// Ignore pgzest.toml: do not index the project, use only
        /// `--schema` and the target files themselves.
        #[arg(long = "no-project")]
        no_project: bool,
    },

    /// Describe named queries (parameters, columns, nullability,
    /// cardinality, dependencies) as JSON descriptors.
    ///
    /// Each file is a named-query file (`-- name: …` headers). The schema is
    /// the project's (pgzest.toml) plus any `--schema` files. Exits 1 if a
    /// query cannot be described; nothing is ever guessed.
    Describe {
        /// Named-query files to describe
        files: Vec<String>,
        /// A single query given inline instead of (or in addition to) files
        #[arg(long)]
        sql: Option<String>,
        /// Name of the `--sql` query
        #[arg(long, default_value = "query")]
        name: String,
        /// Extra DDL/schema files that provide context (can be repeated)
        #[arg(long = "schema")]
        schemas: Vec<String>,
        /// Ignore pgzest.toml
        #[arg(long = "no-project")]
        no_project: bool,
        /// Also write each descriptor to `.pgzest/queries/<fingerprint>.json`
        #[arg(long)]
        cache: bool,
        /// Print the generated Rust module (as `pgzest-build` writes it)
        /// instead of the JSON descriptors
        #[arg(long)]
        rust: bool,
    },

    /// List available macros: the ones shipped with the tool, plus the
    /// project's own from `[macros] directory`.
    Macros {
        /// Show each macro's header pattern and its expansion template.
        #[arg(long)]
        verbose: bool,
    },

    /// Expand macros in a `.pgsql` file and print standard SQL.
    ///
    /// With `--out-dir`, writes one `.sql` per input instead of printing.
    Build {
        /// `.pgsql` files to expand (at least one required)
        #[arg(required = true)]
        files: Vec<String>,
        /// Directory to write the generated `.sql` files into
        #[arg(long = "out-dir")]
        out_dir: Option<String>,
        /// What the PL/pgSQL routines record of their runs (default: `[log]`
        /// of pgzest.toml, else `none`). `debug` also makes `@snapshot`
        /// copy the temp tables and variables into `pgzest_debug`. The
        /// tables written to are created first.
        #[arg(long, value_enum)]
        log: Option<CliLogLevel>,
        /// Where the log entries go (default: `[log]` of pgzest.toml, else
        /// `table`).
        #[arg(long = "log-target", value_enum)]
        log_target: Option<CliLogTarget>,
    },

    /// Apply the routines of the project's schema files to a development
    /// database — with run logs and snapshots by default — then again on
    /// each save.
    ///
    /// Functions, procedures and triggers only (`CREATE OR REPLACE`);
    /// tables go through migrations. A routine a file no longer declares
    /// is dropped if this session created it.
    Dev {
        /// Database URL (default: `DATABASE_URL`)
        #[arg(long, env = "DATABASE_URL")]
        url: String,
        /// What the routines record of their runs (see `pgzest build`)
        #[arg(long, value_enum, default_value = "debug")]
        log: CliLogLevel,
        /// Where the log entries go
        #[arg(long = "log-target", value_enum, default_value = "table")]
        log_target: CliLogTarget,
        /// Apply once and exit instead of watching
        #[arg(long)]
        once: bool,
    },

    /// Database subcommands
    Db {
        #[command(subcommand)]
        action: DbAction,
    },

    /// Lint a SQL file for style and best-practice violations.
    Lint {
        /// SQL file to lint
        file: String,
    },

    /// Migration subcommands
    Migrate {
        #[command(subcommand)]
        action: MigrateAction,
    },

    /// Schema subcommands
    Schema {
        #[command(subcommand)]
        action: SchemaAction,
    },

    /// Parse and re-emit the SQL AST (normalization round-trip).
    Emit {
        /// SQL file to emit
        file: String,
    },

    /// Run the language server over stdio (what editors start).
    Lsp,
}

#[derive(Subcommand)]
enum DbAction {
    /// Compare a database with what its migrations produce: what was
    /// changed in it by hand (a routine, an index, a trigger…). Exits 1 if
    /// it differs.
    ///
    /// The migrations it received (`_prisma_migrations`,
    /// `__diesel_schema_migrations`, or all of them) are replayed in an
    /// empty database created on the server, then dropped; both are
    /// described by PostgreSQL itself.
    Check {
        /// Database URL (default: `DATABASE_URL`)
        #[arg(long, env = "DATABASE_URL")]
        url: String,
        /// Server the migrations are replayed on, same PostgreSQL version
        /// (default: the database's; needs the CREATEDB privilege)
        #[arg(long = "scratch-url")]
        scratch_url: Option<String>,
        /// Last migration the database received, for a database without
        /// migrations table
        #[arg(long)]
        until: Option<String>,
    },

    /// Generate the catalog pack of an extension pgzest does not ship, from
    /// a server where it is installed, for `[postgres] packs`.
    ///
    /// The extension is created in a temporary database, then dropped
    /// (needs the CREATEDB privilege). The server must run the PostgreSQL
    /// version the project targets.
    Pack {
        /// Server URL (default: `DATABASE_URL`)
        #[arg(long, env = "DATABASE_URL")]
        url: String,
        /// Extension name, as in `CREATE EXTENSION`
        #[arg(long)]
        extension: String,
        /// Extension version (default: the server's default one)
        #[arg(long)]
        version: Option<String>,
        /// Directory to write `<name>-<version>.json` into (default: the
        /// first directory of `[postgres] packs`)
        #[arg(long)]
        out: Option<std::path::PathBuf>,
    },
}

#[derive(Subcommand)]
enum SchemaAction {
    /// Import an existing project: replay its migrations and write the
    /// resulting declarative schema (one `<schema>/schema.sql` per
    /// PostgreSQL schema) plus a baseline migration with its header.
    Import {
        /// Migrations directory
        migrations: String,
        /// How migrations are laid out
        #[arg(long, value_enum, default_value = "flat")]
        layout: import::Layout,
        /// Output directory for the declarative schema
        #[arg(long, default_value = "schema")]
        out: String,
        /// Baseline migration file (default: `<out>/baseline.sql`)
        #[arg(long)]
        baseline: Option<String>,
        /// Write even if the replay reported errors
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand)]
enum MigrateAction {
    /// Migration between two schemas (SQL files or directories), without a
    /// project: both are replayed and their catalogs compared.
    Diff {
        /// Old schema (file or directory)
        old: String,
        /// New schema (file or directory)
        new: String,
    },

    /// Generate the next migration of a declarative project
    /// (`[schema] declarative = true`): what separates the state reached by
    /// the migrations from the declarative schema, checked by replay
    /// before it is written. It is the simplest migration that reaches the
    /// schema: steps that lose data or may fail on a populated database are
    /// listed in its header, for the developer to adapt.
    Create {
        /// Migration name (lowercase, digits, `_`)
        name: String,
        /// Print the migration instead of writing it
        #[arg(long)]
        dry_run: bool,
    },

    /// Fail if the migrations do not reach the declarative schema (CI).
    Check,
}

fn read_file(path: &str) -> Result<String, String> {
    fs::read_to_string(path).map_err(|e| format!("cannot read '{}': {}", path, e))
}

fn offset_to_line_col(sql: &str, offset: usize) -> (usize, usize) {
    let mut line = 1usize;
    let mut col = 1usize;
    for (i, ch) in sql.char_indices() {
        if i >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}

fn cmd_check(
    files: &[String],
    schemas: &[String],
    on_unparsable: Option<pgzest_schema::SkipLevel>,
    on_unsupported: Option<pgzest_schema::SkipLevel>,
    no_project: bool,
) -> i32 {
    // ── Project discovery ──────────────────────────────────────────────────────
    // Same mechanism as the LSP: walk up to pgzest.toml. Without it, the
    // editor and CI do not see the same schema for the same file — one
    // indexes the whole project, the other requires explicit --schema, and
    // the diagnostics diverge.
    let project = if no_project {
        None
    } else {
        match discover_project() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("error: {e}");
                return 2;
            }
        }
    };

    let registry = pgzest_plugin::registry::PluginRegistry::new();
    let builtin_plugins = project
        .as_ref()
        .map(|(_, c)| c.builtin_plugins())
        .unwrap_or_default();
    if let Err(e) = registry.check_names(builtin_plugins.keys()) {
        eprintln!("error: {}: {e}", pgzest_schema::ProjectConfig::FILENAME);
        return 2;
    }

    // ── Parse context/schema files ─────────────────────────────────────────────
    // Each file is kept with its raw SQL so we can detect its default schema.
    let mut context_files: Vec<(String, Vec<Statement>)> = Vec::new(); // (sql, stmts)
    for path in schemas {
        let sql = match read_file(path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("error: {}", e);
                return 1;
            }
        };
        match pgzest_parser::parse(&sql) {
            Ok(s) => context_files.push((sql, s)),
            Err(e) => eprintln!("warning: failed to parse schema '{}': {}", path, e),
        }
    }
    let context_stmts: Vec<Statement> = context_files
        .iter()
        .flat_map(|(_, s)| s.iter().cloned())
        .collect();

    // ── Parser tolerance policy ────────────────────────────────────────────────
    let parse_cfg = {
        let mut cfg = project
            .as_ref()
            .map(|(_, c)| c.parse.clone())
            .unwrap_or_default();
        if let Some(level) = on_unparsable {
            cfg.unparsable = level;
        }
        if let Some(level) = on_unsupported {
            cfg.unsupported = level;
        }
        cfg
    };
    let analysis_opts = pgzest_analysis::AnalysisOptions {
        parse: parse_cfg.clone(),
    };

    // Macros: shipped + the project's. Desugaring is attempted on every
    // file — a source without macros is copied verbatim.
    let (macros, macro_errors) = load_macros(project.as_ref());
    for e in &macro_errors {
        eprintln!("warning: {e}");
    }
    let packs = match load_packs(project.as_ref()) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            return 2;
        }
    };

    // ── Parse target files ─────────────────────────────────────────────────────
    // `parse_with_outcome` keeps track of skipped statements instead of
    // losing them: that is what allows reporting them below.
    type Target = (
        String,
        String,
        Vec<Statement>,
        Vec<pgzest_parser::SkippedStatement>,
        Vec<pgzest_diagnostics::Diagnostic>,
    );
    let parsed_targets: Vec<Target> = {
        // (path, source, stmts, skipped, macro diagnostics)
        let mut out = Vec::new();
        for file in files {
            let sql = match read_file(file) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("error: {}", e);
                    return 1;
                }
            };
            // Desugaring: the spans returned by the parse are already in the
            // edited file's frame, so nothing to translate back afterwards.
            // Query file: `$name?` markers blanked for the parse.
            let query_file = project.as_ref().is_some_and(|(root, cfg)| {
                cfg.classify_file(Path::new(file), root) == pgzest_schema::FileRole::Query
            });
            let prepared = if query_file {
                pgzest_analysis::prepare_query_source(&sql, &macros)
            } else {
                pgzest_analysis::prepare_source(&sql, &macros)
            };
            match prepared.parse_with_outcome() {
                Ok(o) => out.push((
                    file.clone(),
                    sql,
                    o.statements,
                    o.skipped,
                    prepared.diagnostics,
                )),
                Err(e) => {
                    eprintln!("error: {}: parse error: {}", file, e);
                    return 1;
                }
            }
        }
        out
    };

    // ── Catalog and check ──────────────────────────────────────────────────────
    // A single replay: `--schema` schemas, project files (same construction
    // as the LSP), then targets outside the project. Each statement is
    // checked as it applies, against the catalog state at that point; a
    // PL/pgSQL body is checked against the final state (`pgzest-sema`).
    let canonical = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    // Same classification as the LSP: a project file outside the schema and
    // migrations directories runs on the final schema.
    let is_query_file = |file: &str| {
        project.as_ref().is_some_and(|(root, cfg)| {
            cfg.classify_file(Path::new(file), root) == pgzest_schema::FileRole::Query
        })
    };
    let mut builder = pgzest_catalog::project::ProjectBuilder::new()
        .with_typer(Some(pgzest_sema::ViewTyper::shared()))
        .with_checker(Some(pgzest_sema::check::SemaChecker::shared()))
        .with_instrumentation(macros.instrumentation())
        .declarative(project.as_ref().is_some_and(|(_, c)| c.schema.declarative))
        .with_packs(packs);
    let mut replayed: std::collections::HashSet<std::path::PathBuf> =
        std::collections::HashSet::new();
    for (path, (sql, stmts)) in schemas.iter().zip(&context_files) {
        let outcome = pgzest_parser::ParseOutcome {
            statements: stmts.clone(),
            skipped: Vec::new(),
        };
        let path = canonical(Path::new(path));
        builder.add_file(&path, sql, &pgzest_schema::FileRole::Schema, Ok(&outcome));
        replayed.insert(path);
    }
    if let Some((root, cfg)) = &project {
        let source = cfg.to_source(root);
        for chunk in pgzest_schema::SchemaSource::chunks(&source) {
            let parsed = pgzest_catalog::project::parse_source(
                &chunk.sql,
                chunk.origin.byte_range.start,
                &macros,
            );
            let path = canonical(&chunk.origin.path);
            builder.add_file(&path, &chunk.sql, &chunk.role, parsed.as_ref());
            replayed.insert(path);
        }
    }
    for (file, sql, stmts, _, _) in &parsed_targets {
        let path = canonical(Path::new(file));
        if is_query_file(file) || replayed.contains(&path) {
            continue;
        }
        let outcome = pgzest_parser::ParseOutcome {
            statements: stmts.clone(),
            skipped: Vec::new(),
        };
        builder.add_file(&path, sql, &pgzest_schema::FileRole::Schema, Ok(&outcome));
        replayed.insert(path);
    }
    let catalog = std::sync::Arc::new(builder.finish());

    // An unreadable statement in a file in scope removes its objects from the
    // catalog, which then produces `does not exist` false positives. Say so
    // rather than pretend the catalog is complete.
    if parse_cfg.unparsable != pgzest_schema::SkipLevel::Ignore {
        for (path, sk) in &catalog.skipped {
            eprintln!(
                "warning: {}: statement not indexed, the objects it defines \
                 will not be found ({:?})",
                path.display(),
                sk.reason
            );
        }
    }

    // Plugin rules: they see the schema through `SchemaProvider`.
    let has_external_schema = !schemas.is_empty();
    let mut composite = pgzest_schema::provider::CompositeSchemaProvider::new();
    composite.add(Box::new(SharedCatalog(catalog.clone())));

    let mut checker = pgzest_analysis::Checker::default();
    for rule in registry.collect_rules(&builtin_plugins) {
        checker.add_rule(rule);
    }
    // WASM plugins listed in [plugins] wasm, relative to the config file.
    let wasm_paths: Vec<std::path::PathBuf> = project
        .as_ref()
        .map(|(root, c)| c.plugins.wasm.iter().map(|p| root.join(p)).collect())
        .unwrap_or_default();
    for wasm_path in &wasm_paths {
        match pgzest_plugin::WasmPlugin::from_file(wasm_path) {
            Ok(plugin) => {
                let rule = pgzest_plugin::WasmCheckRule::new(std::sync::Arc::new(plugin));
                checker.add_rule(Box::new(rule));
            }
            Err(e) => {
                eprintln!(
                    "error: failed to load wasm plugin '{}': {}",
                    wasm_path.display(),
                    e
                );
                return 2;
            }
        }
    }

    // ── Check each target file ─────────────────────────────────────────────────
    let mut has_errors = false;
    let show_filename = parsed_targets.len() > 1 || has_external_schema;

    // Severity overrides from [rules], for checker and lint alike.
    let rule_overrides = project
        .as_ref()
        .map(|(_, c)| c.rule_overrides())
        .unwrap_or_default();

    // Queries of each query directory, per module: a query may reuse the
    // rows of another (`-> a::b::other`).
    let mut query_trees: std::collections::HashMap<
        std::path::PathBuf,
        Vec<pgzest_query::QueryModule>,
    > = std::collections::HashMap::new();
    for (file, sql, stmts, skipped, macro_diags) in &parsed_targets {
        // Plugin rule context: the triggers of the other files.
        let all_other_stmts: Vec<Statement> = context_stmts
            .iter()
            .chain(
                parsed_targets
                    .iter()
                    .filter(|(f, _, _, _, _)| f != file)
                    .flat_map(|(_, _, s, _, _)| s.iter()),
            )
            .cloned()
            .collect();
        let mut diagnostics = checker.run_with_context(stmts, &all_other_stmts, &composite);

        // Semantic check: a query file runs on the final schema; a replayed
        // file carries its replay diagnostics.
        let this_file = canonical(Path::new(file));
        if is_query_file(file) {
            diagnostics.extend(pgzest_analysis::check_statements(stmts, &catalog));
            let located = project
                .as_ref()
                .and_then(|(root, cfg)| cfg.query_module(Path::new(file), root));
            let (here, others) = match located {
                Some((qroot, here)) => {
                    let tree = query_trees.entry(qroot.clone()).or_insert_with(|| {
                        pgzest_analysis::query_modules(&qroot, &catalog, &macros, &|p| {
                            std::fs::read_to_string(p).ok()
                        })
                    });
                    (here, tree.clone())
                }
                None => (Vec::new(), Vec::new()),
            };
            diagnostics.extend(pgzest_analysis::query_descriptor_diagnostics(
                sql, &catalog, &macros, &here, &others,
            ));
        } else {
            diagnostics.extend(
                catalog
                    .diagnostics
                    .iter()
                    .filter(|(p, _)| *p == this_file)
                    .map(|(_, d)| d.clone()),
            );
        }

        // Statements skipped by the parser → diagnostics per the config.
        let mut all = macro_diags.clone();
        // Named query file: its format is checked as well.
        if let Some((root, cfg)) = &project
            && cfg.is_query_file(Path::new(file), root)
        {
            all.extend(pgzest_analysis::query_file_diagnostics(
                Path::new(file),
                sql,
                &macros,
            ));
        }
        all.extend(pgzest_analysis::skipped_to_diagnostics(
            skipped,
            &analysis_opts.parse,
        ));
        all.append(&mut diagnostics);
        let mut diagnostics = all;

        pgzest_diagnostics::apply_rule_overrides(&mut diagnostics, &rule_overrides);

        // Comment directives (-- pgzest-disable-next-line, …).
        pgzest_diagnostics::Suppressions::scan(sql).apply(&mut diagnostics);
        // In file order.
        diagnostics.sort_by_key(|d| d.span.as_ref().map_or(0, |s| s.start));

        for diag in &diagnostics {
            use pgzest_diagnostics::Severity;
            let sev_str = match diag.severity {
                Severity::Error => {
                    has_errors = true;
                    "error"
                }
                Severity::Warning => "warning",
                Severity::Info => "info",
            };
            let (line, col) = diag
                .span
                .as_ref()
                .filter(|s| s.start > 0 || s.end > 0)
                .map(|s| offset_to_line_col(sql, s.start))
                .unwrap_or((1, 1));
            let location = if show_filename {
                format!("{}:{}:{}", file, line, col)
            } else {
                format!("{}:{}", line, col)
            };
            if let Some(hint) = &diag.hint {
                println!("{}: {}[{}]: {}", location, sev_str, diag.code, diag.message);
                println!("  hint: {}", hint);
            } else {
                println!("{}: {}[{}]: {}", location, sev_str, diag.code, diag.message);
            }
        }
    }

    if has_errors { 1 } else { 0 }
}

fn cmd_lint(file: &str) -> i32 {
    let sql = match read_file(file) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {}", e);
            return 1;
        }
    };

    let stmts = match pgzest_parser::parse(&sql) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: parse error: {}", e);
            return 1;
        }
    };

    let config = pgzest_lint::LintConfig::default();
    let mut diagnostics = pgzest_lint::lint_statements(&stmts, &config).diagnostics;

    // Same severity overrides as `check` and the LSP: a rule set to "off"
    // in [rules] must be silent everywhere.
    if let Some((_, project)) = discover_project_or_exit() {
        pgzest_diagnostics::apply_rule_overrides(&mut diagnostics, &project.rule_overrides());
    }

    // Same comment directives as `check`: a rule silenced there must be
    // silenced here too, otherwise `-- pgzest-disable-next-line L001` would
    // work in the editor and not in the CLI.
    pgzest_diagnostics::Suppressions::scan(&sql).apply(&mut diagnostics);

    let mut has_errors = false;
    for diag in &diagnostics {
        use pgzest_diagnostics::Severity;
        let sev_str = match diag.severity {
            Severity::Error => {
                has_errors = true;
                "error"
            }
            Severity::Warning => "warning",
            Severity::Info => "info",
        };
        let (line, col) = diag
            .span
            .as_ref()
            .filter(|s| s.start > 0 || s.end > 0)
            .map(|s| offset_to_line_col(&sql, s.start))
            .unwrap_or((1, 1));
        let location = format!("{}:{}", line, col);
        if let Some(hint) = &diag.hint {
            println!("{}: {}[{}]: {}", location, sev_str, diag.code, diag.message);
            println!("  hint: {}", hint);
        } else {
            println!("{}: {}[{}]: {}", location, sev_str, diag.code, diag.message);
        }
    }

    if diagnostics.is_empty() {
        println!("no issues found");
    }

    if has_errors { 1 } else { 0 }
}

fn cmd_emit(file: &str) -> i32 {
    let sql = match read_file(file) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {}", e);
            return 1;
        }
    };

    let stmts = match pgzest_parser::parse(&sql) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: parse error: {}", e);
            return 1;
        }
    };

    for stmt in &stmts {
        println!("{:#?}", stmt);
    }

    0
}

fn main() {
    let cli = Cli::parse();

    let exit_code = match cli.command {
        Command::Check {
            files,
            schemas,
            on_unparsable,
            on_unsupported,
            no_project,
        } => cmd_check(
            &files,
            &schemas,
            on_unparsable.map(Into::into),
            on_unsupported.map(Into::into),
            no_project,
        ),
        Command::Describe {
            files,
            sql,
            name,
            schemas,
            no_project,
            cache,
            rust,
        } => describe::run(describe::Options {
            files: &files,
            sql: sql.as_deref(),
            name: &name,
            schemas: &schemas,
            no_project,
            cache,
            rust,
        }),
        Command::Macros { verbose } => cmd_macros(verbose),
        Command::Build {
            files,
            out_dir,
            log,
            log_target,
        } => cmd_build(
            &files,
            out_dir.as_deref(),
            log.map(Into::into),
            log_target.map(Into::into),
        ),
        Command::Dev {
            url,
            log,
            log_target,
            once,
        } => dev::run(dev::Options {
            url,
            log: log.into(),
            target: log_target.into(),
            once,
        }),
        Command::Db { action } => match action {
            DbAction::Check {
                url,
                scratch_url,
                until,
            } => db::check(db::Options {
                url,
                scratch_url,
                until,
            }),
            DbAction::Pack {
                url,
                extension,
                version,
                out,
            } => pack::run(pack::Options {
                url,
                extension,
                version,
                out,
            }),
        },
        Command::Lint { file } => cmd_lint(&file),
        Command::Schema { action } => match action {
            SchemaAction::Import {
                migrations,
                layout,
                out,
                baseline,
                force,
            } => import::run(
                Path::new(&migrations),
                layout,
                Path::new(&out),
                baseline.as_deref().map(Path::new),
                force,
            ),
        },
        Command::Migrate { action } => match action {
            MigrateAction::Diff { old, new } => migrate::diff_files(&old, &new),
            MigrateAction::Create { name, dry_run } => migrate::create(&name, dry_run),
            MigrateAction::Check => migrate::check(),
        },
        Command::Emit { file } => cmd_emit(&file),
        Command::Lsp => lsp::run(),
    };

    process::exit(exit_code);
}

// ── Macros ────────────────────────────────────────────────────────────────────

/// The project's macro set: the shipped ones, plus those of `[macros] directory`,
/// instrumented at the `[log]` level.
///
/// Definition errors are reported but do not block: the other macros remain
/// usable.
fn load_macros(
    project: Option<&(std::path::PathBuf, pgzest_schema::ProjectConfig)>,
) -> (pgzest_expand::MacroSet, Vec<String>) {
    let mut set = pgzest_expand::MacroSet::builtin();
    let mut errors = Vec::new();

    if let Some((root, config)) = project {
        let dir = root.join(&config.macros.directory);
        if dir.is_dir() {
            errors = set.load_dir(&dir);
        }
        set = set.with_instrumentation(config.log.instrumentation());
    }
    (set, errors)
}

/// Extension packs of the project (`[postgres] packs`). A configured pack
/// that cannot be used is an error, like an invalid config: never skipped.
fn load_packs(
    project: Option<&(std::path::PathBuf, pgzest_schema::ProjectConfig)>,
) -> Result<std::sync::Arc<pgzest_catalog::ProjectPacks>, String> {
    let Some((root, config)) = project else {
        return Ok(Default::default());
    };
    let packs = pgzest_catalog::ProjectPacks::for_config(root, config);
    let mut errors = Vec::new();
    for (path, d) in &packs.diagnostics {
        let line = format!("{}: {}", path.display(), d.message);
        match d.severity {
            pgzest_diagnostics::Severity::Error => errors.push(line),
            _ => eprintln!("info: {line}"),
        }
    }
    if errors.is_empty() {
        Ok(std::sync::Arc::new(packs))
    } else {
        Err(errors.join("\n"))
    }
}

/// Project discovered by walking up from the current directory.
///
/// `Ok(None)`: no `pgzest.toml`. `Err`: a file exists but is invalid — to
/// be reported, never replaced by the default config.
fn discover_project() -> Result<Option<(std::path::PathBuf, pgzest_schema::ProjectConfig)>, String>
{
    let cwd = std::path::Path::new(".")
        .canonicalize()
        .map_err(|e| format!("cannot resolve current directory: {e}"))?;
    pgzest_schema::ProjectConfig::find_and_load(&cwd).map_err(|e| e.to_string())
}

/// Like [`discover_project`], for commands that exit with an error rather
/// than continue on an invalid config.
fn discover_project_or_exit() -> Option<(std::path::PathBuf, pgzest_schema::ProjectConfig)> {
    match discover_project() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            process::exit(2);
        }
    }
}

fn cmd_macros(verbose: bool) -> i32 {
    let project = discover_project_or_exit();
    let (set, errors) = load_macros(project.as_ref());
    for e in &errors {
        eprintln!("warning: {e}");
    }

    println!("{} macros available:\n", set.len());
    for name in set.names() {
        let def = set.get(name).expect("listed name");
        let origin = if def.origin.starts_with("<builtin>") {
            "builtin".to_string()
        } else {
            def.origin.clone()
        };
        println!("  @{name}  ({origin})");

        if verbose {
            let slots = set.slots_of(name).unwrap_or_default();
            if !slots.is_empty() {
                let rendered: Vec<String> = slots
                    .iter()
                    .map(|(n, k)| format!("{n}:{}", k.as_str()))
                    .collect();
                println!("      slots   : {}", rendered.join(", "));
            }
            if !def.flags.is_empty() {
                let f: Vec<&str> = def.flags.iter().map(|f| f.keyword.as_str()).collect();
                println!("      flags   : {}", f.join(", "));
            }
            if !def.options.is_empty() {
                let o: Vec<String> = def
                    .options
                    .iter()
                    .map(|o| match &o.default {
                        Some(d) => format!("{} (default {d})", o.keyword),
                        None => o.keyword.clone(),
                    })
                    .collect();
                println!("      options : {}", o.join(", "));
            }
            println!();
        }
    }

    if !verbose {
        println!("\n`pgzest macros --verbose` details each macro.");
        println!("To add one: a `*.pgmacro` in the `[macros] directory` directory.");
    }
    if errors.is_empty() { 0 } else { 1 }
}

fn cmd_build(
    files: &[String],
    out_dir: Option<&str>,
    log: Option<pgzest_expand::LogLevel>,
    target: Option<pgzest_expand::LogTarget>,
) -> i32 {
    let project = discover_project_or_exit();
    let (set, errors) = load_macros(project.as_ref());
    let configured = set.instrumentation();
    let instrumentation = pgzest_expand::Instrumentation {
        log: log.unwrap_or(configured.log),
        target: target.unwrap_or(configured.target),
        ..configured
    };
    let setup = instrumentation.setup_sql();
    let set = set.with_instrumentation(instrumentation);
    for e in &errors {
        eprintln!("warning: {e}");
    }

    let mut failed = false;
    for path in files {
        let src = match read_file(path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("error: {e}");
                failed = true;
                continue;
            }
        };
        let mut expansion = pgzest_expand::expand_source(&src, &set);
        if !setup.is_empty() {
            expansion.sql = format!("{setup}\n{}", expansion.sql);
        }

        for d in &expansion.diagnostics {
            use pgzest_diagnostics::Severity;
            let sev = match d.severity {
                Severity::Error => {
                    failed = true;
                    "error"
                }
                Severity::Warning => "warning",
                Severity::Info => "info",
            };
            // Macro diagnostic spans are already in the source frame.
            let (line, col) = d
                .span
                .as_ref()
                .map(|s| offset_to_line_col(&src, s.start))
                .unwrap_or((1, 1));
            eprintln!("{path}:{line}:{col}: {sev}[{}]: {}", d.code, d.message);
        }

        match out_dir {
            None => print!("{}", expansion.sql),
            Some(dir) => {
                let stem = std::path::Path::new(path)
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| "out".into());
                let target = std::path::Path::new(dir).join(format!("{stem}.sql"));
                if let Some(parent) = target.parent()
                    && let Err(e) = std::fs::create_dir_all(parent)
                {
                    eprintln!("error: {}: {e}", parent.display());
                    failed = true;
                    continue;
                }
                match std::fs::write(&target, &expansion.sql) {
                    Ok(()) => println!(
                        "{} → {} ({} macro(s) desugared)",
                        path,
                        target.display(),
                        expansion.expanded
                    ),
                    Err(e) => {
                        eprintln!("error: {}: {e}", target.display());
                        failed = true;
                    }
                }
            }
        }
    }
    if failed { 1 } else { 0 }
}

/// Shared catalog seen as a `SchemaProvider` (plugin rules).
struct SharedCatalog(std::sync::Arc<pgzest_catalog::ProjectCatalog>);

impl pgzest_schema::provider::SchemaProvider for SharedCatalog {
    fn get_table(&self, name: &str) -> Option<&pgzest_schema::provider::TableSchema> {
        self.0.get_table(name)
    }

    fn get_function(&self, name: &str) -> Option<&pgzest_schema::provider::FunctionSignature> {
        self.0.get_function(name)
    }

    fn all_table_names(&self) -> Vec<&str> {
        self.0.all_table_names()
    }

    fn as_rls_aware(&self) -> Option<&dyn pgzest_schema::provider::RlsAwareSchemaProvider> {
        self.0.as_rls_aware()
    }
}
