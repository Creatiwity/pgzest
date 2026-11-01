/// L009 — JOIN table that is never referenced in SELECT, WHERE, or ORDER BY.
///
/// An unused JOIN wastes I/O and can produce unexpected row multiplication.
/// Remove the JOIN or verify that it was intended.
use std::collections::HashSet;

use pgweave_ast::expr::Expr;
use pgweave_ast::stmt::{JoinConstraint, SelectItem, SelectStmt, Statement};
use pgweave_diagnostics::Diagnostic;

use crate::config::{LintConfig, RuleLevel};
use pgweave_ast::expr::ExprKind;

pub const CODE: &str = "L009";

pub fn check(stmt: &Statement, config: &LintConfig) -> Vec<Diagnostic> {
    if config.rules.unused_join == RuleLevel::Off {
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

    if sel.joins.is_empty() {
        return;
    }

    // Collect referenced table aliases from SELECT columns, WHERE, ORDER BY, HAVING, GROUP BY
    let mut referenced: HashSet<String> = HashSet::new();
    for item in &sel.columns {
        if let SelectItem::Expr { expr, .. } = item {
            collect_table_refs(expr, &mut referenced);
        }
        if let SelectItem::QualifiedWildcard(tbl) = item {
            referenced.insert(tbl.value.to_lowercase());
        }
    }
    if let Some(w) = &sel.where_clause {
        collect_table_refs(w, &mut referenced);
    }
    if let Some(h) = &sel.having {
        collect_table_refs(h, &mut referenced);
    }
    for o in &sel.order_by {
        collect_table_refs(&o.expr, &mut referenced);
    }
    for g in &sel.group_by {
        collect_table_refs(g, &mut referenced);
    }

    // Check each JOIN
    for join in &sel.joins {
        let join_alias = join
            .table
            .visible_name()
            .map(str::to_lowercase)
            .unwrap_or_default();

        // Also collect refs from ON condition before evaluating
        match &join.constraint {
            JoinConstraint::On(on_expr) => {
                collect_table_refs(on_expr, &mut referenced);
            }
            JoinConstraint::Using(cols) => {
                // USING columns don't carry a table prefix, so nothing to collect here
                let _ = cols;
            }
            JoinConstraint::Natural | JoinConstraint::None => {}
        }

        if !join_alias.is_empty() && !referenced.contains(&join_alias) {
            let msg = format!(
                "JOIN on '{}' appears unused — no column from this table is referenced",
                join_alias
            );
            let d = match config.rules.unused_join {
                RuleLevel::Error => Diagnostic::error(CODE, msg),
                _ => Diagnostic::warning(CODE, msg),
            }
            .with_hint("Remove the JOIN if unneeded, or reference a column from the joined table.");
            out.push(d);
        }
    }
}

fn collect_table_refs(expr: &Expr, out: &mut HashSet<String>) {
    match &expr.kind {
        ExprKind::Column(col_ref) => {
            if let Some(tbl) = &col_ref.table {
                out.insert(tbl.value.to_lowercase());
            }
        }
        ExprKind::BinaryOp { left, right, .. } | ExprKind::Quantified { left, right, .. } => {
            collect_table_refs(left, out);
            collect_table_refs(right, out);
        }
        ExprKind::UnaryOp { expr, .. } => collect_table_refs(expr, out),
        ExprKind::Cast { expr, .. } => collect_table_refs(expr, out),
        ExprKind::FunctionCall { args, .. } => {
            for a in args {
                collect_table_refs(a, out);
            }
        }
        ExprKind::Case {
            operand,
            when_clauses,
            else_clause,
        } => {
            if let Some(op) = operand {
                collect_table_refs(op, out);
            }
            for (w, t) in when_clauses {
                collect_table_refs(w, out);
                collect_table_refs(t, out);
            }
            if let Some(el) = else_clause {
                collect_table_refs(el, out);
            }
        }
        ExprKind::InList { expr, list, .. } => {
            collect_table_refs(expr, out);
            for e in list {
                collect_table_refs(e, out);
            }
        }
        ExprKind::Between {
            expr, low, high, ..
        } => {
            collect_table_refs(expr, out);
            collect_table_refs(low, out);
            collect_table_refs(high, out);
        }
        ExprKind::Subquery(sub) => collect_select_refs(sub, out),
        ExprKind::InSubquery { expr, subquery, .. } => {
            collect_table_refs(expr, out);
            collect_select_refs(subquery, out);
        }
        _ => {}
    }
}

fn collect_select_refs(sel: &SelectStmt, out: &mut HashSet<String>) {
    if let Some(w) = &sel.where_clause {
        collect_table_refs(w, out);
    }
}
