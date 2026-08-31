pub mod config;
pub mod linter;
pub mod rules;

pub use config::{LintConfig, RuleLevel, RulesConfig};
pub use linter::{LintResult, lint_sql, lint_statements};
