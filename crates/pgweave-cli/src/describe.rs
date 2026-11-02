//! `pgweave describe` : descripteurs des requêtes nommées (PLAN.md §4), en
//! JSON sur la sortie standard ; diagnostics sur la sortie d'erreur.

use std::path::{Path, PathBuf};

use pgweave_catalog::project::{ProjectBuilder, parse_source};
use pgweave_diagnostics::Diagnostic;
use pgweave_query::Descriptor;

pub struct Options<'a> {
    pub files: &'a [String],
    pub sql: Option<&'a str>,
    pub name: &'a str,
    pub schemas: &'a [String],
    pub no_project: bool,
    /// Écrit chaque descripteur dans le cache du projet
    /// (`.pgweave/queries/<empreinte>.json`).
    pub cache: bool,
    /// Affiche le code Rust généré (comme `pgweave-build`) au lieu du JSON.
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

    // Catalogue : schémas `--schema`, puis le projet, rejoués dans l'ordre.
    let mut builder = ProjectBuilder::new().with_typer(Some(pgweave_sema::ViewTyper::shared()));
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
            &pgweave_schema::FileRole::Schema,
            parsed.as_ref(),
        );
    }
    if let Some((root, cfg)) = &project {
        let source = cfg.to_source(root);
        for chunk in pgweave_schema::SchemaSource::chunks(&source) {
            let parsed = parse_source(&chunk.sql, chunk.origin.byte_range.start, &macros);
            builder.add_file(&chunk.origin.path, &chunk.sql, &chunk.role, parsed.as_ref());
        }
    }
    let catalog = builder.finish();

    let mut descriptors: Vec<Descriptor> = Vec::new();
    let mut failed = false;
    if let Some(sql) = o.sql {
        match pgweave_query::describe_sql(&catalog, o.name, sql) {
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
        let parsed = pgweave_query::parse_query_file(&text);
        if !parsed.diagnostics.is_empty() {
            failed |= parsed
                .diagnostics
                .iter()
                .any(|d| d.severity == pgweave_diagnostics::Severity::Error);
            report(file, &text, &parsed.diagnostics);
        }
        for q in &parsed.queries {
            match pgweave_query::describe_named(&catalog, q) {
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
            if let Err(e) = pgweave_query::descriptor::write_cache(&dir, d) {
                eprintln!("error: {}: {e}", dir.display());
                return 1;
            }
        }
    }

    if o.rust {
        match pgweave_codegen::rust::generate_rust(
            &descriptors,
            &catalog,
            &pgweave_codegen::rust::RustOptions::default(),
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

/// `.pgweave/queries` à la racine du projet, sinon dans le répertoire
/// courant.
fn cache_dir(root: Option<&Path>) -> PathBuf {
    root.unwrap_or(Path::new("."))
        .join(".pgweave")
        .join("queries")
}

fn report(file: &str, text: &str, diags: &[Diagnostic]) {
    for d in diags {
        let (line, col) = d
            .span
            .as_ref()
            .map_or((1, 1), |s| crate::offset_to_line_col(text, s.start));
        let sev = match d.severity {
            pgweave_diagnostics::Severity::Error => "error",
            pgweave_diagnostics::Severity::Warning => "warning",
            pgweave_diagnostics::Severity::Info => "info",
        };
        eprintln!("{file}:{line}:{col}: {sev}[{}]: {}", d.code, d.message);
        if let Some(h) = &d.hint {
            eprintln!("  hint: {h}");
        }
    }
}
