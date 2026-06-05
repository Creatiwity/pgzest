/// L001 — SELECT * used outside a CTE definition.
///
/// SELECT * makes schema changes silently break downstream consumers.
/// Prefer explicitly listing columns.
use pgsql_ast::stmt::{SelectItem, SelectStmt, Statement};
use pgsql_checker::diagnostic::Diagnostic;

use crate::config::{LintConfig, RuleLevel};

pub const CODE: &str = "L001";

pub fn check(stmt: &Statement, config: &LintConfig) -> Vec<Diagnostic> {
    if config.rules.no_select_star == RuleLevel::Off {
        return vec![];
    }

    match stmt {
        Statement::Select(sel) => check_select(sel, false, config),
        _ => vec![],
    }
}

fn check_select(sel: &SelectStmt, inside_cte: bool, config: &LintConfig) -> Vec<Diagnostic> {
    let mut diags = vec![];

    // Check CTE bodies — wildcards inside a CTE are acceptable per convention.
    // We still recurse into them but mark them as `inside_cte = true`.
    for cte in &sel.with {
        diags.extend(check_select(&cte.query, true, config));
    }

    if !inside_cte {
        for col in &sel.columns {
            if matches!(col, SelectItem::Wildcard | SelectItem::QualifiedWildcard(_)) {
                let msg = "SELECT * is discouraged; list columns explicitly";
                let d = match config.rules.no_select_star {
                    RuleLevel::Error => Diagnostic::error(CODE, msg),
                    _ => Diagnostic::warning(CODE, msg),
                }
                .with_hint("Replace * with an explicit column list to avoid silent breakage when the schema changes.");
                diags.push(d);
            }
        }
    }

    diags
}
