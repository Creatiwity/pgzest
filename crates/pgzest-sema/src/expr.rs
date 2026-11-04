//! Expression typing (`parse_expr.c`): each expression gets a type and a
//! nullability; parameters get the type their first use imposes on them,
//! as in PostgreSQL.

use pgzest_ast::expr::{BinaryOp, Expr, ExprKind, Literal, ObjectName, UnaryOp};
use pgzest_ast::types::DataType;
use pgzest_ast::{Span, stmt::SelectStmt};
use pgzest_catalog::SearchPath;
use pgzest_catalog::model::RoutineKind;
use pgzest_diagnostics::codes;

use crate::coerce::{CoercionContext, Pathway};
use crate::resolve::{Resolution, ResolveError, ResolvedCall};
use crate::routine::{Language, Routine, VarRef, VarTy};
use crate::scope::{Nullability, Scope, SemaError};
use crate::types::{Ty, TypeSystem, pg};

/// Type and nullability of an expression.
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

/// Query parameter: `$1`… or `$name` (D6), in order.
#[derive(Debug, Clone, PartialEq)]
pub struct Param {
    pub name: String,
    /// `None` as long as no use has fixed its type.
    pub ty: Option<Ty>,
    /// NULL accepted? Established when the parameter is assigned to a column
    /// (which is `NOT NULL` or not); unknown otherwise.
    pub nullable: Nullability,
}

/// Analyzer of a statement: type system, search path, parameters
/// encountered, visible CTEs.
pub struct Analyzer<'c> {
    pub ts: TypeSystem<'c>,
    pub path: SearchPath,
    pub params: Vec<Param>,
    /// Visible CTEs, the most recent last.
    pub(crate) ctes: Vec<(String, Vec<crate::scope::Column>)>,
    /// Routine whose body is being analyzed: its parameters and variables.
    pub routine: Option<Routine>,
    /// References resolved to a routine variable: position of the
    /// expression and type of the value (what PostgreSQL passes as a
    /// parameter to the query).
    pub var_refs: Vec<(Span, Ty)>,
    /// Resolved column (or variable) references, with their type and
    /// nullability: what each name of the query designates.
    pub column_refs: Vec<(Span, Typed)>,
}

/// PostgreSQL operator name of an AST binary operator.
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

/// What we were trying to resolve, for the error message.
enum Subject<'a> {
    Operator(&'a str, Option<&'a Ty>, &'a Ty),
    Function(&'a str, &'a [Ty]),
    Other(String),
}

/// Name of a type as `format_type` displays it (`integer`, `text[]`…).
pub(crate) fn format_type(ty: &Ty) -> String {
    if ty.schema != "pg_catalog" {
        return ty.name.clone();
    }
    if let Some(elem) = ty.name.strip_prefix('_') {
        return format!("{}[]", format_type(&pg(elem)));
    }
    match ty.name.as_str() {
        "int2" => "smallint",
        "int4" => "integer",
        "int8" => "bigint",
        "float4" => "real",
        "float8" => "double precision",
        "bool" => "boolean",
        "varchar" => "character varying",
        "bpchar" => "character",
        "timestamp" => "timestamp without time zone",
        "timestamptz" => "timestamp with time zone",
        "time" => "time without time zone",
        "timetz" => "time with time zone",
        "varbit" => "bit varying",
        other => other,
    }
    .to_string()
}

/// Resolution error, with PostgreSQL's messages (`parse_oper.c`,
/// `parse_func.c`).
fn resolve_error(e: ResolveError, subject: Subject, span: Span) -> SemaError {
    const CAST: &str = "You might need to add explicit type casts.";
    let types = |tys: &[Ty]| tys.iter().map(format_type).collect::<Vec<_>>().join(", ");
    match (e, subject) {
        (ResolveError::Polymorphic(m), _) => SemaError::new(codes::TYPE_MISMATCH, m, span),
        (e, Subject::Operator(op, l, r)) => {
            let sig = match l {
                Some(l) => format!("{} {op} {}", format_type(l), format_type(r)),
                None => format!("{op} {}", format_type(r)),
            };
            match e {
                ResolveError::Ambiguous => SemaError::new(
                    codes::AMBIGUOUS_FUNCTION,
                    format!("operator is not unique: {sig}"),
                    span,
                )
                .with_hint(format!(
                    "Could not choose a best candidate operator. {CAST}"
                )),
                _ => SemaError::new(
                    codes::FUNCTION_NOT_FOUND,
                    format!("operator does not exist: {sig}"),
                    span,
                )
                .with_hint(format!(
                    "No operator matches the given name and argument types. {CAST}"
                )),
            }
        }
        (e, Subject::Function(name, args)) => {
            let sig = format!("{name}({})", types(args));
            match e {
                ResolveError::Ambiguous => SemaError::new(
                    codes::AMBIGUOUS_FUNCTION,
                    format!("function {sig} is not unique"),
                    span,
                )
                .with_hint(format!(
                    "Could not choose a best candidate function. {CAST}"
                )),
                _ => SemaError::new(
                    codes::FUNCTION_NOT_FOUND,
                    format!("function {sig} does not exist"),
                    span,
                )
                .with_hint(format!(
                    "No function matches the given name and argument types. {CAST}"
                )),
            }
        }
        (ResolveError::Ambiguous, Subject::Other(what)) => SemaError::new(
            codes::AMBIGUOUS_FUNCTION,
            format!("{what} is not unique"),
            span,
        ),
        (_, Subject::Other(what)) => SemaError::new(
            codes::FUNCTION_NOT_FOUND,
            format!("{what} does not exist"),
            span,
        ),
    }
}

/// Column designated by `c` in the query scope, or whole row of a
/// source named `c`.
fn column_ref(
    c: &pgzest_ast::expr::ColumnRef,
    scope: &Scope,
    span: Span,
) -> Result<Typed, SemaError> {
    let col = match scope.lookup_ref(c, span) {
        Ok(col) => col,
        // Name of a source: the whole row, of its row type.
        Err(e) if e.code == codes::COLUMN_NOT_FOUND && c.table.is_none() => {
            return match scope.whole_row(&c.column.value) {
                Some(ty) => Ok(Typed::new(ty, Nullability::Unknown)),
                None => Err(e),
            };
        }
        Err(e) => return Err(e),
    };
    let ty = col
        .ty
        .clone()
        .ok_or_else(|| SemaError::not_analyzed(format!("type of column \"{}\"", col.name), span))?;
    Ok(Typed::new(ty, col.nullable))
}

fn emit_column_ref(c: &pgzest_ast::expr::ColumnRef) -> String {
    [c.schema.as_ref(), c.table.as_ref(), Some(&c.column)]
        .into_iter()
        .flatten()
        .map(|i| i.value.as_str())
        .collect::<Vec<_>>()
        .join(".")
}

/// Type of a numeric literal (`make_const`): integer on 4 then 8
/// bytes, otherwise `numeric`.
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
            routine: None,
            var_refs: Vec::new(),
            column_refs: Vec::new(),
        }
    }

    /// Index of the parameter `$n` or `$name`.
    fn param_index(&mut self, name: &str) -> usize {
        if let Some(n) = name.strip_prefix('$').and_then(|d| d.parse::<usize>().ok())
            && n >= 1
        {
            while self.params.len() < n {
                let i = self.params.len() + 1;
                self.params.push(Param {
                    name: format!("${i}"),
                    ty: None,
                    nullable: Nullability::Unknown,
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
                    nullable: Nullability::Unknown,
                });
                self.params.len() - 1
            }
        }
    }

    /// Fixes the type of a not-yet-typed parameter (`coerce_type` of an
    /// unknown parameter towards its target).
    /// The parameter `e` (if it is one) is assigned a value of this
    /// nullability: a `NOT NULL` column refuses it as null.
    pub(crate) fn param_nullability(&mut self, e: &Expr, n: Nullability) {
        let mut e = e;
        while let ExprKind::Collate { expr, .. } | ExprKind::Cast { expr, .. } = &e.kind {
            e = expr;
        }
        if let ExprKind::Parameter(name) = &e.kind
            && self.routine.is_none()
        {
            let i = self.param_index(name);
            if self.params[i].nullable == Nullability::Unknown {
                self.params[i].nullable = n;
            }
        }
    }

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

    /// Written type (`CAST`, `::`).
    pub fn data_type(&self, dt: &DataType, span: Span) -> Result<Ty, SemaError> {
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
                        codes::TYPE_NOT_FOUND,
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
                    .map(|t| {
                        self.ts
                            .depend(crate::types::Dependency::Type(t.name.clone()));
                        t.name.clone()
                    })
                    .ok_or_else(|| {
                        SemaError::new(
                            codes::TYPE_NOT_FOUND,
                            format!("type \"{name}\" does not exist"),
                            span,
                        )
                    });
            }
        };
        Ok(pg(builtin))
    }

    /// Expression expected to be boolean (`WHERE`, `AND`…).
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
                codes::TYPE_MISMATCH,
                format!(
                    "argument must be type boolean, not type {}",
                    format_type(&t.ty)
                ),
                e.span,
            ));
        }
        Ok(Typed::new(pg("bool"), t.nullable))
    }

    /// Type of a resolution input: an untyped parameter counts as an
    /// `unknown` literal.
    pub fn expr(&mut self, e: &Expr, scope: &Scope) -> Result<Typed, SemaError> {
        let span = e.span;
        match &e.kind {
            ExprKind::Literal(lit) => Ok(match lit {
                Literal::Number(n) => Typed::new(number_type(n), Nullability::NonNull),
                Literal::String(_) => Typed::new(pg("unknown"), Nullability::NonNull),
                Literal::Boolean(_) => Typed::new(pg("bool"), Nullability::NonNull),
                Literal::Null => Typed::new(pg("unknown"), Nullability::Nullable),
                Literal::BitString(_) => Typed::new(pg("bit"), Nullability::NonNull),
            }),
            ExprKind::Parameter(name) if self.routine.is_some() => {
                self.routine_parameter(name, span)
            }
            ExprKind::Parameter(name) => {
                let i = self.param_index(name);
                Ok(Typed::new(
                    self.params[i].ty.clone().unwrap_or_else(|| pg("unknown")),
                    Nullability::Unknown,
                ))
            }
            ExprKind::Column(c) => {
                let t = self.column(c, scope, span)?;
                if span != Span::DUMMY {
                    self.column_refs.push((span, t.clone()));
                }
                Ok(t)
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
                UnaryOp::Prefix(op) => self.operator(op, None, expr, scope, span),
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
                        codes::SUBQUERY_COLUMNS,
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
                // `(a, b) IN (SELECT x, y …)`: row-by-row comparison
                // (`make_row_comparison_op`).
                if let ExprKind::Tuple(items) = &expr.kind {
                    if items.len() != cols.len() {
                        return Err(SemaError::new(
                            codes::SUBQUERY_COLUMNS,
                            if items.len() < cols.len() {
                                "subquery has too many columns"
                            } else {
                                "subquery has too few columns"
                            },
                            span,
                        ));
                    }
                    for (item, c) in items.iter().zip(&cols) {
                        let l = self.expr(item, scope)?;
                        let call = self
                            .ts
                            .resolve_operator(None, "=", Some(&l.ty), &c.ty, &self.path)
                            .map_err(|e| {
                                resolve_error(e, Subject::Operator("=", Some(&l.ty), &c.ty), span)
                            })?;
                        self.coerce_to(item, &call.arg_types[0]);
                    }
                    return Ok(Typed::new(pg("bool"), Nullability::Unknown));
                }
                let [c] = cols.as_slice() else {
                    return Err(SemaError::new(
                        codes::SUBQUERY_COLUMNS,
                        "subquery has too many columns",
                        span,
                    ));
                };
                let l = self.expr(expr, scope)?;
                let call = self
                    .ts
                    .resolve_operator(None, "=", Some(&l.ty), &c.ty, &self.path)
                    .map_err(|e| {
                        resolve_error(e, Subject::Operator("=", Some(&l.ty), &c.ty), span)
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
                    .map_err(|e| resolve_error(e, Subject::Other("AT TIME ZONE".into()), span))?;
                self.coerce_to(time_zone, &call.arg_types[0]);
                self.coerce_to(timestamp, &call.arg_types[1]);
                Ok(Typed::new(
                    call.returns.expect("timezone returns a value"),
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
                "named argument outside a call",
                span,
            )),
            ExprKind::Wildcard | ExprKind::QualifiedWildcard(_) => {
                Err(SemaError::not_analyzed("`*` in an expression", span))
            }
            ExprKind::GroupingSets { .. } => Err(SemaError::new(
                codes::TYPE_MISMATCH,
                "ROLLUP, CUBE and GROUPING SETS are only allowed in GROUP BY",
                span,
            )),
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
        // `ARRAY[]::int[]`: the empty array takes the type of the target.
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
                codes::TYPE_MISMATCH,
                format!(
                    "cannot cast type {} to {}",
                    format_type(&t.ty),
                    format_type(&target)
                ),
                span,
            ));
        }
        Ok(Typed::new(target, t.nullable))
    }

    /// `left op right`, or `op right` without `left`.
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
                resolve_error(
                    e,
                    Subject::Operator(name, l.as_ref().map(|t| &t.ty), &r.ty),
                    span,
                )
            })?;
        if let Some(le) = left {
            self.coerce_to(le, &call.arg_types[0]);
        }
        self.coerce_to(right, call.arg_types.last().expect("one operand"));
        let inputs = l.iter().chain(Some(&r)).map(|t| t.nullable);
        Ok(Typed::new(
            call.returns.expect("an operator has a result"),
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
        // Row comparison: operator resolved pair by pair
        // (`make_row_comparison_op`).
        if let (ExprKind::Tuple(ls), ExprKind::Tuple(rs)) = (&left.kind, &right.kind)
            && let Some(name) =
                operator_name(op).filter(|n| matches!(*n, "=" | "<>" | "<" | "<=" | ">" | ">="))
        {
            if ls.len() != rs.len() {
                return Err(SemaError::new(
                    codes::TYPE_MISMATCH,
                    "unequal number of entries in row expressions",
                    span,
                ));
            }
            let mut nulls = Vec::new();
            for (l, r) in ls.iter().zip(rs) {
                nulls.push(self.operator(name, Some(l), r, scope, span)?.nullable);
            }
            return Ok(Typed::new(pg("bool"), Nullability::strict(nulls)));
        }
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
                let name = operator_name(other).expect("named operator");
                self.operator(name, Some(left), right, scope, span)
            }
        }
    }

    /// `x IN (a, b…)` (`transformAExprIn`): common type of `x` and of the
    /// list, otherwise one equality per element.
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
                        resolve_error(e, Subject::Operator("=", Some(&common), &common), expr.span)
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

    /// `left op ANY (array)` (`make_scalar_array_op`).
    fn quantified(
        &mut self,
        left: &Expr,
        op: &BinaryOp,
        right: &Expr,
        scope: &Scope,
        span: Span,
    ) -> Result<Typed, SemaError> {
        let name = operator_name(op)
            .ok_or_else(|| SemaError::not_analyzed("quantified operator", span))?;
        let l = self.expr(left, scope)?;
        let (elem, from_subquery) = match &right.kind {
            ExprKind::Subquery(q) => {
                let cols = self.subquery(q, scope)?;
                let [c] = cols.as_slice() else {
                    return Err(SemaError::new(
                        codes::SUBQUERY_COLUMNS,
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
                            codes::TYPE_MISMATCH,
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
            .map_err(|e| resolve_error(e, Subject::Operator(name, Some(&l.ty), &elem), span))?;
        self.coerce_to(left, &call.arg_types[0]);
        if !from_subquery && let Some(arr) = self.ts.array_of(&call.arg_types[1]) {
            self.coerce_to(right, &arr);
        }
        Ok(Typed::new(pg("bool"), Nullability::Unknown))
    }

    /// `CASE`: common type of the results, `ELSE` first as in
    /// PostgreSQL (implicit `ELSE NULL`).
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
                codes::TYPE_MISMATCH,
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
                        codes::COLUMN_NOT_FOUND,
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
                        codes::TYPE_MISMATCH,
                        format!(
                            "cannot subscript type {} because it does not support subscripting",
                            t.ty.name
                        ),
                        span,
                    ));
                }
            } else {
                return Err(SemaError::not_analyzed(format!("access \"{step}\""), span));
            }
        }
        Ok(t)
    }

    /// Type of an attribute of a composite type (row type of a table included).
    /// Column of the query scope or, in a routine body, variable
    /// designated by `c` (see [`crate::routine`]).
    fn column(
        &mut self,
        c: &pgzest_ast::expr::ColumnRef,
        scope: &Scope,
        span: Span,
    ) -> Result<Typed, SemaError> {
        // `DEFAULT` is only a value when assigned to a column (`VALUES` of an
        // INSERT, `SET`), handled before reaching here (`SetToDefault`).
        if c.schema.is_none()
            && c.table.is_none()
            && !c.column.quoted
            && c.column.value == "default"
        {
            return Err(SemaError::new(
                codes::DEFAULT_NOT_ALLOWED,
                "DEFAULT is not allowed in this context",
                span,
            ));
        }
        let core = column_ref(c, scope, span);
        let Some(routine) = &self.routine else {
            return core;
        };
        match core {
            Err(e) if e.code == codes::AMBIGUOUS_COLUMN => Err(e),
            Ok(t) => match (routine.language, routine.lookup(c)) {
                (Language::Plpgsql, Some(_)) => Err(SemaError::new(
                    codes::AMBIGUOUS_COLUMN,
                    format!(
                        "column reference \"{}\" is ambiguous (PL/pgSQL variable or table column)",
                        emit_column_ref(c)
                    ),
                    span,
                )),
                _ => Ok(t),
            },
            Err(e) => match routine.lookup(c) {
                Some(v) => {
                    let t = self.variable(v, span)?;
                    self.var_refs.push((span, t.ty.clone()));
                    Ok(t)
                }
                // PL/pgSQL expression without `FROM`: the name can only
                // designate a variable.
                None if routine.language == Language::Plpgsql
                    && e.code == codes::COLUMN_NOT_FOUND
                    && c.table.is_none()
                    && scope.is_empty() =>
                {
                    let mut known: Vec<&str> = routine
                        .frames
                        .iter()
                        .flat_map(|f| &f.vars)
                        .map(|v| v.name.as_str())
                        .filter(|n| !n.starts_with("tg_") && *n != "found")
                        .collect();
                    known.sort_unstable();
                    known.dedup();
                    let err = SemaError::new(
                        codes::UNDECLARED_VARIABLE,
                        format!("variable '{}' is not declared", c.column.value),
                        span,
                    );
                    Err(if known.is_empty() {
                        err.with_hint("declare it in the DECLARE section or as a parameter")
                    } else {
                        err.with_hint(format!(
                            "declared variables and parameters: {}",
                            known.join(", ")
                        ))
                    })
                }
                None => Err(e),
            },
        }
    }

    /// Value of a routine variable or of one of its fields. A
    /// variable is never asserted non-null.
    pub(crate) fn variable(&self, v: VarRef, span: Span) -> Result<Typed, SemaError> {
        let unknown = |why: &str| SemaError::not_analyzed(why.to_string(), span);
        let ty = match v {
            VarRef::Whole(var) => match var.ty {
                VarTy::Scalar(t) | VarTy::Row(t) => t,
                VarTy::Record(Some(_)) => pg("record"),
                VarTy::Record(None) => {
                    return Err(unknown(&format!(
                        "structure of record \"{}\", not assigned at this point",
                        var.name
                    )));
                }
                VarTy::Unknown(why) => return Err(unknown(&why)),
            },
            VarRef::Field(var, field) => {
                let found = match &var.ty {
                    VarTy::Row(t) => {
                        let base = self.ts.base_type(t);
                        self.composite_attribute(&base, &field)
                    }
                    VarTy::Record(Some(cols)) => match cols.iter().find(|c| c.name == field) {
                        Some(c) => Some(c.ty.clone().ok_or_else(|| {
                            unknown(&format!("type of field \"{}.{field}\"", var.name))
                        })?),
                        None => None,
                    },
                    VarTy::Record(None) => {
                        return Err(unknown(&format!(
                            "structure of record \"{}\", not assigned at this point",
                            var.name
                        )));
                    }
                    VarTy::Unknown(why) => return Err(unknown(why)),
                    VarTy::Scalar(_) => None,
                };
                found.ok_or_else(|| {
                    // The row type is named: for NEW and OLD, it is
                    // the trigger's table.
                    let of = match &var.ty {
                        VarTy::Row(t) => format!(" (type {})", t.name),
                        _ => String::new(),
                    };
                    SemaError::new(
                        codes::COLUMN_NOT_FOUND,
                        format!("record \"{}\" has no field \"{field}\"{of}", var.name),
                        span,
                    )
                })?
            }
        };
        Ok(Typed::new(ty, Nullability::Unknown))
    }

    /// `$n` in a routine body: the n-th parameter.
    fn routine_parameter(&mut self, name: &str, span: Span) -> Result<Typed, SemaError> {
        let n = name
            .strip_prefix('$')
            .and_then(|d| d.parse::<usize>().ok())
            .filter(|n| *n >= 1);
        let ty = n.and_then(|n| self.routine.as_ref()?.positional.get(n - 1).cloned());
        let Some(ty) = ty else {
            return Err(SemaError::new(
                codes::PARAMETER_NOT_FOUND,
                format!("there is no parameter {name}"),
                span,
            ));
        };
        let t = self.variable(
            VarRef::Whole(crate::routine::Var {
                name: name.to_string(),
                ty,
            }),
            span,
        )?;
        self.var_refs.push((span, t.ty.clone()));
        Ok(t)
    }

    pub(crate) fn composite_attribute(&self, ty: &Ty, field: &str) -> Option<Ty> {
        use pgzest_catalog::model::TypeKind;
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
            unreachable!("function call expected")
        };
        let span = e.span;
        for o in order_by.iter().chain(within_group) {
            self.expr(&o.expr, scope)?;
        }
        if let Some(f) = filter {
            self.boolean(f, scope)?;
        }
        if let Some(w) = over.as_deref()
            && let pgzest_ast::expr::Window::Spec(spec) = w
        {
            for x in spec.exprs() {
                self.expr(x, scope)?;
            }
        }
        // `ROW(a, b)`: row constructor, like `(a, b)`.
        if name.0.len() == 1 && name.0[0].value == "row" && !name.0[0].quoted {
            for a in args {
                self.expr(a, scope)?;
            }
            return Ok(Typed::new(pg("record"), Nullability::NonNull));
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
                            codes::FUNCTION_NOT_FOUND,
                            "nullif takes two arguments",
                            span,
                        ));
                    };
                    // Result: the type of the first argument after resolving
                    // `=` (`transformNullIfExpr`).
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
            // `grouping(a, b)`: a grammar construct (`GroupingFunc`),
            // `int4` bit mask.
            if fname == "grouping" {
                for a in args {
                    self.expr(a, scope)?;
                }
                return Ok(Typed::new(pg("int4"), Nullability::NonNull));
            }
            // SQL functions without parentheses (`SQLValueFunction`), of fixed type.
            if args.is_empty()
                && let Some((ty, nullable)) = sql_value_function(fname)
            {
                return Ok(Typed::new(pg(ty), nullable));
            }
        }
        // `count(*)`: call without arguments. Ordered-set aggregate: the
        // `WITHIN GROUP` expressions follow the direct arguments.
        let args: Vec<&Expr> = match args.as_slice() {
            [a] if matches!(a.kind, ExprKind::Wildcard) => Vec::new(),
            _ => args
                .iter()
                .chain(within_group.iter().map(|o| &o.expr))
                .collect(),
        };
        // Named arguments (`name => value`): the value, and its name.
        let mut names: Vec<Option<String>> = Vec::with_capacity(args.len());
        let args: Vec<&Expr> = args
            .into_iter()
            .map(|a| match &a.kind {
                ExprKind::NamedArgument { name, value } => {
                    names.push(Some(name.value.clone()));
                    &**value
                }
                _ => {
                    names.push(None);
                    a
                }
            })
            .collect();
        let mut typed = Vec::with_capacity(args.len());
        for a in &args {
            typed.push(self.expr(a, scope)?);
        }
        let inputs: Vec<Ty> = typed.iter().map(|t| t.ty.clone()).collect();
        let call: ResolvedCall = self
            .ts
            .resolve_function_named(schema, fname, &inputs, &names, &self.path)
            .map_err(|err| resolve_error(err, Subject::Function(fname, &inputs), span))?;
        for (a, t) in args.iter().zip(&call.arg_types) {
            self.coerce_to(a, t);
        }
        let ty = call.returns.clone().ok_or_else(|| {
            SemaError::new(
                codes::TYPE_MISMATCH,
                format!("{fname} is a procedure"),
                span,
            )
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

    /// `COALESCE`, `GREATEST`, `LEAST`: common type of the arguments.
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
                codes::TYPE_MISMATCH,
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

    /// `ARRAY[…]`: array of the common type of the elements; array
    /// elements give a multidimensional array of the same type.
    fn array_constructor(
        &mut self,
        elems: &[Expr],
        scope: &Scope,
        span: Span,
    ) -> Result<Typed, SemaError> {
        if elems.is_empty() {
            return Err(SemaError::new(
                codes::TYPE_MISMATCH,
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
                codes::TYPE_MISMATCH,
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
                    codes::TYPE_MISMATCH,
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

/// Type and nullability of SQL functions without parentheses (`SQLValueFunction`).
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
        // NULL if no schema of the path exists.
        "current_schema" => ("name", Nullability::Nullable),
        _ => return None,
    })
}

/// Elements of an `ARRAY[…]` constructor (unquoted call to `array`).
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

/// `[a:b]`: slice, as opposed to `['a:b']`.
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
