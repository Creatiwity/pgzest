//! Conversion layer: sqlparser 0.54 AST → pgsql_ast types.
//!
//! Entry point is [`convert_statement`].

use pgsql_ast::{
    ddl::{
        AlterColumnAction, AlterTable, AlterTableOp, ColumnConstraint, ColumnDef,
        CreateFunction, CreateIndex, CreatePolicy, CreateProcedure, CreateTable, CreateTrigger,
        DdlStatement, FunctionBody, FunctionLanguage, FunctionParam, ParamMode, PolicyCommand,
        ReferentialAction, ReturnType, TableConstraint, TriggerEvent, TriggerForEach,
        TriggerTiming, Volatility,
    },
    expr::{BinaryOp, ColumnRef, Expr, Ident, Literal, ObjectName, UnaryOp},
    node::Span,
    stmt::{
        Assignment, Cte, DeleteStmt, InsertSource, InsertStmt, Join, JoinConstraint, JoinKind,
        OnConflict, OnConflictAction, OnConflictTarget, OrderByExpr, SelectItem, SelectStmt,
        TableRef, UpdateStmt,
    },
    types::DataType,
    Statement,
};

use crate::error::ParseError;
use sqlparser::ast as sp;

// ── Public entry point ───────────────────────────────────────────────────────

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
        sp::Statement::Update {
            table,
            assignments,
            from,
            selection,
            returning,
            ..
        } => {
            let s = convert_update(table, assignments, from, selection, returning)?;
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
        } => {
            let ddl = convert_create_procedure(or_alter, name, params, body)?;
            Ok(Statement::Ddl(DdlStatement::CreateProcedure(ddl)))
        }
        sp::Statement::CreateTrigger {
            name,
            period,
            events,
            table_name,
            trigger_object,
            condition,
            exec_body,
            ..
        } => {
            let ddl = convert_create_trigger(name, period, events, table_name, trigger_object, condition, exec_body)?;
            Ok(Statement::Ddl(DdlStatement::CreateTrigger(ddl)))
        }
        sp::Statement::CreatePolicy {
            name,
            table_name,
            policy_type,
            command,
            to,
            using,
            with_check,
        } => {
            let ddl = convert_create_policy(name, table_name, policy_type, command, to, using, with_check)?;
            Ok(Statement::Ddl(DdlStatement::CreatePolicy(ddl)))
        }
        sp::Statement::AlterTable {
            name, operations, ..
        } => {
            let ddl = convert_alter_table(name, operations)?;
            Ok(Statement::Ddl(DdlStatement::AlterTable(ddl)))
        }
        // CREATE EXTENSION — silently skip (no pgsql_ast equivalent yet)
        sp::Statement::CreateExtension { .. } => {
            Err(ParseError::Unsupported(
                "CREATE EXTENSION is not yet supported".to_string(),
            ))
        }
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

    // ORDER BY
    let order_by = if let Some(ob) = query.order_by {
        ob.exprs
            .into_iter()
            .map(convert_order_by_expr)
            .collect::<Result<Vec<_>, _>>()?
    } else {
        vec![]
    };

    let limit = query.limit.map(convert_expr).transpose()?;
    let offset = query
        .offset
        .map(|o| convert_expr(o.value))
        .transpose()?;

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
            ))
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
        sp::SelectItem::QualifiedWildcard(name, _) => {
            // Take last part as the qualifier
            let last = name
                .0
                .last()
                .ok_or_else(|| ParseError::Unsupported("empty qualified wildcard".to_string()))?
                .clone();
            Ok(SelectItem::QualifiedWildcard(convert_ident(last)))
        }
        sp::SelectItem::UnnamedExpr(e) => Ok(SelectItem::Expr {
            expr: convert_expr(e)?,
            alias: None,
        }),
        sp::SelectItem::ExprWithAlias { expr, alias } => Ok(SelectItem::Expr {
            expr: convert_expr(expr)?,
            alias: Some(convert_ident(alias)),
        }),
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
        sp::JoinOperator::Inner(c) => (JoinKind::Inner, convert_join_constraint(c)?),
        sp::JoinOperator::LeftOuter(c) => (JoinKind::Left, convert_join_constraint(c)?),
        sp::JoinOperator::RightOuter(c) => (JoinKind::Right, convert_join_constraint(c)?),
        sp::JoinOperator::FullOuter(c) => (JoinKind::Full, convert_join_constraint(c)?),
        sp::JoinOperator::CrossJoin => (JoinKind::Cross, JoinConstraint::Natural),
        other => {
            return Err(ParseError::Unsupported(format!(
                "unsupported join operator: {:?}",
                other
            )))
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
        sp::JoinConstraint::Using(names) => {
            // In sqlparser 0.54, Using holds Vec<ObjectName>
            let idents = names
                .into_iter()
                .map(|on| {
                    on.0.into_iter()
                        .last()
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
    Ok(OrderByExpr {
        expr: convert_expr(o.expr)?,
        asc: o.asc,
        nulls_first: o.nulls_first,
    })
}

// ── Expression conversion ────────────────────────────────────────────────────

fn convert_expr(expr: sp::Expr) -> Result<Expr, ParseError> {
    match expr {
        sp::Expr::Value(v) => convert_value(v),
        sp::Expr::Identifier(id) => {
            // Single ident — could be a column name
            Ok(Expr::Column(ColumnRef {
                table: None,
                column: convert_ident(id),
            }))
        }
        sp::Expr::CompoundIdentifier(parts) => {
            match parts.len() {
                0 => Err(ParseError::Unsupported(
                    "empty compound identifier".to_string(),
                )),
                1 => Ok(Expr::Column(ColumnRef {
                    table: None,
                    column: convert_ident(parts.into_iter().next().unwrap()),
                })),
                _ => {
                    let mut iter = parts.into_iter().rev();
                    let col = convert_ident(iter.next().unwrap());
                    let tbl = convert_ident(iter.next().unwrap());
                    Ok(Expr::Column(ColumnRef {
                        table: Some(tbl),
                        column: col,
                    }))
                }
            }
        }
        sp::Expr::IsNull(e) => Ok(Expr::UnaryOp {
            op: UnaryOp::IsNull,
            expr: Box::new(convert_expr(*e)?),
        }),
        sp::Expr::IsNotNull(e) => Ok(Expr::UnaryOp {
            op: UnaryOp::IsNotNull,
            expr: Box::new(convert_expr(*e)?),
        }),
        sp::Expr::BinaryOp { left, op, right } => {
            let l = convert_expr(*left)?;
            let r = convert_expr(*right)?;
            let bop = convert_binary_op(op)?;
            Ok(Expr::BinaryOp {
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
                    )))
                }
            };
            Ok(Expr::UnaryOp {
                op: uop,
                expr: Box::new(e),
            })
        }
        sp::Expr::Cast { expr, data_type, .. } => Ok(Expr::Cast {
            expr: Box::new(convert_expr(*expr)?),
            data_type: convert_data_type(data_type)?,
        }),
        sp::Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => {
            let op = operand.map(|e| convert_expr(*e)).transpose()?.map(Box::new);
            let when_clauses = conditions
                .into_iter()
                .zip(results)
                .map(|(cond, res)| Ok((convert_expr(cond)?, convert_expr(res)?)))
                .collect::<Result<Vec<_>, ParseError>>()?;
            let else_clause = else_result
                .map(|e| convert_expr(*e))
                .transpose()?
                .map(Box::new);
            Ok(Expr::Case {
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
            Ok(Expr::InList {
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
            Ok(Expr::InSubquery {
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
        } => Ok(Expr::Between {
            expr: Box::new(convert_expr(*expr)?),
            low: Box::new(convert_expr(*low)?),
            high: Box::new(convert_expr(*high)?),
            negated,
        }),
        sp::Expr::Subquery(q) => {
            let sub = convert_query(*q)?;
            Ok(Expr::Subquery(Box::new(sub)))
        }
        sp::Expr::Function(f) => {
            let name = convert_object_name(f.name);
            let args = convert_function_args(f.args)?;
            // filter is not in pgsql_ast yet — ignore it
            Ok(Expr::FunctionCall {
                name,
                args,
                filter: None,
                distinct: false,
            })
        }
        sp::Expr::Wildcard(_) => Ok(Expr::Wildcard),
        sp::Expr::QualifiedWildcard(parts, _) => {
            let last = parts
                .0
                .into_iter()
                .last()
                .ok_or_else(|| ParseError::Unsupported("empty qualified wildcard".to_string()))?;
            Ok(Expr::QualifiedWildcard(convert_ident(last)))
        }
        sp::Expr::Like {
            negated,
            expr,
            pattern,
            ..
        } => {
            let op = if negated { BinaryOp::NotLike } else { BinaryOp::Like };
            Ok(Expr::BinaryOp {
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
                return Err(ParseError::Unsupported("NOT ILIKE not supported".to_string()));
            }
            Ok(Expr::BinaryOp {
                left: Box::new(convert_expr(*expr)?),
                op: BinaryOp::ILike,
                right: Box::new(convert_expr(*pattern)?),
            })
        }
        sp::Expr::IsTrue(e) => {
            // Rewrite as `expr = TRUE`
            Ok(Expr::BinaryOp {
                left: Box::new(convert_expr(*e)?),
                op: BinaryOp::Is,
                right: Box::new(Expr::Literal(Literal::Boolean(true))),
            })
        }
        sp::Expr::IsFalse(e) => {
            Ok(Expr::BinaryOp {
                left: Box::new(convert_expr(*e)?),
                op: BinaryOp::Is,
                right: Box::new(Expr::Literal(Literal::Boolean(false))),
            })
        }
        sp::Expr::IsNotTrue(e) => {
            Ok(Expr::BinaryOp {
                left: Box::new(convert_expr(*e)?),
                op: BinaryOp::IsNot,
                right: Box::new(Expr::Literal(Literal::Boolean(true))),
            })
        }
        sp::Expr::IsNotFalse(e) => {
            Ok(Expr::BinaryOp {
                left: Box::new(convert_expr(*e)?),
                op: BinaryOp::IsNot,
                right: Box::new(Expr::Literal(Literal::Boolean(false))),
            })
        }
        sp::Expr::IsDistinctFrom(l, r) => Ok(Expr::BinaryOp {
            left: Box::new(convert_expr(*l)?),
            op: BinaryOp::IsNot,
            right: Box::new(convert_expr(*r)?),
        }),
        sp::Expr::IsNotDistinctFrom(l, r) => Ok(Expr::BinaryOp {
            left: Box::new(convert_expr(*l)?),
            op: BinaryOp::Is,
            right: Box::new(convert_expr(*r)?),
        }),
        sp::Expr::Nested(e) => convert_expr(*e),
        other => Err(ParseError::Unsupported(format!(
            "unsupported expression: {}",
            other
        ))),
    }
}

fn convert_value(v: sp::Value) -> Result<Expr, ParseError> {
    match v {
        sp::Value::Number(n, _) => Ok(Expr::Literal(Literal::Number(n))),
        sp::Value::SingleQuotedString(s) => Ok(Expr::Literal(Literal::String(s))),
        sp::Value::DollarQuotedString(dqs) => Ok(Expr::Literal(Literal::String(dqs.value))),
        sp::Value::Boolean(b) => Ok(Expr::Literal(Literal::Boolean(b))),
        sp::Value::Null => Ok(Expr::Literal(Literal::Null)),
        sp::Value::DoubleQuotedString(s) => Ok(Expr::Literal(Literal::String(s))),
        sp::Value::EscapedStringLiteral(s) => Ok(Expr::Literal(Literal::String(s))),
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
        other => Err(ParseError::Unsupported(format!(
            "unsupported binary operator: {:?}",
            other
        ))),
    }
}

fn convert_function_args(
    args: sp::FunctionArguments,
) -> Result<Vec<Expr>, ParseError> {
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
                sp::FunctionArg::Unnamed(sp::FunctionArgExpr::Wildcard) => Ok(Expr::Wildcard),
                sp::FunctionArg::Unnamed(sp::FunctionArgExpr::QualifiedWildcard(n)) => {
                    let last = n
                        .0
                        .into_iter()
                        .last()
                        .ok_or_else(|| {
                            ParseError::Unsupported("empty qualified wildcard".to_string())
                        })?;
                    Ok(Expr::QualifiedWildcard(convert_ident(last)))
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
        sp::TableObject::TableFunction(_) => {
            return Err(ParseError::Unsupported(
                "INSERT INTO TABLE FUNCTION not supported".to_string(),
            ))
        }
    };

    let columns = ins.columns.into_iter().map(convert_ident).collect();

    let source = if let Some(q) = ins.source {
        match *q.body {
            sp::SetExpr::Values(v) => {
                let rows = v
                    .rows
                    .into_iter()
                    .map(|row| {
                        row.into_iter()
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
                .last()
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
            .last()
            .map(convert_ident)
            .ok_or_else(|| ParseError::Unsupported("empty assignment target".to_string()))?,
        sp::AssignmentTarget::Tuple(_) => {
            return Err(ParseError::Unsupported(
                "tuple assignment not supported".to_string(),
            ))
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

    let from_tables = match from {
        None => vec![],
        Some(sp::UpdateTableFromKind::BeforeSet(twj))
        | Some(sp::UpdateTableFromKind::AfterSet(twj)) => {
            vec![convert_table_factor(twj.relation)?]
        }
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
        .flat_map(|twj| std::iter::once(twj.relation).chain(twj.joins.into_iter().map(|j| j.relation)))
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
            sp::ColumnOption::Unique { is_primary, .. } => {
                if is_primary {
                    constraints.push(ColumnConstraint::PrimaryKey);
                } else {
                    constraints.push(ColumnConstraint::Unique);
                }
            }
            sp::ColumnOption::ForeignKey {
                foreign_table,
                referred_columns,
                on_delete,
                on_update,
                ..
            } => {
                constraints.push(ColumnConstraint::References {
                    table: convert_object_name(foreign_table),
                    column: referred_columns.into_iter().next().map(convert_ident),
                    on_delete: on_delete.map(convert_referential_action),
                    on_update: on_update.map(convert_referential_action),
                });
            }
            sp::ColumnOption::Check(e) => {
                constraints.push(ColumnConstraint::Check(convert_expr(e)?));
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
        sp::TableConstraint::PrimaryKey { columns, .. } => Ok(TableConstraint::PrimaryKey {
            columns: columns.into_iter().map(convert_ident).collect(),
        }),
        sp::TableConstraint::Unique { columns, .. } => Ok(TableConstraint::Unique {
            columns: columns.into_iter().map(convert_ident).collect(),
        }),
        sp::TableConstraint::ForeignKey {
            columns,
            foreign_table,
            referred_columns,
            on_delete,
            on_update,
            ..
        } => Ok(TableConstraint::ForeignKey {
            columns: columns.into_iter().map(convert_ident).collect(),
            ref_table: convert_object_name(foreign_table),
            ref_columns: referred_columns.into_iter().map(convert_ident).collect(),
            on_delete: on_delete.map(convert_referential_action),
            on_update: on_update.map(convert_referential_action),
        }),
        sp::TableConstraint::Check { expr, .. } => {
            Ok(TableConstraint::Check(convert_expr(*expr)?))
        }
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
            .last()
            .map(convert_ident)
            .unwrap_or_else(|| Ident::new(""))
    });
    let table = convert_object_name(ci.table_name);
    let columns = ci
        .columns
        .into_iter()
        .map(|ob| convert_ident_from_expr(ob.expr))
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
            .last()
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
    let language = match cf.language.as_ref().map(|l| l.value.to_lowercase()).as_deref() {
        Some("sql") => FunctionLanguage::Sql,
        Some("plpgsql") => FunctionLanguage::PlPgSql,
        Some("c") => FunctionLanguage::C,
        Some("internal") => FunctionLanguage::Internal,
        Some(other) => FunctionLanguage::Other(other.to_string()),
        None => FunctionLanguage::Other("unknown".to_string()),
    };

    // Return type
    let return_type = match &cf.return_type {
        None => ReturnType::Void,
        Some(dt) => convert_return_type(dt.clone())?,
    };

    // Volatility
    let volatility = match cf.behavior {
        Some(sp::FunctionBehavior::Immutable) => Volatility::Immutable,
        Some(sp::FunctionBehavior::Stable) => Volatility::Stable,
        _ => Volatility::Volatile,
    };

    // Security definer — extract from language string? No, from options:
    // sqlparser exposes it in the function body but there's no dedicated field.
    // We'll approximate: security definer is not in sqlparser 0.54 CreateFunction struct;
    // we'll default to false. TODO: parse AS body keyword options.
    let security_definer = false;

    // Body
    let body = convert_function_body(cf.function_body, &language)?;

    Ok(CreateFunction {
        id: crate::next_node_id(),
        span: Span::DUMMY,
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

fn convert_return_type(dt: sp::DataType) -> Result<ReturnType, ParseError> {
    match &dt {
        sp::DataType::Trigger => Ok(ReturnType::Trigger),
        // SETOF <type> — sqlparser may represent this as Custom("SETOF <type>") or Custom("SETOF")
        sp::DataType::Custom(name, _) => {
            let s = name
                .0
                .iter()
                .map(|id| id.value.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            let upper = s.to_uppercase();
            if upper.starts_with("SETOF") {
                let rest = s[5..].trim();
                if rest.is_empty() {
                    // SETOF without a type name — treat as SETOF record
                    return Ok(ReturnType::SetOf(DataType::Custom("record".to_string())));
                }
                return Ok(ReturnType::SetOf(parse_simple_type(rest)));
            }
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
        "BOOLEAN" | "BOOL" => DataType::Boolean,
        "TIMESTAMPTZ" | "TIMESTAMP WITH TIME ZONE" => DataType::TimestampTz,
        "TIMESTAMP" => DataType::Timestamp,
        "DATE" => DataType::Date,
        "REAL" | "FLOAT4" => DataType::Real,
        "DOUBLE PRECISION" | "FLOAT8" => DataType::DoublePrecision,
        "JSON" => DataType::Json,
        "JSONB" => DataType::Jsonb,
        "BYTEA" => DataType::Bytea,
        _ => DataType::Custom(s.to_string()),
    }
}

fn convert_function_body(
    body: Option<sp::CreateFunctionBody>,
    language: &FunctionLanguage,
) -> Result<FunctionBody, ParseError> {
    let raw = match body {
        None => {
            return Err(ParseError::Unsupported(
                "function with no body".to_string(),
            ))
        }
        Some(sp::CreateFunctionBody::AsBeforeOptions(e))
        | Some(sp::CreateFunctionBody::AsAfterOptions(e))
        | Some(sp::CreateFunctionBody::Return(e)) => extract_body_string(e),
    };

    // For LANGUAGE plpgsql, attempt to parse the body with the PL/pgSQL parser
    if matches!(language, FunctionLanguage::PlPgSql) {
        match crate::plpgsql::parse_plpgsql_body(&raw) {
            Ok(parsed) => return Ok(FunctionBody::Parsed(Box::new(parsed))),
            Err(e) => {
                // Graceful degradation — fall through to Raw
                eprintln!("PL/pgSQL parse warning: {}", e);
            }
        }
    }

    // For LANGUAGE sql, try to parse as a SelectStmt
    if matches!(language, FunctionLanguage::Sql) {
        // Attempt to parse the body string as a SELECT
        let dialect = sqlparser::dialect::PostgreSqlDialect {};
        if let Ok(stmts) =
            sqlparser::parser::Parser::parse_sql(&dialect, raw.trim())
        {
            if let Some(sp::Statement::Query(q)) = stmts.into_iter().next() {
                if let Ok(sel) = convert_query(*q) {
                    return Ok(FunctionBody::SqlQuery(Box::new(sel)));
                }
            }
        }
    }

    Ok(FunctionBody::Raw(raw))
}

fn extract_body_string(expr: sp::Expr) -> String {
    match expr {
        sp::Expr::Value(sp::Value::DollarQuotedString(dqs)) => dqs.value,
        sp::Expr::Value(sp::Value::SingleQuotedString(s)) => s,
        other => other.to_string(),
    }
}

// ── CREATE PROCEDURE ──────────────────────────────────────────────────────────

fn convert_create_procedure(
    _or_alter: bool,
    name: sp::ObjectName,
    params: Option<Vec<sp::ProcedureParam>>,
    body: Vec<sp::Statement>,
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

    // The body of a procedure in sqlparser is a Vec<Statement>;
    // for PL/pgSQL procedures, this is typically a single Begin..End block
    // represented as a raw string by the parser.
    let raw_body = body
        .into_iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>()
        .join("; ");

    // Attempt to parse with the PL/pgSQL parser
    let parsed_body = match crate::plpgsql::parse_plpgsql_body(&raw_body) {
        Ok(parsed) => FunctionBody::Parsed(Box::new(parsed)),
        Err(e) => {
            eprintln!("PL/pgSQL procedure parse warning: {}", e);
            FunctionBody::Raw(raw_body)
        }
    };

    Ok(CreateProcedure {
        id: crate::next_node_id(),
        span: Span::DUMMY,
        name: pname,
        or_replace,
        params: params_out,
        language: FunctionLanguage::PlPgSql,
        body: parsed_body,
    })
}

// ── CREATE TRIGGER ────────────────────────────────────────────────────────────

fn convert_create_trigger(
    name: sp::ObjectName,
    period: sp::TriggerPeriod,
    events: Vec<sp::TriggerEvent>,
    table_name: sp::ObjectName,
    trigger_object: sp::TriggerObject,
    condition: Option<sp::Expr>,
    exec_body: sp::TriggerExecBody,
) -> Result<CreateTrigger, ParseError> {
    let tname = name
        .0
        .into_iter()
        .last()
        .map(convert_ident)
        .ok_or_else(|| ParseError::Unsupported("empty trigger name".to_string()))?;

    let table = convert_object_name(table_name);

    let timing = match period {
        sp::TriggerPeriod::Before => TriggerTiming::Before,
        sp::TriggerPeriod::After => TriggerTiming::After,
        sp::TriggerPeriod::InsteadOf => TriggerTiming::InsteadOf,
    };

    let evts = events
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

    let for_each = match trigger_object {
        sp::TriggerObject::Row => TriggerForEach::Row,
        sp::TriggerObject::Statement => TriggerForEach::Statement,
    };

    let when = condition.map(convert_expr).transpose()?;

    let function = convert_object_name(exec_body.func_desc.name);

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
        sp::AlterTableOperation::DropColumn {
            column_name,
            if_exists,
            ..
        } => Ok(AlterTableOp::DropColumn {
            name: convert_ident(column_name),
            if_exists,
        }),
        sp::AlterTableOperation::AlterColumn { column_name, op } => {
            Ok(AlterTableOp::AlterColumn {
                name: convert_ident(column_name),
                action: convert_alter_column_op(op)?,
            })
        }
        sp::AlterTableOperation::AddConstraint(tc) => {
            Ok(AlterTableOp::AddConstraint(convert_table_constraint(tc)?))
        }
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
        sp::AlterColumnOperation::SetDataType { data_type, .. } => {
            Ok(AlterColumnAction::SetDataType(convert_data_type(data_type)?))
        }
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
        sp::DataType::BigInt(_)
        | sp::DataType::Int8(_)
        | sp::DataType::Int64 => Ok(DataType::BigInt),
        // Numeric / decimal
        sp::DataType::Numeric(info) | sp::DataType::Decimal(info) | sp::DataType::Dec(info) => {
            let (precision, scale) = match info {
                sp::ExactNumberInfo::None => (None, None),
                sp::ExactNumberInfo::Precision(p) => (Some(p), None),
                sp::ExactNumberInfo::PrecisionAndScale(p, s) => (Some(p), Some(s)),
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
        sp::DataType::Text | sp::DataType::MediumText | sp::DataType::LongText => Ok(DataType::Text),
        sp::DataType::Varchar(len) | sp::DataType::CharacterVarying(len) | sp::DataType::CharVarying(len) => {
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
        sp::DataType::Interval => Ok(DataType::Interval),
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
                    // untyped array — fallback
                    return Ok(DataType::Custom("array".to_string()));
                }
                sp::ArrayElemTypeDef::AngleBracket(inner)
                | sp::ArrayElemTypeDef::SquareBracket(inner, _)
                | sp::ArrayElemTypeDef::Parenthesis(inner) => *inner,
            };
            Ok(DataType::Array(Box::new(convert_data_type(inner)?)))
        }
        // Custom / enum / user-defined
        sp::DataType::Custom(name, _) => {
            let s = name
                .0
                .into_iter()
                .map(|id| id.value)
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

fn convert_object_name(name: sp::ObjectName) -> ObjectName {
    ObjectName(name.0.into_iter().map(convert_ident).collect())
}
