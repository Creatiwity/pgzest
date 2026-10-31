use pgweave_ast::stmt::Statement;
use pgweave_diagnostics::Diagnostic;

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
        use pgweave_diagnostics::Severity;
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
