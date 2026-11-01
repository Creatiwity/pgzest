/// L004 — NOT IN (subquery) is used.
///
/// `NOT IN (SELECT ...)` returns no rows if the subquery contains any NULL,
/// which is almost never the intended behaviour. Use `NOT EXISTS (...)` instead.
use pgweave_ast::expr::{BinaryOp, Expr, ExprKind};
use pgweave_ast::stmt::{InsertSource, SelectStmt, Statement};
use pgweave_diagnostics::Diagnostic;

use crate::config::{LintConfig, RuleLevel};

pub const CODE: &str = "L004";

pub fn check(stmt: &Statement, config: &LintConfig) -> Vec<Diagnostic> {
    if config.rules.no_not_in_subquery == RuleLevel::Off {
        return vec![];
    }

    let mut diags = vec![];
    visit_stmt(stmt, config, &mut diags);
    diags
}

fn visit_stmt(stmt: &Statement, config: &LintConfig, out: &mut Vec<Diagnostic>) {
    match stmt {
        Statement::Select(sel) => visit_select(sel, config, out),
        Statement::Insert(ins) => {
            if let InsertSource::Select(sel) = &ins.source {
                visit_select(sel, config, out);
            }
        }
        Statement::Update(upd) => {
            if let Some(ref w) = upd.where_clause {
                visit_expr(w, config, out);
            }
        }
        Statement::Delete(del) => {
            if let Some(ref w) = del.where_clause {
                visit_expr(w, config, out);
            }
        }
        Statement::Ddl(_) | Statement::Utility(_) => {}
    }
}

fn visit_select(sel: &SelectStmt, config: &LintConfig, out: &mut Vec<Diagnostic>) {
    // CTEs
    for cte in &sel.with {
        visit_select(&cte.query, config, out);
    }
    if let Some(ref w) = sel.where_clause {
        visit_expr(w, config, out);
    }
    if let Some(ref h) = sel.having {
        visit_expr(h, config, out);
    }
}

fn visit_expr(expr: &Expr, config: &LintConfig, out: &mut Vec<Diagnostic>) {
    match &expr.kind {
        ExprKind::InSubquery { negated: true, .. } => {
            let msg =
                "NOT IN (subquery) can silently return no rows when the subquery contains NULL";
            let d = match config.rules.no_not_in_subquery {
                RuleLevel::Error => Diagnostic::error(CODE, msg),
                _ => Diagnostic::warning(CODE, msg),
            }
            .with_hint(
                "Replace `NOT IN (SELECT ...)` with `NOT EXISTS (SELECT 1 FROM ... WHERE ...)`.",
            );
            out.push(d);
        }
        // Also catch the BinaryOp NOT IN form (some ASTs use this)
        ExprKind::BinaryOp {
            op: BinaryOp::NotIn,
            right,
            ..
        } => {
            if matches!(&right.kind, ExprKind::Subquery(_)) {
                let msg =
                    "NOT IN (subquery) can silently return no rows when the subquery contains NULL";
                let d = match config.rules.no_not_in_subquery {
                    RuleLevel::Error => Diagnostic::error(CODE, msg),
                    _ => Diagnostic::warning(CODE, msg),
                }
                .with_hint("Replace `NOT IN (SELECT ...)` with `NOT EXISTS (SELECT 1 FROM ... WHERE ...)`.");
                out.push(d);
            }
            visit_expr(right, config, out);
        }
        ExprKind::BinaryOp { left, right, .. } | ExprKind::Quantified { left, right, .. } => {
            visit_expr(left, config, out);
            visit_expr(right, config, out);
        }
        ExprKind::UnaryOp { expr, .. } => visit_expr(expr, config, out),
        ExprKind::FunctionCall { args, filter, .. } => {
            for a in args {
                visit_expr(a, config, out);
            }
            if let Some(f) = filter {
                visit_expr(f, config, out);
            }
        }
        ExprKind::Cast { expr, .. } => visit_expr(expr, config, out),
        ExprKind::Case {
            operand,
            when_clauses,
            else_clause,
        } => {
            if let Some(op) = operand {
                visit_expr(op, config, out);
            }
            for (w, t) in when_clauses {
                visit_expr(w, config, out);
                visit_expr(t, config, out);
            }
            if let Some(e) = else_clause {
                visit_expr(e, config, out);
            }
        }
        ExprKind::Subquery(sel) => visit_select(sel, config, out),
        ExprKind::InSubquery { expr, subquery, .. } => {
            visit_expr(expr, config, out);
            visit_select(subquery, config, out);
        }
        ExprKind::InList { expr, list, .. } => {
            visit_expr(expr, config, out);
            for e in list {
                visit_expr(e, config, out);
            }
        }
        ExprKind::Between {
            expr, low, high, ..
        } => {
            visit_expr(expr, config, out);
            visit_expr(low, config, out);
            visit_expr(high, config, out);
        }
        // Descendre dans les nouvelles variantes : un `NOT IN` peut se cacher
        // dans la sous-requête d'un `EXISTS` ou dans un constructeur de ligne.
        ExprKind::Exists { subquery, .. } => visit_select(subquery, config, out),
        ExprKind::AtTimeZone {
            timestamp,
            time_zone,
        } => {
            visit_expr(timestamp, config, out);
            visit_expr(time_zone, config, out);
        }
        ExprKind::Collate { expr, .. } => visit_expr(expr, config, out),
        ExprKind::Tuple(items) => {
            for e in items {
                visit_expr(e, config, out);
            }
        }
        ExprKind::FieldAccess { root, .. } => visit_expr(root, config, out),

        ExprKind::Literal(_)
        | ExprKind::Column(_)
        | ExprKind::Wildcard
        | ExprKind::QualifiedWildcard(_)
        | ExprKind::Interval(_)
        | ExprKind::Parameter(_)
        | ExprKind::Unsupported(_) => {}
    }
}
