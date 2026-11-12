//! Checking the statements of a file through semantic analysis: what
//! PostgreSQL would refuse when executing the statement, or when compiling
//! a routine body.
//!
//! Two moments, as in PostgreSQL:
//!
//! - [`check_applied`]: what runs or is validated when the statement
//!   is applied — queries, views, policies, `COPY`, `DO` block,
//!   `LANGUAGE sql` bodies (validated at creation) — against the catalog
//!   state at that point;
//! - [`check_deferred`]: a PL/pgSQL body, whose queries are only
//!   analyzed at execution, hence against the final state.
//!
//! [`check_statements`] chains both against the same catalog.

use pgzest_diagnostics::codes;
use std::collections::BTreeMap;

use pgzest_ast::Span;
use pgzest_ast::ddl::{
    CreateFunction, CreatePolicy, DdlStatement, FunctionBody, FunctionParam, FunctionSetting,
    ParamMode, ReturnType, SettingValue,
};
use pgzest_ast::expr::{ColumnRef, Expr, Ident, ObjectName};
use pgzest_ast::plpgsql::{
    Block, Declaration, IntoTarget, PlStatement, PlStatementKind, PlpgsqlBody, VarType,
};
use pgzest_ast::stmt::{SelectStmt, Statement};
use pgzest_ast::utility::{CopySource, UtilityKind};
use pgzest_catalog::{Catalog, QName, SearchPath};
use pgzest_diagnostics::Diagnostic;

use crate::expr::{Analyzer, split_name};
use crate::routine::{Language, Routine, VarRef, VarTy};
use crate::scope::{Column, Nullability, Scope, SemaError, Source};
use crate::types::{Ty, TypeSystem, is_polymorphic, pg};

// ── Diagnostics ───────────────────────────────────────────────────────────────

/// Diagnostic of an analysis error; without its own position, it takes
/// that of `fallback` (the statement).
fn diagnostic(e: SemaError, fallback: Span) -> Diagnostic {
    let span = if e.span == Span::DUMMY {
        fallback
    } else {
        e.span
    };
    let mut d = match e.code {
        // What the analysis does not cover is stated, without being an error.
        codes::NOT_ANALYZED => Diagnostic::info(e.code, e.message),
        _ => Diagnostic::error(e.code, e.message),
    };
    d.hint = e.hint;
    if span == Span::DUMMY {
        d
    } else {
        d.with_span(pgzest_diagnostics::Span {
            start: span.start as usize,
            end: span.end as usize,
        })
    }
}

/// One diagnostic per error, attached to the statement failing anything better.
struct Sink<'o> {
    out: &'o mut Vec<Diagnostic>,
}

impl Sink<'_> {
    fn push(&mut self, r: Result<(), SemaError>, fallback: Span) {
        if let Err(e) = r {
            self.out.push(diagnostic(e, fallback));
        }
    }
}

// ── Entry points ──────────────────────────────────────────────────────────────

/// Checking during the catalog replay ([`StatementChecker`]).
///
/// [`StatementChecker`]: pgzest_catalog::StatementChecker
pub struct SemaChecker;

impl SemaChecker {
    pub fn shared() -> std::sync::Arc<dyn pgzest_catalog::StatementChecker> {
        std::sync::Arc::new(SemaChecker)
    }
}

impl pgzest_catalog::StatementChecker for SemaChecker {
    fn check_applied(
        &self,
        catalog: &Catalog,
        path: &SearchPath,
        stmt: &Statement,
        out: &mut Vec<Diagnostic>,
    ) {
        check_applied(catalog, path, stmt, out);
    }

    fn is_deferred(&self, stmt: &Statement) -> bool {
        is_deferred(stmt)
    }

    fn check_deferred(
        &self,
        catalog: &Catalog,
        path: &SearchPath,
        stmt: &Statement,
        out: &mut Vec<Diagnostic>,
    ) {
        check_deferred(catalog, path, stmt, out);
    }
}

/// All statements against the same catalog (query file,
/// standalone file, final state of a project).
pub fn check_statements(
    catalog: &Catalog,
    path: &SearchPath,
    stmts: &[Statement],
) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let mut path = path.clone();
    for stmt in stmts {
        if let Some(p) = search_path_change(stmt) {
            path = p;
        }
        check_applied(catalog, &path, stmt, &mut out);
        check_deferred(catalog, &path, stmt, &mut out);
    }
    out
}

/// `SET search_path`: the path of the following statements.
pub fn search_path_change(stmt: &Statement) -> Option<SearchPath> {
    let Statement::Utility(u) = stmt else {
        return None;
    };
    match &u.kind {
        UtilityKind::Set {
            name,
            value: pgzest_ast::ddl::SettingValue::Values(v),
            ..
        } if name.eq_ignore_ascii_case("search_path") => {
            Some(SearchPath::new(v.iter().filter(|s| *s != "$user").cloned()))
        }
        _ => None,
    }
}

/// Search path a routine body is analyzed with. A quoted body (`AS $$ … $$`,
/// PL/pgSQL or SQL) is validated and run with the routine's `SET
/// search_path`; a standard SQL body (`BEGIN ATOMIC`, `RETURN`) is resolved at
/// creation, with `path`. `FROM CURRENT` and `DEFAULT` leave `path`.
fn body_search_path(
    path: &SearchPath,
    settings: &[FunctionSetting],
    body: &FunctionBody,
) -> SearchPath {
    if matches!(body, FunctionBody::Atomic(_) | FunctionBody::SqlReturn(_)) {
        return path.clone();
    }
    // As in PostgreSQL, the last `SET` of a parameter wins.
    match settings
        .iter()
        .rev()
        .find(|s| s.name.eq_ignore_ascii_case("search_path"))
        .map(|s| &s.value)
    {
        Some(SettingValue::Values(v)) if !v.is_empty() => {
            // `''` names no schema: only `pg_catalog` (and `pg_temp`) remain.
            SearchPath::new(v.iter().filter(|s| *s != "$user" && !s.is_empty()).cloned())
        }
        _ => path.clone(),
    }
}

/// `SET LOCAL search_path` to `path`, for a body query prepared on its own.
fn set_local_search_path(path: &SearchPath) -> String {
    let schemas: Vec<String> = path
        .schemas()
        .iter()
        .map(|s| pgzest_catalog::model::quote_sql_ident(s))
        .collect();
    if schemas.is_empty() {
        "SET LOCAL search_path TO ''".into()
    } else {
        format!("SET LOCAL search_path TO {}", schemas.join(", "))
    }
}

/// The body of this statement is only analyzed by PostgreSQL at execution:
/// it is checked by [`check_deferred`].
pub fn is_deferred(stmt: &Statement) -> bool {
    match stmt {
        Statement::Ddl(DdlStatement::CreateFunction(f)) => {
            matches!(f.body, FunctionBody::Parsed(_))
        }
        Statement::Ddl(DdlStatement::CreateProcedure(p)) => {
            matches!(p.body, FunctionBody::Parsed(_))
        }
        _ => false,
    }
}

/// What PostgreSQL checks when applying `stmt`, against `catalog` (the
/// state after application for a DDL).
pub fn check_applied(
    catalog: &Catalog,
    path: &SearchPath,
    stmt: &Statement,
    out: &mut Vec<Diagnostic>,
) {
    let span = pgzest_ast::stmt_span(stmt).unwrap_or(Span::DUMMY);
    let mut sink = Sink { out };
    let mut a = Analyzer::new(TypeSystem::new(catalog), path.clone());
    match stmt {
        Statement::Select(_)
        | Statement::Insert(_)
        | Statement::Update(_)
        | Statement::Delete(_) => {
            sink.push(a.describe(stmt).map(|_| ()), span);
        }
        Statement::Utility(u) => match &u.kind {
            UtilityKind::Call { .. } => sink.push(a.describe(stmt).map(|_| ()), span),
            UtilityKind::Copy(c) => match &c.source {
                CopySource::Table { name, columns } => {
                    sink.push(a.table_columns(name, columns), span);
                }
                CopySource::Query(q) => sink.push(a.select(q, None, true).map(|_| ()), span),
            },
            UtilityKind::Maintenance { tables, .. } => {
                for t in tables {
                    sink.push(a.table_columns(&t.table, &t.columns), span);
                }
            }
            UtilityKind::Do { body, .. } => {
                empty_body(body, "DO", span, sink.out);
                if let FunctionBody::Parsed(b) = body {
                    let routine = Routine::new(Language::Plpgsql, "inline_code_block");
                    let mut w = Walker::new(catalog, path, routine, None);
                    w.body(b, sink.out);
                }
            }
            _ => {}
        },
        Statement::Ddl(ddl) => match ddl {
            DdlStatement::CreateView(v) => {
                sink.push(a.select(&v.query, None, true).map(|_| ()), span)
            }
            DdlStatement::CreatePolicy(p) => sink.push(a.policy(p), p.span),
            DdlStatement::CreateTable(t) if t.as_query.is_none() => {
                let checks: Vec<&Expr> = t
                    .constraints
                    .iter()
                    .filter_map(|c| match c {
                        pgzest_ast::ddl::TableConstraint::Check { expr, .. } => Some(expr),
                        _ => None,
                    })
                    .collect();
                sink.push(a.table_expressions(&t.name, &t.columns, &checks), t.span);
            }
            DdlStatement::AlterTable(t) => {
                use pgzest_ast::ddl::{AlterColumnAction, AlterTableOp, TableConstraint};
                for op in &t.operations {
                    let r = match op {
                        AlterTableOp::AddColumn { column, .. } => {
                            a.table_expressions(&t.name, std::slice::from_ref(column), &[])
                        }
                        AlterTableOp::AddConstraint {
                            constraint: TableConstraint::Check { expr, .. },
                            ..
                        } => a.table_expressions(&t.name, &[], &[expr]),
                        AlterTableOp::AlterColumn {
                            name,
                            action: AlterColumnAction::SetDefault(e),
                        } => a.column_default(&t.name, &name.value, e),
                        _ => Ok(()),
                    };
                    sink.push(r, span);
                }
            }
            DdlStatement::CreateDomain(d) => sink.push(a.domain_checks(d), span),
            DdlStatement::CreateIndex(ci) => sink.push(a.index(ci), span),
            DdlStatement::CreateFunction(f) => {
                empty_body(
                    &f.body,
                    &pgzest_ast::emit::emit_object_name(&f.name),
                    f.span,
                    sink.out,
                );
                if !matches!(f.body, FunctionBody::Parsed(_)) {
                    let path = body_search_path(path, &f.settings, &f.body);
                    sql_routine(
                        catalog, &path, &f.name, &f.params, &f.body, sink.out, &mut None,
                    );
                }
            }
            DdlStatement::CreateProcedure(p) => {
                empty_body(
                    &p.body,
                    &pgzest_ast::emit::emit_object_name(&p.name),
                    p.span,
                    sink.out,
                );
                if !matches!(p.body, FunctionBody::Parsed(_)) {
                    let path = body_search_path(path, &p.settings, &p.body);
                    sql_routine(
                        catalog, &path, &p.name, &p.params, &p.body, sink.out, &mut None,
                    );
                }
            }
            _ => {}
        },
    }
}

/// PL/pgSQL body of `stmt`, against the catalog state where it will run.
pub fn check_deferred(
    catalog: &Catalog,
    path: &SearchPath,
    stmt: &Statement,
    out: &mut Vec<Diagnostic>,
) {
    deferred(catalog, path, stmt, out, &mut None);
}

fn deferred(
    catalog: &Catalog,
    path: &SearchPath,
    stmt: &Statement,
    out: &mut Vec<Diagnostic>,
    collect: &mut Option<Collected>,
) {
    match stmt {
        Statement::Ddl(DdlStatement::CreateFunction(f)) => {
            if let FunctionBody::Parsed(b) = &f.body {
                plpgsql_function(catalog, path, f, b, out, collect);
            }
        }
        Statement::Ddl(DdlStatement::CreateProcedure(p)) => {
            if let FunctionBody::Parsed(b) = &p.body {
                let path = &body_search_path(path, &p.settings, &p.body);
                let mut a = Analyzer::new(TypeSystem::new(catalog), path.clone());
                let (_, name) = split_name(&p.name);
                let routine =
                    routine_scope(&mut a, Language::Plpgsql, name, &p.params, out, p.span);
                run(Walker::new(catalog, path, routine, None), b, out, collect);
            }
        }
        _ => {}
    }
}

/// Query of a routine body, as PostgreSQL prepares it at
/// execution: each variable is a parameter in it.
#[derive(Debug, Clone)]
pub struct BodyQuery {
    pub statement: Statement,
    /// Described columns, or the error (code and message).
    pub result: Result<Vec<crate::query::OutputColumn>, String>,
    /// References to variables: position of the expression and type of the
    /// parameter that replaces it.
    pub var_refs: Vec<(Span, Ty)>,
    /// SQL that recreates what the body sees in addition to the schema: its DDL
    /// (temporary tables…) and the trigger's transition tables.
    pub setup: Vec<String>,
}

/// Queries of the body of the routine created by `stmt` (PL/pgSQL or SQL),
/// analyzed in its scope, against `catalog`.
pub fn body_queries(catalog: &Catalog, path: &SearchPath, stmt: &Statement) -> Vec<BodyQuery> {
    collected(catalog, path, stmt).queries
}

/// What the analysis of the body of the routine created by `stmt` resolved:
/// its column references and calls, positioned in the file.
pub fn body_refs(catalog: &Catalog, path: &SearchPath, stmt: &Statement) -> BodyRefs {
    collected(catalog, path, stmt).refs
}

/// Column references and calls resolved in a routine body.
#[derive(Debug, Clone, Default)]
pub struct BodyRefs {
    pub columns: Vec<(Span, crate::expr::Typed)>,
    pub calls: Vec<crate::expr::CallRef>,
}

/// What a body walk collects on request.
#[derive(Default)]
struct Collected {
    queries: Vec<BodyQuery>,
    refs: BodyRefs,
}

impl Collected {
    /// Keeps the references resolved by `a`.
    fn harvest(&mut self, a: &Analyzer) {
        self.refs.columns.extend(a.column_refs.iter().cloned());
        self.refs.calls.extend(a.call_refs.iter().cloned());
    }
}

fn collected(catalog: &Catalog, path: &SearchPath, stmt: &Statement) -> Collected {
    let mut collect = Some(Collected::default());
    let mut ignored = Vec::new();
    match stmt {
        Statement::Ddl(DdlStatement::CreateFunction(f))
            if !matches!(f.body, FunctionBody::Parsed(_)) =>
        {
            sql_routine(
                catalog,
                &body_search_path(path, &f.settings, &f.body),
                &f.name,
                &f.params,
                &f.body,
                &mut ignored,
                &mut collect,
            );
        }
        Statement::Ddl(DdlStatement::CreateProcedure(p))
            if !matches!(p.body, FunctionBody::Parsed(_)) =>
        {
            sql_routine(
                catalog,
                &body_search_path(path, &p.settings, &p.body),
                &p.name,
                &p.params,
                &p.body,
                &mut ignored,
                &mut collect,
            );
        }
        _ => deferred(catalog, path, stmt, &mut ignored, &mut collect),
    }
    let mut collected = collect.unwrap_or_default();
    // A body with its own search path: its queries are prepared with it.
    let own = match stmt {
        Statement::Ddl(DdlStatement::CreateFunction(f)) => {
            Some(body_search_path(path, &f.settings, &f.body))
        }
        Statement::Ddl(DdlStatement::CreateProcedure(p)) => {
            Some(body_search_path(path, &p.settings, &p.body))
        }
        _ => None,
    };
    if let Some(own) = own.filter(|p| p != path) {
        let set = set_local_search_path(&own);
        for q in &mut collected.queries {
            q.setup.insert(0, set.clone());
        }
    }
    collected
}

/// Walks `body` with `w`, collecting its queries if requested.
fn run(
    mut w: Walker,
    body: &PlpgsqlBody,
    out: &mut Vec<Diagnostic>,
    collect: &mut Option<Collected>,
) {
    if collect.is_some() {
        w.collected = Some(Collected::default());
    }
    w.body(body, out);
    if let (Some(c), Some(q)) = (collect.as_mut(), w.collected.take()) {
        c.queries.extend(q.queries);
        c.refs.columns.extend(q.refs.columns);
        c.refs.calls.extend(q.refs.calls);
    }
}

// ── Simple statements ─────────────────────────────────────────────────────────

/// Empty body: the only certain thing that can be said about an unanalyzed
/// body (PL/pgSQL refuses it: "syntax error at end of input").
fn empty_body(body: &FunctionBody, name: &str, span: Span, out: &mut Vec<Diagnostic>) {
    if let FunctionBody::Raw(raw) = body
        && raw.trim().is_empty()
    {
        let d = Diagnostic::warning(
            codes::EMPTY_FUNCTION_BODY,
            format!("function '{name}' has an empty body"),
        )
        .with_hint("add a BEGIN … END block or a RETURN statement");
        out.push(if span == Span::DUMMY {
            d
        } else {
            d.with_span(pgzest_diagnostics::Span {
                start: span.start as usize,
                end: span.end as usize,
            })
        });
    }
}

impl Analyzer<'_> {
    /// `COPY table (columns)`, `ANALYZE table (columns)`: the table and its
    /// columns exist.
    fn table_columns(&mut self, name: &ObjectName, columns: &[Ident]) -> Result<(), SemaError> {
        let (schema, n) = split_name(name);
        let rel = self.ts.relation(schema, n, &self.path).ok_or_else(|| {
            SemaError::new(
                codes::TABLE_NOT_FOUND,
                format!("relation \"{n}\" does not exist"),
                Span::DUMMY,
            )
        })?;
        for c in columns {
            if !rel.columns.iter().any(|x| x.name == c.value) {
                return Err(SemaError::new(
                    codes::COLUMN_NOT_FOUND,
                    format!("column \"{}\" of relation \"{n}\" does not exist", c.value),
                    Span::DUMMY,
                ));
            }
        }
        Ok(())
    }

    /// Expressions of a table, in its context (`transformColumnDefinition`,
    /// `cookDefault`, `cookConstraint`): `DEFAULT` without columns, convertible
    /// to the column; generated expression over non-generated columns,
    /// convertible to the column; boolean `CHECK` over the columns.
    fn table_expressions(
        &mut self,
        table: &ObjectName,
        columns: &[pgzest_ast::ddl::ColumnDef],
        checks: &[&Expr],
    ) -> Result<(), SemaError> {
        // Missing table: its replay failed, already reported.
        let Ok(source) = self.relation_source(table, None) else {
            return Ok(());
        };
        let rel = source.rowtype.clone();
        let mut scope = Scope::default();
        scope.push(source);
        for col in columns {
            let target = scope.sources[0]
                .columns
                .iter()
                .find(|c| c.name == col.name.value)
                .cloned();
            if let Some(e) = &col.default {
                self.default_expr(&col.name.value, target.as_ref(), e)?;
            }
            if let Some(g) = &col.generated {
                if let Some(r) = &rel
                    && let Some(other) = first_column_ref(&g.expr).filter(|c| {
                        self.ts.catalog.relations.get(r).is_some_and(|rel| {
                            rel.columns
                                .iter()
                                .any(|x| x.name == c.column.value && x.generated.is_some())
                        })
                    })
                {
                    return Err(SemaError::new(
                        codes::GENERATED_COLUMN,
                        format!(
                            "cannot use generated column \"{}\" in column generation expression",
                            other.column.value
                        ),
                        g.expr.span,
                    ));
                }
                let t = self.expr(&g.expr, &scope)?;
                self.assignable(
                    &col.name.value,
                    target.as_ref(),
                    &t.ty,
                    "generation",
                    g.expr.span,
                )?;
            }
            for c in &col.constraints {
                if let pgzest_ast::ddl::ColumnConstraint::Check(e) = c {
                    self.boolean(e, &scope)?;
                }
            }
        }
        for e in checks {
            self.boolean(e, &scope)?;
        }
        Ok(())
    }

    /// `CREATE INDEX` (`DefineIndex`): indexed and `INCLUDE` columns
    /// exist, expressions typed in the table's context,
    /// boolean predicate, operator class known for the method.
    ///
    /// Not checked: compatibility of the operator class with the
    /// type, existence of a default class, immutability of the
    /// functions of an expression.
    fn index(&mut self, ci: &pgzest_ast::ddl::CreateIndex) -> Result<(), SemaError> {
        use pgzest_ast::ddl::IndexElement;
        // Missing table: its replay failed, already reported.
        let Ok(source) = self.relation_source(&ci.table, None) else {
            return Ok(());
        };
        let missing = |name: &str| {
            SemaError::new(
                codes::COLUMN_NOT_FOUND,
                format!("column \"{name}\" does not exist"),
                Span::DUMMY,
            )
        };
        let mut scope = Scope::default();
        scope.push(source);
        let method = ci.using.as_deref().unwrap_or("btree");
        for item in &ci.elements {
            match &item.element {
                IndexElement::Column(c) => {
                    if !scope.sources[0].columns.iter().any(|x| x.name == c.value) {
                        return Err(missing(&c.value));
                    }
                }
                IndexElement::Expr(e) => {
                    self.expr(e, &scope)?;
                }
            }
            if let Some(op) = &item.opclass {
                let (schema, name) = match op.0.as_slice() {
                    [n] => (None, n.value.as_str()),
                    [s, n] => (Some(s.value.as_str()), n.value.as_str()),
                    _ => continue,
                };
                let lookup = self.path.lookup_order();
                let known = self.ts.catalog.opclasses.iter().any(|o| {
                    o.access_method == method
                        && o.name.name == name
                        && match schema {
                            Some(s) => o.name.schema == s,
                            None => lookup.contains(&o.name.schema.as_str()),
                        }
                });
                if !known {
                    return Err(SemaError::new(
                        codes::OBJECT_NOT_FOUND,
                        format!(
                            "operator class \"{}\" does not exist for access method \"{method}\"",
                            pgzest_ast::emit::emit_object_name(op)
                        ),
                        Span::DUMMY,
                    ));
                }
            }
        }
        for c in &ci.include {
            if !scope.sources[0].columns.iter().any(|x| x.name == c.value) {
                return Err(missing(&c.value));
            }
        }
        if let Some(w) = &ci.where_clause {
            self.boolean(w, &scope)?;
        }
        Ok(())
    }

    /// `ALTER COLUMN … SET DEFAULT`.
    fn column_default(
        &mut self,
        table: &ObjectName,
        column: &str,
        e: &Expr,
    ) -> Result<(), SemaError> {
        let Ok(source) = self.relation_source(table, None) else {
            return Ok(());
        };
        let target = source.columns.iter().find(|c| c.name == column).cloned();
        self.default_expr(column, target.as_ref(), e)
    }

    /// `DEFAULT`: no column, and a value convertible (assignment)
    /// to the column's type.
    fn default_expr(
        &mut self,
        column: &str,
        target: Option<&Column>,
        e: &Expr,
    ) -> Result<(), SemaError> {
        if first_column_ref(e).is_some() {
            return Err(SemaError::new(
                codes::COLUMN_NOT_FOUND,
                "cannot use column reference in DEFAULT expression",
                e.span,
            ));
        }
        let t = self.expr(e, &Scope::default())?;
        self.assignable(column, target, &t.ty, "default", e.span)
    }

    fn assignable(
        &self,
        column: &str,
        target: Option<&Column>,
        ty: &Ty,
        what: &str,
        span: Span,
    ) -> Result<(), SemaError> {
        let Some(target) = target.and_then(|c| c.ty.as_ref()) else {
            return Ok(());
        };
        if *ty == pg("unknown")
            || self.ts.can_coerce(
                std::slice::from_ref(ty),
                std::slice::from_ref(target),
                crate::coerce::CoercionContext::Assignment,
            )
        {
            return Ok(());
        }
        Err(SemaError::new(
            codes::TYPE_MISMATCH,
            format!(
                "column \"{column}\" is of type {} but {what} expression is of type {}",
                crate::expr::format_type(target),
                crate::expr::format_type(ty)
            ),
            span,
        ))
    }

    /// `CREATE DOMAIN … CHECK (VALUE …)`: `VALUE` is the value of the base
    /// type.
    fn domain_checks(&mut self, d: &pgzest_ast::ddl::CreateDomain) -> Result<(), SemaError> {
        if d.checks.is_empty() {
            return Ok(());
        }
        let base = self.data_type(&d.data_type, Span::DUMMY)?;
        let mut scope = Scope::default();
        scope.push(Source::new(
            None,
            vec![Column {
                name: "value".into(),
                ty: Some(base),
                modifiers: Vec::new(),
                nullable: Nullability::Unknown,
            }],
        ));
        for c in &d.checks {
            self.boolean(&c.expr, &scope)?;
        }
        Ok(())
    }

    /// Columns fed by `COPY table [(columns)] FROM`: those
    /// listed, or all except generated columns (`CopyGetAttnums`).
    pub fn copy_columns(
        &self,
        table: &ObjectName,
        columns: &[Ident],
    ) -> Result<Vec<crate::query::OutputColumn>, SemaError> {
        let source = self.relation_source(table, None)?;
        let rel = source
            .rowtype
            .as_ref()
            .and_then(|r| self.ts.catalog.relations.get(r));
        let generated = |name: &str| {
            rel.is_some_and(|r| {
                r.columns
                    .iter()
                    .any(|c| c.name == name && c.generated.is_some())
            })
        };
        let chosen: Vec<&Column> = if columns.is_empty() {
            source
                .columns
                .iter()
                .filter(|c| !generated(&c.name))
                .collect()
        } else {
            let mut out = Vec::new();
            for c in columns {
                let col = source
                    .columns
                    .iter()
                    .find(|x| x.name == c.value)
                    .ok_or_else(|| {
                        SemaError::new(
                            codes::COLUMN_NOT_FOUND,
                            format!(
                                "column \"{}\" of relation \"{}\" does not exist",
                                c.value,
                                source.name.clone().unwrap_or_default()
                            ),
                            Span::DUMMY,
                        )
                    })?;
                if generated(&c.value) {
                    return Err(SemaError::new(
                        codes::GENERATED_COLUMN,
                        format!("column \"{}\" is a generated column", c.value),
                        Span::DUMMY,
                    ));
                }
                out.push(col);
            }
            out
        };
        chosen
            .into_iter()
            .map(|c| {
                Ok(crate::query::OutputColumn {
                    name: c.name.clone(),
                    ty: c.ty.clone().ok_or_else(|| {
                        SemaError::not_analyzed(
                            format!("type of column \"{}\"", c.name),
                            Span::DUMMY,
                        )
                    })?,
                    modifiers: c.modifiers.clone(),
                    nullable: c.nullable,
                })
            })
            .collect()
    }

    /// `CREATE POLICY`: `USING` and `WITH CHECK` are conditions on a row
    /// of the table.
    fn policy(&mut self, p: &CreatePolicy) -> Result<(), SemaError> {
        let source = self.relation_source(&p.table, None)?;
        let mut scope = Scope::default();
        scope.push(source);
        for e in [&p.using, &p.with_check].into_iter().flatten() {
            self.boolean(e, &scope)?;
        }
        Ok(())
    }
}

// ── Routines ──────────────────────────────────────────────────────────────────

/// Type of a variable declared with the written type `dt`.
fn var_ty(a: &Analyzer, dt: &pgzest_ast::types::DataType, span: Span) -> Result<VarTy, SemaError> {
    let ty = a.data_type(dt, span)?;
    Ok(classify(&a.ts, ty))
}

fn classify(ts: &TypeSystem, ty: Ty) -> VarTy {
    if is_polymorphic(&ty) {
        VarTy::Unknown(format!("type of polymorphic argument {}", ty.name))
    } else if ty == pg("record") {
        VarTy::Record(None)
    } else if ts.is_complex(&ts.base_type(&ty)) {
        VarTy::Row(ty)
    } else {
        VarTy::Scalar(ty)
    }
}

/// Scope of a routine's parameters: named (in the block carrying the
/// routine's name) and positional.
fn routine_scope(
    a: &mut Analyzer,
    language: Language,
    name: &str,
    params: &[FunctionParam],
    out: &mut Vec<Diagnostic>,
    span: Span,
) -> Routine {
    let mut r = Routine::new(language, name);
    for p in params {
        let input = !matches!(p.mode, ParamMode::Out);
        // `$n`: all arguments in PL/pgSQL, the input ones in SQL.
        let positional = language == Language::Plpgsql || input;
        // An `OUT` parameter is only a variable in PL/pgSQL.
        let named = language == Language::Plpgsql || input;
        let ty = match var_ty(a, &p.data_type, span) {
            Ok(t) => t,
            Err(e) => {
                out.push(diagnostic(e, span));
                VarTy::Unknown("parameter type not found".into())
            }
        };
        if positional {
            r.positional.push(ty.clone());
        }
        if named && let Some(n) = &p.name {
            r.declare(&n.value, ty);
        }
    }
    r
}

/// `LANGUAGE sql` body: each statement, with the parameters in scope.
fn sql_routine(
    catalog: &Catalog,
    path: &SearchPath,
    name: &ObjectName,
    params: &[FunctionParam],
    body: &FunctionBody,
    out: &mut Vec<Diagnostic>,
    collect: &mut Option<Collected>,
) {
    let (_, fname) = split_name(name);
    let stmts: Vec<Statement> = match body {
        FunctionBody::Sql(stmts) | FunctionBody::Atomic(stmts) => stmts.clone(),
        FunctionBody::SqlQuery(q) => vec![Statement::Select((**q).clone())],
        FunctionBody::SqlReturn(e) => {
            let mut a = Analyzer::new(TypeSystem::new(catalog), path.clone());
            a.routine = Some(routine_scope(
                &mut a,
                Language::Sql,
                fname,
                params,
                out,
                e.span,
            ));
            if let Err(err) = a.expr(e, &Scope::default()) {
                out.push(diagnostic(err, e.span));
            }
            if let Some(c) = collect.as_mut() {
                c.harvest(&a);
            }
            return;
        }
        FunctionBody::Raw(_) | FunctionBody::Parsed(_) => return,
    };
    // Tables created by the body: visible to the following statements.
    let overlay = body_overlay(catalog, path, stmts.iter());
    let cat = overlay.as_ref().unwrap_or(catalog);
    for stmt in &stmts {
        let span = pgzest_ast::stmt_span(stmt).unwrap_or(Span::DUMMY);
        if !matches!(
            stmt,
            Statement::Select(_)
                | Statement::Insert(_)
                | Statement::Update(_)
                | Statement::Delete(_)
        ) {
            continue;
        }
        let mut a = Analyzer::new(TypeSystem::new(cat), path.clone());
        let mut scratch = Vec::new();
        a.routine = Some(routine_scope(
            &mut a,
            Language::Sql,
            fname,
            params,
            &mut scratch,
            span,
        ));
        let result = a.describe(stmt);
        if let Some(c) = collect.as_mut() {
            c.harvest(&a);
            c.queries.push(BodyQuery {
                statement: stmt.clone(),
                result: result
                    .as_ref()
                    .map(|d| d.columns.clone())
                    .map_err(|e| format!("[{}] {}", e.code, e.message)),
                var_refs: a.var_refs.clone(),
                setup: stmts
                    .iter()
                    .filter(|s| matches!(s, Statement::Ddl(_)))
                    .map(pgzest_ast::emit::emit_statement)
                    .collect(),
            });
        }
        if let Err(e) = result {
            out.push(diagnostic(e, span));
        }
    }
}

/// Catalog extended with the DDL of a body (temporary tables…), if it
/// contains any.
fn body_overlay<'s>(
    catalog: &Catalog,
    path: &SearchPath,
    stmts: impl Iterator<Item = &'s Statement>,
) -> Option<Catalog> {
    let ddl: Vec<&Statement> = stmts.filter(|s| matches!(s, Statement::Ddl(_))).collect();
    if ddl.is_empty() {
        return None;
    }
    let mut replay = pgzest_catalog::Replay::new(catalog.clone());
    replay.search_path = path.clone();
    let mut ignored = Vec::new();
    for s in ddl {
        replay.apply(s, &mut ignored);
    }
    Some(replay.catalog)
}

/// Context of a trigger function: the table of one of its triggers.
#[derive(Debug, Clone)]
struct TriggerContext {
    table: QName,
    row_level: bool,
    /// Transition tables, visible as relations.
    transition: Vec<String>,
}

/// Contexts of a trigger function, one per table that fires it.
fn trigger_contexts(catalog: &Catalog, function: &QName) -> Vec<TriggerContext> {
    let mut out: BTreeMap<QName, TriggerContext> = BTreeMap::new();
    for (table, triggers) in &catalog.triggers {
        for t in triggers.values().filter(|t| &t.function == function) {
            let ctx = out.entry(table.clone()).or_insert_with(|| TriggerContext {
                table: table.clone(),
                row_level: false,
                transition: Vec::new(),
            });
            ctx.row_level |= t.for_each_row;
            for n in [&t.old_table, &t.new_table].into_iter().flatten() {
                if !ctx.transition.contains(n) {
                    ctx.transition.push(n.clone());
                }
            }
        }
    }
    out.into_values().collect()
}

/// Qualified name of a routine created under `name`.
fn routine_qname(name: &ObjectName, path: &SearchPath) -> QName {
    let (schema, n) = split_name(name);
    QName::new(
        schema.unwrap_or_else(|| path.creation_schema().unwrap_or("public")),
        n,
    )
}

fn plpgsql_function(
    catalog: &Catalog,
    path: &SearchPath,
    f: &CreateFunction,
    body: &PlpgsqlBody,
    out: &mut Vec<Diagnostic>,
    collect: &mut Option<Collected>,
) {
    let (_, name) = split_name(&f.name);
    // The routine is named with the path of its creation, its body runs
    // with its own.
    let qname = routine_qname(&f.name, path);
    let path = &body_search_path(path, &f.settings, &f.body);
    let make = |out: &mut Vec<Diagnostic>| {
        let mut a = Analyzer::new(TypeSystem::new(catalog), path.clone());
        let mut r = routine_scope(&mut a, Language::Plpgsql, name, &f.params, out, f.span);
        if let ReturnType::Table(cols) = &f.return_type {
            for (n, dt) in cols {
                let ty = var_ty(&a, dt, f.span)
                    .unwrap_or_else(|_| VarTy::Unknown("column type not found".into()));
                r.declare(&n.value, ty);
            }
        }
        r
    };
    let is_event_trigger = matches!(
        &f.return_type,
        ReturnType::Scalar(pgzest_ast::types::DataType::Custom(n)) if n.eq_ignore_ascii_case("event_trigger")
    );
    if is_event_trigger {
        let mut r = make(out);
        r.declare("tg_event", VarTy::Scalar(pg("text")));
        r.declare("tg_tag", VarTy::Scalar(pg("text")));
        run(Walker::new(catalog, path, r, None), body, out, collect);
        return;
    }
    if f.return_type != ReturnType::Trigger {
        let r = make(out);
        run(Walker::new(catalog, path, r, None), body, out, collect);
        return;
    }
    let contexts = trigger_contexts(catalog, &qname);
    if contexts.is_empty() {
        out.push(
            Diagnostic::warning(
                codes::TRIGGER_NO_CONTEXT,
                format!(
                    "trigger function '{name}' has no associated CREATE TRIGGER in the \
                     analyzed files: NEW/OLD cannot be verified"
                ),
            )
            .with_span(pgzest_diagnostics::Span {
                start: f.span.start as usize,
                end: f.span.end as usize,
            }),
        );
        let r = make(out);
        let mut d = Vec::new();
        run(
            Walker::new(catalog, path, r, Some(None)),
            body,
            &mut d,
            collect,
        );
        // NEW and OLD without a table: what depends on them is not checkable, and
        // has just been said once for the function.
        out.extend(
            d.into_iter().filter(|d| {
                !(d.code == codes::NOT_ANALYZED && d.message.contains(NO_TRIGGER_TABLE))
            }),
        );
        return;
    }
    // A function shared between several tables is checked for each
    // one: NEW and OLD take the type of the firing table.
    let passes: Vec<(String, Vec<Diagnostic>)> = contexts
        .into_iter()
        .map(|ctx| {
            let collect = &mut *collect;
            let mut d = Vec::new();
            let r = make(&mut d);
            let table = ctx.table.name.clone();
            run(
                Walker::new(catalog, path, r, Some(Some(ctx))),
                body,
                &mut d,
                collect,
            );
            (table, d)
        })
        .collect();
    out.extend(merge_per_table_passes(passes));
}

const NO_TRIGGER_TABLE: &str = "unknown trigger table";

/// Merges the diagnostics of the "one per table" passes: a diagnostic
/// common to all of them does not depend on the table and is emitted only
/// once; the others name the tables concerned.
fn merge_per_table_passes(passes: Vec<(String, Vec<Diagnostic>)>) -> Vec<Diagnostic> {
    type Key = (&'static str, String, Option<(usize, usize)>);
    let key = |d: &Diagnostic| -> Key {
        (
            d.code,
            d.message.clone(),
            d.span.as_ref().map(|s| (s.start, s.end)),
        )
    };
    let mut merged: Vec<(Key, Diagnostic, Vec<&str>)> = Vec::new();
    for (table, diags) in &passes {
        for d in diags {
            let k = key(d);
            match merged.iter_mut().find(|(mk, _, _)| *mk == k) {
                Some((_, _, tables)) => {
                    if !tables.contains(&table.as_str()) {
                        tables.push(table);
                    }
                }
                None => merged.push((k, d.clone(), vec![table])),
            }
        }
    }
    merged
        .into_iter()
        .map(|(_, mut d, tables)| {
            if tables.len() < passes.len() && !tables.iter().any(|t| d.message.contains(t)) {
                d.message = format!("{} (trigger on {})", d.message, tables.join(", "));
            }
            d
        })
        .collect()
}

// ── PL/pgSQL ──────────────────────────────────────────────────────────────────

/// Walk of a PL/pgSQL body (`pl_comp.c` for the declarations, then
/// each expression and query analyzed the way PostgreSQL does at
/// execution, with variables in scope).
struct Walker<'c> {
    catalog: &'c Catalog,
    path: SearchPath,
    routine: Routine,
    /// Trigger function: `Some(Some(ctx))` with its table, `Some(None)` if
    /// no table is known.
    trigger: Option<Option<TriggerContext>>,
    /// Columns of the queries of declared cursors, by name.
    cursors: BTreeMap<String, Option<Vec<Column>>>,
    /// Analyzed queries and references, if they are collected
    /// ([`body_queries`], [`body_refs`]).
    collected: Option<Collected>,
    /// Context of the collected queries (see [`BodyQuery::setup`]).
    setup: Vec<String>,
}

impl<'c> Walker<'c> {
    fn new(
        catalog: &'c Catalog,
        path: &SearchPath,
        routine: Routine,
        trigger: Option<Option<TriggerContext>>,
    ) -> Self {
        Walker {
            catalog,
            path: path.clone(),
            routine,
            trigger,
            cursors: BTreeMap::new(),
            collected: None,
            setup: Vec::new(),
        }
    }

    fn body(&mut self, body: &PlpgsqlBody, out: &mut Vec<Diagnostic>) {
        // The DDL of the body (temporary tables…) is visible to its queries.
        let mut ddl = Vec::new();
        collect_sql(&body.block, &mut ddl);
        if self.collected.is_some() {
            self.setup = ddl
                .iter()
                .filter(|s| matches!(s, Statement::Ddl(_)))
                .map(|s| pgzest_ast::emit::emit_statement(s))
                .collect();
            if let Some(Some(ctx)) = &self.trigger {
                let table = format!(
                    "{}.{}",
                    pgzest_catalog::model::quote_sql_ident(&ctx.table.schema),
                    pgzest_catalog::model::quote_sql_ident(&ctx.table.name)
                );
                for n in &ctx.transition {
                    self.setup.push(format!(
                        "CREATE TEMP TABLE {} (LIKE {table})",
                        pgzest_catalog::model::quote_sql_ident(n)
                    ));
                }
            }
        }
        let overlay = body_overlay(self.catalog, &self.path, ddl.into_iter());
        let catalog = overlay.as_ref().unwrap_or(self.catalog);
        let mut ctx = Ctx { catalog, out };
        self.routine.declare("found", VarTy::Scalar(pg("bool")));
        if let Some(trigger) = self.trigger.clone() {
            self.declare_trigger(&trigger, catalog);
        }
        self.block(&body.block, &mut ctx);
    }

    /// Variables of a trigger function (`plpgsql_compile`, `TG_*`).
    fn declare_trigger(&mut self, trigger: &Option<TriggerContext>, catalog: &Catalog) {
        for (n, t) in [
            ("tg_name", "name"),
            ("tg_when", "text"),
            ("tg_level", "text"),
            ("tg_op", "text"),
            ("tg_relid", "oid"),
            ("tg_relname", "name"),
            ("tg_table_name", "name"),
            ("tg_table_schema", "name"),
            ("tg_nargs", "int4"),
            ("tg_argv", "_text"),
        ] {
            self.routine.declare(n, VarTy::Scalar(pg(t)));
        }
        let row = match trigger {
            Some(ctx) if ctx.row_level && catalog.relations.contains_key(&ctx.table) => {
                VarTy::Row(ctx.table.clone())
            }
            // Statement-level trigger: NEW and OLD are NULL.
            Some(_) => VarTy::Record(None),
            None => VarTy::Unknown(format!("NEW/OLD: {NO_TRIGGER_TABLE}")),
        };
        self.routine.declare("new", row.clone());
        self.routine.declare("old", row);
    }

    /// Analyzer of an expression or query of the body.
    fn analyzer<'a>(&self, catalog: &'a Catalog) -> Analyzer<'a> {
        let mut a = Analyzer::new(TypeSystem::new(catalog), self.path.clone());
        a.routine = Some(self.routine.clone());
        // Transition tables of a trigger: named relations.
        if let Some(Some(ctx)) = &self.trigger
            && let Some(rel) = catalog.relations.get(&ctx.table)
        {
            let cols: Vec<Column> = rel
                .columns
                .iter()
                .map(|c| Column {
                    name: c.name.clone(),
                    ty: a.ts.of_ref(&c.ty),
                    modifiers: crate::query::ref_modifiers(&c.ty),
                    nullable: if c.not_null {
                        Nullability::NonNull
                    } else {
                        Nullability::Nullable
                    },
                })
                .collect();
            for n in &ctx.transition {
                a.ctes.push((n.clone(), cols.clone()));
            }
        }
        a
    }

    fn block(&mut self, b: &Block, ctx: &mut Ctx) {
        self.routine.frames.push(crate::routine::Frame {
            label: b.label.clone(),
            vars: Vec::new(),
        });
        for d in &b.declarations {
            self.declaration(d, ctx);
        }
        self.statements(&b.statements, ctx);
        if let Some(exc) = &b.exception {
            for h in &exc.handlers {
                self.routine.frames.push(crate::routine::Frame::default());
                self.routine.declare("sqlstate", VarTy::Scalar(pg("text")));
                self.routine.declare("sqlerrm", VarTy::Scalar(pg("text")));
                self.statements(&h.statements, ctx);
                self.routine.frames.pop();
            }
        }
        self.routine.frames.pop();
    }

    fn declaration(&mut self, d: &Declaration, ctx: &mut Ctx) {
        match d {
            Declaration::Variable(v) => {
                // The initial value sees the variables already declared.
                if let Some(e) = &v.default {
                    self.expr(e, ctx);
                }
                let ty = match &v.var_type {
                    VarType::Data(dt) => {
                        let a = self.analyzer(ctx.catalog);
                        var_ty(&a, dt, v.span)
                    }
                    VarType::Record => Ok(VarTy::Record(None)),
                    VarType::RowTypeOf(t) => self.rowtype_of(t, ctx.catalog, v.span),
                    VarType::TypeOf(t) => self.type_of(t, ctx.catalog, v.span),
                };
                let ty = ty.unwrap_or_else(|e| {
                    ctx.out.push(diagnostic(e, v.span));
                    VarTy::Unknown(format!("type of variable \"{}\"", v.name.value))
                });
                self.routine.declare(&v.name.value, ty);
            }
            Declaration::Alias { name, target } => {
                let target_ref = ColumnRef::new(None, Ident::new(target));
                let ty = if let Some(n) = target
                    .strip_prefix('$')
                    .and_then(|d| d.parse::<usize>().ok())
                {
                    self.routine.positional.get(n.wrapping_sub(1)).cloned()
                } else {
                    match self.routine.lookup(&target_ref) {
                        Some(VarRef::Whole(v)) => Some(v.ty),
                        _ => None,
                    }
                };
                match ty {
                    Some(ty) => self.routine.declare(&name.value, ty),
                    None => {
                        ctx.out.push(Diagnostic::error(
                            codes::UNDECLARED_VARIABLE,
                            format!("variable \"{target}\" does not exist"),
                        ));
                        self.routine
                            .declare(&name.value, VarTy::Unknown("invalid alias".into()));
                    }
                }
            }
            Declaration::Cursor {
                name,
                params,
                query,
            } => {
                let mut a = self.analyzer(ctx.catalog);
                let mut frame = crate::routine::Frame::default();
                for (p, dt) in params {
                    let ty = var_ty(&a, dt, query.span)
                        .unwrap_or_else(|_| VarTy::Unknown("cursor parameter type".into()));
                    frame.vars.push(crate::routine::Var {
                        name: p.value.clone(),
                        ty,
                    });
                }
                if let Some(r) = a.routine.as_mut() {
                    r.frames.push(frame);
                }
                let result = a.select(query, None, true);
                self.harvest(&a);
                let cols = match result {
                    Ok(cols) => Some(cols.iter().map(Column::from).collect()),
                    Err(e) => {
                        ctx.out.push(diagnostic(e, query.span));
                        None
                    }
                };
                self.cursors.insert(name.value.clone(), cols);
                self.routine
                    .declare(&name.value, VarTy::Scalar(pg("refcursor")));
            }
        }
    }

    /// `table%ROWTYPE`.
    fn rowtype_of(
        &self,
        t: &ObjectName,
        catalog: &Catalog,
        span: Span,
    ) -> Result<VarTy, SemaError> {
        let (schema, n) = split_name(t);
        if let Some(rel) = catalog.resolve_relation(schema, n, &self.path) {
            return Ok(VarTy::Row(rel.name.clone()));
        }
        if let Some(ty) = catalog.resolve_type(schema, n, &self.path) {
            return Ok(VarTy::Row(ty.name.clone()));
        }
        Err(SemaError::new(
            codes::TYPE_NOT_FOUND,
            format!("relation \"{n}\" does not exist"),
            span,
        ))
    }

    /// `table.column%TYPE` or `variable%TYPE`.
    fn type_of(&self, t: &ObjectName, catalog: &Catalog, span: Span) -> Result<VarTy, SemaError> {
        let parts: Vec<&str> = t.0.iter().map(|i| i.value.as_str()).collect();
        if let [v] = parts.as_slice()
            && let Some(var) = self.routine.var(v)
        {
            return Ok(var.ty.clone());
        }
        let (rel_schema, rel, col) = match parts.as_slice() {
            [r, c] => (None, *r, *c),
            [s, r, c] => (Some(*s), *r, *c),
            _ => {
                return Err(SemaError::new(
                    codes::TYPE_NOT_FOUND,
                    format!("type \"{}%TYPE\" does not exist", parts.join(".")),
                    span,
                ));
            }
        };
        // `variable.field%TYPE`.
        if rel_schema.is_none() && self.routine.var(rel).is_some() {
            let a = self.analyzer(catalog);
            let r = ColumnRef::new(Some(Ident::new(rel)), Ident::new(col));
            return a.variable_type(&r, span).map(|ty| classify(&a.ts, ty));
        }
        let ts = TypeSystem::new(catalog);
        let relation = catalog
            .resolve_relation(rel_schema, rel, &self.path)
            .ok_or_else(|| {
                SemaError::new(
                    codes::TABLE_NOT_FOUND,
                    format!("relation \"{rel}\" does not exist"),
                    span,
                )
            })?;
        let column = relation
            .columns
            .iter()
            .find(|c| c.name == col)
            .ok_or_else(|| {
                SemaError::new(
                    codes::COLUMN_NOT_FOUND,
                    format!("column \"{col}\" of relation \"{rel}\" does not exist"),
                    span,
                )
            })?;
        match ts.of_ref(&column.ty) {
            Some(ty) => Ok(classify(&ts, ty)),
            None => Ok(VarTy::Unknown(format!("type of column \"{rel}.{col}\""))),
        }
    }

    fn statements(&mut self, stmts: &[PlStatement], ctx: &mut Ctx) {
        for s in stmts {
            let first = ctx.out.len();
            self.statement(s, ctx);
            if s.span != Span::DUMMY {
                for d in &mut ctx.out[first..] {
                    if d.span.is_none() {
                        d.span = Some(pgzest_diagnostics::Span {
                            start: s.span.start as usize,
                            end: s.span.end as usize,
                        });
                    }
                }
            }
        }
    }

    /// Body expression: `SELECT expr`, variables in scope.
    fn expr(&mut self, e: &Expr, ctx: &mut Ctx) -> Option<crate::expr::Typed> {
        let mut a = self.analyzer(ctx.catalog);
        let result = a.expr(e, &Scope::default());
        self.harvest(&a);
        match result {
            Ok(t) => Some(t),
            Err(err) => {
                ctx.out.push(diagnostic(err, e.span));
                None
            }
        }
    }

    /// Body query: its columns, if it is valid.
    fn query(&mut self, q: &SelectStmt, ctx: &mut Ctx) -> Option<Vec<Column>> {
        let mut a = self.analyzer(ctx.catalog);
        let result = a.select(q, None, true);
        self.record(|| Statement::Select(q.clone()), &result, &a);
        match result {
            Ok(cols) => Some(cols.iter().map(Column::from).collect()),
            Err(err) => {
                ctx.out.push(diagnostic(err, q.span));
                None
            }
        }
    }

    /// Keeps the references resolved by `a`, if requested ([`body_refs`]).
    fn harvest(&mut self, a: &Analyzer) {
        if let Some(c) = self.collected.as_mut() {
            c.harvest(a);
        }
    }

    /// Collects an analyzed query, if requested ([`body_queries`]).
    fn record(
        &mut self,
        statement: impl FnOnce() -> Statement,
        result: &Result<Vec<crate::query::OutputColumn>, SemaError>,
        a: &Analyzer,
    ) {
        self.harvest(a);
        if let Some(c) = self.collected.as_mut() {
            c.queries.push(BodyQuery {
                statement: statement(),
                result: result
                    .clone()
                    .map_err(|e| format!("[{}] {}", e.code, e.message)),
                var_refs: a.var_refs.clone(),
                setup: self.setup.clone(),
            });
        }
    }

    fn statement(&mut self, s: &PlStatement, ctx: &mut Ctx) {
        match &s.kind {
            PlStatementKind::Assign { target, value } => {
                let t = self.expr(value, ctx);
                self.assign_target(target, s.span, ctx);
                // Record assigned a composite value: its structure.
                if let ([var], Some(t)) = (target.as_slice(), t) {
                    self.set_shape(&var.value, self.shape_of(&t.ty, ctx.catalog));
                }
            }
            PlStatementKind::Sql { statement, into } => {
                let cols = match statement {
                    Statement::Select(_)
                    | Statement::Insert(_)
                    | Statement::Update(_)
                    | Statement::Delete(_) => {
                        let mut a = self.analyzer(ctx.catalog);
                        let result = a.describe(statement).map(|d| d.columns);
                        self.record(|| statement.clone(), &result, &a);
                        match result {
                            Ok(cols) => Some(cols.iter().map(Column::from).collect()),
                            Err(e) => {
                                ctx.out.push(diagnostic(e, s.span));
                                None
                            }
                        }
                    }
                    Statement::Utility(u) => {
                        if let UtilityKind::Call { .. } = u.kind {
                            let mut a = self.analyzer(ctx.catalog);
                            let result = a.describe(statement);
                            self.harvest(&a);
                            if let Err(e) = result {
                                ctx.out.push(diagnostic(e, s.span));
                            }
                        }
                        None
                    }
                    // DDL: applied to the body's catalog (see `body`).
                    Statement::Ddl(_) => None,
                };
                if let Some(into) = into {
                    self.into(
                        into,
                        cols.map(Shape::Known).unwrap_or(Shape::Invalid),
                        s.span,
                        ctx,
                    );
                }
            }
            PlStatementKind::Perform(q) | PlStatementKind::ReturnQuery(q) => {
                self.query(q, ctx);
            }
            PlStatementKind::Execute { sql, into, using } => {
                self.expr(sql, ctx);
                for e in using {
                    self.expr(e, ctx);
                }
                if let Some(into) = into {
                    self.into(into, Shape::Dynamic, s.span, ctx);
                }
            }
            PlStatementKind::Return(e) | PlStatementKind::ReturnNext(e) => {
                if let Some(e) = e {
                    self.expr(e, ctx);
                }
            }
            PlStatementKind::ReturnQueryExecute { sql, using } => {
                self.expr(sql, ctx);
                for e in using {
                    self.expr(e, ctx);
                }
            }
            PlStatementKind::Raise {
                message,
                args,
                options,
                ..
            } => {
                for e in args {
                    self.expr(e, ctx);
                }
                for o in options {
                    self.expr(&o.value, ctx);
                }
                // `pl_gram.y`: the number of `%` in the format equals the number of
                // arguments, checked at compile time.
                if let Some(m) = message
                    && (!args.is_empty() || m.contains('%'))
                    && let Some(n) = raise_placeholders(m)
                    && n != args.len()
                {
                    ctx.out.push(Diagnostic::error(
                        codes::RAISE_ARGUMENTS,
                        if n > args.len() {
                            "too few parameters specified for RAISE"
                        } else {
                            "too many parameters specified for RAISE"
                        },
                    ));
                }
            }
            PlStatementKind::Assert { condition, message } => {
                self.expr(condition, ctx);
                if let Some(m) = message {
                    self.expr(m, ctx);
                }
            }
            PlStatementKind::If {
                condition,
                then_branch,
                elsif_branches,
                else_branch,
            } => {
                self.expr(condition, ctx);
                self.statements(then_branch, ctx);
                for (c, b) in elsif_branches {
                    self.expr(c, ctx);
                    self.statements(b, ctx);
                }
                if let Some(b) = else_branch {
                    self.statements(b, ctx);
                }
            }
            PlStatementKind::Case {
                operand,
                whens,
                else_branch,
            } => {
                if let Some(o) = operand {
                    self.expr(o, ctx);
                }
                for (values, b) in whens {
                    for v in values {
                        self.expr(v, ctx);
                    }
                    self.statements(b, ctx);
                }
                if let Some(b) = else_branch {
                    self.statements(b, ctx);
                }
            }
            PlStatementKind::Loop { label, body } => self.loop_body(label, body, ctx),
            PlStatementKind::While {
                label,
                condition,
                body,
            } => {
                self.expr(condition, ctx);
                self.loop_body(label, body, ctx);
            }
            PlStatementKind::ForRange {
                label,
                var,
                from,
                to,
                by,
                body,
                ..
            } => {
                for e in [Some(from), Some(to), by.as_ref()].into_iter().flatten() {
                    self.expr(e, ctx);
                }
                // The loop variable is declared `integer`, in a block of its own
                // for the loop.
                self.routine.frames.push(crate::routine::Frame {
                    label: label.clone(),
                    vars: Vec::new(),
                });
                self.routine.declare(&var.value, VarTy::Scalar(pg("int4")));
                self.statements(body, ctx);
                self.routine.frames.pop();
            }
            PlStatementKind::ForQuery {
                label,
                targets,
                query,
                body,
            } => {
                let cols = self.query(query, ctx);
                self.loop_targets(
                    targets,
                    cols.map(Shape::Known).unwrap_or(Shape::Invalid),
                    s.span,
                    ctx,
                );
                self.loop_body(label, body, ctx);
            }
            PlStatementKind::ForExecute {
                label,
                targets,
                sql,
                using,
                body,
            } => {
                self.expr(sql, ctx);
                for e in using {
                    self.expr(e, ctx);
                }
                self.loop_targets(targets, Shape::Dynamic, s.span, ctx);
                self.loop_body(label, body, ctx);
            }
            PlStatementKind::ForCursor {
                label,
                var,
                cursor,
                args,
                body,
            } => {
                for e in args {
                    self.expr(e, ctx);
                }
                let shape = match self.cursors.get(&cursor.value) {
                    Some(Some(cols)) => VarTy::Record(Some(cols.clone())),
                    Some(None) => VarTy::Unknown(format!("query of cursor \"{}\"", cursor.value)),
                    None => {
                        ctx.out.push(Diagnostic::error(
                            codes::UNDECLARED_VARIABLE,
                            format!("cursor \"{}\" does not exist", cursor.value),
                        ));
                        VarTy::Unknown(format!("cursor \"{}\"", cursor.value))
                    }
                };
                // The loop variable is a record declared by the loop.
                self.routine.frames.push(crate::routine::Frame {
                    label: label.clone(),
                    vars: Vec::new(),
                });
                self.routine.declare(&var.value, shape);
                self.statements(body, ctx);
                self.routine.frames.pop();
            }
            PlStatementKind::Foreach {
                label,
                targets,
                array,
                body,
                ..
            } => {
                self.expr(array, ctx);
                for t in targets {
                    self.assign_target(std::slice::from_ref(t), s.span, ctx);
                }
                self.loop_body(label, body, ctx);
            }
            PlStatementKind::Exit { when, .. } | PlStatementKind::Continue { when, .. } => {
                if let Some(e) = when {
                    self.expr(e, ctx);
                }
            }
            PlStatementKind::GetDiagnostics { items, .. } => {
                for i in items {
                    self.assign_target(std::slice::from_ref(&i.var), s.span, ctx);
                }
            }
            PlStatementKind::Open {
                cursor,
                args,
                query,
            } => {
                for e in args {
                    self.expr(e, ctx);
                }
                self.assign_target(std::slice::from_ref(cursor), s.span, ctx);
                if let Some(q) = query {
                    let cols = self.query(q, ctx);
                    self.cursors.insert(cursor.value.clone(), cols);
                }
            }
            PlStatementKind::Fetch { cursor, into } => {
                self.assign_target(std::slice::from_ref(cursor), s.span, ctx);
                let shape = match self.cursors.get(&cursor.value) {
                    Some(Some(cols)) => Shape::Known(cols.clone()),
                    _ => Shape::Dynamic,
                };
                self.into(into, shape, s.span, ctx);
            }
            PlStatementKind::Close(cursor) => {
                self.assign_target(std::slice::from_ref(cursor), s.span, ctx);
            }
            PlStatementKind::Block(b) => self.block(b, ctx),
            PlStatementKind::Null => {}
        }
    }

    fn loop_body(&mut self, label: &Option<String>, body: &[PlStatement], ctx: &mut Ctx) {
        self.routine.frames.push(crate::routine::Frame {
            label: label.clone(),
            vars: Vec::new(),
        });
        self.statements(body, ctx);
        self.routine.frames.pop();
    }

    /// Assignment target: a visible variable, or an existing field of a row
    /// variable (`pl_gram.y` refuses the rest at compile time).
    fn assign_target(&mut self, target: &[Ident], span: Span, ctx: &mut Ctx) {
        let r = match target {
            [] => return,
            [v] => ColumnRef::new(None, v.clone()),
            [v, f] => ColumnRef::new(Some(v.clone()), f.clone()),
            [l, v, f, ..] => ColumnRef {
                schema: Some(l.clone()),
                table: Some(v.clone()),
                column: f.clone(),
            },
        };
        let name = target
            .iter()
            .map(|i| i.value.as_str())
            .collect::<Vec<_>>()
            .join(".");
        match self.routine.lookup(&r) {
            None => ctx.out.push(
                Diagnostic::error(
                    codes::UNDECLARED_VARIABLE,
                    format!("\"{name}\" is not a known variable"),
                )
                .with_span(pgzest_diagnostics::Span {
                    start: span.start as usize,
                    end: span.end as usize,
                }),
            ),
            Some(VarRef::Field(..)) => {
                let a = self.analyzer(ctx.catalog);
                if let Err(e) = a.variable_type(&r, span)
                    && e.code != codes::NOT_ANALYZED
                {
                    ctx.out.push(diagnostic(e, span));
                }
            }
            Some(VarRef::Whole(_)) => {}
        }
    }

    /// Targets of an `INTO`: a single record takes the structure of the row.
    fn into(&mut self, into: &IntoTarget, shape: Shape, span: Span, ctx: &mut Ctx) {
        for t in &into.targets {
            self.assign_target(t, span, ctx);
        }
        if let [t] = into.targets.as_slice()
            && let [var] = t.as_slice()
        {
            self.set_record(&var.value, shape);
        }
    }

    /// Targets of a loop over a query: declared outside the loop.
    fn loop_targets(&mut self, targets: &[Ident], shape: Shape, span: Span, ctx: &mut Ctx) {
        for t in targets {
            self.assign_target(std::slice::from_ref(t), span, ctx);
        }
        if let [var] = targets {
            self.set_record(&var.value, shape);
        }
    }

    /// Structure of a record after a row is assigned to it.
    fn set_record(&mut self, name: &str, shape: Shape) {
        let Some(var) = self.routine.var(name) else {
            return;
        };
        if !matches!(var.ty, VarTy::Record(_) | VarTy::Unknown(_)) {
            return;
        }
        let ty = match shape {
            Shape::Known(cols) => VarTy::Record(Some(cols)),
            Shape::Dynamic => {
                VarTy::Unknown(format!("structure of record \"{name}\" (dynamic SQL)"))
            }
            Shape::Invalid => VarTy::Unknown(format!("structure of record \"{name}\"")),
        };
        self.routine.assign(name, ty);
    }

    fn set_shape(&mut self, name: &str, cols: Option<Vec<Column>>) {
        if let Some(cols) = cols {
            self.set_record(name, Shape::Known(cols));
        }
    }

    /// Attributes of a composite type.
    fn shape_of(&self, ty: &Ty, catalog: &Catalog) -> Option<Vec<Column>> {
        let ts = TypeSystem::new(catalog);
        let base = ts.base_type(ty);
        if let Some(rel) = catalog.relations.get(&base) {
            return Some(
                rel.columns
                    .iter()
                    .map(|c| Column {
                        name: c.name.clone(),
                        ty: ts.of_ref(&c.ty),
                        modifiers: Vec::new(),
                        nullable: Nullability::Unknown,
                    })
                    .collect(),
            );
        }
        match ts.def(&base).map(|d| &d.kind) {
            Some(pgzest_catalog::model::TypeKind::Composite { attributes }) => Some(
                attributes
                    .iter()
                    .map(|a| Column {
                        name: a.name.clone(),
                        ty: ts.of_ref(&a.ty),
                        modifiers: Vec::new(),
                        nullable: Nullability::Unknown,
                    })
                    .collect(),
            ),
            _ => None,
        }
    }
}

/// Row assigned to targets.
enum Shape {
    Known(Vec<Column>),
    /// Dynamic SQL: known only at execution.
    Dynamic,
    /// Invalid query (already reported).
    Invalid,
}

struct Ctx<'c, 'o> {
    catalog: &'c Catalog,
    out: &'o mut Vec<Diagnostic>,
}

/// SQL statements of a block and of its nested blocks.
fn collect_sql<'b>(b: &'b Block, out: &mut Vec<&'b Statement>) {
    fn walk<'b>(stmts: &'b [PlStatement], out: &mut Vec<&'b Statement>) {
        for s in stmts {
            match &s.kind {
                PlStatementKind::Sql { statement, .. } => out.push(statement),
                PlStatementKind::Block(b) => collect_sql(b, out),
                PlStatementKind::If {
                    then_branch,
                    elsif_branches,
                    else_branch,
                    ..
                } => {
                    walk(then_branch, out);
                    for (_, b) in elsif_branches {
                        walk(b, out);
                    }
                    if let Some(b) = else_branch {
                        walk(b, out);
                    }
                }
                PlStatementKind::Case {
                    whens, else_branch, ..
                } => {
                    for (_, b) in whens {
                        walk(b, out);
                    }
                    if let Some(b) = else_branch {
                        walk(b, out);
                    }
                }
                PlStatementKind::Loop { body, .. }
                | PlStatementKind::While { body, .. }
                | PlStatementKind::ForRange { body, .. }
                | PlStatementKind::ForQuery { body, .. }
                | PlStatementKind::ForExecute { body, .. }
                | PlStatementKind::ForCursor { body, .. }
                | PlStatementKind::Foreach { body, .. } => walk(body, out),
                _ => {}
            }
        }
    }
    walk(&b.statements, out);
    if let Some(exc) = &b.exception {
        for h in &exc.handlers {
            walk(&h.statements, out);
        }
    }
}

/// First column reference of an expression, outside subqueries.
fn first_column_ref(e: &Expr) -> Option<&ColumnRef> {
    use pgzest_ast::expr::ExprKind;
    match &e.kind {
        ExprKind::Column(c) => Some(c),
        ExprKind::Subquery(_) | ExprKind::Exists { .. } | ExprKind::InSubquery { .. } => None,
        _ => e.children().into_iter().find_map(first_column_ref),
    }
}

/// Number of `%` in a `RAISE` format (`%%` is a literal `%`).
fn raise_placeholders(m: &str) -> Option<usize> {
    let mut n = 0;
    let mut chars = m.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            if chars.peek() == Some(&'%') {
                chars.next();
            } else {
                n += 1;
            }
        }
    }
    Some(n)
}

impl Analyzer<'_> {
    /// Type of a routine variable designated by `r` (for `%TYPE` and
    /// assignment targets).
    pub(crate) fn variable_type(&self, r: &ColumnRef, span: Span) -> Result<Ty, SemaError> {
        let v = self
            .routine
            .as_ref()
            .and_then(|routine| routine.lookup(r))
            .ok_or_else(|| {
                SemaError::new(
                    codes::UNDECLARED_VARIABLE,
                    format!("\"{}\" is not a known variable", r.column.value),
                    span,
                )
            })?;
        self.variable(v, span).map(|t| t.ty)
    }

    /// `FROM` source made of a relation (policy target).
    pub(crate) fn relation_source(
        &self,
        name: &ObjectName,
        alias: Option<&Ident>,
    ) -> Result<Source, SemaError> {
        let (schema, n) = split_name(name);
        let rel = self.ts.relation(schema, n, &self.path).ok_or_else(|| {
            SemaError::new(
                codes::TABLE_NOT_FOUND,
                format!("relation \"{n}\" does not exist"),
                Span::DUMMY,
            )
        })?;
        let columns = rel
            .columns
            .iter()
            .map(|c| Column {
                name: c.name.clone(),
                ty: self.ts.of_ref(&c.ty),
                modifiers: crate::query::ref_modifiers(&c.ty),
                nullable: if c.not_null {
                    Nullability::NonNull
                } else {
                    Nullability::Nullable
                },
            })
            .collect();
        let mut source = Source::new(
            Some(alias.map_or_else(|| n.to_string(), |a| a.value.clone())),
            columns,
        )
        .with_system_columns();
        source.rowtype = Some(rel.name.clone());
        if alias.is_none() {
            source.relation = Some(rel.name.clone());
        }
        Ok(source)
    }
}
