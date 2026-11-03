//! Query analysis (`analyze.c`, `parse_clause.c`, `parse_target.c`):
//! `FROM` scope, named and typed output columns, parameters.

use pgweave_ast::Span;
use pgweave_ast::expr::{Expr, ExprKind, Ident};
use pgweave_ast::stmt::{
    CteQuery, InsertSource, JoinConstraint, JoinKind, SelectItem, SelectStmt, Statement, TableRef,
    TableSource,
};
use pgweave_ast::utility::UtilityKind;
use pgweave_catalog::QName;
use pgweave_catalog::model::{ArgMode, RelationKind, TypeKind};
use pgweave_parser::normalize_select;

use crate::coerce::CoercionContext;
use crate::expr::{Analyzer, split_name};
use crate::scope::{Column, Nullability, Scope, SemaError, Source};
use crate::types::{Ty, pg};

/// Output column of a query.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputColumn {
    pub name: String,
    pub ty: Ty,
    /// Type modifiers (`exprTypmod`), empty if it has none.
    pub modifiers: Vec<String>,
    pub nullable: Nullability,
}

impl From<&OutputColumn> for Column {
    fn from(c: &OutputColumn) -> Self {
        Column {
            name: c.name.clone(),
            ty: Some(c.ty.clone()),
            modifiers: c.modifiers.clone(),
            nullable: c.nullable,
        }
    }
}

/// Name PostgreSQL gives an output column without an alias
/// (`FigureColname`); `?column?` by default.
pub fn figure_colname(e: &Expr) -> String {
    figure(e).map_or_else(|| "?column?".to_string(), |(n, _)| n)
}

/// Name and strength (2: real name, 1: type name) — `FigureColnameInternal`.
fn figure(e: &Expr) -> Option<(String, u8)> {
    match &e.kind {
        ExprKind::Column(c) => Some((c.column.value.clone(), 2)),
        ExprKind::FieldAccess { root, path } => match path.last() {
            Some(p) if p.starts_with('.') => Some((p[1..].to_string(), 2)),
            Some(_) => figure(root),
            None => figure(root),
        },
        ExprKind::FunctionCall { name, .. } => {
            let (_, n) = split_name(name);
            Some((n.to_string(), 2))
        }
        ExprKind::Cast { expr, data_type } => match figure(expr) {
            Some((n, 2)) => Some((n, 2)),
            _ => Some((cast_type_name(data_type), 1)),
        },
        ExprKind::Collate { expr, .. } => figure(expr),
        // The name of the `ELSE` if it is real, otherwise `case`.
        ExprKind::Case { else_clause, .. } => match else_clause.as_deref().and_then(figure) {
            Some((n, 2)) => Some((n, 2)),
            _ => Some(("case".into(), 1)),
        },
        ExprKind::Exists { .. } => Some(("exists".into(), 2)),
        ExprKind::Subquery(q) => q.columns.first().and_then(|item| match item {
            SelectItem::Expr { alias: Some(a), .. } => Some((a.value.clone(), 2)),
            SelectItem::Expr { expr, .. } => figure(expr).filter(|(_, s)| *s == 2),
            _ => None,
        }),
        ExprKind::Interval(_) => Some(("interval".into(), 1)),
        ExprKind::AtTimeZone { .. } => Some(("timezone".into(), 2)),
        ExprKind::Tuple(_) => Some(("row".into(), 2)),
        _ => None,
    }
}

/// Last segment of the type name as the grammar writes it (`int` →
/// `int4`, `varchar(3)` → `varchar`).
fn cast_type_name(dt: &pgweave_ast::types::DataType) -> String {
    use pgweave_ast::types::DataType::*;
    match dt {
        SmallInt => "int2".into(),
        Integer => "int4".into(),
        BigInt => "int8".into(),
        Numeric { .. } => "numeric".into(),
        Real => "float4".into(),
        DoublePrecision => "float8".into(),
        Text => "text".into(),
        Varchar(_) => "varchar".into(),
        Char(_) => "bpchar".into(),
        Date => "date".into(),
        Time(_) => "time".into(),
        TimeTz(_) => "timetz".into(),
        Timestamp(_) => "timestamp".into(),
        TimestampTz(_) => "timestamptz".into(),
        Interval => "interval".into(),
        Boolean => "bool".into(),
        Bytea => "bytea".into(),
        Json => "json".into(),
        Jsonb => "jsonb".into(),
        Uuid => "uuid".into(),
        Array(inner) => cast_type_name(inner),
        Custom(n) | Modified { name: n, .. } => n.rsplit('.').next().unwrap_or(n).to_string(),
    }
}

/// What a statement describes: returned columns and parameters.
#[derive(Debug, Clone, PartialEq)]
pub struct Description {
    pub columns: Vec<OutputColumn>,
    pub params: Vec<crate::expr::Param>,
}

impl<'c> Analyzer<'c> {
    /// Analyzes a DML statement: returned columns (empty without
    /// `RETURNING`) and parameter types.
    pub fn describe(&mut self, stmt: &Statement) -> Result<Description, SemaError> {
        let columns = match stmt {
            Statement::Select(s) => self.select(s, None, true)?,
            Statement::Insert(s) => self.insert(s)?,
            Statement::Update(s) => self.update(s)?,
            Statement::Delete(s) => self.delete(s)?,
            Statement::Utility(u) => match &u.kind {
                UtilityKind::Call { name, args } => self.call(name, args, u.span)?,
                _ => return Err(SemaError::not_analyzed("utility statement", u.span)),
            },
            Statement::Ddl(_) => {
                return Err(SemaError::not_analyzed("non-DML statement", Span::DUMMY));
            }
        };
        Ok(Description {
            columns,
            params: self.params.clone(),
        })
    }

    /// Registers the CTEs of a `WITH`, visible to the rest of the statement.
    fn with(
        &mut self,
        ctes: &[pgweave_ast::stmt::Cte],
        recursive: bool,
        outer: Option<&Scope>,
    ) -> Result<(), SemaError> {
        for cte in ctes {
            let name = cte.name.value.clone();
            let query = match &cte.query {
                CteQuery::Select(q) => q,
                // Data-modifying body: its rows are those of its `RETURNING`.
                CteQuery::Modify(stmt) => {
                    let cols = match &**stmt {
                        Statement::Insert(s) => self.insert(s)?,
                        Statement::Update(s) => self.update(s)?,
                        Statement::Delete(s) => self.delete(s)?,
                        _ => return Err(SemaError::not_analyzed("CTE body", Span::DUMMY)),
                    };
                    self.ctes.push((name, rename(&cols, &cte.columns)));
                    continue;
                }
            };
            // Recursive: the non-recursive term fixes the types.
            if recursive && !query.set_ops.is_empty() {
                let mut base = (**query).clone();
                base.set_ops.clear();
                base.order_by.clear();
                base.limit = None;
                base.offset = None;
                let cols = self.select(&base, outer, false)?;
                self.ctes.push((name.clone(), rename(&cols, &cte.columns)));
            }
            let cols = self.select(query, outer, true)?;
            if recursive {
                self.ctes.retain(|(n, _)| *n != name);
            }
            self.ctes.push((name, rename(&cols, &cte.columns)));
        }
        Ok(())
    }

    /// Columns of a `SELECT` query (with its set operations).
    /// `resolve_unknowns`: an untyped literal in the output becomes `text`
    /// (except in a set operation branch).
    pub(crate) fn select(
        &mut self,
        stmt: &SelectStmt,
        outer: Option<&Scope>,
        resolve_unknowns: bool,
    ) -> Result<Vec<OutputColumn>, SemaError> {
        let saved = self.ctes.len();
        let result = self.select_inner(stmt, outer, resolve_unknowns);
        self.ctes.truncate(saved);
        result
    }

    fn select_inner(
        &mut self,
        stmt: &SelectStmt,
        outer: Option<&Scope>,
        resolve_unknowns: bool,
    ) -> Result<Vec<OutputColumn>, SemaError> {
        let n = normalize_select(stmt.clone());
        self.with(&n.with, n.with_recursive, outer)?;

        let mut scope = match outer {
            Some(p) => Scope::child(p),
            None => Scope::default(),
        };
        for t in &n.from {
            let src = self.table_ref(t, &scope)?;
            scope.push(src);
        }
        for j in &n.joins {
            self.join(j, &mut scope)?;
        }
        if let Some(w) = &n.where_clause {
            self.boolean(w, &scope)?;
        }
        for g in &n.group_by {
            self.group_or_order(g, &n.columns, &scope, Clause::GroupBy)?;
        }
        if let Some(h) = &n.having {
            self.boolean(h, &scope)?;
        }
        // `WINDOW w AS (…)`: partitions and sorts over the `FROM` scope.
        for w in &n.windows {
            for x in w.spec.exprs() {
                self.expr(x, &scope)?;
            }
        }

        let leaf = !n.set_ops.is_empty();
        let mut columns = if n.values.is_empty() {
            self.projection(&n.columns, &scope, resolve_unknowns && !leaf)?
        } else {
            self.values(&n.values, &scope)?
        };

        for d in &n.distinct_on {
            self.group_or_order(d, &n.columns, &scope, Clause::DistinctOn)?;
        }

        // Set operations: common type column by column, from left to
        // right; the names come from the first SELECT.
        for op in &n.set_ops {
            let other = self.select(&op.select, outer, false)?;
            if other.len() != columns.len() {
                return Err(SemaError::new(
                    "set-operation-columns",
                    "each UNION query must have the same number of columns",
                    op.select.span,
                ));
            }
            for (c, o) in columns.iter_mut().zip(other) {
                c.ty = self
                    .ts
                    .common_type(&[c.ty.clone(), o.ty.clone()])
                    .map_err(|e| {
                        SemaError::new(
                            "type-mismatch",
                            format!("UNION types cannot be matched: {e:?}"),
                            op.select.span,
                        )
                    })?;
                c.nullable = Nullability::strict([c.nullable, o.nullable]);
                // The modifier only survives if it is the same everywhere.
                if c.modifiers != o.modifiers || c.ty != o.ty {
                    c.modifiers.clear();
                }
            }
        }
        if resolve_unknowns {
            for c in &mut columns {
                if c.ty == pg("unknown") {
                    c.ty = pg("text");
                }
            }
        }

        // ORDER BY sees the output columns (aliases) and the scope.
        if n.set_ops.is_empty() {
            for o in &n.order_by {
                self.group_or_order(&o.expr, &n.columns, &scope, Clause::OrderBy)?;
            }
        }
        for l in n.limit.iter().chain(&n.offset) {
            let t = self.expr(l, &scope)?;
            self.coerce_to(l, &pg("int8"));
            if t.ty != pg("unknown")
                && !self.ts.can_coerce(
                    std::slice::from_ref(&t.ty),
                    &[pg("int8")],
                    CoercionContext::Assignment,
                )
            {
                return Err(SemaError::new(
                    "type-mismatch",
                    format!(
                        "argument of LIMIT must be type bigint, not type {}",
                        crate::expr::format_type(&t.ty)
                    ),
                    l.span,
                ));
            }
        }
        Ok(columns)
    }

    /// `VALUES` (`transformValuesClause`): columns `column1`… of the common
    /// type of their rows.
    fn values(
        &mut self,
        rows: &[Vec<Expr>],
        scope: &Scope,
    ) -> Result<Vec<OutputColumn>, SemaError> {
        let width = rows[0].len();
        let mut typed: Vec<Vec<crate::expr::Typed>> = Vec::new();
        for row in rows {
            if row.len() != width {
                return Err(SemaError::new(
                    "values-columns",
                    "VALUES lists must all be the same length",
                    row.first().map_or(Span::DUMMY, |e| e.span),
                ));
            }
            let mut t = Vec::with_capacity(width);
            for e in row {
                t.push(self.expr(e, scope)?);
            }
            typed.push(t);
        }
        let mut out = Vec::with_capacity(width);
        for i in 0..width {
            let types: Vec<Ty> = typed.iter().map(|r| r[i].ty.clone()).collect();
            let common = self.ts.common_type(&types).map_err(|e| {
                SemaError::new(
                    "type-mismatch",
                    format!("VALUES types cannot be matched: {e:?}"),
                    rows[0][i].span,
                )
            })?;
            for row in rows {
                self.coerce_to(&row[i], &common);
            }
            out.push(OutputColumn {
                name: format!("column{}", i + 1),
                ty: common,
                modifiers: Vec::new(),
                nullable: Nullability::strict(typed.iter().map(|r| r[i].nullable)),
            });
        }
        Ok(out)
    }

    /// `GROUP BY` / `ORDER BY` / `DISTINCT ON`: position, output column,
    /// or expression (`findTargetlistEntrySQL92`).
    fn group_or_order(
        &mut self,
        e: &Expr,
        items: &[SelectItem],
        scope: &Scope,
        clause: Clause,
    ) -> Result<(), SemaError> {
        if let ExprKind::Literal(pgweave_ast::expr::Literal::Number(_)) = e.kind {
            return Ok(());
        }
        if let ExprKind::GroupingSets { sets, .. } = &e.kind {
            for x in sets.iter().flatten() {
                self.group_or_order(x, items, scope, clause)?;
            }
            return Ok(());
        }
        // A bare name designates the output column of that name; in `GROUP BY`,
        // a `FROM` column with the same name takes precedence.
        if let ExprKind::Column(c) = &e.kind
            && c.table.is_none()
            && !(clause == Clause::GroupBy && scope.has_local_column(&c.column.value))
        {
            let mut found: Option<&Expr> = None;
            for item in items {
                let SelectItem::Expr { expr, alias } = item else {
                    continue;
                };
                let name = alias
                    .as_ref()
                    .map_or_else(|| figure_colname(expr), |a| a.value.clone());
                if name != c.column.value {
                    continue;
                }
                match found {
                    Some(prev)
                        if pgweave_ast::emit::emit_expr(prev)
                            != pgweave_ast::emit::emit_expr(expr) =>
                    {
                        return Err(SemaError::new(
                            "ambiguous-column",
                            format!("{} \"{}\" is ambiguous", clause.label(), c.column.value),
                            e.span,
                        ));
                    }
                    Some(_) => {}
                    None => found = Some(expr),
                }
            }
            if found.is_some() {
                return Ok(());
            }
        }
        self.expr(e, scope).map(|_| ())
    }

    /// `SELECT` list: `*` expanded, each expression typed and named.
    fn projection(
        &mut self,
        items: &[SelectItem],
        scope: &Scope,
        resolve_unknowns: bool,
    ) -> Result<Vec<OutputColumn>, SemaError> {
        let mut out = Vec::new();
        for item in items {
            match item {
                SelectItem::Wildcard => {
                    if scope.sources.is_empty() {
                        return Err(SemaError::new(
                            "type-mismatch",
                            "SELECT * with no tables specified is not valid",
                            Span::DUMMY,
                        ));
                    }
                    for (s, c) in &scope.star {
                        out.push(star_column(&scope.sources[*s].columns[*c])?);
                    }
                }
                SelectItem::QualifiedWildcard(t) => {
                    let idx = scope
                        .sources
                        .iter()
                        .position(|s| s.name.as_deref() == Some(t.value.as_str()))
                        .ok_or_else(|| {
                            SemaError::new(
                                "table-not-found",
                                format!("missing FROM-clause entry for table \"{}\"", t.value),
                                Span::DUMMY,
                            )
                        })?;
                    for c in &scope.sources[idx].columns {
                        out.push(star_column(c)?);
                    }
                }
                SelectItem::Expr { expr, alias } => {
                    let t = self.expr(expr, scope)?;
                    let ty = if resolve_unknowns && t.ty == pg("unknown") {
                        pg("text")
                    } else {
                        t.ty
                    };
                    out.push(OutputColumn {
                        name: alias
                            .as_ref()
                            .map_or_else(|| figure_colname(expr), |a| a.value.clone()),
                        ty,
                        modifiers: expr_modifiers(expr, scope),
                        nullable: t.nullable,
                    });
                }
            }
        }
        Ok(out)
    }

    /// `FROM` source.
    fn table_ref(&mut self, t: &TableRef, scope: &Scope) -> Result<Source, SemaError> {
        // Table (not a view nor a CTE): system columns visible.
        let mut system = false;
        let mut rowtype = None;
        let (name, mut columns) = match &t.source {
            TableSource::Table(obj) => {
                let (schema, n) = split_name(obj);
                let cte = schema
                    .is_none()
                    .then(|| self.ctes.iter().rev().find(|(c, _)| c == n))
                    .flatten();
                let columns = match cte {
                    Some((_, cols)) => cols.clone(),
                    None => {
                        let rel = self.ts.relation(schema, n, &self.path).ok_or_else(|| {
                            SemaError::new(
                                "table-not-found",
                                format!("relation \"{n}\" does not exist"),
                                t.span,
                            )
                        })?;
                        system = !matches!(rel.kind, RelationKind::View);
                        rowtype = Some(rel.name.clone());
                        rel.columns
                            .iter()
                            .map(|c| Column {
                                name: c.name.clone(),
                                ty: self.ts.of_ref(&c.ty),
                                modifiers: ref_modifiers(&c.ty),
                                nullable: if c.not_null {
                                    Nullability::NonNull
                                } else {
                                    Nullability::Nullable
                                },
                            })
                            .collect()
                    }
                };
                (Some(n.to_string()), columns)
            }
            TableSource::Subquery { lateral, query } => {
                let cols = if *lateral {
                    self.select(query, Some(scope), true)?
                } else {
                    self.select(query, scope.parent, true)?
                };
                (None, cols.iter().map(Column::from).collect())
            }
            TableSource::Function {
                name,
                args,
                with_ordinality,
                ..
            } => {
                let (schema_part, fname) = split_name(name);
                // `unnest(a, b)` in FROM: `ROWS FROM (unnest(a), unnest(b))`.
                let (mut cols, scalar) =
                    if schema_part.is_none() && fname == "unnest" && args.len() > 1 {
                        let mut all = Vec::new();
                        for a in args {
                            let (c, _) =
                                self.table_function(name, std::slice::from_ref(a), scope)?;
                            all.extend(c);
                        }
                        (all, false)
                    } else {
                        self.table_function(name, args, scope)?
                    };
                // Scalar function: its column carries the source's alias.
                if scalar
                    && t.column_aliases.is_empty()
                    && let Some(a) = &t.alias
                {
                    cols[0].name = a.value.clone();
                }
                if *with_ordinality {
                    cols.push(Column {
                        name: "ordinality".into(),
                        ty: Some(pg("int8")),
                        modifiers: Vec::new(),
                        nullable: Nullability::NonNull,
                    });
                }
                (Some(fname.to_string()), cols)
            }
        };
        if t.column_types.is_empty() {
            for (c, a) in columns.iter_mut().zip(&t.column_aliases) {
                c.name = a.value.clone();
            }
        } else {
            // Column definition list: it gives the columns of a
            // function returning `record`.
            if !matches!(t.source, TableSource::Function { .. })
                || columns.len() != 1
                || columns[0].ty != Some(pg("record"))
            {
                return Err(SemaError::new(
                    "type-mismatch",
                    "a column definition list is only allowed for functions returning \"record\"",
                    Span::DUMMY,
                ));
            }
            columns = t
                .column_aliases
                .iter()
                .zip(&t.column_types)
                .map(|(a, dt)| {
                    Ok(Column {
                        name: a.value.clone(),
                        ty: Some(self.data_type(dt, Span::DUMMY)?),
                        modifiers: data_type_modifiers(dt),
                        nullable: Nullability::Unknown,
                    })
                })
                .collect::<Result<_, SemaError>>()?;
        }
        let mut source = Source::new(t.alias.as_ref().map(|a| a.value.clone()).or(name), columns);
        if t.alias.is_none() {
            source.relation = rowtype.clone();
        }
        source.rowtype = rowtype;
        Ok(if system {
            source.with_system_columns()
        } else {
            source
        })
    }

    /// Columns of a function in `FROM`: `OUT`/`TABLE` columns, attributes
    /// of a composite, otherwise one column named like the function.
    fn table_function(
        &mut self,
        name: &pgweave_ast::ObjectName,
        args: &[Expr],
        scope: &Scope,
    ) -> Result<(Vec<Column>, bool), SemaError> {
        let call = Expr::bare(ExprKind::FunctionCall {
            name: name.clone(),
            args: args.to_vec(),
            order_by: vec![],
            filter: None,
            distinct: false,
            within_group: vec![],
            over: None,
        });
        let t = self.expr(&call, scope)?;
        let (schema, fname) = split_name(name);
        let inputs: Vec<Ty> = {
            let mut v = Vec::new();
            for a in args {
                v.push(self.expr(a, scope)?.ty);
            }
            v
        };
        let resolved = self
            .ts
            .resolve_function(schema, fname, &inputs, &self.path)
            .ok();
        if let Some(crate::resolve::Resolution::Routine(r)) = resolved.as_ref().map(|c| &c.target) {
            let outs: Vec<_> = r
                .args
                .iter()
                .filter(|a| matches!(a.mode, ArgMode::Out | ArgMode::InOut | ArgMode::Table))
                .collect();
            // Columns named by the `OUT` / `TABLE` parameters, even a single
            // one (`jsonb_array_elements` → `value`).
            let named = outs.len() == 1 && outs[0].name.is_some();
            if outs.len() > 1 || (outs.len() == 1 && (t.ty == pg("record") || named)) {
                let single = outs.len() == 1;
                let cols: Vec<Column> = outs
                    .iter()
                    .map(|a| Column {
                        name: a.name.clone().unwrap_or_default(),
                        // Single: the resolved type of the call (polymorphism).
                        ty: if single {
                            Some(t.ty.clone())
                        } else {
                            self.ts.of_ref(&a.ty)
                        },
                        modifiers: ref_modifiers(&a.ty),
                        nullable: Nullability::Unknown,
                    })
                    .collect();
                return Ok((cols, false));
            }
        }
        if let Some(TypeKind::Composite { attributes }) = self.ts.def(&t.ty).map(|d| &d.kind) {
            let cols: Vec<Column> = attributes
                .iter()
                .map(|a| Column {
                    name: a.name.clone(),
                    ty: self.ts.of_ref(&a.ty),
                    modifiers: ref_modifiers(&a.ty),
                    nullable: Nullability::Unknown,
                })
                .collect();
            return Ok((cols, false));
        }
        if let Some(rel) = self.ts.catalog.relations.get(&t.ty) {
            let cols: Vec<Column> = rel
                .columns
                .iter()
                .map(|c| Column {
                    name: c.name.clone(),
                    ty: self.ts.of_ref(&c.ty),
                    modifiers: ref_modifiers(&c.ty),
                    nullable: Nullability::Unknown,
                })
                .collect();
            return Ok((cols, false));
        }
        Ok((
            vec![Column {
                name: fname.to_string(),
                ty: Some(t.ty),
                modifiers: Vec::new(),
                nullable: t.nullable,
            }],
            true,
        ))
    }

    /// Join: nullability of the outer sides, `USING` columns merged
    /// at the head of `*`.
    fn join(&mut self, j: &pgweave_ast::stmt::Join, scope: &mut Scope) -> Result<(), SemaError> {
        let left_sources = scope.sources.len();
        let mut src = self.table_ref(&j.table, scope)?;
        // Columns before the effect of the outer join: the merged column
        // of a `USING` depends on it.
        let left_before: Vec<Column> = scope
            .star
            .iter()
            .map(|(s, c)| scope.sources[*s].columns[*c].clone())
            .collect();
        let right_before = src.columns.clone();
        if matches!(j.kind, JoinKind::Left | JoinKind::Full) {
            for c in &mut src.columns {
                c.nullable = c.nullable.outer();
            }
        }
        if matches!(j.kind, JoinKind::Right | JoinKind::Full) {
            for s in &mut scope.sources[..left_sources] {
                for c in &mut s.columns {
                    c.nullable = c.nullable.outer();
                }
            }
        }
        let using: Vec<String> = match &j.constraint {
            JoinConstraint::Using(cols) => cols.iter().map(|c| c.value.clone()).collect(),
            JoinConstraint::Natural => src
                .columns
                .iter()
                .filter(|c| {
                    scope
                        .star
                        .iter()
                        .any(|(s, i)| scope.sources[*s].columns[*i].name == c.name)
                })
                .map(|c| c.name.clone())
                .collect(),
            _ => Vec::new(),
        };
        let right = scope.sources.len();
        let right_cols = src.columns.len();
        scope.sources.push(src);
        if using.is_empty() {
            scope.star.extend((0..right_cols).map(|c| (right, c)));
        } else {
            // Common columns first (left side), then the rest on the left,
            // then the rest on the right.
            let left_star: Vec<(usize, usize)> = std::mem::take(&mut scope.star);
            let name_of =
                |s: &Scope, (si, ci): (usize, usize)| s.sources[si].columns[ci].name.clone();
            // Merged columns (`buildMergedJoinVar`): common type of both
            // sides; left or right depending on the preserved side, `COALESCE`
            // of both for a full join.
            let mut merged = Vec::new();
            for u in &using {
                let Some(l) = left_before.iter().find(|c| c.name == *u) else {
                    return Err(SemaError::new(
                        "column-not-found",
                        format!(
                            "column \"{u}\" specified in USING clause does not exist in left table"
                        ),
                        Span::DUMMY,
                    ));
                };
                let Some(r) = right_before.iter().find(|c| c.name == *u) else {
                    return Err(SemaError::new(
                        "column-not-found",
                        format!(
                            "column \"{u}\" specified in USING clause does not exist in right table"
                        ),
                        Span::DUMMY,
                    ));
                };
                let ty = match (&l.ty, &r.ty) {
                    (Some(a), Some(b)) if a == b => Some(a.clone()),
                    (Some(a), Some(b)) => {
                        Some(self.ts.common_type(&[a.clone(), b.clone()]).map_err(|_| {
                            SemaError::new(
                                "type-mismatch",
                                format!(
                                    "JOIN/USING types {} and {} cannot be matched",
                                    a.name, b.name
                                ),
                                Span::DUMMY,
                            )
                        })?)
                    }
                    _ => None,
                };
                let nullable = match j.kind {
                    JoinKind::Right => r.nullable,
                    JoinKind::Full => match (l.nullable, r.nullable) {
                        (Nullability::NonNull, Nullability::NonNull) => Nullability::NonNull,
                        (Nullability::Unknown, _) | (_, Nullability::Unknown) => {
                            Nullability::Unknown
                        }
                        _ => Nullability::Nullable,
                    },
                    _ => l.nullable,
                };
                let modifiers = if ty == l.ty && l.modifiers == r.modifiers {
                    l.modifiers.clone()
                } else {
                    Vec::new()
                };
                merged.push(Column {
                    name: u.clone(),
                    ty,
                    modifiers,
                    nullable,
                });
            }
            let merged_source = scope.sources.len();
            scope.sources.push(Source::new(None, merged));
            let mut star: Vec<(usize, usize)> =
                (0..using.len()).map(|i| (merged_source, i)).collect();
            star.extend(
                left_star
                    .iter()
                    .filter(|p| !using.contains(&name_of(scope, **p))),
            );
            star.extend(
                (0..right_cols)
                    .map(|c| (right, c))
                    .filter(|p| !using.contains(&name_of(scope, *p))),
            );
            scope.star = star;
        }
        if let JoinConstraint::On(e) = &j.constraint {
            self.boolean(e, scope)?;
        }
        Ok(())
    }

    /// Target table of a modification, visible under its alias.
    fn target(
        &mut self,
        name: &pgweave_ast::ObjectName,
        alias: Option<&Ident>,
    ) -> Result<(Source, pgweave_catalog::QName), SemaError> {
        let (schema, n) = split_name(name);
        let rel = self.ts.relation(schema, n, &self.path).ok_or_else(|| {
            SemaError::new(
                "table-not-found",
                format!("relation \"{n}\" does not exist"),
                Span::DUMMY,
            )
        })?;
        let qname = rel.name.clone();
        let columns = rel
            .columns
            .iter()
            .map(|c| Column {
                name: c.name.clone(),
                ty: self.ts.of_ref(&c.ty),
                modifiers: ref_modifiers(&c.ty),
                nullable: if c.not_null {
                    Nullability::NonNull
                } else {
                    Nullability::Nullable
                },
            })
            .collect();
        let source = Source::new(
            Some(alias.map_or_else(|| n.to_string(), |a| a.value.clone())),
            columns,
        );
        let mut source = source.with_system_columns();
        source.rowtype = Some(qname.clone());
        if alias.is_none() {
            source.relation = Some(qname.clone());
        }
        Ok((source, qname))
    }

    /// Value assigned to a column (assignment context).
    fn assign(&mut self, value: &Expr, column: &Column, scope: &Scope) -> Result<(), SemaError> {
        if let ExprKind::Unsupported(_) = value.kind {
            return self.expr(value, scope).map(|_| ());
        }
        let is_default = matches!(&value.kind, ExprKind::Column(c) if c.table.is_none() && c.column.value == "default" && !c.column.quoted);
        if is_default {
            return Ok(());
        }
        let t = self.expr(value, scope)?;
        self.param_nullability(value, column.nullable);
        let Some(target) = &column.ty else {
            return Ok(());
        };
        self.coerce_to(value, target);
        if t.ty != pg("unknown")
            && !self.ts.can_coerce(
                std::slice::from_ref(&t.ty),
                std::slice::from_ref(target),
                CoercionContext::Assignment,
            )
        {
            return Err(SemaError::new(
                "type-mismatch",
                format!(
                    "column \"{}\" is of type {} but expression is of type {}",
                    column.name,
                    crate::expr::format_type(target),
                    crate::expr::format_type(&t.ty)
                ),
                value.span,
            ));
        }
        Ok(())
    }

    /// `RETURNING`: the statement's scope, plus the rows before (`old`)
    /// and after (`new`) modification (PostgreSQL 18), outside `*`.
    fn returning(
        &mut self,
        items: &[SelectItem],
        scope: &Scope,
        target: &Source,
        aliases: &pgweave_ast::stmt::ReturningAliases,
        old_exists: bool,
        new_exists: bool,
    ) -> Result<Vec<OutputColumn>, SemaError> {
        if items.is_empty() {
            return Ok(Vec::new());
        }
        let mut rscope = scope.clone();
        for (alias, default, exists) in [
            (&aliases.old, "old", old_exists),
            (&aliases.new, "new", new_exists),
        ] {
            let name = alias.as_ref().map_or(default, |a| a.value.as_str());
            if rscope.source(name).is_some() {
                continue;
            }
            let mut src = Source::new(Some(name.to_string()), target.columns.clone());
            if !exists {
                for c in &mut src.columns {
                    c.nullable = Nullability::Nullable;
                }
            }
            src.rowtype = target.rowtype.clone();
            rscope.sources.push(src);
        }
        self.projection(items, &rscope, true)
    }

    fn insert(
        &mut self,
        s: &pgweave_ast::stmt::InsertStmt,
    ) -> Result<Vec<OutputColumn>, SemaError> {
        self.with(&s.with, s.with_recursive, None)?;
        let (src, rel) = self.target(&s.table, s.alias.as_ref())?;
        let targets: Vec<Column> = if s.columns.is_empty() {
            src.columns.clone()
        } else {
            s.columns
                .iter()
                .map(|c| {
                    src.columns
                        .iter()
                        .find(|x| x.name == c.value)
                        .cloned()
                        .ok_or_else(|| {
                            SemaError::new(
                                "column-not-found",
                                format!(
                                    "column \"{}\" of relation \"{}\" does not exist",
                                    c.value,
                                    src.name.clone().unwrap_or_default()
                                ),
                                Span::DUMMY,
                            )
                        })
                })
                .collect::<Result<_, _>>()?
        };
        let empty = Scope::default();
        match &s.source {
            InsertSource::Values(rows) => {
                for row in rows {
                    for (v, c) in row.iter().zip(&targets) {
                        if !is_default_keyword(v) {
                            self.check_writable(&rel, &c.name, "insert", v.span)?;
                        }
                        self.assign(v, c, &empty)?;
                    }
                }
            }
            InsertSource::Select(q) => {
                // Untyped literals and parameters of the list are
                // converted to the target columns, not frozen as `text`.
                let mut cols = self.select(q, None, false)?;
                for c in targets.iter().take(cols.len()) {
                    self.check_writable(&rel, &c.name, "insert", q.span)?;
                }
                if q.set_ops.is_empty() && q.values.is_empty() {
                    for ((o, item), c) in cols.iter_mut().zip(&q.columns).zip(&targets) {
                        if let (SelectItem::Expr { expr, .. }, Some(t)) = (item, &c.ty)
                            && o.ty == pg("unknown")
                        {
                            self.coerce_to(expr, t);
                            o.ty = t.clone();
                        }
                    }
                }
                for (o, c) in cols.iter().zip(&targets) {
                    if let Some(t) = &c.ty
                        && !self.ts.can_coerce(
                            std::slice::from_ref(&o.ty),
                            std::slice::from_ref(t),
                            CoercionContext::Assignment,
                        )
                    {
                        return Err(SemaError::new(
                            "type-mismatch",
                            format!(
                                "column \"{}\" is of type {} but expression is of type {}",
                                c.name,
                                crate::expr::format_type(t),
                                crate::expr::format_type(&o.ty)
                            ),
                            q.span,
                        ));
                    }
                }
            }
            InsertSource::DefaultValues => {}
        }
        let mut scope = Scope::default();
        scope.push(src.clone());
        if let Some(oc) = &s.on_conflict
            && let pgweave_ast::stmt::OnConflictAction::Update {
                assignments,
                where_clause,
            } = &oc.action
        {
            let mut excluded = src.clone();
            excluded.name = Some("excluded".into());
            let mut uscope = Scope::default();
            uscope.push(src.clone());
            uscope.push(excluded);
            for a in assignments {
                self.assignment(a, &src, &rel, &uscope)?;
            }
            if let Some(w) = where_clause {
                self.boolean(w, &uscope)?;
            }
        }
        self.returning(
            &s.returning,
            &scope,
            &src,
            &s.returning_aliases,
            false,
            true,
        )
    }

    fn assignment(
        &mut self,
        a: &pgweave_ast::stmt::Assignment,
        target: &Source,
        rel: &QName,
        scope: &Scope,
    ) -> Result<(), SemaError> {
        // Generated column or `ALWAYS` identity: only `DEFAULT`.
        for (i, c) in a.columns.iter().enumerate() {
            let value = match (&a.value.kind, a.columns.len()) {
                (_, 1) => Some(&a.value),
                (ExprKind::Tuple(values), _) => values.get(i),
                _ => None,
            };
            if value.is_none_or(|v| !is_default_keyword(v)) {
                self.check_writable(rel, &c.value, "update", a.value.span)?;
            }
        }
        let cols: Vec<Column> = a
            .columns
            .iter()
            .map(|c| {
                target
                    .columns
                    .iter()
                    .find(|x| x.name == c.value)
                    .cloned()
                    .ok_or_else(|| {
                        SemaError::new(
                            "column-not-found",
                            format!("column \"{}\" does not exist", c.value),
                            Span::DUMMY,
                        )
                    })
            })
            .collect::<Result<_, _>>()?;
        match (cols.as_slice(), &a.value.kind) {
            // `SET home.city = …`: the composite's field receives the value.
            ([c], _) if !a.subfields.is_empty() => {
                let mut target = c.clone();
                let mut ty = c.ty.clone();
                for f in &a.subfields {
                    let base = ty.as_ref().map(|t| self.ts.base_type(t));
                    ty = base.and_then(|b| self.composite_attribute(&b, &f.value));
                    if ty.is_none() {
                        return Err(SemaError::new(
                            "column-not-found",
                            format!(
                                "cannot assign to field \"{}\" of column \"{}\"",
                                f.value, c.name
                            ),
                            a.value.span,
                        ));
                    }
                }
                target.name = a.subfields.last().map_or(target.name, |f| f.value.clone());
                target.ty = ty;
                target.modifiers.clear();
                self.assign(&a.value, &target, scope)
            }
            ([c], _) => self.assign(&a.value, c, scope),
            (_, ExprKind::Tuple(values)) => {
                for (v, c) in values.iter().zip(&cols) {
                    self.assign(v, c, scope)?;
                }
                Ok(())
            }
            // `SET (a, b) = (SELECT x, y …)`: one subquery column
            // per target (`MULTIEXPR_SUBLINK`).
            (_, ExprKind::Subquery(q)) => {
                let out = self.subquery(q, scope)?;
                if out.len() != cols.len() {
                    return Err(SemaError::new(
                        "subquery-columns",
                        format!(
                            "number of columns does not match number of values ({} for {})",
                            out.len(),
                            cols.len()
                        ),
                        a.value.span,
                    ));
                }
                for (o, c) in out.iter().zip(&cols) {
                    if let Some(t) = &c.ty
                        && !self.ts.can_coerce(
                            std::slice::from_ref(&o.ty),
                            std::slice::from_ref(t),
                            CoercionContext::Assignment,
                        )
                    {
                        return Err(SemaError::new(
                            "type-mismatch",
                            format!(
                                "column \"{}\" is of type {} but expression is of type {}",
                                c.name,
                                crate::expr::format_type(t),
                                crate::expr::format_type(&o.ty)
                            ),
                            a.value.span,
                        ));
                    }
                }
                Ok(())
            }
            _ => self.expr(&a.value, scope).map(|_| ()),
        }
    }

    fn update(
        &mut self,
        s: &pgweave_ast::stmt::UpdateStmt,
    ) -> Result<Vec<OutputColumn>, SemaError> {
        self.with(&s.with, s.with_recursive, None)?;
        let (src, rel) = self.target(&s.table.name, s.table.alias.as_ref())?;
        let mut scope = Scope::default();
        scope.push(src.clone());
        for t in &s.from {
            let f = self.table_ref(t, &scope)?;
            scope.push(f);
        }
        for j in &s.joins {
            self.join(j, &mut scope)?;
        }
        for a in &s.assignments {
            self.assignment(a, &src, &rel, &scope)?;
        }
        if let Some(w) = &s.where_clause {
            self.boolean(w, &scope)?;
        }
        self.returning(&s.returning, &scope, &src, &s.returning_aliases, true, true)
    }

    fn delete(
        &mut self,
        s: &pgweave_ast::stmt::DeleteStmt,
    ) -> Result<Vec<OutputColumn>, SemaError> {
        self.with(&s.with, s.with_recursive, None)?;
        let (src, _) = self.target(&s.table.name, s.table.alias.as_ref())?;
        let mut scope = Scope::default();
        scope.push(src.clone());
        for t in &s.using {
            let f = self.table_ref(t, &scope)?;
            scope.push(f);
        }
        for j in &s.joins {
            self.join(j, &mut scope)?;
        }
        if let Some(w) = &s.where_clause {
            self.boolean(w, &scope)?;
        }
        self.returning(
            &s.returning,
            &scope,
            &src,
            &s.returning_aliases,
            true,
            false,
        )
    }
}

impl Analyzer<'_> {
    /// `CALL procedure(…)`: resolved among procedures; its `INOUT`
    /// arguments form the returned row.
    fn call(
        &mut self,
        name: &pgweave_ast::ObjectName,
        args: &[Expr],
        span: Span,
    ) -> Result<Vec<OutputColumn>, SemaError> {
        let empty = Scope::default();
        let mut inputs = Vec::new();
        for a in args {
            if let ExprKind::NamedArgument { .. } = a.kind {
                return Err(SemaError::not_analyzed("named argument", a.span));
            }
            inputs.push(self.expr(a, &empty)?.ty);
        }
        let (schema, n) = split_name(name);
        let call = self
            .ts
            .resolve_function(schema, n, &inputs, &self.path)
            .map_err(|_| {
                SemaError::new(
                    "function-not-found",
                    format!("procedure {n} does not exist"),
                    span,
                )
            })?;
        let crate::resolve::Resolution::Routine(r) = call.target else {
            return Err(SemaError::new(
                "function-not-found",
                format!("procedure {n} does not exist"),
                span,
            ));
        };
        if r.kind != pgweave_catalog::model::RoutineKind::Procedure {
            return Err(SemaError::new(
                "type-mismatch",
                format!("{n} is not a procedure"),
                span,
            ));
        }
        if r.args.iter().any(|a| a.mode == ArgMode::Out) {
            return Err(SemaError::not_analyzed(
                "procedure with OUT arguments",
                span,
            ));
        }
        for (a, t) in args.iter().zip(&call.arg_types) {
            self.coerce_to(a, t);
        }
        Ok(r.args
            .iter()
            .zip(&call.arg_types)
            .filter(|(a, _)| a.mode == ArgMode::InOut)
            .map(|(a, t)| OutputColumn {
                name: a.name.clone().unwrap_or_default(),
                ty: t.clone(),
                modifiers: Vec::new(),
                nullable: Nullability::Unknown,
            })
            .collect())
    }
}

/// Clause that designates an output column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Clause {
    GroupBy,
    OrderBy,
    DistinctOn,
}

impl Clause {
    fn label(self) -> &'static str {
        match self {
            Clause::GroupBy => "GROUP BY",
            Clause::OrderBy => "ORDER BY",
            Clause::DistinctOn => "DISTINCT ON",
        }
    }
}

/// `DEFAULT` keyword in place of a value.
fn is_default_keyword(e: &Expr) -> bool {
    matches!(&e.kind, ExprKind::Column(c) if c.table.is_none() && c.column.value == "default" && !c.column.quoted)
}

impl Analyzer<'_> {
    /// Can an explicit value be written into `rel.column`? No for a
    /// generated column or a `GENERATED ALWAYS` identity.
    fn check_writable(
        &self,
        rel: &QName,
        column: &str,
        verb: &str,
        span: Span,
    ) -> Result<(), SemaError> {
        let Some(col) = self
            .ts
            .catalog
            .relations
            .get(rel)
            .and_then(|r| r.columns.iter().find(|c| c.name == column))
        else {
            return Ok(());
        };
        let always = col.identity == Some(pgweave_catalog::model::IdentityKind::Always);
        if col.generated.is_none() && !always {
            return Ok(());
        }
        let message = if verb == "insert" {
            format!("cannot insert a non-DEFAULT value into column \"{column}\"")
        } else {
            format!("column \"{column}\" can only be updated to DEFAULT")
        };
        Err(SemaError::new("generated-column", message, span))
    }
}

fn star_column(c: &Column) -> Result<OutputColumn, SemaError> {
    Ok(OutputColumn {
        name: c.name.clone(),
        ty: c.ty.clone().ok_or_else(|| {
            SemaError::not_analyzed(format!("type of column \"{}\"", c.name), Span::DUMMY)
        })?,
        modifiers: c.modifiers.clone(),
        nullable: c.nullable,
    })
}

/// Modifiers of a catalog type reference (those of the element for
/// an array, like PostgreSQL's `typmod`).
pub(crate) fn ref_modifiers(r: &pgweave_catalog::TypeRef) -> Vec<String> {
    match r {
        pgweave_catalog::TypeRef::Named { modifiers, .. } => modifiers.clone(),
        pgweave_catalog::TypeRef::Array(inner) => ref_modifiers(inner),
        _ => Vec::new(),
    }
}

/// Modifiers of a written type (`varchar(20)`, `numeric(12, 2)`).
fn data_type_modifiers(dt: &pgweave_ast::types::DataType) -> Vec<String> {
    use pgweave_ast::types::DataType::*;
    match dt {
        Varchar(Some(n)) | Char(Some(n)) => vec![n.to_string()],
        Numeric {
            precision: Some(p),
            scale,
        } => std::iter::once(p.to_string())
            .chain(scale.map(|s| s.to_string()))
            .collect(),
        Time(Some(p)) | TimeTz(Some(p)) | Timestamp(Some(p)) | TimestampTz(Some(p)) => {
            vec![p.to_string()]
        }
        Modified { modifiers, .. } => modifiers.clone(),
        Array(inner) => data_type_modifiers(inner),
        _ => Vec::new(),
    }
}

/// `exprTypmod`, for the forms that keep a modifier: column,
/// conversion, `COLLATE`. Any other expression has none (-1).
fn expr_modifiers(e: &Expr, scope: &Scope) -> Vec<String> {
    match &e.kind {
        ExprKind::Column(c) => scope
            .lookup_ref(c, e.span)
            .map(|col| col.modifiers.clone())
            .unwrap_or_default(),
        ExprKind::Cast { data_type, .. } => data_type_modifiers(data_type),
        ExprKind::Collate { expr, .. } => expr_modifiers(expr, scope),
        _ => Vec::new(),
    }
}

/// Columns renamed by `WITH name (a, b)`.
fn rename(cols: &[OutputColumn], names: &[Ident]) -> Vec<Column> {
    cols.iter()
        .enumerate()
        .map(|(i, c)| Column {
            name: names
                .get(i)
                .map_or_else(|| c.name.clone(), |n| n.value.clone()),
            ty: Some(c.ty.clone()),
            modifiers: c.modifiers.clone(),
            nullable: c.nullable,
        })
        .collect()
}
