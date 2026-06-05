/// L004 — NOT IN (subquery) is used.
///
/// `NOT IN (SELECT ...)` returns no rows if the subquery contains any NULL,
/// which is almost never the intended behaviour. Use `NOT EXISTS (...)` instead.
use pgsql_ast::expr::{BinaryOp, Expr};
use pgsql_ast::stmt::{InsertSource, SelectStmt, Statement};
use pgsql_checker::diagnostic::Diagnostic;

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
        Statement::Ddl(_) => {}
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
    match expr {
        Expr::InSubquery { negated: true, .. } => {
            let msg = "NOT IN (subquery) can silently return no rows when the subquery contains NULL";
            let d = match config.rules.no_not_in_subquery {
                RuleLevel::Error => Diagnostic::error(CODE, msg),
                _ => Diagnostic::warning(CODE, msg),
            }
            .with_hint("Replace `NOT IN (SELECT ...)` with `NOT EXISTS (SELECT 1 FROM ... WHERE ...)`.");
            out.push(d);
        }
        // Also catch the BinaryOp NOT IN form (some ASTs use this)
        Expr::BinaryOp { op: BinaryOp::NotIn, right, .. } => {
            if matches!(right.as_ref(), Expr::Subquery(_)) {
                let msg = "NOT IN (subquery) can silently return no rows when the subquery contains NULL";
                let d = match config.rules.no_not_in_subquery {
                    RuleLevel::Error => Diagnostic::error(CODE, msg),
                    _ => Diagnostic::warning(CODE, msg),
                }
                .with_hint("Replace `NOT IN (SELECT ...)` with `NOT EXISTS (SELECT 1 FROM ... WHERE ...)`.");
                out.push(d);
            }
            visit_expr(right, config, out);
        }
        Expr::BinaryOp { left, right, .. } => {
            visit_expr(left, config, out);
            visit_expr(right, config, out);
        }
        Expr::UnaryOp { expr, .. } => visit_expr(expr, config, out),
        Expr::FunctionCall { args, filter, .. } => {
            for a in args {
                visit_expr(a, config, out);
            }
            if let Some(f) = filter {
                visit_expr(f, config, out);
            }
        }
        Expr::Cast { expr, .. } => visit_expr(expr, config, out),
        Expr::Case { operand, when_clauses, else_clause } => {
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
        Expr::Subquery(sel) => visit_select(sel, config, out),
        Expr::InSubquery { expr, subquery, .. } => {
            visit_expr(expr, config, out);
            visit_select(subquery, config, out);
        }
        Expr::InList { expr, list, .. } => {
            visit_expr(expr, config, out);
            for e in list {
                visit_expr(e, config, out);
            }
        }
        Expr::Between { expr, low, high, .. } => {
            visit_expr(expr, config, out);
            visit_expr(low, config, out);
            visit_expr(high, config, out);
        }
        Expr::Literal(_) | Expr::Column(_) | Expr::Wildcard | Expr::QualifiedWildcard(_) => {}
    }
}
