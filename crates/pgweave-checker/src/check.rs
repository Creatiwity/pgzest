//! Vérification d'un script par l'analyse sémantique (`pgweave-sema`).

use pgweave_ast::Statement;
use pgweave_catalog::ProjectCatalog;
use pgweave_catalog::project::ProjectBuilder;
use pgweave_diagnostics::{Diagnostic, codes};
use pgweave_parser::ParseOutcome;
use pgweave_schema::FileRole;

/// Parse `sql`, rejoue son DDL et vérifie chaque statement au moment où il
/// s'applique (un corps PL/pgSQL, sur l'état final) : le mode d'un fichier
/// vérifié sans schéma externe.
pub fn check_sql_self_contained(sql: &str) -> Vec<Diagnostic> {
    match pgweave_parser::parse(sql) {
        Ok(stmts) => check_script(&stmts),
        Err(e) => vec![Diagnostic::error(
            codes::PARSE_ERROR,
            format!("parse error: {e}"),
        )],
    }
}

/// Statements rejoués sur le catalogue de base, chacun vérifié à son
/// application (voir [`check_sql_self_contained`]).
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

/// Parse `sql` et vérifie ses statements contre `catalog`, sans rien y
/// appliquer : le mode d'un fichier de requêtes.
pub fn check_sql(sql: &str, catalog: &ProjectCatalog) -> Vec<Diagnostic> {
    match pgweave_parser::parse(sql) {
        Ok(stmts) => check_statements(&stmts, catalog),
        Err(e) => vec![Diagnostic::error(
            codes::PARSE_ERROR,
            format!("parse error: {e}"),
        )],
    }
}

/// Statements vérifiés contre l'état de `catalog`.
pub fn check_statements(stmts: &[Statement], catalog: &ProjectCatalog) -> Vec<Diagnostic> {
    pgweave_sema::check::check_statements(&catalog.catalog, &catalog.search_path, stmts)
}
