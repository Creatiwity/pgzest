use crate::{
    DdlStatement, Statement,
    ddl::{CreateFunction, CreateIndex, CreatePolicy, CreateProcedure, CreateTable, CreateTrigger},
    expr::{Expr, ExprKind},
    stmt::{DeleteStmt, InsertStmt, SelectItem, SelectStmt, UpdateStmt},
};

// ── Read-only visitor ─────────────────────────────────────────────────────────

/// Read-only AST visitor. Override only the methods relevant to your use case.
/// The default implementations do nothing (no-op).
///
/// For rules that need the full semantic context, use SemanticCtx from pgsql-semantic.
/// This trait is intentionally simple: it provides structural traversal, not type info.
pub trait Visit {
    // Statement level
    fn visit_statement(&mut self, stmt: &Statement) {
        walk_statement(self, stmt);
    }
    fn visit_select(&mut self, node: &SelectStmt) {
        walk_select(self, node);
    }
    fn visit_insert(&mut self, _node: &InsertStmt) {}
    fn visit_update(&mut self, _node: &UpdateStmt) {}
    fn visit_delete(&mut self, _node: &DeleteStmt) {}

    // DDL
    fn visit_create_table(&mut self, _node: &CreateTable) {}
    fn visit_create_function(&mut self, _node: &CreateFunction) {}
    fn visit_create_procedure(&mut self, _node: &CreateProcedure) {}
    fn visit_create_policy(&mut self, _node: &CreatePolicy) {}
    fn visit_create_index(&mut self, _node: &CreateIndex) {}
    fn visit_create_trigger(&mut self, _node: &CreateTrigger) {}

    // Expressions
    fn visit_expr(&mut self, expr: &Expr) {
        walk_expr(self, expr);
    }
}

// ── Mutable visitor (for transformations like normalize) ───────────────────────

pub trait VisitMut {
    fn enter_statement(&mut self, _stmt: &mut Statement) {}
    fn exit_statement(&mut self, _stmt: &mut Statement) {}
    fn enter_select(&mut self, _node: &mut SelectStmt) {}
    fn exit_select(&mut self, _node: &mut SelectStmt) {}
    fn enter_expr(&mut self, _expr: &mut Expr) {}
    fn exit_expr(&mut self, _expr: &mut Expr) {}
    fn enter_create_table(&mut self, _node: &mut CreateTable) {}
    fn exit_create_table(&mut self, _node: &mut CreateTable) {}
}

// ── Walk helpers (call visitor methods + recurse) ─────────────────────────────

pub fn walk_statements<V: Visit + ?Sized>(visitor: &mut V, stmts: &[Statement]) {
    for stmt in stmts {
        visitor.visit_statement(stmt);
    }
}

pub fn walk_statement<V: Visit + ?Sized>(visitor: &mut V, stmt: &Statement) {
    match stmt {
        Statement::Select(s) => visitor.visit_select(s),
        Statement::Insert(s) => visitor.visit_insert(s),
        Statement::Update(s) => visitor.visit_update(s),
        Statement::Delete(s) => visitor.visit_delete(s),
        Statement::Ddl(ddl) => match ddl {
            DdlStatement::CreateTable(n) => visitor.visit_create_table(n),
            DdlStatement::CreateFunction(n) => visitor.visit_create_function(n),
            DdlStatement::CreateProcedure(n) => visitor.visit_create_procedure(n),
            DdlStatement::CreatePolicy(n) => visitor.visit_create_policy(n),
            DdlStatement::CreateIndex(n) => visitor.visit_create_index(n),
            DdlStatement::CreateTrigger(n) => visitor.visit_create_trigger(n),
            _ => {}
        },
    }
}

pub fn walk_select<V: Visit + ?Sized>(visitor: &mut V, node: &SelectStmt) {
    for item in &node.columns {
        if let SelectItem::Expr { expr, .. } = item {
            visitor.visit_expr(expr);
        }
    }
    if let Some(w) = &node.where_clause {
        visitor.visit_expr(w);
    }
    for e in &node.group_by {
        visitor.visit_expr(e);
    }
    if let Some(h) = &node.having {
        visitor.visit_expr(h);
    }
    for o in &node.order_by {
        visitor.visit_expr(&o.expr);
    }
}

pub fn walk_expr<V: Visit + ?Sized>(visitor: &mut V, expr: &Expr) {
    match &expr.kind {
        ExprKind::BinaryOp { left, right, .. } => {
            visitor.visit_expr(left);
            visitor.visit_expr(right);
        }
        ExprKind::UnaryOp { expr, .. } => visitor.visit_expr(expr),
        ExprKind::Cast { expr, .. } => visitor.visit_expr(expr),
        ExprKind::FunctionCall { args, .. } => {
            for arg in args {
                visitor.visit_expr(arg);
            }
        }
        ExprKind::Case {
            operand,
            when_clauses,
            else_clause,
        } => {
            if let Some(op) = operand {
                visitor.visit_expr(op);
            }
            for (w, t) in when_clauses {
                visitor.visit_expr(w);
                visitor.visit_expr(t);
            }
            if let Some(el) = else_clause {
                visitor.visit_expr(el);
            }
        }
        ExprKind::Subquery(sub) => visitor.visit_select(sub),
        ExprKind::InSubquery { expr, subquery, .. } => {
            visitor.visit_expr(expr);
            visitor.visit_select(subquery);
        }
        ExprKind::InList { expr, list, .. } => {
            visitor.visit_expr(expr);
            for e in list {
                visitor.visit_expr(e);
            }
        }
        ExprKind::Between {
            expr, low, high, ..
        } => {
            visitor.visit_expr(expr);
            visitor.visit_expr(low);
            visitor.visit_expr(high);
        }
        ExprKind::Column(_)
        | ExprKind::Literal(_)
        | ExprKind::Wildcard
        | ExprKind::QualifiedWildcard(_) => {}
    }
}
