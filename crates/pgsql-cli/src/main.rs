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
        if i >= offset { break; }
        if ch == '\n' { line += 1; col = 1; } else { col += 1; }
    }
    (line, col)
}

fn cmd_check(files: &[String], schemas: &[String]) -> i32 {
    let config = pgsql_plugin::config::PgsqlConfig::load(
        &std::path::PathBuf::from("pgsql.toml")
    ).unwrap_or_default();

    let registry = pgsql_plugin::registry::PluginRegistry::new();

    // ── Parse context/schema files ─────────────────────────────────────────────
    // Each file is kept with its raw SQL so we can detect its default schema.
    let mut context_files: Vec<(String, Vec<Statement>)> = Vec::new(); // (sql, stmts)
    for path in schemas {
        let sql = match read_file(path) {
            Ok(s) => s,
            Err(e) => { eprintln!("error: {}", e); return 1; }
        };
        match pgsql_parser::parse(&sql) {
            Ok(s) => context_files.push((sql, s)),
            Err(e) => eprintln!("warning: failed to parse schema '{}': {}", path, e),
        }
    }
    let context_stmts: Vec<Statement> = context_files.iter()
        .flat_map(|(_, s)| s.iter().cloned())
        .collect();

    // ── Parse target files ─────────────────────────────────────────────────────
    let parsed_targets: Vec<(String, String, Vec<Statement>)> = { // (path, sql, stmts)
        let mut out = Vec::new();
        for file in files {
            let sql = match read_file(file) {
                Ok(s) => s,
                Err(e) => { eprintln!("error: {}", e); return 1; }
            };
            match pgsql_parser::parse(&sql) {
                Ok(s) => out.push((file.clone(), sql, s)),
                Err(e) => {
                    eprintln!("error: {}: parse error: {}", file, e);
                    return 1;
                }
            }
        }
        out
    };

    // ── Build global schema (per-file schema detection) ────────────────────────
    let has_external_schema = !schemas.is_empty();
    let mut composite = pgsql_checker::CompositeSchemaProvider::new();

    if has_external_schema {
        // Explicit --schema files are added first (highest priority).
        for (sql, stmts) in &context_files {
            let sp = pgsql_checker::detect_search_path(sql);
            let sp_refs: Vec<&str> = sp.iter().map(String::as_str).collect();
            composite.add(Box::new(
                pgsql_checker::DdlSchemaProvider::from_statements_with_search_path(stmts, &sp_refs)
            ));
        }
    }
    // Always include target files' own definitions so intra-file references
    // (e.g. a function defined later in the same file) resolve correctly.
    for (_, sql, stmts) in &parsed_targets {
        let sp = pgsql_checker::detect_search_path(sql);
        let sp_refs: Vec<&str> = sp.iter().map(String::as_str).collect();
        composite.add(Box::new(
            pgsql_checker::DdlSchemaProvider::from_statements_with_search_path(stmts, &sp_refs)
        ));
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
    let rule_overrides: std::collections::HashMap<String, String> = config.rules.iter()
        .map(|(k, v)| (k.clone(), v.level().to_string()))
        .collect();

    for (file, sql, stmts) in &parsed_targets {
        let mut diagnostics = if has_external_schema || parsed_targets.len() > 1 {
            // Cross-file mode: trigger map built from context + ALL target files.
            let all_other_stmts: Vec<Statement> = context_stmts.iter()
                .chain(parsed_targets.iter().filter(|(f, _, _)| f != file).flat_map(|(_, _, s)| s.iter()))
                .cloned()
                .collect();
            checker.run_with_context(stmts, &all_other_stmts, &composite)
        } else {
            // Single file, no schema → self-contained mode with per-file schema detection.
            let sp = pgsql_checker::detect_search_path(sql);
            let sp_refs: Vec<&str> = sp.iter().map(String::as_str).collect();
            pgsql_checker::check_statements(
                stmts,
                &pgsql_checker::DdlSchemaProvider::from_statements_with_search_path(stmts, &sp_refs),
            )
        };

        pgsql_checker::apply_rule_overrides(&mut diagnostics, &rule_overrides);

        for diag in &diagnostics {
            use pgsql_checker::Severity;
            let sev_str = match diag.severity {
                Severity::Error => { has_errors = true; "error" }
                Severity::Warning => "warning",
                Severity::Info => "info",
            };
            let (line, col) = diag.span.as_ref()
                .filter(|s| s.start > 0 || s.end > 0)
                .map(|s| offset_to_line_col(sql, s.start as usize))
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
        Err(e) => { eprintln!("error: {}", e); return 1; }
    };

    let stmts = match pgsql_parser::parse(&sql) {
        Ok(s) => s,
        Err(e) => { eprintln!("error: parse error: {}", e); return 1; }
    };

    let config = pgsql_lint::LintConfig::default();
    let result = pgsql_lint::lint_statements(&stmts, &config);

    let mut has_errors = false;
    for diag in &result.diagnostics {
        use pgsql_checker::Severity;
        let sev_str = match diag.severity {
            Severity::Error => { has_errors = true; "error" }
            Severity::Warning => "warning",
            Severity::Info => "info",
        };
        let (line, col) = diag.span.as_ref()
            .filter(|s| s.start > 0 || s.end > 0)
            .map(|s| offset_to_line_col(&sql, s.start as usize))
            .unwrap_or((1, 1));
        let location = format!("{}:{}", line, col);
        if let Some(hint) = &diag.hint {
            println!("{}: {}[{}]: {}", location, sev_str, diag.code, diag.message);
            println!("  hint: {}", hint);
        } else {
            println!("{}: {}[{}]: {}", location, sev_str, diag.code, diag.message);
        }
    }

    if result.diagnostics.is_empty() {
        println!("no issues found");
    }

    if has_errors { 1 } else { 0 }
}

fn cmd_migrate_diff(old: &str, new: &str) -> i32 {
    let old_sql = match read_file(old) {
        Ok(s) => s,
        Err(e) => { eprintln!("error: {}", e); return 1; }
    };
    let new_sql = match read_file(new) {
        Ok(s) => s,
        Err(e) => { eprintln!("error: {}", e); return 1; }
    };

    let old_stmts = match pgsql_parser::parse(&old_sql) {
        Ok(s) => s,
        Err(e) => { eprintln!("error: failed to parse '{}': {}", old, e); return 1; }
    };
    let new_stmts = match pgsql_parser::parse(&new_sql) {
        Ok(s) => s,
        Err(e) => { eprintln!("error: failed to parse '{}': {}", new, e); return 1; }
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
        Err(e) => { eprintln!("error: {}", e); return 1; }
    };

    let stmts = match pgsql_parser::parse(&sql) {
        Ok(s) => s,
        Err(e) => { eprintln!("error: parse error: {}", e); return 1; }
    };

    for stmt in &stmts {
        println!("{:#?}", stmt);
    }

    0
}

fn cmd_codegen(input: &str, output: Option<&str>) -> i32 {
    let sql = match read_file(input) {
        Ok(s) => s,
        Err(e) => { eprintln!("error: {}", e); return 1; }
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
        Command::Check { files, schemas } => cmd_check(&files, &schemas),
        Command::Lint { file } => cmd_lint(&file),
        Command::Migrate { action } => match action {
            MigrateAction::Diff { old, new } => cmd_migrate_diff(&old, &new),
            MigrateAction::Create { name, schema_dir, migrations_dir, snapshots_dir } => {
                migrate::cmd_migrate_create(
                    &name,
                    Path::new(&schema_dir),
                    Path::new(&migrations_dir),
                    Path::new(&snapshots_dir),
                )
            }
        },
        Command::Emit { file } => cmd_emit(&file),
        Command::Codegen { input, output } => cmd_codegen(&input, output.as_deref()),
    };

    process::exit(exit_code);
}
