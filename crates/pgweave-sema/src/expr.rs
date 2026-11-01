//! Typage des expressions (`parse_expr.c`) : chaque expression reçoit un type
//! et une nullabilité ; les paramètres reçoivent le type que leur premier
//! usage leur impose, comme dans PostgreSQL.

use pgweave_ast::expr::{BinaryOp, Expr, ExprKind, Literal, ObjectName, UnaryOp};
use pgweave_ast::types::DataType;
use pgweave_ast::{Span, stmt::SelectStmt};
use pgweave_catalog::SearchPath;
use pgweave_catalog::model::RoutineKind;

use crate::coerce::{CoercionContext, Pathway};
use crate::resolve::{Resolution, ResolveError, ResolvedCall};
use crate::scope::{Nullability, Scope, SemaError};
use crate::types::{Ty, TypeSystem, pg};

/// Type et nullabilité d'une expression.
#[derive(Debug, Clone, PartialEq)]
pub struct Typed {
    pub ty: Ty,
    pub nullable: Nullability,
}

impl Typed {
    fn new(ty: Ty, nullable: Nullability) -> Self {
        Typed { ty, nullable }
    }
}

/// Paramètre de la requête : `$1`… ou `$nom` (D6), dans l'ordre.
#[derive(Debug, Clone, PartialEq)]
pub struct Param {
    pub name: String,
    /// `None` tant qu'aucun usage n'a fixé son type.
    pub ty: Option<Ty>,
}

/// Analyseur d'un statement : système de types, chemin de recherche,
/// paramètres rencontrés, CTE visibles.
pub struct Analyzer<'c> {
    pub ts: TypeSystem<'c>,
    pub path: SearchPath,
    pub params: Vec<Param>,
    /// CTE visibles, la plus récente en dernier.
    pub(crate) ctes: Vec<(String, Vec<crate::scope::Column>)>,
}

/// Nom d'opérateur PostgreSQL d'un opérateur binaire de l'AST.
fn operator_name(op: &BinaryOp) -> Option<&str> {
    Some(match op {
        BinaryOp::Eq => "=",
        BinaryOp::NotEq => "<>",
        BinaryOp::Lt => "<",
        BinaryOp::Lte => "<=",
        BinaryOp::Gt => ">",
        BinaryOp::Gte => ">=",
        BinaryOp::Plus => "+",
        BinaryOp::Minus => "-",
        BinaryOp::Multiply => "*",
        BinaryOp::Divide => "/",
        BinaryOp::Modulo => "%",
        BinaryOp::Like => "~~",
        BinaryOp::NotLike => "!~~",
        BinaryOp::ILike => "~~*",
        BinaryOp::Concat => "||",
        BinaryOp::Custom(s) => s,
        _ => return None,
    })
}

fn resolve_error(e: ResolveError, what: String, span: Span) -> SemaError {
    match e {
        ResolveError::NotFound => {
            SemaError::new("function-not-found", format!("{what} does not exist"), span)
        }
        ResolveError::Ambiguous => {
            SemaError::new("ambiguous-function", format!("{what} is not unique"), span)
        }
        ResolveError::Polymorphic(m) => SemaError::new("type-mismatch", m, span),
    }
}

fn type_list(tys: &[Ty]) -> String {
    tys.iter()
        .map(|t| t.name.clone())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Type d'un littéral numérique (`make_const`) : entier sur 4 puis 8
/// octets, sinon `numeric`.
fn number_type(n: &str) -> Ty {
    if n.contains(['.', 'e', 'E']) {
        return pg("numeric");
    }
    let digits = n.replace('_', "");
    if digits.parse::<i32>().is_ok() {
        pg("int4")
    } else if digits.parse::<i64>().is_ok() {
        pg("int8")
    } else {
        pg("numeric")
    }
}

impl<'c> Analyzer<'c> {
    pub fn new(ts: TypeSystem<'c>, path: SearchPath) -> Self {
        Analyzer {
            ts,
            path,
            params: Vec::new(),
            ctes: Vec::new(),
        }
    }

    /// Indice du paramètre `$n` ou `$nom`.
    fn param_index(&mut self, name: &str) -> usize {
        if let Some(n) = name.strip_prefix('$').and_then(|d| d.parse::<usize>().ok())
            && n >= 1
        {
            while self.params.len() < n {
                let i = self.params.len() + 1;
                self.params.push(Param {
                    name: format!("${i}"),
                    ty: None,
                });
            }
            return n - 1;
        }
        match self.params.iter().position(|p| p.name == name) {
            Some(i) => i,
            None => {
                self.params.push(Param {
                    name: name.to_string(),
                    ty: None,
                });
                self.params.len() - 1
            }
        }
    }

    /// Fixe le type d'un paramètre pas encore typé (`coerce_type` d'un
    /// paramètre inconnu vers sa cible).
    pub(crate) fn coerce_to(&mut self, e: &Expr, target: &Ty) {
        let mut e = e;
        while let ExprKind::Collate { expr, .. } = &e.kind {
            e = expr;
        }
        if let ExprKind::Parameter(name) = &e.kind {
            let i = self.param_index(name);
            if self.params[i].ty.is_none() && *target != pg("unknown") && *target != pg("any") {
                self.params[i].ty = Some(target.clone());
            }
        }
    }

    /// Type écrit (`CAST`, `::`).
    pub(crate) fn data_type(&self, dt: &DataType, span: Span) -> Result<Ty, SemaError> {
        let builtin = match dt {
            DataType::SmallInt => "int2",
            DataType::Integer => "int4",
            DataType::BigInt => "int8",
            DataType::Numeric { .. } => "numeric",
            DataType::Real => "float4",
            DataType::DoublePrecision => "float8",
            DataType::Text => "text",
            DataType::Varchar(_) => "varchar",
            DataType::Char(_) => "bpchar",
            DataType::Date => "date",
            DataType::Time(_) => "time",
            DataType::TimeTz(_) => "timetz",
            DataType::Timestamp(_) => "timestamp",
            DataType::TimestampTz(_) => "timestamptz",
            DataType::Interval => "interval",
            DataType::Boolean => "bool",
            DataType::Bytea => "bytea",
            DataType::Json => "json",
            DataType::Jsonb => "jsonb",
            DataType::Uuid => "uuid",
            DataType::Array(inner) => {
                let e = self.data_type(inner, span)?;
                return self.ts.array_of(&e).ok_or_else(|| {
                    SemaError::new(
                        "type-not-found",
                        format!("could not find array type for data type {e}"),
                        span,
                    )
                });
            }
            DataType::Custom(name) | DataType::Modified { name, .. } => {
                let (schema, n) = match name.rsplit_once('.') {
                    Some((s, n)) => (Some(s), n),
                    None => (None, name.as_str()),
                };
                return self
                    .ts
                    .catalog
                    .resolve_type(schema, n, &self.path)
                    .map(|t| t.name.clone())
                    .ok_or_else(|| {
                        SemaError::new(
                            "type-not-found",
                            format!("type \"{name}\" does not exist"),
                            span,
                        )
                    });
            }
        };
        Ok(pg(builtin))
    }

    /// Expression attendue booléenne (`WHERE`, `AND`…).
    pub(crate) fn boolean(&mut self, e: &Expr, scope: &Scope) -> Result<Typed, SemaError> {
        let t = self.expr(e, scope)?;
        self.coerce_to(e, &pg("bool"));
        if t.ty != pg("bool")
            && t.ty != pg("unknown")
            && !self.ts.can_coerce(
                std::slice::from_ref(&t.ty),
                &[pg("bool")],
                CoercionContext::Assignment,
            )
        {
            return Err(SemaError::new(
                "type-mismatch",
                format!("argument must be type boolean, not type {}", t.ty.name),
                e.span,
            ));
        }
        Ok(Typed::new(pg("bool"), t.nullable))
    }

    /// Type d'une entrée de résolution : un paramètre non typé compte comme
    /// littéral `unknown`.
    pub fn expr(&mut self, e: &Expr, scope: &Scope) -> Result<Typed, SemaError> {
        let span = e.span;
        match &e.kind {
            ExprKind::Literal(lit) => Ok(match lit {
                Literal::Number(n) => Typed::new(number_type(n), Nullability::NonNull),
                Literal::String(_) => Typed::new(pg("unknown"), Nullability::NonNull),
                Literal::Boolean(_) => Typed::new(pg("bool"), Nullability::NonNull),
                Literal::Null => Typed::new(pg("unknown"), Nullability::Nullable),
            }),
            ExprKind::Parameter(name) => {
                let i = self.param_index(name);
                Ok(Typed::new(
                    self.params[i].ty.clone().unwrap_or_else(|| pg("unknown")),
                    Nullability::Unknown,
                ))
            }
            ExprKind::Column(c) => {
                let col = scope.lookup(
                    c.table.as_ref().map(|t| t.value.as_str()),
                    &c.column.value,
                    span,
                )?;
                let ty = col.ty.clone().ok_or_else(|| {
                    SemaError::not_analyzed(format!("type de la colonne « {} »", col.name), span)
                })?;
                Ok(Typed::new(ty, col.nullable))
            }
            ExprKind::Interval(_) => Ok(Typed::new(pg("interval"), Nullability::NonNull)),
            ExprKind::Collate { expr, .. } => self.expr(expr, scope),
            ExprKind::Cast { expr, data_type } => self.cast(expr, data_type, scope, span),
            ExprKind::BinaryOp { left, op, right } => self.binary(left, op, right, scope, span),
            ExprKind::UnaryOp { op, expr } => match op {
                UnaryOp::Not => {
                    let t = self.boolean(expr, scope)?;
                    Ok(Typed::new(pg("bool"), t.nullable))
                }
                UnaryOp::IsNull | UnaryOp::IsNotNull => {
                    self.expr(expr, scope)?;
                    Ok(Typed::new(pg("bool"), Nullability::NonNull))
                }
                UnaryOp::Neg => self.operator("-", None, expr, scope, span),
            },
            ExprKind::FunctionCall { .. } => self.function(e, scope),
            ExprKind::Case {
                operand,
                when_clauses,
                else_clause,
            } => self.case(
                operand.as_deref(),
                when_clauses,
                else_clause.as_deref(),
                scope,
            ),
            ExprKind::Subquery(q) => {
                let cols = self.subquery(q, scope)?;
                match cols.as_slice() {
                    [c] => Ok(Typed::new(c.ty.clone(), Nullability::Nullable)),
                    _ => Err(SemaError::new(
                        "subquery-columns",
                        "subquery must return only one column",
                        span,
                    )),
                }
            }
            ExprKind::Exists { subquery, .. } => {
                self.subquery(subquery, scope)?;
                Ok(Typed::new(pg("bool"), Nullability::NonNull))
            }
            ExprKind::InSubquery { expr, subquery, .. } => {
                let cols = self.subquery(subquery, scope)?;
                let [c] = cols.as_slice() else {
                    return Err(SemaError::new(
                        "subquery-columns",
                        "subquery has too many columns",
                        span,
                    ));
                };
                let l = self.expr(expr, scope)?;
                let call = self
                    .ts
                    .resolve_operator(None, "=", Some(&l.ty), &c.ty, &self.path)
                    .map_err(|e| {
                        resolve_error(e, format!("operator {} = {}", l.ty.name, c.ty.name), span)
                    })?;
                self.coerce_to(expr, &call.arg_types[0]);
                Ok(Typed::new(pg("bool"), Nullability::Unknown))
            }
            ExprKind::InList { expr, list, .. } => self.in_list(expr, list, scope),
            ExprKind::Between {
                expr, low, high, ..
            } => {
                let a = self.operator(">=", Some(expr), low, scope, span)?;
                let b = self.operator("<=", Some(expr), high, scope, span)?;
                Ok(Typed::new(
                    pg("bool"),
                    Nullability::strict([a.nullable, b.nullable]),
                ))
            }
            ExprKind::Quantified {
                left, op, right, ..
            } => self.quantified(left, op, right, scope, span),
            ExprKind::AtTimeZone {
                timestamp,
                time_zone,
            } => {
                let z = self.expr(time_zone, scope)?;
                let t = self.expr(timestamp, scope)?;
                let call = self
                    .ts
                    .resolve_function(None, "timezone", &[z.ty.clone(), t.ty.clone()], &self.path)
                    .map_err(|e| resolve_error(e, "AT TIME ZONE".into(), span))?;
                self.coerce_to(time_zone, &call.arg_types[0]);
                self.coerce_to(timestamp, &call.arg_types[1]);
                Ok(Typed::new(
                    call.returns.expect("timezone renvoie une valeur"),
                    Nullability::strict([z.nullable, t.nullable]),
                ))
            }
            ExprKind::Tuple(items) => {
                for i in items {
                    self.expr(i, scope)?;
                }
                Ok(Typed::new(pg("record"), Nullability::NonNull))
            }
            ExprKind::FieldAccess { root, path } => self.field_access(root, path, scope, span),
            ExprKind::NamedArgument { .. } => Err(SemaError::not_analyzed(
                "argument nommé hors d'un appel",
                span,
            )),
            ExprKind::Wildcard | ExprKind::QualifiedWildcard(_) => {
                Err(SemaError::not_analyzed("`*` dans une expression", span))
            }
            ExprKind::Unsupported(what) => Err(SemaError::not_analyzed(what, span)),
        }
    }

    fn cast(
        &mut self,
        inner: &Expr,
        dt: &DataType,
        scope: &Scope,
        span: Span,
    ) -> Result<Typed, SemaError> {
        let target = self.data_type(dt, span)?;
        // `ARRAY[]::int[]` : le tableau vide prend le type de la cible.
        if is_array_constructor(inner).is_some_and(|args| args.is_empty()) {
            return Ok(Typed::new(target, Nullability::NonNull));
        }
        let t = self.expr(inner, scope)?;
        self.coerce_to(inner, &target);
        if t.ty != pg("unknown")
            && self.ts.pathway(&t.ty, &target, CoercionContext::Explicit) == Pathway::None
            && !(self.ts.is_complex(&t.ty) && self.ts.is_complex(&target))
        {
            return Err(SemaError::new(
                "type-mismatch",
                format!("cannot cast type {} to {}", t.ty.name, target.name),
                span,
            ));
        }
        Ok(Typed::new(target, t.nullable))
    }

    /// `left op right`, ou `op right` sans `left`.
    pub(crate) fn operator(
        &mut self,
        name: &str,
        left: Option<&Expr>,
        right: &Expr,
        scope: &Scope,
        span: Span,
    ) -> Result<Typed, SemaError> {
        let l = left.map(|e| self.expr(e, scope)).transpose()?;
        let r = self.expr(right, scope)?;
        let call = self
            .ts
            .resolve_operator(None, name, l.as_ref().map(|t| &t.ty), &r.ty, &self.path)
            .map_err(|e| {
                let what = match &l {
                    Some(l) => format!("operator {} {name} {}", l.ty.name, r.ty.name),
                    None => format!("operator {name} {}", r.ty.name),
                };
                resolve_error(e, what, span)
            })?;
        if let Some(le) = left {
            self.coerce_to(le, &call.arg_types[0]);
        }
        self.coerce_to(right, call.arg_types.last().expect("un opérande"));
        let inputs = l.iter().chain(Some(&r)).map(|t| t.nullable);
        Ok(Typed::new(
            call.returns.expect("un opérateur a un résultat"),
            Nullability::strict(inputs),
        ))
    }

    fn binary(
        &mut self,
        left: &Expr,
        op: &BinaryOp,
        right: &Expr,
        scope: &Scope,
        span: Span,
    ) -> Result<Typed, SemaError> {
        match op {
            BinaryOp::And | BinaryOp::Or => {
                let a = self.boolean(left, scope)?;
                let b = self.boolean(right, scope)?;
                let n = if a.nullable == Nullability::NonNull && b.nullable == Nullability::NonNull
                {
                    Nullability::NonNull
                } else {
                    Nullability::Unknown
                };
                Ok(Typed::new(pg("bool"), n))
            }
            BinaryOp::Is | BinaryOp::IsNot => {
                self.expr(left, scope)?;
                self.expr(right, scope)?;
                Ok(Typed::new(pg("bool"), Nullability::NonNull))
            }
            BinaryOp::IsDistinctFrom | BinaryOp::IsNotDistinctFrom => {
                self.operator("=", Some(left), right, scope, span)?;
                Ok(Typed::new(pg("bool"), Nullability::NonNull))
            }
            BinaryOp::In | BinaryOp::NotIn => {
                let t = self.operator("=", Some(left), right, scope, span)?;
                Ok(Typed::new(pg("bool"), t.nullable))
            }
            other => {
                let name = operator_name(other).expect("opérateur nommé");
                self.operator(name, Some(left), right, scope, span)
            }
        }
    }

    /// `x IN (a, b…)` (`transformAExprIn`) : type commun de `x` et de la
    /// liste, sinon une égalité par élément.
    fn in_list(&mut self, expr: &Expr, list: &[Expr], scope: &Scope) -> Result<Typed, SemaError> {
        let l = self.expr(expr, scope)?;
        let mut types = vec![l.ty.clone()];
        let mut nulls = vec![l.nullable];
        for item in list {
            let t = self.expr(item, scope)?;
            types.push(t.ty);
            nulls.push(t.nullable);
        }
        match self.ts.common_type(&types) {
            Ok(common) => {
                self.coerce_to(expr, &common);
                for item in list {
                    self.coerce_to(item, &common);
                }
                self.ts
                    .resolve_operator(None, "=", Some(&common), &common, &self.path)
                    .map_err(|e| {
                        resolve_error(
                            e,
                            format!("operator {} = {}", common.name, common.name),
                            expr.span,
                        )
                    })?;
            }
            Err(_) => {
                for item in list {
                    self.operator("=", Some(expr), item, scope, item.span)?;
                }
            }
        }
        Ok(Typed::new(pg("bool"), Nullability::strict(nulls)))
    }

    /// `left op ANY (tableau)` (`make_scalar_array_op`).
    fn quantified(
        &mut self,
        left: &Expr,
        op: &BinaryOp,
        right: &Expr,
        scope: &Scope,
        span: Span,
    ) -> Result<Typed, SemaError> {
        let name = operator_name(op)
            .ok_or_else(|| SemaError::not_analyzed("opérateur quantifié", span))?;
        let l = self.expr(left, scope)?;
        let (elem, from_subquery) = match &right.kind {
            ExprKind::Subquery(q) => {
                let cols = self.subquery(q, scope)?;
                let [c] = cols.as_slice() else {
                    return Err(SemaError::new(
                        "subquery-columns",
                        "subquery has too many columns",
                        span,
                    ));
                };
                (c.ty.clone(), true)
            }
            _ => {
                let r = self.expr(right, scope)?;
                if r.ty == pg("unknown") {
                    (r.ty, false)
                } else {
                    let base = self.ts.base_type(&r.ty);
                    let e = self.ts.element(&base).ok_or_else(|| {
                        SemaError::new(
                            "type-mismatch",
                            "op ANY/ALL (array) requires array on right side",
                            span,
                        )
                    })?;
                    (e, false)
                }
            }
        };
        let call = self
            .ts
            .resolve_operator(None, name, Some(&l.ty), &elem, &self.path)
            .map_err(|e| {
                resolve_error(
                    e,
                    format!("operator {} {name} {}", l.ty.name, elem.name),
                    span,
                )
            })?;
        self.coerce_to(left, &call.arg_types[0]);
        if !from_subquery && let Some(arr) = self.ts.array_of(&call.arg_types[1]) {
            self.coerce_to(right, &arr);
        }
        Ok(Typed::new(pg("bool"), Nullability::Unknown))
    }

    /// `CASE` : type commun des résultats, `ELSE` en tête comme dans
    /// PostgreSQL (`ELSE NULL` implicite).
    fn case(
        &mut self,
        operand: Option<&Expr>,
        whens: &[(Expr, Expr)],
        else_clause: Option<&Expr>,
        scope: &Scope,
    ) -> Result<Typed, SemaError> {
        for (cond, _) in whens {
            match operand {
                Some(op) => {
                    self.operator("=", Some(op), cond, scope, cond.span)?;
                }
                None => {
                    self.boolean(cond, scope)?;
                }
            }
        }
        let mut results: Vec<(&Expr, Typed)> = Vec::new();
        let else_typed = match else_clause {
            Some(e) => Some((e, self.expr(e, scope)?)),
            None => None,
        };
        let mut types = vec![
            else_typed
                .as_ref()
                .map_or_else(|| pg("unknown"), |(_, t)| t.ty.clone()),
        ];
        for (_, then) in whens {
            let t = self.expr(then, scope)?;
            types.push(t.ty.clone());
            results.push((then, t));
        }
        let common = self.ts.common_type(&types).map_err(|e| {
            SemaError::new(
                "type-mismatch",
                format!("CASE types cannot be matched: {e:?}"),
                whens[0].1.span,
            )
        })?;
        let mut nulls: Vec<Nullability> = results.iter().map(|(_, t)| t.nullable).collect();
        match &else_typed {
            Some((e, t)) => {
                self.coerce_to(e, &common);
                nulls.push(t.nullable);
            }
            None => nulls.push(Nullability::Nullable),
        }
        for (e, _) in &results {
            self.coerce_to(e, &common);
        }
        Ok(Typed::new(common, Nullability::strict(nulls)))
    }

    fn field_access(
        &mut self,
        root: &Expr,
        path: &[String],
        scope: &Scope,
        span: Span,
    ) -> Result<Typed, SemaError> {
        let mut t = self.expr(root, scope)?;
        for step in path {
            let base = self.ts.base_type(&t.ty);
            if let Some(field) = step.strip_prefix('.') {
                let attr = self.composite_attribute(&base, field).ok_or_else(|| {
                    SemaError::new(
                        "column-not-found",
                        format!("column \"{field}\" not found in data type {}", t.ty.name),
                        span,
                    )
                })?;
                t = Typed::new(attr, Nullability::Unknown);
            } else if step.starts_with('[') {
                if base == pg("jsonb") {
                    t = Typed::new(base, Nullability::Nullable);
                } else if let Some(elem) = self.ts.element(&base) {
                    let slice = has_top_level_colon(step);
                    t = Typed::new(if slice { base } else { elem }, Nullability::Nullable);
                } else {
                    return Err(SemaError::new(
                        "type-mismatch",
                        format!(
                            "cannot subscript type {} because it does not support subscripting",
                            t.ty.name
                        ),
                        span,
                    ));
                }
            } else {
                return Err(SemaError::not_analyzed(format!("accès « {step} »"), span));
            }
        }
        Ok(t)
    }

    /// Type d'un attribut de type composite (type ligne d'une table compris).
    fn composite_attribute(&self, ty: &Ty, field: &str) -> Option<Ty> {
        use pgweave_catalog::model::TypeKind;
        if let Some(TypeKind::Composite { attributes }) = self.ts.def(ty).map(|d| &d.kind) {
            return attributes
                .iter()
                .find(|a| a.name == field)
                .and_then(|a| self.ts.of_ref(&a.ty));
        }
        self.ts
            .catalog
            .relations
            .get(ty)
            .and_then(|r| r.columns.iter().find(|c| c.name == field))
            .and_then(|c| self.ts.of_ref(&c.ty))
    }

    fn function(&mut self, e: &Expr, scope: &Scope) -> Result<Typed, SemaError> {
        let ExprKind::FunctionCall {
            name,
            args,
            order_by,
            filter,
            within_group,
            over,
            ..
        } = &e.kind
        else {
            unreachable!("appel de fonction attendu")
        };
        let span = e.span;
        for o in order_by.iter().chain(within_group) {
            self.expr(&o.expr, scope)?;
        }
        if let Some(f) = filter {
            self.boolean(f, scope)?;
        }
        if let Some(w) = over.as_deref()
            && let pgweave_ast::expr::Window::Spec(spec) = w
        {
            for x in spec.exprs() {
                self.expr(x, scope)?;
            }
        }
        if let Some(elems) = is_array_constructor(e) {
            return self.array_constructor(elems, scope, span);
        }
        let (schema, fname) = split_name(name);
        if schema.is_none() {
            match fname {
                "coalesce" | "greatest" | "least" => {
                    return self.common_of(args, fname, scope, span);
                }
                "nullif" => {
                    let [a, b] = args.as_slice() else {
                        return Err(SemaError::new(
                            "function-not-found",
                            "nullif takes two arguments",
                            span,
                        ));
                    };
                    // Résultat : le type du premier argument après résolution
                    // de `=` (`transformNullIfExpr`).
                    self.operator("=", Some(a), b, scope, span)?;
                    let l = self.expr(a, scope)?;
                    let ty = if l.ty == pg("unknown") {
                        pg("text")
                    } else {
                        l.ty
                    };
                    return Ok(Typed::new(ty, Nullability::Nullable));
                }
                _ => {}
            }
            // Fonctions SQL sans parenthèses (`SQLValueFunction`), de type fixe.
            if args.is_empty()
                && let Some((ty, nullable)) = sql_value_function(fname)
            {
                return Ok(Typed::new(pg(ty), nullable));
            }
        }
        // `count(*)` : appel sans argument. Agrégat d'ensemble ordonné : les
        // expressions de `WITHIN GROUP` suivent les arguments directs.
        let args: Vec<&Expr> = match args.as_slice() {
            [a] if matches!(a.kind, ExprKind::Wildcard) => Vec::new(),
            _ => args
                .iter()
                .chain(within_group.iter().map(|o| &o.expr))
                .collect(),
        };
        let mut typed = Vec::with_capacity(args.len());
        for a in &args {
            if let ExprKind::NamedArgument { .. } = a.kind {
                return Err(SemaError::not_analyzed("argument nommé", a.span));
            }
            typed.push(self.expr(a, scope)?);
        }
        let inputs: Vec<Ty> = typed.iter().map(|t| t.ty.clone()).collect();
        let call: ResolvedCall = self
            .ts
            .resolve_function(schema, fname, &inputs, &self.path)
            .map_err(|err| {
                resolve_error(
                    err,
                    format!("function {fname}({})", type_list(&inputs)),
                    span,
                )
            })?;
        for (a, t) in args.iter().zip(&call.arg_types) {
            self.coerce_to(a, t);
        }
        let ty = call.returns.clone().ok_or_else(|| {
            SemaError::new("type-mismatch", format!("{fname} is a procedure"), span)
        })?;
        let nullable = match call.target {
            Resolution::Coercion => typed[0].nullable,
            Resolution::Routine(r) => match r.kind {
                RoutineKind::Aggregate if fname == "count" => Nullability::NonNull,
                RoutineKind::Aggregate => Nullability::Nullable,
                _ if r.strict => Nullability::strict(typed.iter().map(|t| t.nullable)),
                _ => Nullability::Unknown,
            },
            Resolution::Operator(_) => Nullability::Unknown,
        };
        Ok(Typed::new(ty, nullable))
    }

    /// `COALESCE`, `GREATEST`, `LEAST` : type commun des arguments.
    fn common_of(
        &mut self,
        args: &[Expr],
        what: &str,
        scope: &Scope,
        span: Span,
    ) -> Result<Typed, SemaError> {
        let mut typed = Vec::new();
        for a in args {
            typed.push(self.expr(a, scope)?);
        }
        let types: Vec<Ty> = typed.iter().map(|t| t.ty.clone()).collect();
        let common = self.ts.common_type(&types).map_err(|e| {
            SemaError::new(
                "type-mismatch",
                format!("{} types cannot be matched: {e:?}", what.to_uppercase()),
                span,
            )
        })?;
        for a in args {
            self.coerce_to(a, &common);
        }
        let nullable = if what == "coalesce" {
            if typed.iter().any(|t| t.nullable == Nullability::NonNull) {
                Nullability::NonNull
            } else if typed.iter().all(|t| t.nullable == Nullability::Nullable) {
                Nullability::Nullable
            } else {
                Nullability::Unknown
            }
        } else {
            Nullability::strict(typed.iter().map(|t| t.nullable))
        };
        Ok(Typed::new(common, nullable))
    }

    /// `ARRAY[…]` : tableau du type commun des éléments ; des éléments
    /// tableaux donnent un tableau à plusieurs dimensions du même type.
    fn array_constructor(
        &mut self,
        elems: &[Expr],
        scope: &Scope,
        span: Span,
    ) -> Result<Typed, SemaError> {
        if elems.is_empty() {
            return Err(SemaError::new(
                "type-mismatch",
                "cannot determine type of empty array",
                span,
            ));
        }
        let mut types = Vec::new();
        for e in elems {
            types.push(self.expr(e, scope)?.ty);
        }
        let common = self.ts.common_type(&types).map_err(|e| {
            SemaError::new(
                "type-mismatch",
                format!("ARRAY types cannot be matched: {e:?}"),
                span,
            )
        })?;
        for e in elems {
            self.coerce_to(e, &common);
        }
        let ty = if self.ts.element(&common).is_some() {
            common
        } else {
            self.ts.array_of(&common).ok_or_else(|| {
                SemaError::new(
                    "type-mismatch",
                    format!("could not find array type for data type {}", common.name),
                    span,
                )
            })?
        };
        Ok(Typed::new(ty, Nullability::NonNull))
    }

    pub(crate) fn subquery(
        &mut self,
        q: &SelectStmt,
        scope: &Scope,
    ) -> Result<Vec<crate::query::OutputColumn>, SemaError> {
        self.select(q, Some(scope), true)
    }
}

/// Type et nullabilité des fonctions SQL sans parenthèses (`SQLValueFunction`).
fn sql_value_function(name: &str) -> Option<(&'static str, Nullability)> {
    Some(match name {
        "current_date" => ("date", Nullability::NonNull),
        "current_time" => ("timetz", Nullability::NonNull),
        "current_timestamp" => ("timestamptz", Nullability::NonNull),
        "localtime" => ("time", Nullability::NonNull),
        "localtimestamp" => ("timestamp", Nullability::NonNull),
        "current_role" | "current_user" | "user" | "session_user" | "current_catalog" => {
            ("name", Nullability::NonNull)
        }
        // NULL si aucun schéma du chemin n'existe.
        "current_schema" => ("name", Nullability::Nullable),
        _ => return None,
    })
}

/// Éléments d'un constructeur `ARRAY[…]` (appel non quoté à `array`).
fn is_array_constructor(e: &Expr) -> Option<&[Expr]> {
    match &e.kind {
        ExprKind::FunctionCall { name, args, .. }
            if name.0.len() == 1 && name.0[0].value == "array" && !name.0[0].quoted =>
        {
            Some(args)
        }
        _ => None,
    }
}

pub(crate) fn split_name(name: &ObjectName) -> (Option<&str>, &str) {
    match name.0.as_slice() {
        [.., s, n] => (Some(s.value.as_str()), n.value.as_str()),
        [n] => (None, n.value.as_str()),
        [] => (None, ""),
    }
}

/// `[a:b]` : tranche, à distinguer de `['a:b']`.
fn has_top_level_colon(step: &str) -> bool {
    let mut quoted = false;
    for c in step.chars() {
        match c {
            '\'' => quoted = !quoted,
            ':' if !quoted => return true,
            _ => {}
        }
    }
    false
}
