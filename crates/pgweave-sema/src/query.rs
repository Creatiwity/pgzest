//! Analyse des requêtes (`analyze.c`, `parse_clause.c`, `parse_target.c`) :
//! portée de `FROM`, colonnes de sortie nommées et typées, paramètres.

use pgweave_ast::Span;
use pgweave_ast::expr::{Expr, ExprKind, Ident};
use pgweave_ast::stmt::{
    InsertSource, JoinConstraint, JoinKind, SelectItem, SelectStmt, Statement, TableRef,
    TableSource,
};
use pgweave_ast::utility::UtilityKind;
use pgweave_catalog::model::{ArgMode, RelationKind, TypeKind};
use pgweave_parser::normalize_select;

use crate::coerce::CoercionContext;
use crate::expr::{Analyzer, split_name};
use crate::scope::{Column, Nullability, Scope, SemaError, Source};
use crate::types::{Ty, pg};

/// Colonne de sortie d'une requête.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputColumn {
    pub name: String,
    pub ty: Ty,
    pub nullable: Nullability,
}

impl From<&OutputColumn> for Column {
    fn from(c: &OutputColumn) -> Self {
        Column {
            name: c.name.clone(),
            ty: Some(c.ty.clone()),
            nullable: c.nullable,
        }
    }
}

/// Nom donné par PostgreSQL à une colonne de sortie sans alias
/// (`FigureColname`) ; `?column?` à défaut.
pub fn figure_colname(e: &Expr) -> String {
    figure(e).map_or_else(|| "?column?".to_string(), |(n, _)| n)
}

/// Nom et force (2 : nom réel, 1 : nom de type) — `FigureColnameInternal`.
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
        // Le nom du `ELSE` s'il est réel, sinon `case`.
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

/// Dernier segment du nom de type tel que la grammaire l'écrit (`int` →
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

/// Ce que décrit un statement : colonnes renvoyées et paramètres.
#[derive(Debug, Clone, PartialEq)]
pub struct Description {
    pub columns: Vec<OutputColumn>,
    pub params: Vec<crate::expr::Param>,
}

impl<'c> Analyzer<'c> {
    /// Analyse un statement DML : colonnes renvoyées (vide sans
    /// `RETURNING`) et types des paramètres.
    pub fn describe(&mut self, stmt: &Statement) -> Result<Description, SemaError> {
        let columns = match stmt {
            Statement::Select(s) => self.select(s, None, true)?,
            Statement::Insert(s) => self.insert(s)?,
            Statement::Update(s) => self.update(s)?,
            Statement::Delete(s) => self.delete(s)?,
            Statement::Utility(u) => match &u.kind {
                UtilityKind::Call { name, args } => self.call(name, args, u.span)?,
                _ => return Err(SemaError::not_analyzed("statement utilitaire", u.span)),
            },
            Statement::Ddl(_) => {
                return Err(SemaError::not_analyzed("statement hors DML", Span::DUMMY));
            }
        };
        Ok(Description {
            columns,
            params: self.params.clone(),
        })
    }

    /// Enregistre les CTE d'un `WITH`, visibles du reste du statement.
    fn with(
        &mut self,
        ctes: &[pgweave_ast::stmt::Cte],
        recursive: bool,
        outer: Option<&Scope>,
    ) -> Result<(), SemaError> {
        for cte in ctes {
            let name = cte.name.value.clone();
            // Récursive : le terme non récursif fixe les types.
            if recursive && !cte.query.set_ops.is_empty() {
                let mut base = (*cte.query).clone();
                base.set_ops.clear();
                base.order_by.clear();
                base.limit = None;
                base.offset = None;
                let cols = self.select(&base, outer, false)?;
                self.ctes.push((name.clone(), rename(&cols, &cte.columns)));
            }
            let cols = self.select(&cte.query, outer, true)?;
            if recursive {
                self.ctes.retain(|(n, _)| *n != name);
            }
            self.ctes.push((name, rename(&cols, &cte.columns)));
        }
        Ok(())
    }

    /// Colonnes d'une requête `SELECT` (avec ses opérations ensemblistes).
    /// `resolve_unknowns` : un littéral sans type en sortie devient `text`
    /// (sauf dans une branche d'opération ensembliste).
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
            self.group_or_order(g, &n.columns, &scope)?;
        }
        if let Some(h) = &n.having {
            self.boolean(h, &scope)?;
        }

        let leaf = !n.set_ops.is_empty();
        let mut columns = if n.values.is_empty() {
            self.projection(&n.columns, &scope, resolve_unknowns && !leaf)?
        } else {
            self.values(&n.values, &scope)?
        };

        for d in &n.distinct_on {
            self.group_or_order(d, &n.columns, &scope)?;
        }

        // Opérations ensemblistes : type commun colonne à colonne, de gauche
        // à droite ; les noms viennent du premier SELECT.
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
            }
        }
        if resolve_unknowns {
            for c in &mut columns {
                if c.ty == pg("unknown") {
                    c.ty = pg("text");
                }
            }
        }

        // ORDER BY voit les colonnes de sortie (alias) et la portée.
        if n.set_ops.is_empty() {
            for o in &n.order_by {
                self.group_or_order(&o.expr, &n.columns, &scope)?;
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
                        t.ty.name
                    ),
                    l.span,
                ));
            }
        }
        Ok(columns)
    }

    /// `VALUES` (`transformValuesClause`) : colonnes `column1`… du type
    /// commun de leurs lignes.
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
                nullable: Nullability::strict(typed.iter().map(|r| r[i].nullable)),
            });
        }
        Ok(out)
    }

    /// `GROUP BY` / `ORDER BY` : position, alias de sortie, ou expression.
    fn group_or_order(
        &mut self,
        e: &Expr,
        items: &[SelectItem],
        scope: &Scope,
    ) -> Result<(), SemaError> {
        if let ExprKind::Literal(pgweave_ast::expr::Literal::Number(_)) = e.kind {
            return Ok(());
        }
        if let ExprKind::Column(c) = &e.kind
            && c.table.is_none()
            && scope.lookup(None, &c.column.value, e.span).is_err()
            && items.iter().any(|i| matches!(i, SelectItem::Expr { alias: Some(a), .. } if a.value == c.column.value))
        {
            return Ok(());
        }
        self.expr(e, scope).map(|_| ())
    }

    /// Liste `SELECT` : `*` développé, chaque expression typée et nommée.
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
                        nullable: t.nullable,
                    });
                }
            }
        }
        Ok(out)
    }

    /// Source de `FROM`.
    fn table_ref(&mut self, t: &TableRef, scope: &Scope) -> Result<Source, SemaError> {
        // Table (pas une vue ni une CTE) : colonnes système visibles.
        let mut system = false;
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
                        let rel = self
                            .ts
                            .catalog
                            .resolve_relation(schema, n, &self.path)
                            .ok_or_else(|| {
                                SemaError::new(
                                    "table-not-found",
                                    format!("relation \"{n}\" does not exist"),
                                    Span::DUMMY,
                                )
                            })?;
                        system = !matches!(rel.kind, RelationKind::View);
                        rel.columns
                            .iter()
                            .map(|c| Column {
                                name: c.name.clone(),
                                ty: self.ts.of_ref(&c.ty),
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
                let (_, fname) = split_name(name);
                let (mut cols, scalar) = self.table_function(name, args, scope)?;
                // Fonction scalaire : sa colonne porte l'alias de la source.
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
                        nullable: Nullability::NonNull,
                    });
                }
                (Some(fname.to_string()), cols)
            }
        };
        for (c, a) in columns.iter_mut().zip(&t.column_aliases) {
            c.name = a.value.clone();
        }
        let source = Source::new(t.alias.as_ref().map(|a| a.value.clone()).or(name), columns);
        Ok(if system {
            source.with_system_columns()
        } else {
            source
        })
    }

    /// Colonnes d'une fonction en `FROM` : colonnes `OUT`/`TABLE`, attributs
    /// d'un composite, sinon une colonne nommée comme la fonction.
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
            if outs.len() > 1 || (outs.len() == 1 && t.ty == pg("record")) {
                let cols: Vec<Column> = outs
                    .iter()
                    .map(|a| Column {
                        name: a.name.clone().unwrap_or_default(),
                        ty: self.ts.of_ref(&a.ty),
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
                    nullable: Nullability::Unknown,
                })
                .collect();
            return Ok((cols, false));
        }
        Ok((
            vec![Column {
                name: fname.to_string(),
                ty: Some(t.ty),
                nullable: t.nullable,
            }],
            true,
        ))
    }

    /// Jointure : nullabilité des côtés externes, colonnes `USING`
    /// fusionnées en tête de `*`.
    fn join(&mut self, j: &pgweave_ast::stmt::Join, scope: &mut Scope) -> Result<(), SemaError> {
        let left_sources = scope.sources.len();
        let mut src = self.table_ref(&j.table, scope)?;
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
            // Colonnes communes d'abord (côté gauche), puis le reste à gauche,
            // puis le reste à droite.
            let left_star: Vec<(usize, usize)> = std::mem::take(&mut scope.star);
            let name_of =
                |s: &Scope, (si, ci): (usize, usize)| s.sources[si].columns[ci].name.clone();
            let mut star = Vec::new();
            for u in &using {
                match left_star.iter().find(|p| name_of(scope, **p) == *u) {
                    Some(p) => star.push(*p),
                    None => {
                        return Err(SemaError::new(
                            "column-not-found",
                            format!(
                                "column \"{u}\" specified in USING clause does not exist in left table"
                            ),
                            Span::DUMMY,
                        ));
                    }
                }
            }
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

    /// Table cible d'une modification, visible sous son alias.
    fn target(
        &mut self,
        name: &pgweave_ast::ObjectName,
        alias: Option<&Ident>,
    ) -> Result<(Source, pgweave_catalog::QName), SemaError> {
        let (schema, n) = split_name(name);
        let rel = self
            .ts
            .catalog
            .resolve_relation(schema, n, &self.path)
            .ok_or_else(|| {
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
        Ok((source.with_system_columns(), qname))
    }

    /// Valeur affectée à une colonne (contexte d'affectation).
    fn assign(&mut self, value: &Expr, column: &Column, scope: &Scope) -> Result<(), SemaError> {
        if let ExprKind::Unsupported(_) = value.kind {
            return self.expr(value, scope).map(|_| ());
        }
        let is_default = matches!(&value.kind, ExprKind::Column(c) if c.table.is_none() && c.column.value == "default" && !c.column.quoted);
        if is_default {
            return Ok(());
        }
        let t = self.expr(value, scope)?;
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
                    column.name, target.name, t.ty.name
                ),
                value.span,
            ));
        }
        Ok(())
    }

    fn returning(
        &mut self,
        items: &[SelectItem],
        scope: &Scope,
    ) -> Result<Vec<OutputColumn>, SemaError> {
        self.projection(items, scope, true)
    }

    fn insert(
        &mut self,
        s: &pgweave_ast::stmt::InsertStmt,
    ) -> Result<Vec<OutputColumn>, SemaError> {
        self.with(&s.with, s.with_recursive, None)?;
        let (src, _) = self.target(&s.table, s.alias.as_ref())?;
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
                        self.assign(v, c, &empty)?;
                    }
                }
            }
            InsertSource::Select(q) => {
                let cols = self.select(q, None, true)?;
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
                                c.name, t.name, o.ty.name
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
                self.assignment(a, &src, &uscope)?;
            }
            if let Some(w) = where_clause {
                self.boolean(w, &uscope)?;
            }
        }
        self.returning(&s.returning, &scope)
    }

    fn assignment(
        &mut self,
        a: &pgweave_ast::stmt::Assignment,
        target: &Source,
        scope: &Scope,
    ) -> Result<(), SemaError> {
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
            ([c], _) => self.assign(&a.value, c, scope),
            (_, ExprKind::Tuple(values)) => {
                for (v, c) in values.iter().zip(&cols) {
                    self.assign(v, c, scope)?;
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
        let (src, _) = self.target(&s.table.name, s.table.alias.as_ref())?;
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
            self.assignment(a, &src, &scope)?;
        }
        if let Some(w) = &s.where_clause {
            self.boolean(w, &scope)?;
        }
        self.returning(&s.returning, &scope)
    }

    fn delete(
        &mut self,
        s: &pgweave_ast::stmt::DeleteStmt,
    ) -> Result<Vec<OutputColumn>, SemaError> {
        self.with(&s.with, s.with_recursive, None)?;
        let (src, _) = self.target(&s.table.name, s.table.alias.as_ref())?;
        let mut scope = Scope::default();
        scope.push(src);
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
        self.returning(&s.returning, &scope)
    }
}

impl Analyzer<'_> {
    /// `CALL procédure(…)` : résolue parmi les procédures ; ses arguments
    /// `INOUT` forment la ligne renvoyée.
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
                return Err(SemaError::not_analyzed("argument nommé", a.span));
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
            return Err(SemaError::not_analyzed("procédure à arguments OUT", span));
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
                nullable: Nullability::Unknown,
            })
            .collect())
    }
}

fn star_column(c: &Column) -> Result<OutputColumn, SemaError> {
    Ok(OutputColumn {
        name: c.name.clone(),
        ty: c.ty.clone().ok_or_else(|| {
            SemaError::not_analyzed(format!("type de la colonne « {} »", c.name), Span::DUMMY)
        })?,
        nullable: c.nullable,
    })
}

/// Colonnes renommées par `WITH nom (a, b)`.
fn rename(cols: &[OutputColumn], names: &[Ident]) -> Vec<Column> {
    cols.iter()
        .enumerate()
        .map(|(i, c)| Column {
            name: names
                .get(i)
                .map_or_else(|| c.name.clone(), |n| n.value.clone()),
            ty: Some(c.ty.clone()),
            nullable: c.nullable,
        })
        .collect()
}
