use std::fs;
use std::path::Path;
use std::process;

use clap::{Parser, Subcommand};
use pgsql_ast::Statement;

mod migrate;

#[derive(Parser)]
#[command(name = "pgsql", about = "PostgreSQL language toolchain", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// Miroir clap de `pgsql_schema::SkipLevel`, pour exposer les niveaux en CLI.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum CliSkipLevel {
    /// Ne rien signaler.
    Ignore,
    /// Signaler sans faire échouer.
    Warn,
    /// Signaler et faire échouer.
    Error,
}

impl From<CliSkipLevel> for pgsql_schema::SkipLevel {
    fn from(v: CliSkipLevel) -> Self {
        match v {
            CliSkipLevel::Ignore => pgsql_schema::SkipLevel::Ignore,
            CliSkipLevel::Warn => pgsql_schema::SkipLevel::Warn,
            CliSkipLevel::Error => pgsql_schema::SkipLevel::Error,
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
        /// Overrides `[parse] unparsable` from pgsql-tools.config.toml.
        #[arg(long = "on-unparsable", value_enum)]
        on_unparsable: Option<CliSkipLevel>,
        /// How to report valid statements the AST does not model.
        /// Overrides `[parse] unsupported`.
        #[arg(long = "on-unsupported", value_enum)]
        on_unsupported: Option<CliSkipLevel>,
        /// Ignore pgsql-tools.config.toml: do not index the project, use only
        /// `--schema` and the target files themselves.
        #[arg(long = "no-project")]
        no_project: bool,
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
    on_unparsable: Option<pgsql_schema::SkipLevel>,
    on_unsupported: Option<pgsql_schema::SkipLevel>,
    no_project: bool,
) -> i32 {
    let config = pgsql_plugin::config::PgsqlConfig::load(&std::path::PathBuf::from("pgsql.toml"))
        .unwrap_or_default();

    let registry = pgsql_plugin::registry::PluginRegistry::new();

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
        match pgsql_parser::parse(&sql) {
            Ok(s) => context_files.push((sql, s)),
            Err(e) => eprintln!("warning: failed to parse schema '{}': {}", path, e),
        }
    }
    let context_stmts: Vec<Statement> = context_files
        .iter()
        .flat_map(|(_, s)| s.iter().cloned())
        .collect();

    // ── Découverte du projet ───────────────────────────────────────────────────
    // Même mécanisme que le LSP : on remonte jusqu'à pgsql-tools.config.toml.
    // Sans ça, l'éditeur et la CI ne voient pas le même schéma pour un même
    // fichier — l'un indexe tout le projet, l'autre exige des --schema
    // explicites, et les diagnostics divergent.
    let project = if no_project {
        None
    } else {
        std::path::Path::new(".")
            .canonicalize()
            .ok()
            .and_then(|d| pgsql_schema::ProjectConfig::find_and_load(&d))
    };

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
    let analysis_opts = pgsql_analysis::AnalysisOptions {
        parse: parse_cfg.clone(),
    };

    // Macros : fournies + celles du projet. Le désucrage est tenté sur tout
    // fichier — une source sans macro est recopiée à l'identique.
    let (macros, macro_errors) = load_macros();
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
        Vec<pgsql_parser::SkippedStatement>,
        Vec<pgsql_diagnostics::Diagnostic>,
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
            let prepared = pgsql_analysis::prepare_source(&sql, &macros);
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

    // ── Build global schema (per-file schema detection) ────────────────────────
    // Index du projet : tables et fonctions de tous les fichiers du scope,
    // indexés sans être checkés. C'est la même construction que côté LSP.
    // `from_chunks_with_macros` et non `from_source` : l'index doit voir les
    // objets déclarés par les macros du PROJET, pas seulement par celles
    // fournies.
    let project_resolver = project.as_ref().map(|(root, cfg)| {
        let source = cfg.to_source(root);
        pgsql_schema::SchemaResolver::from_chunks_with_macros(
            &pgsql_schema::SchemaSource::chunks(&source),
            &macros,
        )
    });

    let has_external_schema = !schemas.is_empty();
    let has_project_schema = project_resolver.is_some();
    let mut composite = pgsql_schema::provider::CompositeSchemaProvider::new();

    if has_external_schema {
        // Explicit --schema files are added first (highest priority).
        for (sql, stmts) in &context_files {
            let sp = pgsql_schema::provider::detect_search_path(sql);
            let sp_refs: Vec<&str> = sp.iter().map(String::as_str).collect();
            composite.add(Box::new(
                pgsql_schema::provider::DdlSchemaProvider::from_statements_with_search_path(
                    stmts, &sp_refs,
                ),
            ));
        }
    }
    // Always include target files' own definitions so intra-file references
    // (e.g. a function defined later in the same file) resolve correctly.
    for (_, sql, stmts, _, _) in &parsed_targets {
        let sp = pgsql_schema::provider::detect_search_path(sql);
        let sp_refs: Vec<&str> = sp.iter().map(String::as_str).collect();
        composite.add(Box::new(
            pgsql_schema::provider::DdlSchemaProvider::from_statements_with_search_path(
                stmts, &sp_refs,
            ),
        ));
    }

    // Index du projet, après les sources explicites (CompositeSchemaProvider est
    // « premier trouvé gagne » : un --schema ou le fichier courant l'emporte).
    if let Some(resolver) = project_resolver {
        // Un statement illisible dans un fichier du scope retire sa table de
        // l'index, ce qui produit ensuite des `does not exist` qui sont des
        // faux positifs. On le dit plutôt que de laisser croire à un index complet.
        if !resolver.skipped.is_empty() && parse_cfg.unparsable != pgsql_schema::SkipLevel::Ignore {
            for (path, sk) in &resolver.skipped {
                eprintln!(
                    "warning: {}: statement non indexé, les objets qu'il définit \
                     seront introuvables ({:?})",
                    path.display(),
                    sk.reason
                );
            }
        }
        composite.add(Box::new(resolver));
    }

    for provider in registry.collect_schema_providers(&config.plugins) {
        composite.add(provider);
    }

    let mut checker = pgsql_checker::Checker::with_builtin_rules();
    for rule in registry.collect_rules(&config.plugins) {
        checker.add_rule(rule);
    }
    // Load external WASM plugins listed in pgsql.toml
    for wasm_path in &config.wasm_plugins {
        match pgsql_plugin_wasm::WasmPlugin::from_file(std::path::Path::new(wasm_path)) {
            Ok(plugin) => {
                let rule = pgsql_plugin_wasm::WasmCheckRule::new(std::sync::Arc::new(plugin));
                checker.add_rule(Box::new(rule));
            }
            Err(e) => eprintln!("warning: failed to load wasm plugin '{}': {}", wasm_path, e),
        }
    }

    // ── Check each target file ─────────────────────────────────────────────────
    let mut has_errors = false;
    let show_filename = parsed_targets.len() > 1 || has_external_schema;

    // Build rule overrides map from config.rules.
    let rule_overrides: std::collections::HashMap<String, String> = config
        .rules
        .iter()
        .map(|(k, v)| (k.clone(), v.level().to_string()))
        .collect();

    for (file, sql, stmts, skipped, macro_diags) in &parsed_targets {
        let mut diagnostics =
            if has_external_schema || has_project_schema || parsed_targets.len() > 1 {
                // Cross-file mode: trigger map built from context + ALL target files.
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
                checker.run_with_context(stmts, &all_other_stmts, &composite)
            } else {
                // Single file, no schema → self-contained mode with per-file schema detection.
                let sp = pgsql_schema::provider::detect_search_path(sql);
                let sp_refs: Vec<&str> = sp.iter().map(String::as_str).collect();
                pgsql_checker::check_statements(
                    stmts,
                    &pgsql_schema::provider::DdlSchemaProvider::from_statements_with_search_path(
                        stmts, &sp_refs,
                    ),
                )
            };

        // Statements écartés par le parser → diagnostics selon la config.
        let mut all = macro_diags.clone();
        all.extend(pgsql_analysis::skipped_to_diagnostics(
            skipped,
            &analysis_opts.parse,
        ));
        all.append(&mut diagnostics);
        let mut diagnostics = all;

        pgsql_diagnostics::apply_rule_overrides(&mut diagnostics, &rule_overrides);

        // Directives en commentaire (-- pgsql-disable-next-line, …).
        pgsql_diagnostics::Suppressions::scan(sql).apply(&mut diagnostics);

        for diag in &diagnostics {
            use pgsql_diagnostics::Severity;
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

    let stmts = match pgsql_parser::parse(&sql) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: parse error: {}", e);
            return 1;
        }
    };

    let config = pgsql_lint::LintConfig::default();
    let mut diagnostics = pgsql_lint::lint_statements(&stmts, &config).diagnostics;

    // Mêmes directives en commentaire que `check` : une règle tue là-bas doit
    // l'être ici aussi, sinon `-- pgsql-disable-next-line L001` marcherait dans
    // l'éditeur et pas en CLI.
    pgsql_diagnostics::Suppressions::scan(&sql).apply(&mut diagnostics);

    let mut has_errors = false;
    for diag in &diagnostics {
        use pgsql_diagnostics::Severity;
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

    let old_stmts = match pgsql_parser::parse(&old_sql) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: failed to parse '{}': {}", old, e);
            return 1;
        }
    };
    let new_stmts = match pgsql_parser::parse(&new_sql) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: failed to parse '{}': {}", new, e);
            return 1;
        }
    };

    let diff = pgsql_codegen::diff_schemas(&old_stmts, &new_stmts);
    let sql_output = diff.to_sql();

    let safety_diags = pgsql_codegen::check_migration_safety(&diff);
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

    let stmts = match pgsql_parser::parse(&sql) {
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

    let ts = pgsql_codegen::generate_ts_types_from_sql(&sql);

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
        Command::Macros { verbose } => cmd_macros(verbose),
        Command::Build { files, out_dir } => cmd_build(&files, out_dir.as_deref()),
        Command::Lint { file } => cmd_lint(&file),
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
fn load_macros() -> (pgsql_macro::MacroSet, Vec<String>) {
    let mut set = pgsql_macro::MacroSet::builtin();
    let mut errors = Vec::new();

    if let Some((root, config)) = std::path::Path::new(".")
        .canonicalize()
        .ok()
        .and_then(|d| pgsql_schema::ProjectConfig::find_and_load(&d))
    {
        let dir = root.join(&config.macros.directory);
        if dir.is_dir() {
            errors = set.load_dir(&dir);
        }
    }
    (set, errors)
}

fn cmd_macros(verbose: bool) -> i32 {
    let (set, errors) = load_macros();
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
        println!("\n`pgsql macros --verbose` détaille chaque macro.");
        println!("Pour en ajouter : un `*.pgmacro` dans le répertoire `[macros] directory`.");
    }
    if errors.is_empty() { 0 } else { 1 }
}

fn cmd_build(files: &[String], out_dir: Option<&str>) -> i32 {
    let (set, errors) = load_macros();
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
        let expansion = pgsql_macro::expand_source(&src, &set);

        for d in &expansion.diagnostics {
            use pgsql_diagnostics::Severity;
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
