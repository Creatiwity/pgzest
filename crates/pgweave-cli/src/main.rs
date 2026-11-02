use std::fs;
use std::path::Path;
use std::process;

use clap::{Parser, Subcommand};
use pgweave_ast::Statement;

mod describe;
mod import;
mod migrate;

#[derive(Parser)]
#[command(name = "pgweave", about = "PostgreSQL language toolchain", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// Miroir clap de `pgweave_schema::SkipLevel`, pour exposer les niveaux en CLI.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum CliSkipLevel {
    /// Ne rien signaler.
    Ignore,
    /// Signaler sans faire échouer.
    Warn,
    /// Signaler et faire échouer.
    Error,
}

impl From<CliSkipLevel> for pgweave_schema::SkipLevel {
    fn from(v: CliSkipLevel) -> Self {
        match v {
            CliSkipLevel::Ignore => pgweave_schema::SkipLevel::Ignore,
            CliSkipLevel::Warn => pgweave_schema::SkipLevel::Warn,
            CliSkipLevel::Error => pgweave_schema::SkipLevel::Error,
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
        /// Overrides `[parse] unparsable` from pgweave.toml.
        #[arg(long = "on-unparsable", value_enum)]
        on_unparsable: Option<CliSkipLevel>,
        /// How to report valid statements the AST does not model.
        /// Overrides `[parse] unsupported`.
        #[arg(long = "on-unsupported", value_enum)]
        on_unsupported: Option<CliSkipLevel>,
        /// Ignore pgweave.toml: do not index the project, use only
        /// `--schema` and the target files themselves.
        #[arg(long = "no-project")]
        no_project: bool,
    },

    /// Describe named queries (parameters, columns, nullability,
    /// cardinality, dependencies) as JSON descriptors (PLAN.md §4).
    ///
    /// Each file is a named-query file (`-- name: …` headers). The schema is
    /// the project's (pgweave.toml) plus any `--schema` files. Exits 1 if a
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
        /// Ignore pgweave.toml
        #[arg(long = "no-project")]
        no_project: bool,
        /// Also write each descriptor to `.pgweave/queries/<fingerprint>.json`
        #[arg(long)]
        cache: bool,
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

    /// Generate TypeScript interfaces from SQL functions and named queries.
    Codegen {
        /// SQL input file
        #[arg(long)]
        input: String,
        /// TypeScript output file (default: stdout)
        #[arg(long)]
        output: Option<String>,
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
    /// Compute a migration SQL diff between two schema files.
    Diff {
        /// Old schema SQL file
        old: String,
        /// New schema SQL file
        new: String,
    },

    /// Create a migration by comparing schema/ against .schema-snapshots/.
    ///
    /// Reads all .sql files from --schema-dir (lexicographic order), diffs
    /// against the last snapshot, writes a timestamped migration file, and
    /// updates the snapshots. Dangerous ops (DROP TABLE, DROP COLUMN) are
    /// commented out — uncomment only after review.
    Create {
        /// Migration name (prefixed with YYYYMMDD-HHMMSS_)
        name: String,
        /// Directory containing canonical CREATE TABLE files
        #[arg(long, default_value = "schema")]
        schema_dir: String,
        /// Directory where migration files are written
        #[arg(long, default_value = "migrations")]
        migrations_dir: String,
        /// Directory for schema snapshots
        #[arg(long, default_value = ".schema-snapshots")]
        snapshots_dir: String,
    },
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
    on_unparsable: Option<pgweave_schema::SkipLevel>,
    on_unsupported: Option<pgweave_schema::SkipLevel>,
    no_project: bool,
) -> i32 {
    // ── Découverte du projet ───────────────────────────────────────────────────
    // Même mécanisme que le LSP : on remonte jusqu'à pgweave.toml. Sans ça,
    // l'éditeur et la CI ne voient pas le même schéma pour un même fichier —
    // l'un indexe tout le projet, l'autre exige des --schema explicites, et
    // les diagnostics divergent.
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

    let registry = pgweave_plugin::registry::PluginRegistry::new();
    let builtin_plugins = project
        .as_ref()
        .map(|(_, c)| c.builtin_plugins())
        .unwrap_or_default();
    if let Err(e) = registry.check_names(builtin_plugins.keys()) {
        eprintln!("error: {}: {e}", pgweave_schema::ProjectConfig::FILENAME);
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
        match pgweave_parser::parse(&sql) {
            Ok(s) => context_files.push((sql, s)),
            Err(e) => eprintln!("warning: failed to parse schema '{}': {}", path, e),
        }
    }
    let context_stmts: Vec<Statement> = context_files
        .iter()
        .flat_map(|(_, s)| s.iter().cloned())
        .collect();

    // ── Politique de tolérance du parser ───────────────────────────────────────
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
    let analysis_opts = pgweave_analysis::AnalysisOptions {
        parse: parse_cfg.clone(),
    };

    // Macros : fournies + celles du projet. Le désucrage est tenté sur tout
    // fichier — une source sans macro est recopiée à l'identique.
    let (macros, macro_errors) = load_macros(project.as_ref());
    for e in &macro_errors {
        eprintln!("warning: {e}");
    }

    // ── Parse target files ─────────────────────────────────────────────────────
    // `parse_with_outcome` conserve la trace des statements écartés au lieu de
    // les perdre : c'est ce qui permet de les signaler plus bas.
    type Target = (
        String,
        String,
        Vec<Statement>,
        Vec<pgweave_parser::SkippedStatement>,
        Vec<pgweave_diagnostics::Diagnostic>,
    );
    let parsed_targets: Vec<Target> = {
        // (path, source, stmts, skipped, diagnostics de macro)
        let mut out = Vec::new();
        for file in files {
            let sql = match read_file(file) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("error: {}", e);
                    return 1;
                }
            };
            // Désucrage : les spans rendus par le parse sont déjà dans le
            // repère du fichier édité, donc rien à retraduire ensuite.
            let prepared = pgweave_analysis::prepare_source(&sql, &macros);
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

    // ── Catalogue et vérification ──────────────────────────────────────────────
    // Un seul rejeu : schémas `--schema`, fichiers du projet (même
    // construction que le LSP), puis les cibles hors projet. Chaque statement
    // y est vérifié quand il s'applique, contre l'état du catalogue à ce
    // point ; un corps PL/pgSQL l'est contre l'état final (`pgweave-sema`).
    let canonical = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    let is_query_file = |file: &str| {
        project
            .as_ref()
            .is_some_and(|(root, cfg)| cfg.is_query_file(Path::new(file), root))
    };
    let mut builder = pgweave_catalog::project::ProjectBuilder::new()
        .with_typer(Some(pgweave_sema::ViewTyper::shared()))
        .with_checker(Some(pgweave_sema::check::SemaChecker::shared()));
    let mut replayed: std::collections::HashSet<std::path::PathBuf> =
        std::collections::HashSet::new();
    for (path, (sql, stmts)) in schemas.iter().zip(&context_files) {
        let outcome = pgweave_parser::ParseOutcome {
            statements: stmts.clone(),
            skipped: Vec::new(),
        };
        let path = canonical(Path::new(path));
        builder.add_file(&path, sql, &pgweave_schema::FileRole::Schema, Ok(&outcome));
        replayed.insert(path);
    }
    if let Some((root, cfg)) = &project {
        let source = cfg.to_source(root);
        for chunk in pgweave_schema::SchemaSource::chunks(&source) {
            let parsed = pgweave_catalog::project::parse_source(
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
        let outcome = pgweave_parser::ParseOutcome {
            statements: stmts.clone(),
            skipped: Vec::new(),
        };
        builder.add_file(&path, sql, &pgweave_schema::FileRole::Schema, Ok(&outcome));
        replayed.insert(path);
    }
    let catalog = std::sync::Arc::new(builder.finish());

    // Un statement illisible dans un fichier du scope retire ses objets du
    // catalogue, ce qui produit ensuite des `does not exist` qui sont des faux
    // positifs. On le dit plutôt que de laisser croire à un catalogue complet.
    if parse_cfg.unparsable != pgweave_schema::SkipLevel::Ignore {
        for (path, sk) in &catalog.skipped {
            eprintln!(
                "warning: {}: statement non indexé, les objets qu'il définit \
                 seront introuvables ({:?})",
                path.display(),
                sk.reason
            );
        }
    }

    // Règles des plugins : elles voient le schéma par `SchemaProvider`.
    let has_external_schema = !schemas.is_empty();
    let mut composite = pgweave_schema::provider::CompositeSchemaProvider::new();
    composite.add(Box::new(SharedCatalog(catalog.clone())));
    for provider in registry.collect_schema_providers(&builtin_plugins) {
        composite.add(provider);
    }

    let mut checker = pgweave_checker::Checker::default();
    for rule in registry.collect_rules(&builtin_plugins) {
        checker.add_rule(rule);
    }
    // Plugins WASM listés dans [plugins] wasm, relatifs au fichier de config.
    let wasm_paths: Vec<std::path::PathBuf> = project
        .as_ref()
        .map(|(root, c)| c.plugins.wasm.iter().map(|p| root.join(p)).collect())
        .unwrap_or_default();
    for wasm_path in &wasm_paths {
        match pgweave_plugin_wasm::WasmPlugin::from_file(wasm_path) {
            Ok(plugin) => {
                let rule = pgweave_plugin_wasm::WasmCheckRule::new(std::sync::Arc::new(plugin));
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

    // Surcharges de sévérité de [rules], checker comme lint.
    let rule_overrides = project
        .as_ref()
        .map(|(_, c)| c.rule_overrides())
        .unwrap_or_default();

    for (file, sql, stmts, skipped, macro_diags) in &parsed_targets {
        // Contexte des règles de plugins : les triggers des autres fichiers.
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

        // Vérification sémantique : un fichier de requêtes s'exécute sur le
        // schéma final ; un fichier rejoué porte ses diagnostics du rejeu.
        let this_file = canonical(Path::new(file));
        if is_query_file(file) {
            diagnostics.extend(pgweave_checker::check_statements(stmts, &catalog));
            diagnostics.extend(pgweave_analysis::query_descriptor_diagnostics(
                sql, &catalog,
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

        // Statements écartés par le parser → diagnostics selon la config.
        let mut all = macro_diags.clone();
        // Fichier de requêtes nommées : son format est vérifié en plus.
        if let Some((root, cfg)) = &project
            && cfg.is_query_file(Path::new(file), root)
        {
            all.extend(pgweave_analysis::query_file_diagnostics(
                Path::new(file),
                sql,
            ));
        }
        all.extend(pgweave_analysis::skipped_to_diagnostics(
            skipped,
            &analysis_opts.parse,
        ));
        all.append(&mut diagnostics);
        let mut diagnostics = all;

        pgweave_diagnostics::apply_rule_overrides(&mut diagnostics, &rule_overrides);

        // Directives en commentaire (-- pgweave-disable-next-line, …).
        pgweave_diagnostics::Suppressions::scan(sql).apply(&mut diagnostics);

        for diag in &diagnostics {
            use pgweave_diagnostics::Severity;
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

    let stmts = match pgweave_parser::parse(&sql) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: parse error: {}", e);
            return 1;
        }
    };

    let config = pgweave_lint::LintConfig::default();
    let mut diagnostics = pgweave_lint::lint_statements(&stmts, &config).diagnostics;

    // Mêmes surcharges de sévérité que `check` et le LSP : une règle mise à
    // "off" dans [rules] doit être muette partout.
    if let Some((_, project)) = discover_project_or_exit() {
        pgweave_diagnostics::apply_rule_overrides(&mut diagnostics, &project.rule_overrides());
    }

    // Mêmes directives en commentaire que `check` : une règle tue là-bas doit
    // l'être ici aussi, sinon `-- pgweave-disable-next-line L001` marcherait dans
    // l'éditeur et pas en CLI.
    pgweave_diagnostics::Suppressions::scan(&sql).apply(&mut diagnostics);

    let mut has_errors = false;
    for diag in &diagnostics {
        use pgweave_diagnostics::Severity;
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

fn cmd_migrate_diff(old: &str, new: &str) -> i32 {
    let old_sql = match read_file(old) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {}", e);
            return 1;
        }
    };
    let new_sql = match read_file(new) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {}", e);
            return 1;
        }
    };

    let old_stmts = match pgweave_parser::parse(&old_sql) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: failed to parse '{}': {}", old, e);
            return 1;
        }
    };
    let new_stmts = match pgweave_parser::parse(&new_sql) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: failed to parse '{}': {}", new, e);
            return 1;
        }
    };

    let diff = pgweave_codegen::diff_schemas(&old_stmts, &new_stmts);
    let sql_output = diff.to_sql();

    let safety_diags = pgweave_codegen::check_migration_safety(&diff);
    if !safety_diags.is_empty() {
        eprintln!("\nMigration safety warnings:");
        for d in &safety_diags {
            eprintln!("  [{}] {}", d.code, d.message);
            if let Some(h) = &d.hint {
                eprintln!("  hint: {}", h);
            }
        }
        eprintln!();
    }

    if sql_output.is_empty() {
        println!("-- no changes");
    } else {
        println!("{}", sql_output);
    }

    0
}

fn cmd_emit(file: &str) -> i32 {
    let sql = match read_file(file) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {}", e);
            return 1;
        }
    };

    let stmts = match pgweave_parser::parse(&sql) {
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

fn cmd_codegen(input: &str, output: Option<&str>) -> i32 {
    let sql = match read_file(input) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {}", e);
            return 1;
        }
    };

    let ts = pgweave_codegen::generate_ts_types_from_sql(&sql);

    match output {
        Some(path) => {
            if let Err(e) = std::fs::write(path, &ts) {
                eprintln!("error: cannot write '{}': {}", path, e);
                return 1;
            }
            eprintln!("written: {}", path);
        }
        None => print!("{}", ts),
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
        } => describe::run(describe::Options {
            files: &files,
            sql: sql.as_deref(),
            name: &name,
            schemas: &schemas,
            no_project,
            cache,
        }),
        Command::Macros { verbose } => cmd_macros(verbose),
        Command::Build { files, out_dir } => cmd_build(&files, out_dir.as_deref()),
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
            MigrateAction::Diff { old, new } => cmd_migrate_diff(&old, &new),
            MigrateAction::Create {
                name,
                schema_dir,
                migrations_dir,
                snapshots_dir,
            } => migrate::cmd_migrate_create(
                &name,
                Path::new(&schema_dir),
                Path::new(&migrations_dir),
                Path::new(&snapshots_dir),
            ),
        },
        Command::Emit { file } => cmd_emit(&file),
        Command::Codegen { input, output } => cmd_codegen(&input, output.as_deref()),
    };

    process::exit(exit_code);
}

// ── Macros ────────────────────────────────────────────────────────────────────

/// Jeu de macros du projet : celles fournies, plus celles de `[macros] directory`.
///
/// Les erreurs de définition sont rapportées mais ne bloquent pas : les autres
/// macros restent utilisables.
fn load_macros(
    project: Option<&(std::path::PathBuf, pgweave_schema::ProjectConfig)>,
) -> (pgweave_macro::MacroSet, Vec<String>) {
    let mut set = pgweave_macro::MacroSet::builtin();
    let mut errors = Vec::new();

    if let Some((root, config)) = project {
        let dir = root.join(&config.macros.directory);
        if dir.is_dir() {
            errors = set.load_dir(&dir);
        }
    }
    (set, errors)
}

/// Projet découvert en remontant depuis le répertoire courant.
///
/// `Ok(None)` : pas de `pgweave.toml`. `Err` : un fichier existe mais est
/// invalide — à signaler, jamais à remplacer par la config par défaut.
fn discover_project() -> Result<Option<(std::path::PathBuf, pgweave_schema::ProjectConfig)>, String>
{
    let cwd = std::path::Path::new(".")
        .canonicalize()
        .map_err(|e| format!("cannot resolve current directory: {e}"))?;
    pgweave_schema::ProjectConfig::find_and_load(&cwd).map_err(|e| e.to_string())
}

/// Comme [`discover_project`], pour les commandes qui sortent en erreur
/// plutôt que de continuer sur une config invalide.
fn discover_project_or_exit() -> Option<(std::path::PathBuf, pgweave_schema::ProjectConfig)> {
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

    println!("{} macros disponibles :\n", set.len());
    for name in set.names() {
        let def = set.get(name).expect("nom listé");
        let origin = if def.origin.starts_with("<builtin>") {
            "fournie".to_string()
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
                println!("      trous   : {}", rendered.join(", "));
            }
            if !def.flags.is_empty() {
                let f: Vec<&str> = def.flags.iter().map(|f| f.keyword.as_str()).collect();
                println!("      drapeaux: {}", f.join(", "));
            }
            if !def.options.is_empty() {
                let o: Vec<String> = def
                    .options
                    .iter()
                    .map(|o| match &o.default {
                        Some(d) => format!("{} (défaut {d})", o.keyword),
                        None => o.keyword.clone(),
                    })
                    .collect();
                println!("      options : {}", o.join(", "));
            }
            println!();
        }
    }

    if !verbose {
        println!("\n`pgweave macros --verbose` détaille chaque macro.");
        println!("Pour en ajouter : un `*.pgmacro` dans le répertoire `[macros] directory`.");
    }
    if errors.is_empty() { 0 } else { 1 }
}

fn cmd_build(files: &[String], out_dir: Option<&str>) -> i32 {
    let project = discover_project_or_exit();
    let (set, errors) = load_macros(project.as_ref());
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
        let expansion = pgweave_macro::expand_source(&src, &set);

        for d in &expansion.diagnostics {
            use pgweave_diagnostics::Severity;
            let sev = match d.severity {
                Severity::Error => {
                    failed = true;
                    "error"
                }
                Severity::Warning => "warning",
                Severity::Info => "info",
            };
            // Les spans des diagnostics de macro sont déjà dans le repère source.
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
                        "{} → {} ({} macro(s) désucrée(s))",
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

/// Catalogue partagé vu comme un `SchemaProvider` (règles de plugins).
struct SharedCatalog(std::sync::Arc<pgweave_catalog::ProjectCatalog>);

impl pgweave_schema::provider::SchemaProvider for SharedCatalog {
    fn get_table(&self, name: &str) -> Option<&pgweave_schema::provider::TableSchema> {
        self.0.get_table(name)
    }

    fn get_function(&self, name: &str) -> Option<&pgweave_schema::provider::FunctionSignature> {
        self.0.get_function(name)
    }

    fn all_table_names(&self) -> Vec<&str> {
        self.0.all_table_names()
    }

    fn as_rls_aware(&self) -> Option<&dyn pgweave_schema::provider::RlsAwareSchemaProvider> {
        self.0.as_rls_aware()
    }
}
