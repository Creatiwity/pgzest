use pgsql_ast::stmt::Statement;
use pgsql_checker::diagnostic::Diagnostic;

use crate::config::LintConfig;
use crate::rules;

/// The result of linting one or more SQL statements.
#[derive(Debug, Default)]
pub struct LintResult {
    pub diagnostics: Vec<Diagnostic>,
}

impl LintResult {
    /// Returns `true` if any diagnostic has `Severity::Error`.
    pub fn has_errors(&self) -> bool {
        use pgsql_checker::diagnostic::Severity;
        self.diagnostics
            .iter()
            .any(|d| matches!(d.severity, Severity::Error))
    }
}

/// Lint a slice of already-parsed statements.
pub fn lint_statements(stmts: &[Statement], config: &LintConfig) -> LintResult {
    let mut diagnostics: Vec<Diagnostic> = stmts
        .iter()
        .flat_map(|stmt| rules::check_statement(stmt, config))
        .collect();
    // Cross-statement rules
    diagnostics.extend(rules::check_all_statements(stmts, config));
    LintResult { diagnostics }
}

/// Convenience entry-point: parse raw SQL then lint.
///
/// Returns a parse error diagnostic if the SQL cannot be parsed.
/// This is intentionally thin — callers that already have an AST should
/// prefer `lint_statements` to avoid double-parsing.
pub fn lint_sql(_sql: &str, _config: &LintConfig) -> LintResult {
    // NOTE: pgsql-parser is a sibling crate that depends on pgsql-ast.
    // We deliberately do NOT add pgsql-parser as a dependency of pgsql-lint
    // to keep the dependency graph acyclic (linter → ast, checker; not parser).
    // Callers that have the parser available should call:
    //
    //   let stmts = pgsql_parser::parse(sql)?;
    //   pgsql_lint::lint_statements(&stmts, config)
    //
    // This stub exists so the public API surface is stable and the signature
    // can be filled in once the workspace wiring is finalised.
    unimplemented!(
        "lint_sql requires a parser integration; use lint_statements with pre-parsed AST nodes"
    )
}
