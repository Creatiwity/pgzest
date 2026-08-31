//! Conversion layer: sqlparser 0.62 AST → pgsql_ast types.
//!
//! Entry point is [`convert_statement`].

use pgsql_ast::{
    Statement,
    ddl::{
        AlterColumnAction, AlterTable, AlterTableOp, ColumnConstraint, ColumnDef, CreateFunction,
        CreateIndex, CreatePolicy, CreateProcedure, CreateTable, CreateTrigger, DdlStatement,
        FunctionBody, FunctionLanguage, FunctionParam, ParamMode, PolicyCommand, ReferentialAction,
        ReturnType, TableConstraint, TriggerEvent, TriggerForEach, TriggerTiming, Volatility,
    },
    expr::{BinaryOp, ColumnRef, Expr, Ident, Literal, ObjectName, UnaryOp},
    node::Span,
    stmt::{
        Assignment, Cte, DeleteStmt, InsertSource, InsertStmt, Join, JoinConstraint, JoinKind,
        OnConflict, OnConflictAction, OnConflictTarget, OrderByExpr, SelectItem, SelectStmt,
        TableRef, UpdateStmt,
    },
    types::DataType,
};

use crate::error::ParseError;
use pgsql_ast::expr::ExprKind;
use sqlparser::ast as sp;

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Extract an `Ident` from an `ObjectNamePart`, discarding function parts.
fn part_to_ident(part: sp::ObjectNamePart) -> Option<sp::Ident> {
    match part {
        sp::ObjectNamePart::Identifier(id) => Some(id),
        sp::ObjectNamePart::Function(_) => None,
    }
}

/// Convert an `IndexColumn` (used in PrimaryKey / Unique constraints) to a
/// plain identifier by examining the column expression.
fn convert_index_column(ic: sp::IndexColumn) -> Result<Ident, ParseError> {
    convert_ident_from_expr(ic.column.expr)
}

// ── Public entry point ────────────────────────────────────────────────────────

/// Convertit un statement en attachant aux expressions leur position dans le
/// document.
///
/// `source` est le texte exact soumis à sqlparser et `base` son offset dans le
/// document. Les deux doivent correspondre : si le texte a été réécrit avant le
/// parse (neutralisation des CHECK, prétraitement des fonctions), les positions
/// ne sont plus celles du document et l'appelant doit utiliser
/// [`convert_statement`], qui laisse les spans à `DUMMY`.
pub fn convert_statement_at(
    stmt: sp::Statement,
    source: &str,
    base: usize,
) -> Result<Statement, ParseError> {
    let _guard = enter_span_ctx(source, base);
    convert_statement(stmt)
}

pub fn convert_statement(stmt: sp::Statement) -> Result<Statement, ParseError> {
    match stmt {
        sp::Statement::Query(q) => {
            let sel = convert_query(*q)?;
            Ok(Statement::Select(sel))
        }
        sp::Statement::Insert(ins) => {
            let s = convert_insert(ins)?;
            Ok(Statement::Insert(s))
        }
        sp::Statement::Update(upd) => {
            let s = convert_update(
                upd.table,
                upd.assignments,
                upd.from,
                upd.selection,
                upd.returning,
            )?;
            Ok(Statement::Update(s))
        }
        sp::Statement::Delete(del) => {
            let s = convert_delete(del)?;
            Ok(Statement::Delete(s))
        }
        sp::Statement::CreateTable(ct) => {
            let ddl = convert_create_table(ct)?;
            Ok(Statement::Ddl(DdlStatement::CreateTable(ddl)))
        }
        sp::Statement::CreateIndex(ci) => {
            let ddl = convert_create_index(ci)?;
            Ok(Statement::Ddl(DdlStatement::CreateIndex(ddl)))
        }
        sp::Statement::CreateFunction(cf) => {
            let ddl = convert_create_function(cf)?;
            Ok(Statement::Ddl(DdlStatement::CreateFunction(ddl)))
        }
        sp::Statement::CreateProcedure {
            or_alter,
            name,
            params,
            body,
            language: _,
        } => {
            let ddl = convert_create_procedure(or_alter, name, params, body)?;
            Ok(Statement::Ddl(DdlStatement::CreateProcedure(ddl)))
        }
        sp::Statement::CreateTrigger(ct) => {
            let ddl = convert_create_trigger(ct)?;
            Ok(Statement::Ddl(DdlStatement::CreateTrigger(ddl)))
        }
        sp::Statement::CreatePolicy(policy) => {
            let ddl = convert_create_policy(
                policy.name,
                policy.table_name,
                policy.policy_type,
                policy.command,
                policy.to,
                policy.using,
                policy.with_check,
            )?;
            Ok(Statement::Ddl(DdlStatement::CreatePolicy(ddl)))
        }
        sp::Statement::AlterTable(at) => {
            let ddl = convert_alter_table(at.name, at.operations)?;
            Ok(Statement::Ddl(DdlStatement::AlterTable(ddl)))
        }
        // CREATE EXTENSION — silently skip (no pgsql_ast equivalent yet)
        sp::Statement::CreateExtension { .. } => Err(ParseError::Unsupported(
            "CREATE EXTENSION is not yet supported".to_string(),
        )),
        other => Err(ParseError::Unsupported(format!(
            "unsupported statement: {}",
            other
        ))),
    }
}

// ── Query / SELECT ────────────────────────────────────────────────────────────

fn convert_query(query: sp::Query) -> Result<SelectStmt, ParseError> {
    // WITH clause
    let with_ctes = if let Some(with) = query.with {
        with.cte_tables
            .into_iter()
            .map(convert_cte)
            .collect::<Result<Vec<_>, _>>()?
    } else {
        vec![]
    };

    // ORDER BY — in 0.62 the kind is OrderByKind::Expressions(Vec<OrderByExpr>) | All(...)
    let order_by = if let Some(ob) = query.order_by {
        match ob.kind {
            sp::OrderByKind::Expressions(exprs) => exprs
                .into_iter()
                .map(convert_order_by_expr)
                .collect::<Result<Vec<_>, _>>()?,
            sp::OrderByKind::All(_) => {
                return Err(ParseError::Unsupported(
                    "ORDER BY ALL is not supported".to_string(),
                ));
            }
        }
    } else {
        vec![]
    };

    // LIMIT / OFFSET — in 0.62 both live inside limit_clause
    let (limit, offset) = match query.limit_clause {
        None => (None, None),
        Some(sp::LimitClause::LimitOffset { limit, offset, .. }) => {
            let l = limit.map(convert_expr).transpose()?;
            let o = offset.map(|o| convert_expr(o.value)).transpose()?;
            (l, o)
        }
        Some(sp::LimitClause::OffsetCommaLimit { offset, limit }) => {
            (Some(convert_expr(limit)?), Some(convert_expr(offset)?))
        }
    };

    // Unwrap the SetExpr into a plain SELECT
    match *query.body {
        sp::SetExpr::Select(select) => {
            let mut stmt = convert_select(*select)?;
            stmt.with = with_ctes;
            stmt.order_by = order_by;
            stmt.limit = limit;
            stmt.offset = offset;
            Ok(stmt)
        }
        other => Err(ParseError::Unsupported(format!(
            "unsupported query body: {}",
            other
        ))),
    }
}

fn convert_cte(cte: sp::Cte) -> Result<Cte, ParseError> {
    let name = convert_ident(cte.alias.name);
    let columns = cte
        .alias
        .columns
        .into_iter()
        .map(|c| convert_ident(c.name))
        .collect();
    let query = convert_query(*cte.query)?;
    Ok(Cte {
        name,
        columns,
        query: Box::new(query),
    })
}

fn convert_select(select: sp::Select) -> Result<SelectStmt, ParseError> {
    let distinct = select.distinct.is_some();

    let columns = select
        .projection
        .into_iter()
        .map(convert_select_item)
        .collect::<Result<Vec<_>, _>>()?;

    // FROM and JOINs
    let mut from_tables = Vec::new();
    let mut joins = Vec::new();

    for table_with_joins in select.from {
        let tref = convert_table_factor(table_with_joins.relation)?;
        from_tables.push(tref);
        for join in table_with_joins.joins {
            joins.push(convert_join(join)?);
        }
    }

    let where_clause = select.selection.map(convert_expr).transpose()?;

    let group_by = match select.group_by {
        sp::GroupByExpr::All(_) => {
            return Err(ParseError::Unsupported(
                "GROUP BY ALL is not supported".to_string(),
            ));
        }
        sp::GroupByExpr::Expressions(exprs, _) => exprs
            .into_iter()
            .map(convert_expr)
            .collect::<Result<Vec<_>, _>>()?,
    };

    let having = select.having.map(convert_expr).transpose()?;

    Ok(SelectStmt {
        id: crate::next_node_id(),
        span: Span::DUMMY,
        with: vec![],
        distinct,
        columns,
        from: from_tables,
        joins,
        where_clause,
        group_by,
        having,
        order_by: vec![],
        limit: None,
        offset: None,
    })
}

fn convert_select_item(item: sp::SelectItem) -> Result<SelectItem, ParseError> {
    match item {
        sp::SelectItem::Wildcard(_) => Ok(SelectItem::Wildcard),
        // In 0.62 the first field is SelectItemQualifiedWildcardKind, not ObjectName
        sp::SelectItem::QualifiedWildcard(name_kind, _) => match name_kind {
            sp::SelectItemQualifiedWildcardKind::ObjectName(name) => {
                let last = name
                    .0
                    .into_iter()
                    .filter_map(part_to_ident)
                    .next_back()
                    .ok_or_else(|| {
                        ParseError::Unsupported("empty qualified wildcard".to_string())
                    })?;
                Ok(SelectItem::QualifiedWildcard(convert_ident(last)))
            }
            sp::SelectItemQualifiedWildcardKind::Expr(_) => Err(ParseError::Unsupported(
                "expression qualified wildcard not supported".to_string(),
            )),
        },
        sp::SelectItem::UnnamedExpr(e) => Ok(SelectItem::Expr {
            expr: convert_expr(e)?,
            alias: None,
        }),
        sp::SelectItem::ExprWithAlias { expr, alias } => Ok(SelectItem::Expr {
            expr: convert_expr(expr)?,
            alias: Some(convert_ident(alias)),
        }),
        _ => Err(ParseError::Unsupported(
            "unsupported select item variant".to_string(),
        )),
    }
}

fn convert_table_factor(factor: sp::TableFactor) -> Result<TableRef, ParseError> {
    match factor {
        sp::TableFactor::Table { name, alias, .. } => Ok(TableRef {
            name: convert_object_name(name),
            alias: alias.map(|a| convert_ident(a.name)),
        }),
        other => Err(ParseError::Unsupported(format!(
            "unsupported table factor: {}",
            other
        ))),
    }
}

fn convert_join(join: sp::Join) -> Result<Join, ParseError> {
    let table = convert_table_factor(join.relation)?;

    let (kind, constraint) = match join.join_operator {
        // Generic JOIN (no explicit INNER keyword) — sqlparser 0.62 new variant
        sp::JoinOperator::Join(c) => (JoinKind::Inner, convert_join_constraint(c)?),
        sp::JoinOperator::Inner(c) => (JoinKind::Inner, convert_join_constraint(c)?),
        // LEFT JOIN and LEFT OUTER JOIN are separate variants in 0.62
        sp::JoinOperator::Left(c) | sp::JoinOperator::LeftOuter(c) => {
            (JoinKind::Left, convert_join_constraint(c)?)
        }
        // RIGHT JOIN and RIGHT OUTER JOIN are separate variants in 0.62
        sp::JoinOperator::Right(c) | sp::JoinOperator::RightOuter(c) => {
            (JoinKind::Right, convert_join_constraint(c)?)
        }
        sp::JoinOperator::FullOuter(c) => (JoinKind::Full, convert_join_constraint(c)?),
        // CrossJoin is now a tuple variant in 0.62
        sp::JoinOperator::CrossJoin(_) => (JoinKind::Cross, JoinConstraint::Natural),
        other => {
            return Err(ParseError::Unsupported(format!(
                "unsupported join operator: {:?}",
                other
            )));
        }
    };

    Ok(Join {
        kind,
        table,
        constraint,
    })
}

fn convert_join_constraint(c: sp::JoinConstraint) -> Result<JoinConstraint, ParseError> {
    match c {
        sp::JoinConstraint::On(e) => Ok(JoinConstraint::On(convert_expr(e)?)),
        // Using still holds Vec<ObjectName> in 0.62; each ObjectName.0 is Vec<ObjectNamePart>
        sp::JoinConstraint::Using(names) => {
            let idents = names
                .into_iter()
                .map(|on| {
                    on.0.into_iter()
                        .filter_map(part_to_ident)
                        .next_back()
                        .map(convert_ident)
                        .ok_or_else(|| ParseError::Unsupported("empty USING ident".to_string()))
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(JoinConstraint::Using(idents))
        }
        sp::JoinConstraint::Natural => Ok(JoinConstraint::Natural),
        sp::JoinConstraint::None => Ok(JoinConstraint::Natural),
    }
}

fn convert_order_by_expr(o: sp::OrderByExpr) -> Result<OrderByExpr, ParseError> {
    // In 0.62, asc/nulls_first live inside OrderByOptions
    Ok(OrderByExpr {
        expr: convert_expr(o.expr)?,
        asc: o.options.asc,
        nulls_first: o.options.nulls_first,
    })
}

// ── Expression conversion ─────────────────────────────────────────────────────

/// Convertit une expression sqlparser en [`Expr`], span compris.
///
/// Point de passage unique : le corps produit un [`ExprKind`], et c'est ici
/// que le span est attaché. Toute expression issue du parse passe par cette
/// fonction, donc il n'y a qu'un endroit à corriger pour la position.
fn convert_expr(expr: sp::Expr) -> Result<Expr, ParseError> {
    let span = sp_expr_span(&expr);
    let kind = convert_expr_kind(expr)?;
    Ok(Expr::new(kind, span))
}

fn convert_expr_kind(expr: sp::Expr) -> Result<ExprKind, ParseError> {
    match expr {
        sp::Expr::Value(v) => convert_value(v.value),
        sp::Expr::Identifier(id) => {
            // Single ident — could be a column name
            Ok(ExprKind::Column(ColumnRef {
                table: None,
                column: convert_ident(id),
            }))
        }
        sp::Expr::CompoundIdentifier(parts) => match parts.len() {
            0 => Err(ParseError::Unsupported(
                "empty compound identifier".to_string(),
            )),
            1 => Ok(ExprKind::Column(ColumnRef {
                table: None,
                column: convert_ident(parts.into_iter().next().unwrap()),
            })),
            _ => {
                let mut iter = parts.into_iter().rev();
                let col = convert_ident(iter.next().unwrap());
                let tbl = convert_ident(iter.next().unwrap());
                Ok(ExprKind::Column(ColumnRef {
                    table: Some(tbl),
                    column: col,
                }))
            }
        },
        sp::Expr::IsNull(e) => Ok(ExprKind::UnaryOp {
            op: UnaryOp::IsNull,
            expr: Box::new(convert_expr(*e)?),
        }),
        sp::Expr::IsNotNull(e) => Ok(ExprKind::UnaryOp {
            op: UnaryOp::IsNotNull,
            expr: Box::new(convert_expr(*e)?),
        }),
        sp::Expr::BinaryOp { left, op, right } => {
            let l = convert_expr(*left)?;
            let r = convert_expr(*right)?;
            let bop = convert_binary_op(op)?;
            Ok(ExprKind::BinaryOp {
                left: Box::new(l),
                op: bop,
                right: Box::new(r),
            })
        }
        sp::Expr::UnaryOp { op, expr } => {
            let e = convert_expr(*expr)?;
            let uop = match op {
                sp::UnaryOperator::Not => UnaryOp::Not,
                sp::UnaryOperator::Minus => UnaryOp::Neg,
                _ => {
                    return Err(ParseError::Unsupported(format!(
                        "unsupported unary op: {:?}",
                        op
                    )));
                }
            };
            Ok(ExprKind::UnaryOp {
                op: uop,
                expr: Box::new(e),
            })
        }
        sp::Expr::Cast {
            expr, data_type, ..
        } => Ok(ExprKind::Cast {
            expr: Box::new(convert_expr(*expr)?),
            data_type: convert_data_type(data_type)?,
        }),
        // In 0.62, Case uses Vec<CaseWhen> (each has .condition and .result);
        // results is gone, and case_token/end_token are new (ignored with ..)
        sp::Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            let op = operand.map(|e| convert_expr(*e)).transpose()?.map(Box::new);
            let when_clauses = conditions
                .into_iter()
                .map(|cw| Ok((convert_expr(cw.condition)?, convert_expr(cw.result)?)))
                .collect::<Result<Vec<_>, ParseError>>()?;
            let else_clause = else_result
                .map(|e| convert_expr(*e))
                .transpose()?
                .map(Box::new);
            Ok(ExprKind::Case {
                operand: op,
                when_clauses,
                else_clause,
            })
        }
        sp::Expr::InList {
            expr,
            list,
            negated,
        } => {
            let e = convert_expr(*expr)?;
            let items = list
                .into_iter()
                .map(convert_expr)
                .collect::<Result<Vec<_>, _>>()?;
            Ok(ExprKind::InList {
                expr: Box::new(e),
                list: items,
                negated,
            })
        }
        sp::Expr::InSubquery {
            expr,
            subquery,
            negated,
        } => {
            let e = convert_expr(*expr)?;
            let sub = convert_query(*subquery)?;
            Ok(ExprKind::InSubquery {
                expr: Box::new(e),
                subquery: Box::new(sub),
                negated,
            })
        }
        sp::Expr::Between {
            expr,
            negated,
            low,
            high,
        } => Ok(ExprKind::Between {
            expr: Box::new(convert_expr(*expr)?),
            low: Box::new(convert_expr(*low)?),
            high: Box::new(convert_expr(*high)?),
            negated,
        }),
        sp::Expr::Subquery(q) => {
            let sub = convert_query(*q)?;
            Ok(ExprKind::Subquery(Box::new(sub)))
        }
        sp::Expr::Function(f) => {
            let name = convert_object_name(f.name);
            let args = convert_function_args(f.args)?;
            Ok(ExprKind::FunctionCall {
                name,
                args,
                filter: None,
                distinct: false,
            })
        }
        sp::Expr::Wildcard(_) => Ok(ExprKind::Wildcard),
        // In 0.62, ObjectName.0 is Vec<ObjectNamePart>
        sp::Expr::QualifiedWildcard(parts, _) => {
            let last = parts
                .0
                .into_iter()
                .filter_map(part_to_ident)
                .next_back()
                .ok_or_else(|| ParseError::Unsupported("empty qualified wildcard".to_string()))?;
            Ok(ExprKind::QualifiedWildcard(convert_ident(last)))
        }
        sp::Expr::Like {
            negated,
            expr,
            pattern,
            ..
        } => {
            let op = if negated {
                BinaryOp::NotLike
            } else {
                BinaryOp::Like
            };
            Ok(ExprKind::BinaryOp {
                left: Box::new(convert_expr(*expr)?),
                op,
                right: Box::new(convert_expr(*pattern)?),
            })
        }
        sp::Expr::ILike {
            negated,
            expr,
            pattern,
            ..
        } => {
            if negated {
                return Err(ParseError::Unsupported(
                    "NOT ILIKE not supported".to_string(),
                ));
            }
            Ok(ExprKind::BinaryOp {
                left: Box::new(convert_expr(*expr)?),
                op: BinaryOp::ILike,
                right: Box::new(convert_expr(*pattern)?),
            })
        }
        sp::Expr::IsTrue(e) => Ok(ExprKind::BinaryOp {
            left: Box::new(convert_expr(*e)?),
            op: BinaryOp::Is,
            // Littéral synthétisé par le désucrage de IS [NOT] TRUE :
            // absent de la source, donc sans span.
            right: Box::new(Expr::bare(ExprKind::Literal(Literal::Boolean(true)))),
        }),
        sp::Expr::IsFalse(e) => Ok(ExprKind::BinaryOp {
            left: Box::new(convert_expr(*e)?),
            op: BinaryOp::Is,
            // Littéral synthétisé par le désucrage de IS [NOT] FALSE :
            // absent de la source, donc sans span.
            right: Box::new(Expr::bare(ExprKind::Literal(Literal::Boolean(false)))),
        }),
        sp::Expr::IsNotTrue(e) => Ok(ExprKind::BinaryOp {
            left: Box::new(convert_expr(*e)?),
            op: BinaryOp::IsNot,
            // Littéral synthétisé par le désucrage de IS [NOT] TRUE :
            // absent de la source, donc sans span.
            right: Box::new(Expr::bare(ExprKind::Literal(Literal::Boolean(true)))),
        }),
        sp::Expr::IsNotFalse(e) => Ok(ExprKind::BinaryOp {
            left: Box::new(convert_expr(*e)?),
            op: BinaryOp::IsNot,
            // Littéral synthétisé par le désucrage de IS [NOT] FALSE :
            // absent de la source, donc sans span.
            right: Box::new(Expr::bare(ExprKind::Literal(Literal::Boolean(false)))),
        }),
        sp::Expr::IsDistinctFrom(l, r) => Ok(ExprKind::BinaryOp {
            left: Box::new(convert_expr(*l)?),
            op: BinaryOp::IsNot,
            right: Box::new(convert_expr(*r)?),
        }),
        sp::Expr::IsNotDistinctFrom(l, r) => Ok(ExprKind::BinaryOp {
            left: Box::new(convert_expr(*l)?),
            op: BinaryOp::Is,
            right: Box::new(convert_expr(*r)?),
        }),
        // `(expr)` : on garde l'expression interne, dont le span est déjà juste.
        sp::Expr::Nested(e) => Ok(convert_expr(*e)?.kind),

        // `expr = ANY (array_expr)` — common in pg_dump CHECK constraints.
        // Map to a boolean literal so the containing statement doesn't fail.
        sp::Expr::AnyOp {
            left,
            compare_op,
            right,
            ..
        } => {
            // Best effort: convert operands, return BinaryOp if possible.
            match (convert_expr(*left), convert_expr(*right)) {
                (Ok(l), Ok(r)) => {
                    let op = convert_binary_op(compare_op).unwrap_or(BinaryOp::Eq);
                    Ok(ExprKind::BinaryOp {
                        left: Box::new(l),
                        op,
                        right: Box::new(r),
                    })
                }
                _ => Ok(ExprKind::Literal(Literal::Boolean(true))),
            }
        }

        // `ALL` operation — same treatment as AnyOp.
        sp::Expr::AllOp {
            left,
            compare_op,
            right,
        } => match (convert_expr(*left), convert_expr(*right)) {
            (Ok(l), Ok(r)) => {
                let op = convert_binary_op(compare_op).unwrap_or(BinaryOp::Eq);
                Ok(ExprKind::BinaryOp {
                    left: Box::new(l),
                    op,
                    right: Box::new(r),
                })
            }
            _ => Ok(ExprKind::Literal(Literal::Boolean(true))),
        },

        // `ARRAY[e1, e2, ...]` — array constructor, common in pg_dump.
        // Convert elements individually; treat failures as a null literal.
        sp::Expr::Array(arr) => {
            let args: Vec<Expr> = arr
                .elem
                .into_iter()
                .filter_map(|e| convert_expr(e).ok())
                .collect();
            if args.is_empty() {
                Ok(ExprKind::Literal(Literal::Null))
            } else {
                Ok(ExprKind::FunctionCall {
                    name: pgsql_ast::ObjectName(vec![pgsql_ast::Ident {
                        value: "array".into(),
                        quoted: false,
                    }]),
                    args,
                    filter: None,
                    distinct: false,
                })
            }
        }

        // `DATE '2025-01-01'` / `'value'::type` typed string literals.
        sp::Expr::TypedString(ts) => match ts.value.into_string() {
            Some(v) => Ok(ExprKind::Literal(Literal::String(v))),
            None => Ok(ExprKind::Literal(Literal::Null)),
        },

        // Any other unsupported expression — return a null literal rather than
        // failing the whole statement. CHECK constraints, DEFAULT values, and
        // other DDL-context expressions don't need to be fully type-checked.
        _other => Ok(ExprKind::Literal(Literal::Null)),
    }
}

fn convert_value(v: sp::Value) -> Result<ExprKind, ParseError> {
    match v {
        sp::Value::Number(n, _) => Ok(ExprKind::Literal(Literal::Number(n))),
        sp::Value::SingleQuotedString(s) => Ok(ExprKind::Literal(Literal::String(s))),
        sp::Value::DollarQuotedString(dqs) => Ok(ExprKind::Literal(Literal::String(dqs.value))),
        sp::Value::Boolean(b) => Ok(ExprKind::Literal(Literal::Boolean(b))),
        sp::Value::Null => Ok(ExprKind::Literal(Literal::Null)),
        sp::Value::DoubleQuotedString(s) => Ok(ExprKind::Literal(Literal::String(s))),
        sp::Value::EscapedStringLiteral(s) => Ok(ExprKind::Literal(Literal::String(s))),
        sp::Value::Placeholder(p) => Err(ParseError::Unsupported(format!(
            "parameter placeholder {} is not supported",
            p
        ))),
        other => Err(ParseError::Unsupported(format!(
            "unsupported literal value: {:?}",
            other
        ))),
    }
}

fn convert_binary_op(op: sp::BinaryOperator) -> Result<BinaryOp, ParseError> {
    match op {
        sp::BinaryOperator::Eq => Ok(BinaryOp::Eq),
        sp::BinaryOperator::NotEq => Ok(BinaryOp::NotEq),
        sp::BinaryOperator::Lt => Ok(BinaryOp::Lt),
        sp::BinaryOperator::LtEq => Ok(BinaryOp::Lte),
        sp::BinaryOperator::Gt => Ok(BinaryOp::Gt),
        sp::BinaryOperator::GtEq => Ok(BinaryOp::Gte),
        sp::BinaryOperator::And => Ok(BinaryOp::And),
        sp::BinaryOperator::Or => Ok(BinaryOp::Or),
        sp::BinaryOperator::Plus => Ok(BinaryOp::Plus),
        sp::BinaryOperator::Minus => Ok(BinaryOp::Minus),
        sp::BinaryOperator::Multiply => Ok(BinaryOp::Multiply),
        sp::BinaryOperator::Divide => Ok(BinaryOp::Divide),
        sp::BinaryOperator::Modulo => Ok(BinaryOp::Modulo),
        // PostgreSQL extension operators — map to Custom instead of erroring out.
        // PostGIS spatial operators
        sp::BinaryOperator::LtDashGt => Ok(BinaryOp::Custom("<->".into())),
        sp::BinaryOperator::PGOverlap => Ok(BinaryOp::Custom("&&".into())),
        sp::BinaryOperator::At => Ok(BinaryOp::Custom("@".into())),
        sp::BinaryOperator::TildeEq => Ok(BinaryOp::Custom("~=".into())),
        sp::BinaryOperator::AndLt => Ok(BinaryOp::Custom("&<".into())),
        sp::BinaryOperator::AndGt => Ok(BinaryOp::Custom("&>".into())),
        sp::BinaryOperator::LtLtPipe => Ok(BinaryOp::Custom("<<|".into())),
        sp::BinaryOperator::PipeGtGt => Ok(BinaryOp::Custom("|>>".into())),
        sp::BinaryOperator::AndLtPipe => Ok(BinaryOp::Custom("&<|".into())),
        sp::BinaryOperator::PipeAndGt => Ok(BinaryOp::Custom("|&>".into())),
        sp::BinaryOperator::LtCaret => Ok(BinaryOp::Custom("<^".into())),
        sp::BinaryOperator::GtCaret => Ok(BinaryOp::Custom(">^".into())),
        sp::BinaryOperator::QuestionHash => Ok(BinaryOp::Custom("?#".into())),
        sp::BinaryOperator::QuestionDash => Ok(BinaryOp::Custom("?-".into())),
        sp::BinaryOperator::QuestionDashPipe => Ok(BinaryOp::Custom("?-|".into())),
        sp::BinaryOperator::QuestionDoublePipe => Ok(BinaryOp::Custom("?||".into())),
        // PostgreSQL JSONB operators
        sp::BinaryOperator::Arrow => Ok(BinaryOp::Custom("->".into())),
        sp::BinaryOperator::LongArrow => Ok(BinaryOp::Custom("->>".into())),
        sp::BinaryOperator::HashArrow => Ok(BinaryOp::Custom("#>".into())),
        sp::BinaryOperator::HashLongArrow => Ok(BinaryOp::Custom("#>>".into())),
        sp::BinaryOperator::AtArrow => Ok(BinaryOp::Custom("@>".into())),
        sp::BinaryOperator::ArrowAt => Ok(BinaryOp::Custom("<@".into())),
        sp::BinaryOperator::HashMinus => Ok(BinaryOp::Custom("#-".into())),
        sp::BinaryOperator::AtAt => Ok(BinaryOp::Custom("@@".into())),
        // Other PostgreSQL operators
        sp::BinaryOperator::PGRegexMatch => Ok(BinaryOp::Custom("~".into())),
        sp::BinaryOperator::DoubleHash => Ok(BinaryOp::Custom("##".into())),
        // Generic custom / user-defined operators
        sp::BinaryOperator::Custom(s) => Ok(BinaryOp::Custom(s)),
        sp::BinaryOperator::PGCustomBinaryOperator(parts) => Ok(BinaryOp::Custom(parts.join("."))),
        other => Err(ParseError::Unsupported(format!(
            "unsupported binary operator: {:?}",
            other
        ))),
    }
}

fn convert_function_args(args: sp::FunctionArguments) -> Result<Vec<Expr>, ParseError> {
    match args {
        sp::FunctionArguments::None => Ok(vec![]),
        sp::FunctionArguments::Subquery(_) => Err(ParseError::Unsupported(
            "subquery function arguments not supported".to_string(),
        )),
        sp::FunctionArguments::List(list) => list
            .args
            .into_iter()
            .map(|fa| match fa {
                sp::FunctionArg::Unnamed(sp::FunctionArgExpr::Expr(e)) => convert_expr(e),
                sp::FunctionArg::Unnamed(sp::FunctionArgExpr::Wildcard)
                | sp::FunctionArg::Unnamed(sp::FunctionArgExpr::WildcardWithOptions(_)) => {
                    Ok(Expr::bare(ExprKind::Wildcard))
                }
                sp::FunctionArg::Unnamed(sp::FunctionArgExpr::QualifiedWildcard(n)) => {
                    let last =
                        n.0.into_iter()
                            .filter_map(part_to_ident)
                            .next_back()
                            .ok_or_else(|| {
                                ParseError::Unsupported("empty qualified wildcard".to_string())
                            })?;
                    Ok(Expr::bare(ExprKind::QualifiedWildcard(convert_ident(last))))
                }
                sp::FunctionArg::Named { arg, .. } => match arg {
                    sp::FunctionArgExpr::Expr(e) => convert_expr(e),
                    _ => Err(ParseError::Unsupported(
                        "named wildcard function arg not supported".to_string(),
                    )),
                },
                sp::FunctionArg::ExprNamed { arg, .. } => match arg {
                    sp::FunctionArgExpr::Expr(e) => convert_expr(e),
                    _ => Err(ParseError::Unsupported(
                        "unsupported ExprNamed function arg".to_string(),
                    )),
                },
            })
            .collect(),
    }
}

// ── INSERT ────────────────────────────────────────────────────────────────────

fn convert_insert(ins: sp::Insert) -> Result<InsertStmt, ParseError> {
    let table = match ins.table {
        sp::TableObject::TableName(n) => convert_object_name(n),
        sp::TableObject::TableFunction(_) | sp::TableObject::TableQuery(_) => {
            return Err(ParseError::Unsupported(
                "INSERT INTO TABLE FUNCTION/QUERY not supported".to_string(),
            ));
        }
    };

    // In 0.62, Insert.columns is Vec<ObjectName>, not Vec<Ident>
    let columns = ins
        .columns
        .into_iter()
        .map(|on| {
            on.0.into_iter()
                .filter_map(part_to_ident)
                .next_back()
                .map(convert_ident)
                .unwrap_or_else(|| Ident::new(""))
        })
        .collect();

    let source = if let Some(q) = ins.source {
        match *q.body {
            sp::SetExpr::Values(v) => {
                // In 0.62, rows is Vec<Parens<Vec<Expr>>>; access .content
                let rows = v
                    .rows
                    .into_iter()
                    .map(|row| {
                        row.content
                            .into_iter()
                            .map(convert_expr)
                            .collect::<Result<Vec<_>, _>>()
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                InsertSource::Values(rows)
            }
            _ => {
                let sel = convert_query(*q)?;
                InsertSource::Select(Box::new(sel))
            }
        }
    } else {
        InsertSource::Values(vec![])
    };

    let on_conflict = ins
        .on
        .map(|on_insert| match on_insert {
            sp::OnInsert::OnConflict(oc) => convert_on_conflict(oc),
            sp::OnInsert::DuplicateKeyUpdate(_) => Err(ParseError::Unsupported(
                "ON DUPLICATE KEY UPDATE not supported".to_string(),
            )),
            _ => Err(ParseError::Unsupported(
                "unsupported ON INSERT clause".to_string(),
            )),
        })
        .transpose()?;

    let returning = ins
        .returning
        .unwrap_or_default()
        .into_iter()
        .map(convert_select_item)
        .collect::<Result<Vec<_>, _>>()?;

    Ok(InsertStmt {
        id: crate::next_node_id(),
        span: Span::DUMMY,
        table,
        columns,
        source,
        on_conflict,
        returning,
    })
}

fn convert_on_conflict(oc: sp::OnConflict) -> Result<OnConflict, ParseError> {
    let target = match oc.conflict_target {
        None => OnConflictTarget::None,
        Some(sp::ConflictTarget::Columns(cols)) => {
            OnConflictTarget::Column(cols.into_iter().map(convert_ident).collect())
        }
        Some(sp::ConflictTarget::OnConstraint(name)) => {
            let ident = name
                .0
                .into_iter()
                .filter_map(part_to_ident)
                .next_back()
                .map(convert_ident)
                .ok_or_else(|| ParseError::Unsupported("empty constraint name".to_string()))?;
            OnConflictTarget::Constraint(ident)
        }
    };

    let action = match oc.action {
        sp::OnConflictAction::DoNothing => OnConflictAction::DoNothing,
        sp::OnConflictAction::DoUpdate(du) => {
            let assignments = du
                .assignments
                .into_iter()
                .map(convert_assignment)
                .collect::<Result<Vec<_>, _>>()?;
            let where_clause = du.selection.map(convert_expr).transpose()?;
            OnConflictAction::Update {
                assignments,
                where_clause,
            }
        }
    };

    Ok(OnConflict { target, action })
}

fn convert_assignment(a: sp::Assignment) -> Result<Assignment, ParseError> {
    let column = match a.target {
        sp::AssignmentTarget::ColumnName(name) => name
            .0
            .into_iter()
            .filter_map(part_to_ident)
            .next_back()
            .map(convert_ident)
            .ok_or_else(|| ParseError::Unsupported("empty assignment target".to_string()))?,
        sp::AssignmentTarget::Tuple(_) => {
            return Err(ParseError::Unsupported(
                "tuple assignment not supported".to_string(),
            ));
        }
    };
    Ok(Assignment {
        column,
        value: convert_expr(a.value)?,
    })
}

// ── UPDATE ────────────────────────────────────────────────────────────────────

fn convert_update(
    table: sp::TableWithJoins,
    assignments: Vec<sp::Assignment>,
    from: Option<sp::UpdateTableFromKind>,
    selection: Option<sp::Expr>,
    returning: Option<Vec<sp::SelectItem>>,
) -> Result<UpdateStmt, ParseError> {
    let tref = convert_table_factor(table.relation)?;

    // In 0.62, BeforeSet/AfterSet wrap Vec<TableWithJoins>, not a single one
    let from_tables = match from {
        None => vec![],
        Some(sp::UpdateTableFromKind::BeforeSet(tables))
        | Some(sp::UpdateTableFromKind::AfterSet(tables)) => tables
            .into_iter()
            .map(|twj| convert_table_factor(twj.relation))
            .collect::<Result<Vec<_>, _>>()?,
    };

    let assigns = assignments
        .into_iter()
        .map(convert_assignment)
        .collect::<Result<Vec<_>, _>>()?;

    let where_clause = selection.map(convert_expr).transpose()?;
    let ret = returning
        .unwrap_or_default()
        .into_iter()
        .map(convert_select_item)
        .collect::<Result<Vec<_>, _>>()?;

    Ok(UpdateStmt {
        id: crate::next_node_id(),
        span: Span::DUMMY,
        table: tref,
        assignments: assigns,
        from: from_tables,
        where_clause,
        returning: ret,
    })
}

// ── DELETE ────────────────────────────────────────────────────────────────────

fn convert_delete(del: sp::Delete) -> Result<DeleteStmt, ParseError> {
    let table = match del.from {
        sp::FromTable::WithFromKeyword(mut tables) | sp::FromTable::WithoutKeyword(mut tables) => {
            if tables.is_empty() {
                return Err(ParseError::Unsupported(
                    "DELETE with no FROM table".to_string(),
                ));
            }
            convert_table_factor(tables.remove(0).relation)?
        }
    };

    let using = del
        .using
        .unwrap_or_default()
        .into_iter()
        .flat_map(|twj| {
            std::iter::once(twj.relation).chain(twj.joins.into_iter().map(|j| j.relation))
        })
        .map(convert_table_factor)
        .collect::<Result<Vec<_>, _>>()?;

    let where_clause = del.selection.map(convert_expr).transpose()?;
    let returning = del
        .returning
        .unwrap_or_default()
        .into_iter()
        .map(convert_select_item)
        .collect::<Result<Vec<_>, _>>()?;

    Ok(DeleteStmt {
        id: crate::next_node_id(),
        span: Span::DUMMY,
        table,
        using,
        where_clause,
        returning,
    })
}

// ── CREATE TABLE ──────────────────────────────────────────────────────────────

fn convert_create_table(ct: sp::CreateTable) -> Result<CreateTable, ParseError> {
    let name = convert_object_name(ct.name);
    let if_not_exists = ct.if_not_exists;
    let columns = ct
        .columns
        .into_iter()
        .map(convert_column_def)
        .collect::<Result<Vec<_>, _>>()?;
    let constraints = ct
        .constraints
        .into_iter()
        .map(convert_table_constraint)
        .collect::<Result<Vec<_>, _>>()?;

    Ok(CreateTable {
        id: crate::next_node_id(),
        span: Span::DUMMY,
        name,
        if_not_exists,
        temporary: ct.temporary,
        columns,
        constraints,
    })
}

fn convert_column_def(col: sp::ColumnDef) -> Result<ColumnDef, ParseError> {
    let name = convert_ident(col.name);
    let data_type = convert_data_type(col.data_type)?;

    let mut nullable = true;
    let mut default: Option<Expr> = None;
    let mut constraints = Vec::new();

    for opt_def in col.options {
        match opt_def.option {
            sp::ColumnOption::NotNull => {
                nullable = false;
                constraints.push(ColumnConstraint::NotNull);
            }
            sp::ColumnOption::Null => {
                nullable = true;
            }
            sp::ColumnOption::Default(e) => {
                let expr = convert_expr(e)?;
                default = Some(expr.clone());
                constraints.push(ColumnConstraint::Default(expr));
            }
            // In 0.62, PrimaryKey and Unique are separate tuple variants
            sp::ColumnOption::PrimaryKey(_) => {
                constraints.push(ColumnConstraint::PrimaryKey);
            }
            sp::ColumnOption::Unique(_) => {
                constraints.push(ColumnConstraint::Unique);
            }
            // In 0.62, ForeignKey is a tuple variant holding ForeignKeyConstraint
            sp::ColumnOption::ForeignKey(fk) => {
                constraints.push(ColumnConstraint::References {
                    table: convert_object_name(fk.foreign_table),
                    column: fk.referred_columns.into_iter().next().map(convert_ident),
                    on_delete: fk.on_delete.map(convert_referential_action),
                    on_update: fk.on_update.map(convert_referential_action),
                });
            }
            // In 0.62, Check is a tuple variant holding CheckConstraint
            sp::ColumnOption::Check(cc) => {
                constraints.push(ColumnConstraint::Check(convert_expr(*cc.expr)?));
            }
            // Ignore other options (charset, comments, dialect-specific)
            _ => {}
        }
    }

    Ok(ColumnDef {
        name,
        data_type,
        nullable,
        default,
        constraints,
    })
}

fn convert_table_constraint(tc: sp::TableConstraint) -> Result<TableConstraint, ParseError> {
    match tc {
        // In 0.62, PrimaryKey/Unique/ForeignKey/Check are tuple variants
        sp::TableConstraint::PrimaryKey(pk) => Ok(TableConstraint::PrimaryKey {
            columns: pk
                .columns
                .into_iter()
                .map(convert_index_column)
                .collect::<Result<Vec<_>, _>>()?,
        }),
        sp::TableConstraint::Unique(uc) => Ok(TableConstraint::Unique {
            columns: uc
                .columns
                .into_iter()
                .map(convert_index_column)
                .collect::<Result<Vec<_>, _>>()?,
        }),
        sp::TableConstraint::ForeignKey(fk) => Ok(TableConstraint::ForeignKey {
            columns: fk.columns.into_iter().map(convert_ident).collect(),
            ref_table: convert_object_name(fk.foreign_table),
            ref_columns: fk.referred_columns.into_iter().map(convert_ident).collect(),
            on_delete: fk.on_delete.map(convert_referential_action),
            on_update: fk.on_update.map(convert_referential_action),
        }),
        sp::TableConstraint::Check(cc) => Ok(TableConstraint::Check(convert_expr(*cc.expr)?)),
        other => Err(ParseError::Unsupported(format!(
            "unsupported table constraint: {}",
            other
        ))),
    }
}

fn convert_referential_action(action: sp::ReferentialAction) -> ReferentialAction {
    match action {
        sp::ReferentialAction::Cascade => ReferentialAction::Cascade,
        sp::ReferentialAction::SetNull => ReferentialAction::SetNull,
        sp::ReferentialAction::SetDefault => ReferentialAction::SetDefault,
        sp::ReferentialAction::Restrict => ReferentialAction::Restrict,
        sp::ReferentialAction::NoAction => ReferentialAction::NoAction,
    }
}

// ── CREATE INDEX ──────────────────────────────────────────────────────────────

fn convert_create_index(ci: sp::CreateIndex) -> Result<CreateIndex, ParseError> {
    let name = ci.name.map(|n| {
        n.0.into_iter()
            .filter_map(part_to_ident)
            .next_back()
            .map(convert_ident)
            .unwrap_or_else(|| Ident::new(""))
    });
    let table = convert_object_name(ci.table_name);
    let columns = ci
        .columns
        .into_iter()
        .map(|ob| convert_ident_from_expr(ob.column.expr))
        .collect::<Result<Vec<_>, _>>()?;

    Ok(CreateIndex {
        name,
        table,
        columns,
        unique: ci.unique,
        if_not_exists: ci.if_not_exists,
    })
}

fn convert_ident_from_expr(expr: sp::Expr) -> Result<Ident, ParseError> {
    match expr {
        sp::Expr::Identifier(id) => Ok(convert_ident(id)),
        sp::Expr::CompoundIdentifier(parts) => parts
            .into_iter()
            .next_back()
            .map(convert_ident)
            .ok_or_else(|| ParseError::Unsupported("empty compound ident in index".to_string())),
        other => Err(ParseError::Unsupported(format!(
            "unsupported index column expression: {}",
            other
        ))),
    }
}

// ── CREATE FUNCTION ───────────────────────────────────────────────────────────

fn convert_create_function(cf: sp::CreateFunction) -> Result<CreateFunction, ParseError> {
    let name = convert_object_name(cf.name);
    let or_replace = cf.or_replace;

    let params = cf
        .args
        .unwrap_or_default()
        .into_iter()
        .map(convert_function_arg)
        .collect::<Result<Vec<_>, _>>()?;

    // Determine language
    let language = match cf
        .language
        .as_ref()
        .map(|l| l.value.to_lowercase())
        .as_deref()
    {
        Some("sql") => FunctionLanguage::Sql,
        Some("plpgsql") => FunctionLanguage::PlPgSql,
        Some("c") => FunctionLanguage::C,
        Some("internal") => FunctionLanguage::Internal,
        Some(other) => FunctionLanguage::Other(other.to_string()),
        None => FunctionLanguage::Other("unknown".to_string()),
    };

    // Return type — in 0.62, return_type is Option<FunctionReturnType>
    let return_type = match cf.return_type {
        None => ReturnType::Void,
        Some(sp::FunctionReturnType::DataType(dt)) => convert_return_type_from_dt(dt)?,
        Some(sp::FunctionReturnType::SetOf(dt)) => ReturnType::SetOf(convert_data_type(dt)?),
    };

    // Volatility
    let volatility = match cf.behavior {
        Some(sp::FunctionBehavior::Immutable) => Volatility::Immutable,
        Some(sp::FunctionBehavior::Stable) => Volatility::Stable,
        _ => Volatility::Volatile,
    };

    let security_definer = false;

    // Body
    let body = convert_function_body(cf.function_body, &language)?;

    Ok(CreateFunction {
        id: crate::next_node_id(),
        span: Span::DUMMY,
        body_start: 0,
        name,
        or_replace,
        params,
        return_type,
        language,
        body,
        security_definer,
        volatility,
        cost: None,
        rows: None,
    })
}

fn convert_function_arg(arg: sp::OperateFunctionArg) -> Result<FunctionParam, ParseError> {
    let mode = match arg.mode {
        None | Some(sp::ArgMode::In) => ParamMode::In,
        Some(sp::ArgMode::Out) => ParamMode::Out,
        Some(sp::ArgMode::InOut) => ParamMode::InOut,
        Some(sp::ArgMode::Variadic) => ParamMode::Variadic,
    };
    let data_type = convert_data_type(arg.data_type)?;
    let default = arg.default_expr.map(convert_expr).transpose()?;
    Ok(FunctionParam {
        name: arg.name.map(convert_ident),
        data_type,
        mode,
        default,
    })
}

/// Convert a sqlparser DataType to pgsql_ast ReturnType.
fn convert_return_type_from_dt(dt: sp::DataType) -> Result<ReturnType, ParseError> {
    match &dt {
        sp::DataType::Trigger => Ok(ReturnType::Trigger),
        sp::DataType::Custom(name, _) => {
            let s = name
                .0
                .iter()
                .filter_map(|p| match p {
                    sp::ObjectNamePart::Identifier(id) => Some(id.value.as_str()),
                    sp::ObjectNamePart::Function(_) => None,
                })
                .collect::<Vec<_>>()
                .join(" ");
            Ok(ReturnType::Scalar(DataType::Custom(s)))
        }
        other => Ok(ReturnType::Scalar(convert_data_type(other.clone())?)),
    }
}

pub(crate) fn parse_simple_type_pub(s: &str) -> DataType {
    parse_simple_type(s)
}

fn parse_simple_type(s: &str) -> DataType {
    match s.to_uppercase().as_str() {
        "UUID" => DataType::Uuid,
        "TEXT" => DataType::Text,
        "INTEGER" | "INT" | "INT4" => DataType::Integer,
        "BIGINT" | "INT8" => DataType::BigInt,
        "SMALLINT" | "INT2" => DataType::SmallInt,
        "NUMERIC" | "DECIMAL" | "DEC" => DataType::Numeric {
            precision: None,
            scale: None,
        },
        "REAL" | "FLOAT4" | "FLOAT" => DataType::Real,
        "DOUBLE PRECISION" | "FLOAT8" => DataType::DoublePrecision,
        "BOOLEAN" | "BOOL" => DataType::Boolean,
        "TIMESTAMPTZ" | "TIMESTAMP WITH TIME ZONE" => DataType::TimestampTz,
        "TIMESTAMP" => DataType::Timestamp,
        "DATE" => DataType::Date,
        "TIME" => DataType::Time,
        "INTERVAL" => DataType::Interval,
        "JSON" => DataType::Json,
        "JSONB" => DataType::Jsonb,
        "BYTEA" => DataType::Bytea,
        "VARCHAR" | "CHARACTER VARYING" => DataType::Varchar(None),
        "CHAR" | "CHARACTER" => DataType::Char(None),
        _ => DataType::Custom(s.to_string()),
    }
}

fn convert_function_body(
    body: Option<sp::CreateFunctionBody>,
    language: &FunctionLanguage,
) -> Result<FunctionBody, ParseError> {
    let raw = match body {
        None => return Err(ParseError::Unsupported("function with no body".to_string())),
        // AsBeforeOptions is a struct variant in 0.62; other variants are tuple or struct
        Some(sp::CreateFunctionBody::AsBeforeOptions { body: e, .. })
        | Some(sp::CreateFunctionBody::AsAfterOptions(e))
        | Some(sp::CreateFunctionBody::Return(e))
        | Some(sp::CreateFunctionBody::AsReturnExpr(e)) => extract_body_string(e),
        Some(sp::CreateFunctionBody::AsBeginEnd(bes)) => bes
            .statements
            .into_iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .join("; "),
        Some(sp::CreateFunctionBody::AsReturnSelect(select)) => {
            format!("SELECT {select}")
        }
    };

    // PL/pgSQL bodies are parsed later in lib.rs::parse() after the file-level
    // byte offset is known, so that inner SQL fragment spans point to the correct
    // source line. For now, leave them as Raw; lib.rs upgrades them to Parsed.

    // For LANGUAGE sql, try to parse as a SelectStmt
    if matches!(language, FunctionLanguage::Sql) {
        let dialect = sqlparser::dialect::PostgreSqlDialect {};
        if let Ok(stmts) = sqlparser::parser::Parser::parse_sql(&dialect, raw.trim())
            && let Some(sp::Statement::Query(q)) = stmts.into_iter().next()
            && let Ok(sel) = convert_query(*q)
        {
            return Ok(FunctionBody::SqlQuery(Box::new(sel)));
        }
    }

    Ok(FunctionBody::Raw(raw))
}

fn extract_body_string(expr: sp::Expr) -> String {
    match expr {
        sp::Expr::Value(v) => match v.value {
            sp::Value::DollarQuotedString(dqs) => dqs.value,
            sp::Value::SingleQuotedString(s) => s,
            other => format!("{other}"),
        },
        other => other.to_string(),
    }
}

// ── CREATE PROCEDURE ──────────────────────────────────────────────────────────

fn convert_create_procedure(
    _or_alter: bool,
    name: sp::ObjectName,
    params: Option<Vec<sp::ProcedureParam>>,
    body: sp::ConditionalStatements,
) -> Result<CreateProcedure, ParseError> {
    let pname = convert_object_name(name);
    let or_replace = _or_alter;

    let params_out = params
        .unwrap_or_default()
        .into_iter()
        .map(|p| {
            Ok(FunctionParam {
                name: Some(convert_ident(p.name)),
                data_type: convert_data_type(p.data_type)?,
                mode: ParamMode::In,
                default: None,
            })
        })
        .collect::<Result<Vec<_>, ParseError>>()?;

    // In 0.62, body is ConditionalStatements; extract the statement list
    let stmts: Vec<sp::Statement> = match body {
        sp::ConditionalStatements::Sequence { statements } => statements,
        sp::ConditionalStatements::BeginEnd(bes) => bes.statements,
    };
    let raw_body = stmts
        .into_iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>()
        .join("; ");

    // PL/pgSQL parsing is deferred to lib.rs (with correct file offset).
    let parsed_body = FunctionBody::Raw(raw_body);

    Ok(CreateProcedure {
        id: crate::next_node_id(),
        span: Span::DUMMY,
        body_start: 0,
        name: pname,
        or_replace,
        params: params_out,
        language: FunctionLanguage::PlPgSql,
        body: parsed_body,
    })
}

// ── CREATE TRIGGER ────────────────────────────────────────────────────────────

fn convert_create_trigger(ct: sp::CreateTrigger) -> Result<CreateTrigger, ParseError> {
    // In 0.62, name is ObjectName (Vec<ObjectNamePart>)
    let tname = ct
        .name
        .0
        .into_iter()
        .filter_map(part_to_ident)
        .next_back()
        .map(convert_ident)
        .ok_or_else(|| ParseError::Unsupported("empty trigger name".to_string()))?;

    let table = convert_object_name(ct.table_name);

    // In 0.62, period is Option<TriggerPeriod> and also has a For variant
    let timing = match ct.period {
        Some(sp::TriggerPeriod::Before) => TriggerTiming::Before,
        Some(sp::TriggerPeriod::After) => TriggerTiming::After,
        Some(sp::TriggerPeriod::InsteadOf) => TriggerTiming::InsteadOf,
        Some(sp::TriggerPeriod::For) | None => TriggerTiming::After,
    };

    let evts = ct
        .events
        .into_iter()
        .map(|e| match e {
            sp::TriggerEvent::Insert => Ok(TriggerEvent::Insert),
            sp::TriggerEvent::Delete => Ok(TriggerEvent::Delete),
            sp::TriggerEvent::Truncate => Ok(TriggerEvent::Truncate),
            sp::TriggerEvent::Update(cols) => Ok(TriggerEvent::Update(
                cols.into_iter().map(convert_ident).collect(),
            )),
        })
        .collect::<Result<Vec<_>, ParseError>>()?;

    // In 0.62, trigger_object is Option<TriggerObjectKind> wrapping TriggerObject
    let for_each = match ct.trigger_object {
        Some(sp::TriggerObjectKind::For(sp::TriggerObject::Row))
        | Some(sp::TriggerObjectKind::ForEach(sp::TriggerObject::Row)) => TriggerForEach::Row,
        Some(sp::TriggerObjectKind::For(sp::TriggerObject::Statement))
        | Some(sp::TriggerObjectKind::ForEach(sp::TriggerObject::Statement))
        | None => TriggerForEach::Statement,
    };

    let when = ct.condition.map(convert_expr).transpose()?;

    // In 0.62, exec_body is Option<TriggerExecBody>
    let function = ct
        .exec_body
        .map(|eb| convert_object_name(eb.func_desc.name))
        .unwrap_or_else(|| ObjectName(vec![]));

    Ok(CreateTrigger {
        name: tname,
        table,
        timing,
        events: evts,
        for_each,
        when,
        function,
    })
}

// ── CREATE POLICY ─────────────────────────────────────────────────────────────

fn convert_create_policy(
    name: sp::Ident,
    table_name: sp::ObjectName,
    policy_type: Option<sp::CreatePolicyType>,
    command: Option<sp::CreatePolicyCommand>,
    to: Option<Vec<sp::Owner>>,
    using: Option<sp::Expr>,
    with_check: Option<sp::Expr>,
) -> Result<CreatePolicy, ParseError> {
    let table = convert_object_name(table_name);

    let permissive = match policy_type {
        None | Some(sp::CreatePolicyType::Permissive) => true,
        Some(sp::CreatePolicyType::Restrictive) => false,
    };

    let cmd = match command {
        None | Some(sp::CreatePolicyCommand::All) => PolicyCommand::All,
        Some(sp::CreatePolicyCommand::Select) => PolicyCommand::Select,
        Some(sp::CreatePolicyCommand::Insert) => PolicyCommand::Insert,
        Some(sp::CreatePolicyCommand::Update) => PolicyCommand::Update,
        Some(sp::CreatePolicyCommand::Delete) => PolicyCommand::Delete,
    };

    let roles = to
        .unwrap_or_default()
        .into_iter()
        .filter_map(|owner| match owner {
            sp::Owner::Ident(id) => Some(convert_ident(id)),
            _ => None,
        })
        .collect();

    Ok(CreatePolicy {
        id: crate::next_node_id(),
        span: Span::DUMMY,
        name: convert_ident(name),
        table,
        command: cmd,
        permissive,
        roles,
        using: using.map(convert_expr).transpose()?,
        with_check: with_check.map(convert_expr).transpose()?,
    })
}

// ── ALTER TABLE ───────────────────────────────────────────────────────────────

fn convert_alter_table(
    name: sp::ObjectName,
    operations: Vec<sp::AlterTableOperation>,
) -> Result<AlterTable, ParseError> {
    let tname = convert_object_name(name);
    let ops = operations
        .into_iter()
        .map(convert_alter_table_op)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(AlterTable {
        name: tname,
        operations: ops,
    })
}

fn convert_alter_table_op(op: sp::AlterTableOperation) -> Result<AlterTableOp, ParseError> {
    match op {
        sp::AlterTableOperation::EnableRowLevelSecurity => Ok(AlterTableOp::EnableRowLevelSecurity),
        sp::AlterTableOperation::DisableRowLevelSecurity => {
            Ok(AlterTableOp::DisableRowLevelSecurity)
        }
        sp::AlterTableOperation::AddColumn { column_def, .. } => {
            Ok(AlterTableOp::AddColumn(convert_column_def(column_def)?))
        }
        // In 0.62, DropColumn uses column_names (Vec) instead of column_name
        sp::AlterTableOperation::DropColumn {
            column_names,
            if_exists,
            ..
        } => {
            let name = column_names
                .into_iter()
                .next()
                .map(convert_ident)
                .ok_or_else(|| {
                    ParseError::Unsupported("DROP COLUMN with no column name".to_string())
                })?;
            Ok(AlterTableOp::DropColumn { name, if_exists })
        }
        sp::AlterTableOperation::AlterColumn { column_name, op } => Ok(AlterTableOp::AlterColumn {
            name: convert_ident(column_name),
            action: convert_alter_column_op(op)?,
        }),
        // In 0.62, AddConstraint is a struct variant
        sp::AlterTableOperation::AddConstraint { constraint, .. } => Ok(
            AlterTableOp::AddConstraint(convert_table_constraint(constraint)?),
        ),
        sp::AlterTableOperation::DropConstraint {
            name, if_exists, ..
        } => Ok(AlterTableOp::DropConstraint {
            name: convert_ident(name),
            if_exists,
        }),
        other => Err(ParseError::Unsupported(format!(
            "unsupported ALTER TABLE operation: {}",
            other
        ))),
    }
}

fn convert_alter_column_op(op: sp::AlterColumnOperation) -> Result<AlterColumnAction, ParseError> {
    match op {
        sp::AlterColumnOperation::SetDefault { value } => {
            Ok(AlterColumnAction::SetDefault(convert_expr(value)?))
        }
        sp::AlterColumnOperation::DropDefault => Ok(AlterColumnAction::DropDefault),
        sp::AlterColumnOperation::SetNotNull => Ok(AlterColumnAction::SetNotNull),
        sp::AlterColumnOperation::DropNotNull => Ok(AlterColumnAction::DropNotNull),
        sp::AlterColumnOperation::SetDataType { data_type, .. } => Ok(
            AlterColumnAction::SetDataType(convert_data_type(data_type)?),
        ),
        other => Err(ParseError::Unsupported(format!(
            "unsupported ALTER COLUMN operation: {}",
            other
        ))),
    }
}

// ── Data type conversion ──────────────────────────────────────────────────────

fn convert_data_type(dt: sp::DataType) -> Result<DataType, ParseError> {
    match dt {
        // Integer family
        sp::DataType::SmallInt(_) | sp::DataType::Int2(_) => Ok(DataType::SmallInt),
        sp::DataType::Integer(_)
        | sp::DataType::Int(_)
        | sp::DataType::Int4(_)
        | sp::DataType::Int32 => Ok(DataType::Integer),
        sp::DataType::BigInt(_) | sp::DataType::Int8(_) | sp::DataType::Int64 => {
            Ok(DataType::BigInt)
        }
        // Numeric / decimal
        sp::DataType::Numeric(info) | sp::DataType::Decimal(info) | sp::DataType::Dec(info) => {
            let (precision, scale) = match info {
                sp::ExactNumberInfo::None => (None, None),
                sp::ExactNumberInfo::Precision(p) => (Some(p), None),
                // scale is i64 in 0.62, cast to u64
                sp::ExactNumberInfo::PrecisionAndScale(p, s) => (Some(p), Some(s as u64)),
            };
            Ok(DataType::Numeric { precision, scale })
        }
        // Float family
        sp::DataType::Real | sp::DataType::Float4 | sp::DataType::Float32 => Ok(DataType::Real),
        sp::DataType::DoublePrecision
        | sp::DataType::Float8
        | sp::DataType::Float64
        | sp::DataType::Double(_) => Ok(DataType::DoublePrecision),
        sp::DataType::Float(_) => Ok(DataType::Real),
        // Text family
        sp::DataType::Text | sp::DataType::MediumText | sp::DataType::LongText => {
            Ok(DataType::Text)
        }
        sp::DataType::Varchar(len)
        | sp::DataType::CharacterVarying(len)
        | sp::DataType::CharVarying(len) => {
            let n = len.and_then(|l| match l {
                sp::CharacterLength::IntegerLength { length, .. } => Some(length),
                sp::CharacterLength::Max => None,
            });
            Ok(DataType::Varchar(n))
        }
        sp::DataType::Char(len) | sp::DataType::Character(len) => {
            let n = len.and_then(|l| match l {
                sp::CharacterLength::IntegerLength { length, .. } => Some(length),
                sp::CharacterLength::Max => None,
            });
            Ok(DataType::Char(n))
        }
        // Temporal
        sp::DataType::Date | sp::DataType::Date32 => Ok(DataType::Date),
        sp::DataType::Time(_, tz) => match tz {
            sp::TimezoneInfo::None | sp::TimezoneInfo::WithoutTimeZone => Ok(DataType::Time),
            _ => Ok(DataType::Time),
        },
        sp::DataType::Timestamp(_, tz) => match tz {
            sp::TimezoneInfo::WithTimeZone | sp::TimezoneInfo::Tz => Ok(DataType::TimestampTz),
            _ => Ok(DataType::Timestamp),
        },
        // In 0.62, Interval is a struct variant
        sp::DataType::Interval { .. } => Ok(DataType::Interval),
        // Boolean
        sp::DataType::Boolean | sp::DataType::Bool => Ok(DataType::Boolean),
        // Binary
        sp::DataType::Bytea => Ok(DataType::Bytea),
        // JSON
        sp::DataType::JSON => Ok(DataType::Json),
        sp::DataType::JSONB => Ok(DataType::Jsonb),
        // UUID
        sp::DataType::Uuid => Ok(DataType::Uuid),
        // Array — handle `INT[]` or `ARRAY<INT>`
        sp::DataType::Array(elem) => {
            let inner = match elem {
                sp::ArrayElemTypeDef::None => {
                    return Ok(DataType::Custom("array".to_string()));
                }
                sp::ArrayElemTypeDef::AngleBracket(inner)
                | sp::ArrayElemTypeDef::SquareBracket(inner, _)
                | sp::ArrayElemTypeDef::Parenthesis(inner) => *inner,
            };
            Ok(DataType::Array(Box::new(convert_data_type(inner)?)))
        }
        // Custom / enum / user-defined — ObjectName.0 is now Vec<ObjectNamePart>
        sp::DataType::Custom(name, _) => {
            let s = name
                .0
                .into_iter()
                .filter_map(|p| match p {
                    sp::ObjectNamePart::Identifier(id) => Some(id.value),
                    sp::ObjectNamePart::Function(_) => None,
                })
                .collect::<Vec<_>>()
                .join(".");
            Ok(DataType::Custom(s))
        }
        // Trigger return type
        sp::DataType::Trigger => Ok(DataType::Custom("trigger".to_string())),
        other => Err(ParseError::Unsupported(format!(
            "unsupported data type: {}",
            other
        ))),
    }
}

// ── Identifier helpers ────────────────────────────────────────────────────────

fn convert_ident(id: sp::Ident) -> Ident {
    Ident {
        value: id.value,
        quoted: id.quote_style.is_some(),
    }
}

/// In 0.62, ObjectName holds Vec<ObjectNamePart> instead of Vec<Ident>.
/// We filter out any Function parts (dialect-specific) and keep Identifiers.
fn convert_object_name(name: sp::ObjectName) -> ObjectName {
    ObjectName(
        name.0
            .into_iter()
            .filter_map(|p| match p {
                sp::ObjectNamePart::Identifier(id) => Some(convert_ident(id)),
                sp::ObjectNamePart::Function(_) => None,
            })
            .collect(),
    )
}

// ── Résolution des positions ──────────────────────────────────────────────────

/// Contexte de conversion des positions sqlparser en offsets octets.
///
/// sqlparser rend les positions en `Location { line, column }`, 1-basées et
/// comptées en **caractères**, relatives au texte qui lui a été soumis. Pour en
/// faire un offset octet dans le document il faut ce texte et l'offset du
/// statement dans le document.
///
/// Ce contexte est ambiant (thread-local) plutôt que passé en paramètre :
/// `convert_expr` est atteint depuis 11 fonctions et 212 sites d'appel internes
/// à ce module, et l'information n'intéresse aucun des intermédiaires. Il est
/// posé par [`convert_statement_at`] et retiré par un garde RAII, donc il ne
/// peut pas fuir d'un statement au suivant. Étant thread-local, un parse
/// parallèle reste correct.
struct SpanCtx {
    /// Offset octet du début de chaque ligne du texte soumis à sqlparser.
    line_starts: Vec<usize>,
    /// Texte soumis, pour convertir une colonne-caractère en offset octet.
    source: String,
    /// Offset du texte soumis dans le document complet.
    base: usize,
}

impl SpanCtx {
    fn new(source: &str, base: usize) -> Self {
        let mut line_starts = vec![0usize];
        for (i, b) in source.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(i + 1);
            }
        }
        Self {
            line_starts,
            source: source.to_string(),
            base,
        }
    }

    /// `Location` (ligne/colonne 1-basées, colonne en caractères) → offset octet
    /// dans le document.
    fn offset_of(&self, line: u64, column: u64) -> Option<usize> {
        if line == 0 {
            // sqlparser utilise la ligne 0 pour un span vide.
            return None;
        }
        let line_start = *self.line_starts.get((line - 1) as usize)?;
        let line_end = self
            .line_starts
            .get(line as usize)
            .copied()
            .unwrap_or(self.source.len());
        let line_text = self.source.get(line_start..line_end)?;

        // Avancer de (column - 1) caractères, pas d'octets : une colonne after
        // un accent ne correspond pas au même nombre d'octets.
        let char_offset = (column.saturating_sub(1)) as usize;
        let byte_in_line = line_text
            .char_indices()
            .nth(char_offset)
            .map(|(b, _)| b)
            .unwrap_or_else(|| line_text.len());

        Some(self.base + line_start + byte_in_line)
    }
}

thread_local! {
    static SPAN_CTX: std::cell::RefCell<Option<SpanCtx>> =
        const { std::cell::RefCell::new(None) };
}

/// Garde RAII : retire le contexte de position à la sortie de portée.
struct SpanCtxGuard;

impl Drop for SpanCtxGuard {
    fn drop(&mut self) {
        SPAN_CTX.with(|c| *c.borrow_mut() = None);
    }
}

fn enter_span_ctx(source: &str, base: usize) -> SpanCtxGuard {
    SPAN_CTX.with(|c| *c.borrow_mut() = Some(SpanCtx::new(source, base)));
    SpanCtxGuard
}

/// Span d'une expression sqlparser, en offsets octets dans le document.
///
/// Renvoie [`Span::DUMMY`] hors contexte de position — c'est-à-dire quand
/// l'appelant a utilisé [`convert_statement`] plutôt que
/// [`convert_statement_at`], ou quand le texte soumis à sqlparser a été réécrit
/// avant le parse et que les positions ne correspondent plus au document.
/// Mieux vaut pas de span qu'un span faux.
fn sp_expr_span(expr: &sp::Expr) -> Span {
    use sqlparser::ast::Spanned;
    let sp_span = expr.span();
    SPAN_CTX.with(|c| {
        let borrow = c.borrow();
        let Some(ctx) = borrow.as_ref() else {
            return Span::DUMMY;
        };
        let start = ctx.offset_of(sp_span.start.line, sp_span.start.column);
        let end = ctx.offset_of(sp_span.end.line, sp_span.end.column);
        match (start, end) {
            (Some(s), Some(e)) if e >= s => Span {
                start: s as u32,
                end: e as u32,
            },
            _ => Span::DUMMY,
        }
    })
}
