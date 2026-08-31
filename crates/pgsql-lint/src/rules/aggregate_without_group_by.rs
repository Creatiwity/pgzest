/// L007 — Aggregate function mixed with bare column references without GROUP BY.
///
/// SELECT count(*), user_id FROM tasks  -- missing GROUP BY user_id
/// This is invalid SQL in standard mode and causes errors in strict PostgreSQL.
use pgsql_ast::expr::Expr;
use pgsql_ast::stmt::{SelectItem, SelectStmt, Statement};
use pgsql_diagnostics::Diagnostic;

use crate::config::{LintConfig, RuleLevel};
use pgsql_ast::expr::ExprKind;

pub const CODE: &str = "L007";

const AGGREGATE_FUNCTIONS: &[&str] = &[
    "count",
    "sum",
    "avg",
    "min",
    "max",
    "bool_and",
    "bool_or",
    "bit_and",
    "bit_or",
    "string_agg",
    "array_agg",
    "jsonb_agg",
    "json_agg",
    "first",
    "last",
    "percentile_cont",
    "percentile_disc",
];

pub fn check(stmt: &Statement, config: &LintConfig) -> Vec<Diagnostic> {
    if config.rules.aggregate_without_group_by == RuleLevel::Off {
        return vec![];
    }
    let mut diags = Vec::new();
    if let Statement::Select(sel) = stmt {
        check_select(sel, config, &mut diags);
    }
    diags
}

fn check_select(sel: &SelectStmt, config: &LintConfig, out: &mut Vec<Diagnostic>) {
    for cte in &sel.with {
        check_select(&cte.query, config, out);
    }

    // Only flag when GROUP BY is absent
    if !sel.group_by.is_empty() {
        return;
    }

    let has_agg = sel.columns.iter().any(|item| {
        if let SelectItem::Expr { expr, .. } = item {
            contains_aggregate(expr)
        } else {
            false
        }
    });

    let has_bare_col = sel.columns.iter().any(|item| match item {
        SelectItem::Expr { expr, .. } => is_bare_column(expr),
        SelectItem::Wildcard => true,
        SelectItem::QualifiedWildcard(_) => false,
    });

    if has_agg && has_bare_col {
        let msg = "Aggregate function used alongside bare column reference without GROUP BY";
        let d = match config.rules.aggregate_without_group_by {
            RuleLevel::Error => Diagnostic::error(CODE, msg),
            _ => Diagnostic::warning(CODE, msg),
        }
        .with_hint("Add a GROUP BY clause listing all non-aggregated columns, or wrap the column in an aggregate function.");
        out.push(d);
    }
}

fn contains_aggregate(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::FunctionCall { name, .. } => {
            let fname = name
                .0
                .last()
                .map(|i| i.value.to_lowercase())
                .unwrap_or_default();
            AGGREGATE_FUNCTIONS.contains(&fname.as_str())
        }
        ExprKind::BinaryOp { left, right, .. } => {
            contains_aggregate(left) || contains_aggregate(right)
        }
        ExprKind::Cast { expr, .. } => contains_aggregate(expr),
        ExprKind::Case {
            when_clauses,
            else_clause,
            ..
        } => {
            when_clauses
                .iter()
                .any(|(w, t)| contains_aggregate(w) || contains_aggregate(t))
                || else_clause
                    .as_ref()
                    .map(|e| contains_aggregate(e))
                    .unwrap_or(false)
        }
        _ => false,
    }
}

fn is_bare_column(expr: &Expr) -> bool {
    matches!(&expr.kind, ExprKind::Column(_))
}
