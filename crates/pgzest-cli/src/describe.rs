//! `pgzest describe`: named query descriptors (PLAN.md §4), as JSON on
//! standard output; diagnostics on standard error.

use std::path::{Path, PathBuf};

use pgzest_catalog::project::{ProjectBuilder, parse_source};
use pgzest_diagnostics::Diagnostic;
use pgzest_query::Descriptor;

pub struct Options<'a> {
    pub files: &'a [String],
    pub sql: Option<&'a str>,
    pub name: &'a str,
    pub schemas: &'a [String],
    pub no_project: bool,
    /// Writes each descriptor to the project cache
    /// (`.pgzest/queries/<fingerprint>.json`).
    pub cache: bool,
    /// Prints the generated Rust code (like `pgzest-build`) instead of the JSON.
    pub rust: bool,
}

pub fn run(o: Options) -> i32 {
    let project = if o.no_project {
        None
    } else {
        match crate::discover_project() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("error: {e}");
                return 2;
            }
        }
    };
    let (macros, macro_errors) = crate::load_macros(project.as_ref());
    for e in &macro_errors {
        eprintln!("warning: {e}");
    }
    let packs = match crate::load_packs(project.as_ref()) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            return 2;
        }
    };

    // Catalog: `--schema` schemas, then the project, replayed in order.
    let mut builder = ProjectBuilder::new()
        .with_typer(Some(pgzest_sema::ViewTyper::shared()))
        .with_instrumentation(macros.instrumentation())
        .declarative(project.as_ref().is_some_and(|(_, c)| c.schema.declarative))
        .with_packs(packs);
    for path in o.schemas {
        let sql = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("error: cannot read '{path}': {e}");
                return 1;
            }
        };
        let parsed = parse_source(&sql, 0, &macros);
        builder.add_file(
            Path::new(path),
            &sql,
            &pgzest_schema::FileRole::Schema,
            parsed.as_ref(),
        );
    }
    if let Some((root, cfg)) = &project {
        let source = cfg.to_source(root);
        for chunk in pgzest_schema::SchemaSource::chunks(&source) {
            let parsed = parse_source(&chunk.sql, chunk.origin.byte_range.start, &macros);
            builder.add_file(&chunk.origin.path, &chunk.sql, &chunk.role, parsed.as_ref());
        }
    }
    let catalog = builder.finish();

    let mut descriptors: Vec<Descriptor> = Vec::new();
    let mut failed = false;
    if let Some(sql) = o.sql {
        match pgzest_query::describe_sql_with(&catalog, o.name, sql, &macros) {
            Ok(d) => descriptors.push(d),
            Err(diags) => {
                failed = true;
                report("<sql>", sql, &diags);
            }
        }
    }
    for file in o.files {
        let text = match std::fs::read_to_string(file) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("error: cannot read '{file}': {e}");
                return 1;
            }
        };
        let parsed = pgzest_query::parse_query_file_with(&text, &macros);
        if !parsed.diagnostics.is_empty() {
            failed |= parsed
                .diagnostics
                .iter()
                .any(|d| d.severity == pgzest_diagnostics::Severity::Error);
            report(file, &text, &parsed.diagnostics);
        }
        for q in &parsed.queries {
            match pgzest_query::describe_named(&catalog, q) {
                Ok(d) => descriptors.push(d),
                Err(diags) => {
                    failed = true;
                    report(file, &text, &diags);
                }
            }
        }
    }

    if o.cache {
        let dir = cache_dir(project.as_ref().map(|(root, _)| root.as_path()));
        for d in &descriptors {
            if let Err(e) = pgzest_query::descriptor::write_cache(&dir, d) {
                eprintln!("error: {}: {e}", dir.display());
                return 1;
            }
        }
    }

    if o.rust {
        match pgzest_codegen::rust::generate_rust(
            &descriptors,
            &catalog,
            &pgzest_codegen::rust::RustOptions::default(),
        ) {
            Ok(code) => print!("{code}"),
            Err(errors) => {
                for e in errors {
                    eprintln!("error: {e}");
                }
                return 1;
            }
        }
        return if failed { 1 } else { 0 };
    }
    match serde_json::to_string_pretty(&descriptors) {
        Ok(json) => println!("{json}"),
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    }
    if failed { 1 } else { 0 }
}

/// `.pgzest/queries` at the project root, otherwise in the current
/// directory.
fn cache_dir(root: Option<&Path>) -> PathBuf {
    root.unwrap_or(Path::new("."))
        .join(".pgzest")
        .join("queries")
}

fn report(file: &str, text: &str, diags: &[Diagnostic]) {
    for d in diags {
        let (line, col) = d
            .span
            .as_ref()
            .map_or((1, 1), |s| crate::offset_to_line_col(text, s.start));
        let sev = match d.severity {
            pgzest_diagnostics::Severity::Error => "error",
            pgzest_diagnostics::Severity::Warning => "warning",
            pgzest_diagnostics::Severity::Info => "info",
        };
        eprintln!("{file}:{line}:{col}: {sev}[{}]: {}", d.code, d.message);
        if let Some(h) = &d.hint {
            eprintln!("  hint: {h}");
        }
    }
}
