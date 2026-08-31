pub mod add_column_not_null_no_default;
pub mod aggregate_without_group_by;
pub mod missing_fk_index;
pub mod naming;
pub mod no_leading_wildcard;
pub mod no_not_in;
pub mod no_select_star;
pub mod require_pk;
pub mod unused_join;

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
    diags.extend(no_leading_wildcard::check(stmt, config));
    diags.extend(aggregate_without_group_by::check(stmt, config));
    diags.extend(add_column_not_null_no_default::check(stmt, config));
    diags.extend(unused_join::check(stmt, config));
    diags
}

/// Run rules that need cross-statement context (e.g. FK index check).
pub fn check_all_statements(stmts: &[Statement], config: &LintConfig) -> Vec<Diagnostic> {
    missing_fk_index::check_all(stmts, config)
}
