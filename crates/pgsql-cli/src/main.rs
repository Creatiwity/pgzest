use std::env;
use std::fs;
use std::process;

fn print_usage() {
    eprintln!("Usage: pgsql <subcommand> [args]");
    eprintln!();
    eprintln!("Subcommands:");
    eprintln!("  check <file.sql> [--schema <schema.sql>]");
    eprintln!("      Type-check a SQL file, printing diagnostics.");
    eprintln!("      Exits 1 if any errors are found.");
    eprintln!();
    eprintln!("  lint <file.sql>");
    eprintln!("      Lint a SQL file for style and best-practice violations.");
    eprintln!();
    eprintln!("  migrate diff <old.sql> <new.sql>");
    eprintln!("      Compute a migration SQL diff between two schema files.");
    eprintln!();
    eprintln!("  emit <file.sql>");
    eprintln!("      Parse and re-emit the SQL (normalization round-trip).");
}

fn read_file(path: &str) -> Result<String, String> {
    fs::read_to_string(path).map_err(|e| format!("cannot read '{}': {}", path, e))
}

fn cmd_check(args: &[String]) -> i32 {
    if args.is_empty() {
        eprintln!("error: 'check' requires a <file.sql> argument");
        return 2;
    }

    let file = &args[0];
    let mut schema_file: Option<&str> = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--schema" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("error: --schema requires a path argument");
                    return 2;
                }
                schema_file = Some(&args[i]);
            }
            other => {
                eprintln!("error: unexpected argument '{}'", other);
                return 2;
            }
        }
        i += 1;
    }

    let sql = match read_file(file) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {}", e);
            return 1;
        }
    };

    let schema: Box<dyn pgsql_checker::SchemaProvider> = match schema_file {
        Some(path) => {
            let schema_sql = match read_file(path) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("error: {}", e);
                    return 1;
                }
            };
            match pgsql_checker::DdlSchemaProvider::from_sql(&schema_sql) {
                Ok(p) => Box::new(p),
                Err(e) => {
                    eprintln!("error: failed to load schema '{}': {}", path, e);
                    return 1;
                }
            }
        }
        None => Box::new(pgsql_checker::DdlSchemaProvider::from_statements(&[])),
    };

    let diagnostics = pgsql_checker::check_sql(&sql, schema.as_ref());

    let mut has_errors = false;
    for diag in &diagnostics {
        use pgsql_checker::Severity;
        let severity = match diag.severity {
            Severity::Error => {
                has_errors = true;
                "error"
            }
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

fn cmd_lint(args: &[String]) -> i32 {
    if args.is_empty() {
        eprintln!("error: 'lint' requires a <file.sql> argument");
        return 2;
    }

    let file = &args[0];
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
    let result = pgsql_lint::lint_statements(&stmts, &config);

    let mut has_errors = false;
    for diag in &result.diagnostics {
        use pgsql_checker::Severity;
        let severity = match diag.severity {
            Severity::Error => {
                has_errors = true;
                "error"
            }
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

fn cmd_migrate(args: &[String]) -> i32 {
    if args.is_empty() || args[0] != "diff" {
        eprintln!("error: 'migrate' only supports the 'diff' subcommand");
        eprintln!("Usage: pgsql migrate diff <old.sql> <new.sql>");
        return 2;
    }

    let rest = &args[1..];
    if rest.len() < 2 {
        eprintln!("error: 'migrate diff' requires <old.sql> and <new.sql>");
        return 2;
    }

    let old_sql = match read_file(&rest[0]) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {}", e);
            return 1;
        }
    };

    let new_sql = match read_file(&rest[1]) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {}", e);
            return 1;
        }
    };

    let _old_stmts = match pgsql_parser::parse(&old_sql) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: failed to parse '{}': {}", rest[0], e);
            return 1;
        }
    };

    let _new_stmts = match pgsql_parser::parse(&new_sql) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: failed to parse '{}': {}", rest[1], e);
            return 1;
        }
    };

    // pgsql-codegen is not yet implemented; emit a placeholder comment.
    // When pgsql-codegen::diff_schemas is available, replace this block with:
    //   let migration = pgsql_codegen::diff_schemas(&old_stmts, &new_stmts);
    //   print!("{}", migration);
    eprintln!("warning: migrate diff is not yet implemented (pgsql-codegen pending)");
    println!("-- migration diff: {} → {}", rest[0], rest[1]);
    println!("-- TODO: implement via pgsql-codegen::diff_schemas");

    0
}

fn cmd_emit(args: &[String]) -> i32 {
    if args.is_empty() {
        eprintln!("error: 'emit' requires a <file.sql> argument");
        return 2;
    }

    let file = &args[0];
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

fn main() {
    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        print_usage();
        process::exit(2);
    }

    let exit_code = match args[1].as_str() {
        "check" => cmd_check(&args[2..]),
        "lint" => cmd_lint(&args[2..]),
        "migrate" => cmd_migrate(&args[2..]),
        "emit" => cmd_emit(&args[2..]),
        "--help" | "-h" | "help" => {
            print_usage();
            0
        }
        other => {
            eprintln!("error: unknown subcommand '{}'", other);
            eprintln!();
            print_usage();
            2
        }
    };

    process::exit(exit_code);
}
