//! Vérification des statements d'un fichier par l'analyse sémantique : ce
//! que PostgreSQL refuserait en exécutant le statement, ou en compilant le
//! corps d'une routine.
//!
//! Deux moments, comme dans PostgreSQL :
//!
//! - [`check_applied`] : ce qui s'exécute ou se valide quand le statement
//!   est appliqué — requêtes, vues, policies, `COPY`, bloc `DO`, corps
//!   `LANGUAGE sql` (validés à la création) — contre l'état du catalogue à
//!   ce point ;
//! - [`check_deferred`] : un corps PL/pgSQL, dont les requêtes ne sont
//!   analysées qu'à l'exécution, donc contre l'état final.
//!
//! [`check_statements`] enchaîne les deux contre un même catalogue.

use std::collections::BTreeMap;

use pgweave_ast::Span;
use pgweave_ast::ddl::{
    CreateFunction, CreatePolicy, DdlStatement, FunctionBody, FunctionParam, ParamMode, ReturnType,
};
use pgweave_ast::expr::{ColumnRef, Expr, Ident, ObjectName};
use pgweave_ast::plpgsql::{
    Block, Declaration, IntoTarget, PlStatement, PlStatementKind, PlpgsqlBody, VarType,
};
use pgweave_ast::stmt::{SelectStmt, Statement};
use pgweave_ast::utility::{CopySource, UtilityKind};
use pgweave_catalog::{Catalog, QName, SearchPath};
use pgweave_diagnostics::Diagnostic;

use crate::expr::{Analyzer, split_name};
use crate::routine::{Language, Routine, VarRef, VarTy};
use crate::scope::{Column, Nullability, Scope, SemaError, Source};
use crate::types::{Ty, TypeSystem, is_polymorphic, pg};

// ── Diagnostics ───────────────────────────────────────────────────────────────

/// Diagnostic d'une erreur d'analyse ; sans position propre, il prend celle
/// de `fallback` (le statement).
fn diagnostic(e: SemaError, fallback: Span) -> Diagnostic {
    let span = if e.span == Span::DUMMY {
        fallback
    } else {
        e.span
    };
    let mut d = match e.code {
        // Ce que l'analyse ne couvre pas est dit, sans être une erreur.
        "not-analyzed" => Diagnostic::info(e.code, e.message),
        _ => Diagnostic::error(e.code, e.message),
    };
    d.hint = e.hint;
    if span == Span::DUMMY {
        d
    } else {
        d.with_span(pgweave_diagnostics::Span {
            start: span.start as usize,
            end: span.end as usize,
        })
    }
}

/// Un diagnostic par erreur, rattaché au statement à défaut de mieux.
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

// ── Points d'entrée ───────────────────────────────────────────────────────────

/// Vérification pendant le rejeu du catalogue ([`StatementChecker`]).
///
/// [`StatementChecker`]: pgweave_catalog::StatementChecker
pub struct SemaChecker;

impl SemaChecker {
    pub fn shared() -> std::sync::Arc<dyn pgweave_catalog::StatementChecker> {
        std::sync::Arc::new(SemaChecker)
    }
}

impl pgweave_catalog::StatementChecker for SemaChecker {
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

/// Tous les statements contre un même catalogue (fichier de requêtes,
/// fichier isolé, état final d'un projet).
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

/// `SET search_path` : le chemin des statements suivants.
pub fn search_path_change(stmt: &Statement) -> Option<SearchPath> {
    let Statement::Utility(u) = stmt else {
        return None;
    };
    match &u.kind {
        UtilityKind::Set {
            name,
            value: pgweave_ast::ddl::SettingValue::Values(v),
            ..
        } if name.eq_ignore_ascii_case("search_path") => {
            Some(SearchPath::new(v.iter().filter(|s| *s != "$user").cloned()))
        }
        _ => None,
    }
}

/// Le corps de ce statement n'est analysé par PostgreSQL qu'à l'exécution :
/// il est vérifié par [`check_deferred`].
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

/// Ce que PostgreSQL vérifie en appliquant `stmt`, contre `catalog` (l'état
/// après application pour un DDL).
pub fn check_applied(
    catalog: &Catalog,
    path: &SearchPath,
    stmt: &Statement,
    out: &mut Vec<Diagnostic>,
) {
    let span = pgweave_ast::stmt_span(stmt).unwrap_or(Span::DUMMY);
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
                        pgweave_ast::ddl::TableConstraint::Check { expr, .. } => Some(expr),
                        _ => None,
                    })
                    .collect();
                sink.push(a.table_expressions(&t.name, &t.columns, &checks), t.span);
            }
            DdlStatement::AlterTable(t) => {
                use pgweave_ast::ddl::{AlterColumnAction, AlterTableOp, TableConstraint};
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
            DdlStatement::CreateFunction(f) => {
                empty_body(
                    &f.body,
                    &pgweave_ast::emit::emit_object_name(&f.name),
                    f.span,
                    sink.out,
                );
                if !matches!(f.body, FunctionBody::Parsed(_)) {
                    sql_routine(
                        catalog, path, &f.name, &f.params, &f.body, sink.out, &mut None,
                    );
                }
            }
            DdlStatement::CreateProcedure(p) => {
                empty_body(
                    &p.body,
                    &pgweave_ast::emit::emit_object_name(&p.name),
                    p.span,
                    sink.out,
                );
                if !matches!(p.body, FunctionBody::Parsed(_)) {
                    sql_routine(
                        catalog, path, &p.name, &p.params, &p.body, sink.out, &mut None,
                    );
                }
            }
            _ => {}
        },
    }
}

/// Corps PL/pgSQL de `stmt`, contre l'état du catalogue où il s'exécutera.
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
    collect: &mut Option<Vec<BodyQuery>>,
) {
    match stmt {
        Statement::Ddl(DdlStatement::CreateFunction(f)) => {
            if let FunctionBody::Parsed(b) = &f.body {
                plpgsql_function(catalog, path, f, b, out, collect);
            }
        }
        Statement::Ddl(DdlStatement::CreateProcedure(p)) => {
            if let FunctionBody::Parsed(b) = &p.body {
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

/// Requête d'un corps de routine, telle que PostgreSQL la prépare à
/// l'exécution : chaque variable y est un paramètre.
#[derive(Debug, Clone)]
pub struct BodyQuery {
    pub statement: Statement,
    /// Colonnes décrites, ou l'erreur (code et message).
    pub result: Result<Vec<crate::query::OutputColumn>, String>,
    /// Références aux variables : position de l'expression et type du
    /// paramètre qui la remplace.
    pub var_refs: Vec<(Span, Ty)>,
    /// SQL qui recrée ce que le corps voit en plus du schéma : ses DDL
    /// (tables temporaires…) et les tables de transition du trigger.
    pub setup: Vec<String>,
}

/// Requêtes du corps de la routine créée par `stmt` (PL/pgSQL ou SQL),
/// analysées dans sa portée, contre `catalog`.
pub fn body_queries(catalog: &Catalog, path: &SearchPath, stmt: &Statement) -> Vec<BodyQuery> {
    let mut collect = Some(Vec::new());
    let mut ignored = Vec::new();
    match stmt {
        Statement::Ddl(DdlStatement::CreateFunction(f))
            if !matches!(f.body, FunctionBody::Parsed(_)) =>
        {
            sql_routine(
                catalog,
                path,
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
                path,
                &p.name,
                &p.params,
                &p.body,
                &mut ignored,
                &mut collect,
            );
        }
        _ => deferred(catalog, path, stmt, &mut ignored, &mut collect),
    }
    collect.unwrap_or_default()
}

/// Parcourt `body` avec `w`, en recueillant ses requêtes si demandé.
fn run(
    mut w: Walker,
    body: &PlpgsqlBody,
    out: &mut Vec<Diagnostic>,
    collect: &mut Option<Vec<BodyQuery>>,
) {
    if collect.is_some() {
        w.queries = Some(Vec::new());
    }
    w.body(body, out);
    if let (Some(c), Some(q)) = (collect.as_mut(), w.queries.take()) {
        c.extend(q);
    }
}

// ── Statements simples ────────────────────────────────────────────────────────

/// Corps vide : la seule chose certaine qu'on puisse dire d'un corps non
/// analysé (PL/pgSQL le refuse : « syntax error at end of input »).
fn empty_body(body: &FunctionBody, name: &str, span: Span, out: &mut Vec<Diagnostic>) {
    if let FunctionBody::Raw(raw) = body
        && raw.trim().is_empty()
    {
        let d = Diagnostic::warning(
            pgweave_diagnostics::codes::EMPTY_FUNCTION_BODY,
            format!("function '{name}' has an empty body"),
        )
        .with_hint("add a BEGIN … END block or a RETURN statement");
        out.push(if span == Span::DUMMY {
            d
        } else {
            d.with_span(pgweave_diagnostics::Span {
                start: span.start as usize,
                end: span.end as usize,
            })
        });
    }
}

impl Analyzer<'_> {
    /// `COPY table (colonnes)`, `ANALYZE table (colonnes)` : la table et ses
    /// colonnes existent.
    fn table_columns(&mut self, name: &ObjectName, columns: &[Ident]) -> Result<(), SemaError> {
        let (schema, n) = split_name(name);
        let rel = self.ts.relation(schema, n, &self.path).ok_or_else(|| {
            SemaError::new(
                "table-not-found",
                format!("relation \"{n}\" does not exist"),
                Span::DUMMY,
            )
        })?;
        for c in columns {
            if !rel.columns.iter().any(|x| x.name == c.value) {
                return Err(SemaError::new(
                    "column-not-found",
                    format!("column \"{}\" of relation \"{n}\" does not exist", c.value),
                    Span::DUMMY,
                ));
            }
        }
        Ok(())
    }

    /// Expressions d'une table, dans son contexte (`transformColumnDefinition`,
    /// `cookDefault`, `cookConstraint`) : `DEFAULT` sans colonne, convertible
    /// vers la colonne ; expression générée sur les colonnes non générées,
    /// convertible vers la colonne ; `CHECK` booléen sur les colonnes.
    fn table_expressions(
        &mut self,
        table: &ObjectName,
        columns: &[pgweave_ast::ddl::ColumnDef],
        checks: &[&Expr],
    ) -> Result<(), SemaError> {
        // Table absente : son rejeu a échoué, déjà signalé.
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
                        "generated-column",
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
                if let pgweave_ast::ddl::ColumnConstraint::Check(e) = c {
                    self.boolean(e, &scope)?;
                }
            }
        }
        for e in checks {
            self.boolean(e, &scope)?;
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

    /// `DEFAULT` : aucune colonne, et une valeur convertible (affectation)
    /// vers celle de la colonne.
    fn default_expr(
        &mut self,
        column: &str,
        target: Option<&Column>,
        e: &Expr,
    ) -> Result<(), SemaError> {
        if first_column_ref(e).is_some() {
            return Err(SemaError::new(
                "column-not-found",
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
            "type-mismatch",
            format!(
                "column \"{column}\" is of type {} but {what} expression is of type {}",
                target.name, ty.name
            ),
            span,
        ))
    }

    /// `CREATE DOMAIN … CHECK (VALUE …)` : `VALUE` est la valeur du type de
    /// base.
    fn domain_checks(&mut self, d: &pgweave_ast::ddl::CreateDomain) -> Result<(), SemaError> {
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

    /// `CREATE POLICY` : `USING` et `WITH CHECK` sont des conditions sur
    /// une ligne de la table.
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

/// Type d'une variable déclarée avec le type écrit `dt`.
fn var_ty(a: &Analyzer, dt: &pgweave_ast::types::DataType, span: Span) -> Result<VarTy, SemaError> {
    let ty = a.data_type(dt, span)?;
    Ok(classify(&a.ts, ty))
}

fn classify(ts: &TypeSystem, ty: Ty) -> VarTy {
    if is_polymorphic(&ty) {
        VarTy::Unknown(format!("type de l'argument polymorphe {}", ty.name))
    } else if ty == pg("record") {
        VarTy::Record(None)
    } else if ts.is_complex(&ts.base_type(&ty)) {
        VarTy::Row(ty)
    } else {
        VarTy::Scalar(ty)
    }
}

/// Portée des paramètres d'une routine : nommés (dans le bloc portant le nom
/// de la routine) et positionnels.
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
        // `$n` : tous les arguments en PL/pgSQL, ceux d'entrée en SQL.
        let positional = language == Language::Plpgsql || input;
        // Un paramètre `OUT` n'est une variable qu'en PL/pgSQL.
        let named = language == Language::Plpgsql || input;
        let ty = match var_ty(a, &p.data_type, span) {
            Ok(t) => t,
            Err(e) => {
                out.push(diagnostic(e, span));
                VarTy::Unknown("type de paramètre introuvable".into())
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

/// Corps `LANGUAGE sql` : chaque statement, les paramètres en portée.
fn sql_routine(
    catalog: &Catalog,
    path: &SearchPath,
    name: &ObjectName,
    params: &[FunctionParam],
    body: &FunctionBody,
    out: &mut Vec<Diagnostic>,
    collect: &mut Option<Vec<BodyQuery>>,
) {
    let (_, fname) = split_name(name);
    let stmts: Vec<Statement> = match body {
        FunctionBody::Sql(stmts) => stmts.clone(),
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
            return;
        }
        FunctionBody::Raw(_) | FunctionBody::Parsed(_) => return,
    };
    // Tables créées par le corps : visibles des statements suivants.
    let overlay = body_overlay(catalog, path, stmts.iter());
    let cat = overlay.as_ref().unwrap_or(catalog);
    for stmt in &stmts {
        let span = pgweave_ast::stmt_span(stmt).unwrap_or(Span::DUMMY);
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
            c.push(BodyQuery {
                statement: stmt.clone(),
                result: result
                    .as_ref()
                    .map(|d| d.columns.clone())
                    .map_err(|e| format!("[{}] {}", e.code, e.message)),
                var_refs: a.var_refs.clone(),
                setup: stmts
                    .iter()
                    .filter(|s| matches!(s, Statement::Ddl(_)))
                    .map(pgweave_ast::emit::emit_statement)
                    .collect(),
            });
        }
        if let Err(e) = result {
            out.push(diagnostic(e, span));
        }
    }
}

/// Catalogue augmenté des DDL d'un corps (tables temporaires…), s'il en
/// contient.
fn body_overlay<'s>(
    catalog: &Catalog,
    path: &SearchPath,
    stmts: impl Iterator<Item = &'s Statement>,
) -> Option<Catalog> {
    let ddl: Vec<&Statement> = stmts.filter(|s| matches!(s, Statement::Ddl(_))).collect();
    if ddl.is_empty() {
        return None;
    }
    let mut replay = pgweave_catalog::Replay::new(catalog.clone());
    replay.search_path = path.clone();
    let mut ignored = Vec::new();
    for s in ddl {
        replay.apply(s, &mut ignored);
    }
    Some(replay.catalog)
}

/// Contexte d'une fonction trigger : la table d'un de ses triggers.
#[derive(Debug, Clone)]
struct TriggerContext {
    table: QName,
    row_level: bool,
    /// Tables de transition, visibles comme des relations.
    transition: Vec<String>,
}

/// Contextes d'une fonction trigger, un par table qui la déclenche.
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

/// Nom qualifié d'une routine créée sous `name`.
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
    collect: &mut Option<Vec<BodyQuery>>,
) {
    let (_, name) = split_name(&f.name);
    let make = |out: &mut Vec<Diagnostic>| {
        let mut a = Analyzer::new(TypeSystem::new(catalog), path.clone());
        let mut r = routine_scope(&mut a, Language::Plpgsql, name, &f.params, out, f.span);
        if let ReturnType::Table(cols) = &f.return_type {
            for (n, dt) in cols {
                let ty = var_ty(&a, dt, f.span)
                    .unwrap_or_else(|_| VarTy::Unknown("type de colonne introuvable".into()));
                r.declare(&n.value, ty);
            }
        }
        r
    };
    let is_event_trigger = matches!(
        &f.return_type,
        ReturnType::Scalar(pgweave_ast::types::DataType::Custom(n)) if n.eq_ignore_ascii_case("event_trigger")
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
    let contexts = trigger_contexts(catalog, &routine_qname(&f.name, path));
    if contexts.is_empty() {
        out.push(
            Diagnostic::warning(
                pgweave_diagnostics::codes::TRIGGER_NO_CONTEXT,
                format!(
                    "trigger function '{name}' has no associated CREATE TRIGGER in the \
                     analyzed files: NEW/OLD cannot be verified"
                ),
            )
            .with_span(pgweave_diagnostics::Span {
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
        // NEW et OLD sans table : ce qui en dépend n'est pas vérifiable, et
        // vient d'être dit une fois pour la fonction.
        out.extend(
            d.into_iter()
                .filter(|d| !(d.code == "not-analyzed" && d.message.contains(NO_TRIGGER_TABLE))),
        );
        return;
    }
    // Une fonction partagée entre plusieurs tables est vérifiée pour
    // chacune : NEW et OLD y prennent le type de la table qui déclenche.
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

const NO_TRIGGER_TABLE: &str = "table du trigger inconnue";

/// Fusionne les diagnostics des passes « une par table » : un diagnostic
/// commun à toutes ne dépend pas de la table et n'est émis qu'une fois ;
/// les autres nomment les tables concernées.
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

/// Parcours d'un corps PL/pgSQL (`pl_comp.c` pour les déclarations, puis
/// chaque expression et requête analysée comme PostgreSQL le fait à
/// l'exécution, variables en portée).
struct Walker<'c> {
    catalog: &'c Catalog,
    path: SearchPath,
    routine: Routine,
    /// Fonction trigger : `Some(Some(ctx))` avec sa table, `Some(None)` si
    /// aucune table n'est connue.
    trigger: Option<Option<TriggerContext>>,
    /// Colonnes des requêtes de curseurs déclarés, par nom.
    cursors: BTreeMap<String, Option<Vec<Column>>>,
    /// Requêtes analysées, si elles sont recueillies ([`body_queries`]).
    queries: Option<Vec<BodyQuery>>,
    /// Contexte des requêtes recueillies (voir [`BodyQuery::setup`]).
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
            queries: None,
            setup: Vec::new(),
        }
    }

    fn body(&mut self, body: &PlpgsqlBody, out: &mut Vec<Diagnostic>) {
        // Les DDL du corps (tables temporaires…) sont visibles de ses requêtes.
        let mut ddl = Vec::new();
        collect_sql(&body.block, &mut ddl);
        if self.queries.is_some() {
            self.setup = ddl
                .iter()
                .filter(|s| matches!(s, Statement::Ddl(_)))
                .map(|s| pgweave_ast::emit::emit_statement(s))
                .collect();
            if let Some(Some(ctx)) = &self.trigger {
                let table = format!(
                    "{}.{}",
                    pgweave_catalog::model::quote_sql_ident(&ctx.table.schema),
                    pgweave_catalog::model::quote_sql_ident(&ctx.table.name)
                );
                for n in &ctx.transition {
                    self.setup.push(format!(
                        "CREATE TEMP TABLE {} (LIKE {table})",
                        pgweave_catalog::model::quote_sql_ident(n)
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

    /// Variables d'une fonction trigger (`plpgsql_compile`, `TG_*`).
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
            // Trigger de niveau instruction : NEW et OLD sont NULL.
            Some(_) => VarTy::Record(None),
            None => VarTy::Unknown(format!("NEW/OLD : {NO_TRIGGER_TABLE}")),
        };
        self.routine.declare("new", row.clone());
        self.routine.declare("old", row);
    }

    /// Analyseur d'une expression ou requête du corps.
    fn analyzer<'a>(&self, catalog: &'a Catalog) -> Analyzer<'a> {
        let mut a = Analyzer::new(TypeSystem::new(catalog), self.path.clone());
        a.routine = Some(self.routine.clone());
        // Tables de transition d'un trigger : des relations nommées.
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
                // La valeur initiale voit les variables déjà déclarées.
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
                    VarTy::Unknown(format!("type de la variable « {} »", v.name.value))
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
                            "undeclared-variable",
                            format!("variable \"{target}\" does not exist"),
                        ));
                        self.routine
                            .declare(&name.value, VarTy::Unknown("alias invalide".into()));
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
                        .unwrap_or_else(|_| VarTy::Unknown("type de paramètre de curseur".into()));
                    frame.vars.push(crate::routine::Var {
                        name: p.value.clone(),
                        ty,
                    });
                }
                if let Some(r) = a.routine.as_mut() {
                    r.frames.push(frame);
                }
                let cols = match a.select(query, None, true) {
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
            "type-not-found",
            format!("relation \"{n}\" does not exist"),
            span,
        ))
    }

    /// `table.colonne%TYPE` ou `variable%TYPE`.
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
                    "type-not-found",
                    format!("type \"{}%TYPE\" does not exist", parts.join(".")),
                    span,
                ));
            }
        };
        // `variable.champ%TYPE`.
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
                    "table-not-found",
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
                    "column-not-found",
                    format!("column \"{col}\" of relation \"{rel}\" does not exist"),
                    span,
                )
            })?;
        match ts.of_ref(&column.ty) {
            Some(ty) => Ok(classify(&ts, ty)),
            None => Ok(VarTy::Unknown(format!(
                "type de la colonne « {rel}.{col} »"
            ))),
        }
    }

    fn statements(&mut self, stmts: &[PlStatement], ctx: &mut Ctx) {
        for s in stmts {
            let first = ctx.out.len();
            self.statement(s, ctx);
            if s.span != Span::DUMMY {
                for d in &mut ctx.out[first..] {
                    if d.span.is_none() {
                        d.span = Some(pgweave_diagnostics::Span {
                            start: s.span.start as usize,
                            end: s.span.end as usize,
                        });
                    }
                }
            }
        }
    }

    /// Expression du corps : `SELECT expr`, variables en portée.
    fn expr(&mut self, e: &Expr, ctx: &mut Ctx) -> Option<crate::expr::Typed> {
        let mut a = self.analyzer(ctx.catalog);
        match a.expr(e, &Scope::default()) {
            Ok(t) => Some(t),
            Err(err) => {
                ctx.out.push(diagnostic(err, e.span));
                None
            }
        }
    }

    /// Requête du corps : ses colonnes, si elle est valide.
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

    /// Recueille une requête analysée, si demandé ([`body_queries`]).
    fn record(
        &mut self,
        statement: impl FnOnce() -> Statement,
        result: &Result<Vec<crate::query::OutputColumn>, SemaError>,
        a: &Analyzer,
    ) {
        if let Some(q) = self.queries.as_mut() {
            q.push(BodyQuery {
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
                // Record affecté d'une valeur composite : sa structure.
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
                            if let Err(e) = a.describe(statement) {
                                ctx.out.push(diagnostic(e, s.span));
                            }
                        }
                        None
                    }
                    // DDL : appliqué au catalogue du corps (voir `body`).
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
                // `pl_gram.y` : le nombre de `%` du format égale celui des
                // arguments, vérifié à la compilation.
                if let Some(m) = message
                    && (!args.is_empty() || m.contains('%'))
                    && let Some(n) = raise_placeholders(m)
                    && n != args.len()
                {
                    ctx.out.push(Diagnostic::error(
                        "raise-arguments",
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
                // La variable de boucle est déclarée `integer`, dans un bloc
                // propre à la boucle.
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
                    Some(None) => {
                        VarTy::Unknown(format!("requête du curseur « {} »", cursor.value))
                    }
                    None => {
                        ctx.out.push(Diagnostic::error(
                            "undeclared-variable",
                            format!("cursor \"{}\" does not exist", cursor.value),
                        ));
                        VarTy::Unknown(format!("curseur « {} »", cursor.value))
                    }
                };
                // La variable de boucle est un record déclaré par la boucle.
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

    /// Cible d'affectation : une variable visible, ou un champ existant
    /// d'une variable ligne (`pl_gram.y` refuse le reste à la compilation).
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
                    "undeclared-variable",
                    format!("\"{name}\" is not a known variable"),
                )
                .with_span(pgweave_diagnostics::Span {
                    start: span.start as usize,
                    end: span.end as usize,
                }),
            ),
            Some(VarRef::Field(..)) => {
                let a = self.analyzer(ctx.catalog);
                if let Err(e) = a.variable_type(&r, span)
                    && e.code != "not-analyzed"
                {
                    ctx.out.push(diagnostic(e, span));
                }
            }
            Some(VarRef::Whole(_)) => {}
        }
    }

    /// Cibles d'un `INTO` : un record unique prend la structure de la ligne.
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

    /// Cibles d'une boucle sur requête : déclarées hors de la boucle.
    fn loop_targets(&mut self, targets: &[Ident], shape: Shape, span: Span, ctx: &mut Ctx) {
        for t in targets {
            self.assign_target(std::slice::from_ref(t), span, ctx);
        }
        if let [var] = targets {
            self.set_record(&var.value, shape);
        }
    }

    /// Structure d'un record après affectation d'une ligne.
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
                VarTy::Unknown(format!("structure du record « {name} » (SQL dynamique)"))
            }
            Shape::Invalid => VarTy::Unknown(format!("structure du record « {name} »")),
        };
        self.routine.assign(name, ty);
    }

    fn set_shape(&mut self, name: &str, cols: Option<Vec<Column>>) {
        if let Some(cols) = cols {
            self.set_record(name, Shape::Known(cols));
        }
    }

    /// Attributs d'un type composite.
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
            Some(pgweave_catalog::model::TypeKind::Composite { attributes }) => Some(
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

/// Ligne affectée à des cibles.
enum Shape {
    Known(Vec<Column>),
    /// SQL dynamique : connue à l'exécution seulement.
    Dynamic,
    /// Requête invalide (déjà signalée).
    Invalid,
}

struct Ctx<'c, 'o> {
    catalog: &'c Catalog,
    out: &'o mut Vec<Diagnostic>,
}

/// Statements SQL d'un bloc et de ses blocs imbriqués.
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

/// Première référence de colonne d'une expression, hors sous-requêtes.
fn first_column_ref(e: &Expr) -> Option<&ColumnRef> {
    use pgweave_ast::expr::ExprKind;
    match &e.kind {
        ExprKind::Column(c) => Some(c),
        ExprKind::Subquery(_) | ExprKind::Exists { .. } | ExprKind::InSubquery { .. } => None,
        _ => e.children().into_iter().find_map(first_column_ref),
    }
}

/// Nombre de `%` d'un format de `RAISE` (`%%` est un `%` littéral).
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
    /// Type d'une variable de routine désignée par `r` (pour `%TYPE` et les
    /// cibles d'affectation).
    pub(crate) fn variable_type(&self, r: &ColumnRef, span: Span) -> Result<Ty, SemaError> {
        let v = self
            .routine
            .as_ref()
            .and_then(|routine| routine.lookup(r))
            .ok_or_else(|| {
                SemaError::new(
                    "undeclared-variable",
                    format!("\"{}\" is not a known variable", r.column.value),
                    span,
                )
            })?;
        self.variable(v, span).map(|t| t.ty)
    }

    /// Source de `FROM` formée d'une relation (cible de policy).
    pub(crate) fn relation_source(
        &self,
        name: &ObjectName,
        alias: Option<&Ident>,
    ) -> Result<Source, SemaError> {
        let (schema, n) = split_name(name);
        let rel = self.ts.relation(schema, n, &self.path).ok_or_else(|| {
            SemaError::new(
                "table-not-found",
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
