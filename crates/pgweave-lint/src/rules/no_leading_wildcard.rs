/// L006 — LIKE/ILIKE pattern with a leading wildcard.
///
/// LIKE '%foo' cannot use a standard B-tree index and causes a full table scan.
/// Use a full-text index (GIN + tsvector) or a trigram index (pg_trgm) instead.
use pgweave_ast::expr::{BinaryOp, Expr, Literal};
use pgweave_ast::stmt::{InsertSource, SelectStmt, Statement};
use pgweave_diagnostics::Diagnostic;

use crate::config::{LintConfig, RuleLevel};
use pgweave_ast::expr::ExprKind;

pub const CODE: &str = "L006";

pub fn check(stmt: &Statement, config: &LintConfig) -> Vec<Diagnostic> {
    if config.rules.no_leading_wildcard == RuleLevel::Off {
        return vec![];
    }
    let mut diags = Vec::new();
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
            for assign in &upd.assignments {
                visit_expr(&assign.value, config, out);
            }
            if let Some(w) = &upd.where_clause {
                visit_expr(w, config, out);
            }
        }
        Statement::Delete(del) => {
            if let Some(w) = &del.where_clause {
                visit_expr(w, config, out);
            }
        }
        Statement::Ddl(_) => {}
    }
}

fn visit_select(sel: &SelectStmt, config: &LintConfig, out: &mut Vec<Diagnostic>) {
    for cte in &sel.with {
        visit_select(&cte.query, config, out);
    }
    if let Some(w) = &sel.where_clause {
        visit_expr(w, config, out);
    }
    if let Some(h) = &sel.having {
        visit_expr(h, config, out);
    }
}

fn visit_expr(expr: &Expr, config: &LintConfig, out: &mut Vec<Diagnostic>) {
    match &expr.kind {
        ExprKind::BinaryOp {
            op: BinaryOp::Like | BinaryOp::NotLike | BinaryOp::ILike,
            right,
            left,
        } => {
            if let ExprKind::Literal(Literal::String(pattern)) = &right.kind
                && pattern.starts_with('%')
            {
                let msg = format!(
                    "LIKE pattern '{}' has a leading wildcard — index cannot be used",
                    pattern
                );
                let d = match config.rules.no_leading_wildcard {
                        RuleLevel::Error => Diagnostic::error(CODE, msg),
                        _ => Diagnostic::warning(CODE, msg),
                    }
                    .with_hint("Use a GIN index with pg_trgm (USING gin with operator class gin_trgm_ops) for contains/suffix searches.");
                out.push(d);
            }
            // Recurse into left side too
            visit_expr(left, config, out);
        }
        _ => visit_expr_children(expr, config, out),
    }
}

fn visit_expr_children(expr: &Expr, config: &LintConfig, out: &mut Vec<Diagnostic>) {
    match &expr.kind {
        ExprKind::BinaryOp { left, right, .. } => {
            visit_expr(left, config, out);
            visit_expr(right, config, out);
        }
        ExprKind::UnaryOp { expr, .. } => visit_expr(expr, config, out),
        ExprKind::Cast { expr, .. } => visit_expr(expr, config, out),
        ExprKind::FunctionCall { args, .. } => {
            for a in args {
                visit_expr(a, config, out);
            }
        }
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
            if let Some(el) = else_clause {
                visit_expr(el, config, out);
            }
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
        ExprKind::Subquery(sub) => visit_select(sub, config, out),
        ExprKind::InSubquery { expr, subquery, .. } => {
            visit_expr(expr, config, out);
            visit_select(subquery, config, out);
        }
        _ => {}
    }
}
