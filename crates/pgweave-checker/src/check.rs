//! Checking a script with semantic analysis (`pgweave-sema`).

use pgweave_ast::Statement;
use pgweave_catalog::ProjectCatalog;
use pgweave_catalog::project::ProjectBuilder;
use pgweave_diagnostics::{Diagnostic, codes};
use pgweave_parser::ParseOutcome;
use pgweave_schema::FileRole;

/// Parses `sql`, replays its DDL and checks each statement at the point where
/// it applies (a PL/pgSQL body, on the final state): the mode for a file
/// checked without an external schema.
pub fn check_sql_self_contained(sql: &str) -> Vec<Diagnostic> {
    match pgweave_parser::parse(sql) {
        Ok(stmts) => check_script(&stmts),
        Err(e) => vec![Diagnostic::error(
            codes::PARSE_ERROR,
            format!("parse error: {e}"),
        )],
    }
}

/// Statements replayed on the base catalog, each checked when it is
/// applied (see [`check_sql_self_contained`]).
pub fn check_script(stmts: &[Statement]) -> Vec<Diagnostic> {
    let mut b = ProjectBuilder::new()
        .with_typer(Some(pgweave_sema::ViewTyper::shared()))
        .with_checker(Some(pgweave_sema::check::SemaChecker::shared()));
    let outcome = ParseOutcome {
        statements: stmts.to_vec(),
        skipped: Vec::new(),
    };
    b.add_file(
        std::path::Path::new(""),
        "",
        &FileRole::Schema,
        Ok(&outcome),
    );
    b.finish().diagnostics.into_iter().map(|(_, d)| d).collect()
}

/// Parses `sql` and checks its statements against `catalog`, without
/// applying anything to it: the mode for a query file.
pub fn check_sql(sql: &str, catalog: &ProjectCatalog) -> Vec<Diagnostic> {
    match pgweave_parser::parse(sql) {
        Ok(stmts) => check_statements(&stmts, catalog),
        Err(e) => vec![Diagnostic::error(
            codes::PARSE_ERROR,
            format!("parse error: {e}"),
        )],
    }
}

/// Statements checked against the state of `catalog`.
pub fn check_statements(stmts: &[Statement], catalog: &ProjectCatalog) -> Vec<Diagnostic> {
    pgweave_sema::check::check_statements(&catalog.catalog, &catalog.search_path, stmts)
}
