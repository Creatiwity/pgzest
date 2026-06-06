use std::fs;
use std::path::Path;
use std::process;

use clap::{Parser, Subcommand};

mod migrate;

#[derive(Parser)]
#[command(name = "pgsql", about = "PostgreSQL language toolchain", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Type-check a SQL file, printing diagnostics. Exits 1 if any errors are found.
    Check {
        /// SQL file to check
        file: String,
        /// Optional schema DDL file for cross-file type checking
        #[arg(long)]
        schema: Option<String>,
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

fn cmd_check(file: &str, schema: Option<&str>) -> i32 {
    let sql = match read_file(file) {
        Ok(s) => s,
        Err(e) => { eprintln!("error: {}", e); return 1; }
    };

    let config = pgsql_plugin::config::PgsqlConfig::load(
        &std::path::PathBuf::from("pgsql.toml")
    ).unwrap_or_default();

    let registry = pgsql_plugin::registry::PluginRegistry::new();
    let mut composite = pgsql_checker::CompositeSchemaProvider::new();

    if let Some(path) = schema {
        let schema_sql = match read_file(path) {
            Ok(s) => s,
            Err(e) => { eprintln!("error: {}", e); return 1; }
        };
        match pgsql_checker::DdlSchemaProvider::from_sql(&schema_sql) {
            Ok(p) => composite.add(Box::new(p)),
            Err(e) => {
                eprintln!("error: failed to load schema '{}': {}", path, e);
                return 1;
            }
        }
    } else {
        let stmts = pgsql_parser::parse(&sql).unwrap_or_default();
        composite.add(Box::new(pgsql_checker::DdlSchemaProvider::from_statements(&stmts)));
    }

    for provider in registry.collect_schema_providers(&config.plugins) {
        composite.add(provider);
    }

    let mut checker = pgsql_checker::Checker::with_builtin_rules();
    for rule in registry.collect_rules(&config.plugins) {
        checker.add_rule(rule);
    }

    let diagnostics = if schema.is_some() {
        let stmts = match pgsql_parser::parse(&sql) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("error: parse error: {}", e);
                return 1;
            }
        };
        checker.run(&stmts, &composite)
    } else {
        pgsql_checker::check_sql_self_contained(&sql)
    };

    let mut has_errors = false;
    for diag in &diagnostics {
        use pgsql_checker::Severity;
        let severity = match diag.severity {
            Severity::Error => { has_errors = true; "error" }
            Severity::Warning => "warning",
            Severity::Info => "info",
        };
        if let Some(hint) = &diag.hint {
            println!("{}: {}: {} (hint: {})", severity, diag.code, diag.message, hint);
        } else {
            println!("{}: {}: {}", severity, diag.code, diag.message);
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
        let severity = match diag.severity {
            Severity::Error => { has_errors = true; "error" }
            Severity::Warning => "warning",
            Severity::Info => "info",
        };
        if let Some(hint) = &diag.hint {
            println!("{}: {}: {} (hint: {})", severity, diag.code, diag.message, hint);
        } else {
            println!("{}: {}: {}", severity, diag.code, diag.message);
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
        Command::Check { file, schema } => cmd_check(&file, schema.as_deref()),
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
