pub mod naming;
pub mod no_not_in;
pub mod no_select_star;
pub mod require_pk;

use pgsql_ast::stmt::Statement;
use pgsql_checker::diagnostic::Diagnostic;

use crate::config::LintConfig;

/// Run all enabled rules against a single statement and collect diagnostics.
pub fn check_statement(stmt: &Statement, config: &LintConfig) -> Vec<Diagnostic> {
    let mut diags = vec![];
    diags.extend(no_select_star::check(stmt, config));
    diags.extend(require_pk::check(stmt, config));
    diags.extend(naming::check(stmt, config));
    diags.extend(no_not_in::check(stmt, config));
    diags
}
