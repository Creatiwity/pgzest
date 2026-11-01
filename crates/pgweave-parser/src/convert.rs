//! Conversion layer: sqlparser 0.62 AST → pgweave_ast types.
//!
//! Entry point is [`convert_statement`].

use pgweave_ast::{
    Statement,
    ddl::SettingValue,
    ddl::{
        AlterColumnAction, AlterTable, AlterTableOp, AlterType, AlterTypeOp, ColumnConstraint,
        ColumnDef, CreateIndex, CreatePolicy, CreateSchema, CreateTable, CreateTrigger, CreateType,
        CreateView, DdlStatement, DropObject, EnumValuePosition, GeneratedColumn, Identity,
        IndexElement, ObjectKind, OnCommit, PolicyCommand, ReferentialAction, ReturnType,
        TableConstraint, TransitionTable, TriggerEvent, TriggerForEach, TriggerTiming,
        TypeDefinition,
    },
    expr::{BinaryOp, ColumnRef, Expr, Ident, Literal, ObjectName, UnaryOp},
    node::Span,
    stmt::{
        Assignment, Cte, DeleteStmt, InsertSource, InsertStmt, Join, JoinConstraint, JoinKind,
        OnConflict, OnConflictAction, OnConflictTarget, OrderByExpr, SelectItem, SelectStmt,
        SetOperation, SetOperator, TableRef, TableSource, TargetTable, UpdateStmt,
    },
    types::DataType,
    utility::{
        CommentTarget, CopyOption, CopySource, CopyStatement, CopyTarget, TransactionCommand,
        Utility, UtilityKind,
    },
};

use crate::error::ParseError;
use pgweave_ast::expr::{ExprKind, FrameBound, FrameUnits, Window, WindowFrame, WindowSpec};
use pgweave_ast::stmt::NamedWindow;
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

pub fn convert_statement(stmt: sp::Statement) -> Result<Statement, ParseError> {
    let _ids = crate::ids::session();
    match stmt {
        sp::Statement::Query(q) => convert_query_statement(*q),
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
        // Les routines sont lues par la grammaire de pgweave (`grammar::routine`),
        // jamais par sqlparser : y arriver ici serait une erreur d'aiguillage.
        sp::Statement::CreateFunction(_) | sp::Statement::CreateProcedure { .. } => Err(
            ParseError::Unsupported("routine hors de la grammaire de pgweave".to_string()),
        ),
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
            let ddl = convert_alter_table(at)?;
            Ok(Statement::Ddl(DdlStatement::AlterTable(ddl)))
        }
        sp::Statement::CreateType {
            name,
            representation,
        } => Ok(Statement::Ddl(DdlStatement::CreateType(
            convert_create_type(name, representation)?,
        ))),
        sp::Statement::AlterType(at) => Ok(Statement::Ddl(DdlStatement::AlterType(
            convert_alter_type(at),
        ))),
        sp::Statement::CreateView(cv) => Ok(Statement::Ddl(DdlStatement::CreateView(
            convert_create_view(cv)?,
        ))),
        sp::Statement::CreateSchema {
            schema_name,
            if_not_exists,
            options,
            default_collate_spec,
            clone,
            with,
        } => {
            if options.is_some()
                || default_collate_spec.is_some()
                || clone.is_some()
                || with.is_some()
            {
                return Err(ParseError::Unsupported(
                    "options de CREATE SCHEMA hors PostgreSQL".to_string(),
                ));
            }
            // `CREATE SCHEMA AUTHORIZATION r` : le schéma prend le nom du rôle.
            let (name, authorization) = match schema_name {
                sp::SchemaName::Simple(n) => (last_ident(n)?, None),
                sp::SchemaName::NamedAuthorization(n, owner) => {
                    (last_ident(n)?, Some(convert_ident(owner)))
                }
                sp::SchemaName::UnnamedAuthorization(owner) => {
                    (convert_ident(owner.clone()), Some(convert_ident(owner)))
                }
            };
            Ok(Statement::Ddl(DdlStatement::CreateSchema(CreateSchema {
                name,
                if_not_exists,
                authorization,
            })))
        }
        sp::Statement::Drop {
            object_type,
            if_exists,
            names,
            cascade,
            table,
            ..
        } => {
            let kind = match object_type {
                sp::ObjectType::Table => ObjectKind::Table,
                sp::ObjectType::View => ObjectKind::View,
                sp::ObjectType::MaterializedView => ObjectKind::MaterializedView,
                sp::ObjectType::Index => ObjectKind::Index,
                sp::ObjectType::Schema => ObjectKind::Schema,
                sp::ObjectType::Sequence => ObjectKind::Sequence,
                sp::ObjectType::Type => ObjectKind::Type,
                other => {
                    return Err(ParseError::Unsupported(format!("DROP {other}")));
                }
            };
            Ok(drop(kind, names, if_exists, cascade, table))
        }
        sp::Statement::DropFunction(df) => drop_routines(
            ObjectKind::Function,
            df.func_desc,
            df.if_exists,
            df.drop_behavior,
        ),
        sp::Statement::DropProcedure {
            if_exists,
            proc_desc,
            drop_behavior,
        } => drop_routines(ObjectKind::Procedure, proc_desc, if_exists, drop_behavior),
        sp::Statement::DropTrigger(dt) => Ok(drop(
            ObjectKind::Trigger,
            vec![dt.trigger_name],
            dt.if_exists,
            matches!(dt.option, Some(sp::ReferentialAction::Cascade)),
            dt.table_name,
        )),
        sp::Statement::DropDomain(dd) => Ok(drop(
            ObjectKind::Domain,
            vec![dd.name],
            dd.if_exists,
            matches!(dd.drop_behavior, Some(sp::DropBehavior::Cascade)),
            None,
        )),
        sp::Statement::DropPolicy(dp) => Ok(drop(
            ObjectKind::Policy,
            vec![sp::ObjectName(vec![sp::ObjectNamePart::Identifier(
                dp.name,
            )])],
            dp.if_exists,
            matches!(dp.drop_behavior, Some(sp::DropBehavior::Cascade)),
            Some(dp.table_name),
        )),
        sp::Statement::DropExtension(de) => Ok(drop(
            ObjectKind::Extension,
            de.names
                .into_iter()
                .map(|n| sp::ObjectName(vec![sp::ObjectNamePart::Identifier(n)]))
                .collect(),
            de.if_exists,
            matches!(de.cascade_or_restrict, Some(sp::ReferentialAction::Cascade)),
            None,
        )),
        sp::Statement::Call(f) => utility(UtilityKind::Call {
            name: convert_object_name(f.name),
            args: convert_function_args(f.args)?,
        }),
        sp::Statement::Copy {
            source,
            to,
            target,
            options,
            legacy_options,
            values,
        } => {
            if !values.is_empty() {
                return Err(ParseError::Unsupported(
                    "COPY suivi de données en ligne".to_string(),
                ));
            }
            utility(UtilityKind::Copy(convert_copy(
                source,
                to,
                target,
                options,
                legacy_options,
            )?))
        }
        sp::Statement::Set(set) => utility(convert_set(set)?),
        // Modes de transaction (`ISOLATION LEVEL`, `READ ONLY`…) et `AND
        // CHAIN` : non modélisés, refusés plutôt que perdus.
        sp::Statement::StartTransaction {
            modes, statements, ..
        } if !modes.is_empty() || !statements.is_empty() => Err(ParseError::Unsupported(
            "BEGIN avec modes de transaction".to_string(),
        )),
        sp::Statement::StartTransaction { .. } => {
            utility(UtilityKind::Transaction(TransactionCommand::Begin))
        }
        sp::Statement::Commit { chain: true, .. } | sp::Statement::Rollback { chain: true, .. } => {
            Err(ParseError::Unsupported("AND CHAIN".to_string()))
        }
        sp::Statement::Commit { .. } => {
            utility(UtilityKind::Transaction(TransactionCommand::Commit))
        }
        sp::Statement::Rollback { savepoint, .. } => {
            utility(UtilityKind::Transaction(match savepoint {
                Some(n) => TransactionCommand::RollbackToSavepoint(convert_ident(n)),
                None => TransactionCommand::Rollback,
            }))
        }
        sp::Statement::Savepoint { name } => utility(UtilityKind::Transaction(
            TransactionCommand::Savepoint(convert_ident(name)),
        )),
        sp::Statement::ReleaseSavepoint { name } => utility(UtilityKind::Transaction(
            TransactionCommand::ReleaseSavepoint(convert_ident(name)),
        )),
        sp::Statement::Comment {
            object_type,
            object_name,
            comment,
            ..
        } => {
            let target = match object_type {
                sp::CommentObject::Table => CommentTarget::Table,
                sp::CommentObject::Column => CommentTarget::Column,
                sp::CommentObject::View => CommentTarget::View,
                sp::CommentObject::MaterializedView => CommentTarget::MaterializedView,
                sp::CommentObject::Index => CommentTarget::Index,
                sp::CommentObject::Sequence => CommentTarget::Sequence,
                sp::CommentObject::Type => CommentTarget::Type,
                sp::CommentObject::Domain => CommentTarget::Domain,
                sp::CommentObject::Function => CommentTarget::Function,
                sp::CommentObject::Procedure => CommentTarget::Procedure,
                sp::CommentObject::Schema => CommentTarget::Schema,
                sp::CommentObject::Extension => CommentTarget::Extension,
                other => {
                    return Err(ParseError::Unsupported(format!("COMMENT ON {other}")));
                }
            };
            utility(UtilityKind::Comment {
                target,
                name: convert_object_name(object_name),
                text: comment,
            })
        }
        sp::Statement::Grant(g) => utility(UtilityKind::Privilege {
            grant: true,
            privileges: g.privileges.to_string(),
            objects: g
                .objects
                .map(|o| o.to_string())
                .ok_or_else(|| ParseError::Unsupported("GRANT de rôle".to_string()))?,
            grantees: g.grantees.iter().map(|x| x.to_string()).collect(),
        }),
        sp::Statement::Revoke(r) => utility(UtilityKind::Privilege {
            grant: false,
            privileges: r.privileges.to_string(),
            objects: r
                .objects
                .map(|o| o.to_string())
                .ok_or_else(|| ParseError::Unsupported("REVOKE de rôle".to_string()))?,
            grantees: r.grantees.iter().map(|x| x.to_string()).collect(),
        }),
        sp::Statement::Truncate(t) => utility(UtilityKind::Truncate {
            tables: t
                .table_names
                .into_iter()
                .map(|t| convert_object_name(t.name))
                .collect(),
            restart_identity: matches!(t.identity, Some(sp::TruncateIdentityOption::Restart)),
            cascade: matches!(t.cascade, Some(sp::CascadeOption::Cascade)),
        }),
        other => Err(ParseError::Unsupported(format!(
            "unsupported statement: {}",
            other
        ))),
    }
}

// ── Statements utilitaires ────────────────────────────────────────────────────

fn utility(kind: UtilityKind) -> Result<Statement, ParseError> {
    Ok(Statement::Utility(Utility {
        span: Span::DUMMY,
        kind,
    }))
}

fn convert_copy(
    source: sp::CopySource,
    to: bool,
    target: sp::CopyTarget,
    options: Vec<sp::CopyOption>,
    legacy: Vec<sp::CopyLegacyOption>,
) -> Result<CopyStatement, ParseError> {
    let source = match source {
        sp::CopySource::Table {
            table_name,
            columns,
        } => CopySource::Table {
            name: convert_object_name(table_name),
            columns: columns.into_iter().map(convert_ident).collect(),
        },
        sp::CopySource::Query(q) => CopySource::Query(Box::new(convert_query(*q)?)),
    };
    let target = match target {
        sp::CopyTarget::Stdin => CopyTarget::Stdin,
        sp::CopyTarget::Stdout => CopyTarget::Stdout,
        sp::CopyTarget::File { filename } => CopyTarget::File(filename),
        sp::CopyTarget::Program { command } => CopyTarget::Program(command),
    };
    let ids = |v: Vec<sp::Ident>| v.into_iter().map(convert_ident).collect::<Vec<_>>();
    let mut out: Vec<CopyOption> = options
        .into_iter()
        .map(|o| match o {
            sp::CopyOption::Format(f) => CopyOption::Format(f.value.to_lowercase()),
            sp::CopyOption::Freeze(b) => CopyOption::Freeze(b),
            sp::CopyOption::Delimiter(c) => CopyOption::Delimiter(c),
            sp::CopyOption::Null(n) => CopyOption::Null(n),
            sp::CopyOption::Header(b) => CopyOption::Header(b),
            sp::CopyOption::Quote(c) => CopyOption::Quote(c),
            sp::CopyOption::Escape(c) => CopyOption::Escape(c),
            sp::CopyOption::ForceQuote(v) => CopyOption::ForceQuote(ids(v)),
            sp::CopyOption::ForceNotNull(v) => CopyOption::ForceNotNull(ids(v)),
            sp::CopyOption::ForceNull(v) => CopyOption::ForceNull(ids(v)),
            sp::CopyOption::Encoding(e) => CopyOption::Encoding(e),
        })
        .collect();
    // Ancienne syntaxe `WITH CSV HEADER`, encore très répandue.
    for o in legacy {
        match o {
            sp::CopyLegacyOption::Binary => out.push(CopyOption::Format("binary".into())),
            sp::CopyLegacyOption::Delimiter(c) => out.push(CopyOption::Delimiter(c)),
            sp::CopyLegacyOption::Null(n) => out.push(CopyOption::Null(n)),
            sp::CopyLegacyOption::Header => out.push(CopyOption::Header(true)),
            sp::CopyLegacyOption::Csv(csv) => {
                out.push(CopyOption::Format("csv".into()));
                for c in csv {
                    out.push(match c {
                        sp::CopyLegacyCsvOption::Header => CopyOption::Header(true),
                        sp::CopyLegacyCsvOption::Quote(q) => CopyOption::Quote(q),
                        sp::CopyLegacyCsvOption::Escape(e) => CopyOption::Escape(e),
                        sp::CopyLegacyCsvOption::ForceQuote(v) => CopyOption::ForceQuote(ids(v)),
                        sp::CopyLegacyCsvOption::ForceNotNull(v) => {
                            CopyOption::ForceNotNull(ids(v))
                        }
                    });
                }
            }
            other => {
                return Err(ParseError::Unsupported(format!(
                    "option COPY non PostgreSQL : {other}"
                )));
            }
        }
    }
    Ok(CopyStatement {
        source,
        to,
        target,
        options: out,
    })
}

/// Valeur d'un paramètre telle qu'écrite : chaînes sans délimiteurs,
/// identifiants et nombres tels quels.
fn setting_value(e: &sp::Expr) -> String {
    match e {
        sp::Expr::Value(v) => match &v.value {
            sp::Value::SingleQuotedString(s)
            | sp::Value::DollarQuotedString(sp::DollarQuotedString { value: s, .. }) => s.clone(),
            other => other.to_string(),
        },
        sp::Expr::Identifier(id) => id.value.clone(),
        other => other.to_string(),
    }
}

fn convert_set(set: sp::Set) -> Result<UtilityKind, ParseError> {
    match set {
        sp::Set::SingleAssignment {
            scope,
            variable,
            values,
            ..
        } => {
            let name = variable.to_string();
            let is_default = matches!(values.as_slice(), [sp::Expr::Identifier(id)] if id.value.eq_ignore_ascii_case("default"));
            Ok(UtilityKind::Set {
                local: matches!(scope, Some(sp::ContextModifier::Local)),
                name,
                value: SettingValue::Values(if is_default {
                    Vec::new()
                } else {
                    values.iter().map(setting_value).collect()
                }),
            })
        }
        sp::Set::SetTimeZone { local, value } => Ok(UtilityKind::Set {
            local,
            name: "timezone".into(),
            value: SettingValue::Values(vec![setting_value(&value)]),
        }),
        other => Err(ParseError::Unsupported(format!("{other}"))),
    }
}

// ── Query / SELECT ────────────────────────────────────────────────────────────

/// Statement de premier niveau issu d'une requête : un `SELECT`, ou un
/// `INSERT`/`UPDATE`/`DELETE` précédé d'un `WITH` (sqlparser les range sous
/// `Query`).
fn convert_query_statement(query: sp::Query) -> Result<Statement, ParseError> {
    let dml = matches!(
        &*query.body,
        sp::SetExpr::Insert(_) | sp::SetExpr::Update(_) | sp::SetExpr::Delete(_)
    );
    if !dml {
        return Ok(Statement::Select(convert_query(query)?));
    }
    if query.order_by.is_some() || query.limit_clause.is_some() {
        return Err(ParseError::Unsupported(
            "ORDER BY / LIMIT après un statement de modification".into(),
        ));
    }
    let (with, with_recursive) = match query.with {
        Some(w) => (
            w.cte_tables
                .into_iter()
                .map(convert_cte)
                .collect::<Result<Vec<_>, _>>()?,
            w.recursive,
        ),
        None => (vec![], false),
    };
    let inner = match *query.body {
        sp::SetExpr::Insert(s) | sp::SetExpr::Update(s) | sp::SetExpr::Delete(s) => {
            convert_statement(s)?
        }
        _ => unreachable!("vérifié plus haut"),
    };
    Ok(match inner {
        Statement::Insert(mut s) => {
            s.with = with;
            s.with_recursive = with_recursive;
            Statement::Insert(s)
        }
        Statement::Update(mut s) => {
            s.with = with;
            s.with_recursive = with_recursive;
            Statement::Update(s)
        }
        Statement::Delete(mut s) => {
            s.with = with;
            s.with_recursive = with_recursive;
            Statement::Delete(s)
        }
        _ => unreachable!("un corps INSERT/UPDATE/DELETE donne un statement de modification"),
    })
}

pub(crate) fn convert_query(query: sp::Query) -> Result<SelectStmt, ParseError> {
    // WITH clause
    let (with_ctes, with_recursive) = if let Some(with) = query.with {
        (
            with.cte_tables
                .into_iter()
                .map(convert_cte)
                .collect::<Result<Vec<_>, _>>()?,
            with.recursive,
        )
    } else {
        (vec![], false)
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

    let mut stmt = convert_set_expr(*query.body)?;
    stmt.with = with_ctes;
    stmt.with_recursive = with_recursive;
    stmt.order_by = order_by;
    stmt.limit = limit;
    stmt.offset = offset;
    Ok(stmt)
}

/// Corps de requête : SELECT simple, ou opérations ensemblistes mises à plat
/// de gauche à droite (voir `SelectStmt::set_ops`).
fn convert_set_expr(body: sp::SetExpr) -> Result<SelectStmt, ParseError> {
    match body {
        sp::SetExpr::Select(select) => convert_select(*select),
        sp::SetExpr::Query(q) => convert_query(*q),
        sp::SetExpr::SetOperation {
            op,
            set_quantifier,
            left,
            right,
        } => {
            let mut stmt = convert_set_expr(*left)?;
            let op = match op {
                sp::SetOperator::Union => SetOperator::Union,
                sp::SetOperator::Intersect => SetOperator::Intersect,
                sp::SetOperator::Except => SetOperator::Except,
                other => {
                    return Err(ParseError::Unsupported(format!(
                        "opérateur ensembliste {other}"
                    )));
                }
            };
            let all = match set_quantifier {
                sp::SetQuantifier::All => true,
                sp::SetQuantifier::None | sp::SetQuantifier::Distinct => false,
                other => {
                    return Err(ParseError::Unsupported(format!(
                        "quantificateur ensembliste {other}"
                    )));
                }
            };
            stmt.set_ops.push(SetOperation {
                op,
                all,
                select: convert_set_expr(*right)?,
            });
            Ok(stmt)
        }
        sp::SetExpr::Values(v) => {
            let values = v
                .rows
                .into_iter()
                .map(|row| {
                    row.content
                        .into_iter()
                        .map(convert_expr)
                        .collect::<Result<Vec<_>, _>>()
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(SelectStmt {
                id: crate::next_node_id(),
                span: Span::DUMMY,
                with: vec![],
                with_recursive: false,
                distinct: false,
                distinct_on: vec![],
                columns: vec![],
                from: vec![],
                joins: vec![],
                where_clause: None,
                group_by: vec![],
                having: None,
                windows: vec![],
                order_by: vec![],
                limit: None,
                offset: None,
                set_ops: vec![],
                values,
            })
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
    let distinct_on = match select.distinct {
        Some(sp::Distinct::On(exprs)) => exprs
            .into_iter()
            .map(convert_expr)
            .collect::<Result<Vec<_>, _>>()?,
        _ => Vec::new(),
    };

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

    if select.into.is_some() {
        return Err(ParseError::Unsupported(
            "SELECT … INTO table (créer une table : CREATE TABLE … AS)".to_string(),
        ));
    }
    let windows = select
        .named_window
        .into_iter()
        .map(|sp::NamedWindowDefinition(name, def)| {
            let spec = match def {
                sp::NamedWindowExpr::WindowSpec(spec) => convert_window_spec(spec)?,
                sp::NamedWindowExpr::NamedWindow(base) => WindowSpec {
                    base: Some(convert_ident(base)),
                    partition_by: vec![],
                    order_by: vec![],
                    frame: None,
                },
            };
            Ok(NamedWindow {
                name: convert_ident(name),
                spec,
            })
        })
        .collect::<Result<Vec<_>, ParseError>>()?;

    Ok(SelectStmt {
        id: crate::next_node_id(),
        span: Span::DUMMY,
        with: vec![],
        with_recursive: false,
        distinct,
        distinct_on,
        columns,
        from: from_tables,
        joins,
        where_clause,
        group_by,
        having,
        windows,
        order_by: vec![],
        limit: None,
        offset: None,
        set_ops: vec![],
        values: vec![],
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
    let aliases = |alias: Option<sp::TableAlias>| -> (Option<Ident>, Vec<Ident>) {
        match alias {
            Some(a) => (
                Some(convert_ident(a.name)),
                a.columns
                    .into_iter()
                    .map(|c| convert_ident(c.name))
                    .collect(),
            ),
            None => (None, Vec::new()),
        }
    };
    if let sp::TableFactor::Table {
        with_hints,
        version,
        partitions,
        json_path,
        sample,
        index_hints,
        ..
    } = &factor
        && (!with_hints.is_empty()
            || version.is_some()
            || !partitions.is_empty()
            || json_path.is_some()
            || sample.is_some()
            || !index_hints.is_empty())
    {
        return Err(ParseError::Unsupported(format!(
            "option de table non modélisée (TABLESAMPLE…) : {factor}"
        )));
    }
    let (source, alias) = match factor {
        // `FROM f(…)` : sqlparser le range dans Table, avec des arguments.
        sp::TableFactor::Table {
            name,
            alias,
            args: Some(args),
            with_ordinality,
            ..
        } => {
            let args = args
                .args
                .into_iter()
                .map(convert_function_arg_expr)
                .collect::<Result<Vec<_>, _>>()?;
            (
                TableSource::Function {
                    lateral: false,
                    name: convert_object_name(name),
                    args,
                    with_ordinality,
                },
                alias,
            )
        }
        sp::TableFactor::Table { name, alias, .. } => {
            (TableSource::Table(convert_object_name(name)), alias)
        }
        sp::TableFactor::Derived {
            lateral,
            subquery,
            alias,
            ..
        } => (
            TableSource::Subquery {
                lateral,
                query: Box::new(convert_query(*subquery)?),
            },
            alias,
        ),
        sp::TableFactor::Function {
            lateral,
            name,
            args,
            alias,
            with_ordinality,
        } => (
            TableSource::Function {
                lateral,
                name: convert_object_name(name),
                args: args
                    .into_iter()
                    .map(convert_function_arg_expr)
                    .collect::<Result<Vec<_>, _>>()?,
                with_ordinality,
            },
            alias,
        ),
        sp::TableFactor::UNNEST {
            alias,
            array_exprs,
            with_ordinality,
            ..
        } => (
            TableSource::Function {
                lateral: false,
                name: ObjectName(vec![Ident::new("unnest")]),
                args: array_exprs
                    .into_iter()
                    .map(convert_expr)
                    .collect::<Result<Vec<_>, _>>()?,
                with_ordinality,
            },
            alias,
        ),
        other => {
            return Err(ParseError::Unsupported(format!(
                "unsupported table factor: {}",
                other
            )));
        }
    };
    let (alias, column_aliases) = aliases(alias);
    Ok(TableRef {
        source,
        alias,
        column_aliases,
    })
}

/// Argument d'une fonction en `FROM` : expression, éventuellement nommée.
fn convert_function_arg_expr(arg: sp::FunctionArg) -> Result<Expr, ParseError> {
    match arg {
        sp::FunctionArg::Unnamed(sp::FunctionArgExpr::Expr(e)) => convert_expr(e),
        sp::FunctionArg::Named {
            name,
            arg: sp::FunctionArgExpr::Expr(e),
            ..
        }
        | sp::FunctionArg::ExprNamed {
            name: sp::Expr::Identifier(name),
            arg: sp::FunctionArgExpr::Expr(e),
            ..
        } => convert_named_arg(name, e),
        other => Err(ParseError::Unsupported(format!(
            "argument de fonction en FROM : {other}"
        ))),
    }
}

/// `nom => valeur` (ou l'ancienne forme `nom := valeur`).
fn convert_named_arg(name: sp::Ident, value: sp::Expr) -> Result<Expr, ParseError> {
    use sqlparser::ast::Spanned;
    let sp_span = name.span.union(&value.span());
    let span = SPAN_CTX.with(|c| {
        c.borrow()
            .as_ref()
            .map(|ctx| ctx.resolve(sp_span))
            .unwrap_or(Span::DUMMY)
    });
    Ok(Expr::new(
        ExprKind::NamedArgument {
            name: convert_ident(name),
            value: Box::new(convert_expr(value)?),
        },
        span,
    ))
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
        sp::JoinOperator::CrossJoin(_) => (JoinKind::Cross, JoinConstraint::None),
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
pub(crate) fn convert_expr(expr: sp::Expr) -> Result<Expr, ParseError> {
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
                sp::UnaryOperator::Plus => UnaryOp::Prefix("+".into()),
                sp::UnaryOperator::PGAbs => UnaryOp::Prefix("@".into()),
                sp::UnaryOperator::BitwiseNot => UnaryOp::Prefix("~".into()),
                sp::UnaryOperator::PGSquareRoot => UnaryOp::Prefix("|/".into()),
                sp::UnaryOperator::PGCubeRoot => UnaryOp::Prefix("||/".into()),
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
        sp::Expr::Function(f) => convert_function_call(f),
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
        sp::Expr::Like { any: true, .. }
        | sp::Expr::ILike { any: true, .. }
        | sp::Expr::Like {
            escape_char: Some(_),
            ..
        }
        | sp::Expr::ILike {
            escape_char: Some(_),
            ..
        } => Err(ParseError::Unsupported(
            "LIKE … ESCAPE / LIKE ANY non modélisés".to_string(),
        )),
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
            op: BinaryOp::IsDistinctFrom,
            right: Box::new(convert_expr(*r)?),
        }),
        sp::Expr::IsNotDistinctFrom(l, r) => Ok(ExprKind::BinaryOp {
            left: Box::new(convert_expr(*l)?),
            op: BinaryOp::IsNotDistinctFrom,
            right: Box::new(convert_expr(*r)?),
        }),
        // `(expr)` : on garde l'expression interne, dont le span est déjà juste.
        sp::Expr::Nested(e) => Ok(convert_expr(*e)?.kind),

        // `expr op ANY (tableau)` / `ALL (…)` : comparaison à chaque élément,
        // modélisée comme telle — la réduire à `expr op tableau` mentirait sur
        // le sens et sur les types.
        sp::Expr::AnyOp {
            left,
            compare_op,
            right,
            ..
        } => Ok(ExprKind::Quantified {
            left: Box::new(convert_expr(*left)?),
            op: convert_binary_op(compare_op)?,
            quantifier: pgweave_ast::expr::Quantifier::Any,
            right: Box::new(convert_expr(*right)?),
        }),
        sp::Expr::AllOp {
            left,
            compare_op,
            right,
        } => Ok(ExprKind::Quantified {
            left: Box::new(convert_expr(*left)?),
            op: convert_binary_op(compare_op)?,
            quantifier: pgweave_ast::expr::Quantifier::All,
            right: Box::new(convert_expr(*right)?),
        }),
        // `ARRAY[e1, e2, …]` : constructeur de tableau, représenté comme un
        // appel à `array` (vide pour `ARRAY[]`). Un élément illisible fait
        // échouer l'expression entière plutôt que de disparaître.
        sp::Expr::Array(arr) => Ok(ExprKind::FunctionCall {
            name: pgweave_ast::ObjectName(vec![pgweave_ast::Ident {
                value: "array".into(),
                quoted: false,
            }]),
            args: arr
                .elem
                .into_iter()
                .map(convert_expr)
                .collect::<Result<Vec<_>, _>>()?,
            order_by: vec![],
            filter: None,
            distinct: false,
            within_group: vec![],
            over: None,
        }),

        // `DATE '2025-01-01'` : littéral typé, équivalent PostgreSQL de
        // `'2025-01-01'::date`. Le type est conservé — le réduire au littéral
        // seul en ferait une chaîne de type inconnu.
        sp::Expr::TypedString(ts) => {
            let data_type = convert_data_type(ts.data_type)?;
            let literal = match ts.value.into_string() {
                Some(v) => Literal::String(v),
                None => {
                    return Ok(ExprKind::Unsupported("littéral typé non textuel".into()));
                }
            };
            Ok(ExprKind::Cast {
                expr: Box::new(Expr::bare(ExprKind::Literal(literal))),
                data_type,
            })
        }

        // ── Constructions PostgreSQL courantes ────────────────────────────────
        sp::Expr::Exists { subquery, negated } => Ok(ExprKind::Exists {
            subquery: Box::new(convert_query(*subquery)?),
            negated,
        }),

        sp::Expr::Interval(iv) => Ok(ExprKind::Interval(iv.to_string())),

        sp::Expr::AtTimeZone {
            timestamp,
            time_zone,
        } => Ok(ExprKind::AtTimeZone {
            timestamp: Box::new(convert_expr(*timestamp)?),
            time_zone: Box::new(convert_expr(*time_zone)?),
        }),

        sp::Expr::Collate { expr, collation } => Ok(ExprKind::Collate {
            expr: Box::new(convert_expr(*expr)?),
            collation: object_name_to_plain_string(&collation),
        }),

        sp::Expr::Tuple(items) => Ok(ExprKind::Tuple(
            items
                .into_iter()
                .map(convert_expr)
                .collect::<Result<Vec<_>, _>>()?,
        )),

        // `arr[1]`, `(rec).champ`, `payload['clé']`.
        sp::Expr::CompoundFieldAccess { root, access_chain } => Ok(ExprKind::FieldAccess {
            root: Box::new(convert_expr(*root)?),
            path: access_chain.iter().map(|a| a.to_string()).collect(),
        }),

        // Fonctions déguisées en syntaxe : on les ramène à `FunctionCall` plutôt
        // que d'ajouter une variante par mot-clé. Le nom suffit à l'inférence.
        sp::Expr::Extract { field, expr, .. } => Ok(ExprKind::FunctionCall {
            name: ObjectName::unquoted(&["extract"]),
            args: vec![
                Expr::bare(ExprKind::Literal(Literal::String(field.to_string()))),
                convert_expr(*expr)?,
            ],
            filter: None,
            distinct: false,
            order_by: vec![],
            within_group: vec![],
            over: None,
        }),

        // Syntaxes SQL ramenées par la grammaire de PostgreSQL à des appels
        // de fonctions système (`gram.y`, `SystemFuncName`).
        sp::Expr::Position { expr, r#in } => {
            system_call("position", vec![convert_expr(*r#in)?, convert_expr(*expr)?])
        }
        // `substr(x, a, b)` : lu par sqlparser comme `SUBSTRING` abrégé, mais
        // c'est un appel ordinaire à la fonction `substr`.
        sp::Expr::Substring {
            expr,
            substring_from,
            substring_for,
            shorthand: true,
            ..
        } => {
            let mut args = vec![convert_expr(*expr)?];
            for a in substring_from.into_iter().chain(substring_for) {
                args.push(convert_expr(*a)?);
            }
            Ok(ExprKind::FunctionCall {
                name: ObjectName::unquoted(&["substr"]),
                args,
                order_by: vec![],
                filter: None,
                distinct: false,
                within_group: vec![],
                over: None,
            })
        }
        sp::Expr::Substring {
            expr,
            substring_from,
            substring_for,
            ..
        } => {
            let mut args = vec![convert_expr(*expr)?];
            match (substring_from, substring_for) {
                (Some(from), Some(len)) => {
                    args.push(convert_expr(*from)?);
                    args.push(convert_expr(*len)?);
                }
                (Some(from), None) => args.push(convert_expr(*from)?),
                // `SUBSTRING(x FOR n)` : à partir du premier caractère.
                (None, Some(len)) => {
                    args.push(Expr::bare(ExprKind::Literal(Literal::Number("1".into()))));
                    args.push(convert_expr(*len)?);
                }
                (None, None) => {}
            }
            system_call("substring", args)
        }
        sp::Expr::Trim {
            expr,
            trim_where,
            trim_what,
            trim_characters: None,
        } => {
            let name = match trim_where {
                Some(sp::TrimWhereField::Leading) => "ltrim",
                Some(sp::TrimWhereField::Trailing) => "rtrim",
                Some(sp::TrimWhereField::Both) | None => "btrim",
            };
            let mut args = vec![convert_expr(*expr)?];
            if let Some(what) = trim_what {
                args.push(convert_expr(*what)?);
            }
            system_call(name, args)
        }
        sp::Expr::Overlay {
            expr,
            overlay_what,
            overlay_from,
            overlay_for,
        } => {
            let mut args = vec![
                convert_expr(*expr)?,
                convert_expr(*overlay_what)?,
                convert_expr(*overlay_from)?,
            ];
            if let Some(len) = overlay_for {
                args.push(convert_expr(*len)?);
            }
            system_call("overlay", args)
        }
        sp::Expr::Ceil {
            expr,
            field: sp::CeilFloorKind::DateTimeField(sp::DateTimeField::NoDateTime),
        } => system_call("ceil", vec![convert_expr(*expr)?]),
        sp::Expr::Floor {
            expr,
            field: sp::CeilFloorKind::DateTimeField(sp::DateTimeField::NoDateTime),
        } => system_call("floor", vec![convert_expr(*expr)?]),

        _other => Ok(ExprKind::Unsupported(describe_sp_expr(&_other))),
    }
}

/// Appel à `pg_catalog.name(args)`, forme que PostgreSQL donne aux syntaxes
/// SQL `POSITION`, `SUBSTRING`, `TRIM`, `OVERLAY`.
fn system_call(name: &str, args: Vec<Expr>) -> Result<ExprKind, ParseError> {
    Ok(ExprKind::FunctionCall {
        name: ObjectName::unquoted(&["pg_catalog", name]),
        args,
        order_by: vec![],
        filter: None,
        distinct: false,
        within_group: vec![],
        over: None,
    })
}

/// Nom court de la construction sqlparser, pour un diagnostic lisible.
///
/// Le `Debug` complet d'une expression est illisible dans un message ; on n'en
/// garde que le nom de variante.
fn describe_sp_expr(e: &sp::Expr) -> String {
    let dbg = format!("{e:?}");
    let name: String = dbg
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    if name.is_empty() {
        "expression".to_string()
    } else {
        name
    }
}

/// `ObjectName` sqlparser → chaîne pointée, sans passer par notre `ObjectName`.
fn object_name_to_plain_string(name: &sp::ObjectName) -> String {
    name.0
        .iter()
        .filter_map(part_to_ident_ref)
        .map(|i| i.value.clone())
        .collect::<Vec<_>>()
        .join(".")
}

fn part_to_ident_ref(part: &sp::ObjectNamePart) -> Option<&sp::Ident> {
    match part {
        sp::ObjectNamePart::Identifier(i) => Some(i),
        _ => None,
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
        // `$1`, `:nom` : le nœud existe désormais. Auparavant ce bras écartait le
        // statement entier, donc aucune requête paramétrée d'application ne
        // passait — alors que c'est la forme normale du SQL applicatif.
        sp::Value::Placeholder(p) => Ok(ExprKind::Parameter(p)),

        // Littéral d'une forme que nous ne modélisons pas (chaînes nationales,
        // hexadécimales, Unicode…). On ne prétend pas connaître sa valeur, mais
        // on ne détruit pas le statement pour autant.
        other => Ok(ExprKind::Unsupported(format!("literal {other}"))),
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
        // `||` : opérateur non standard le plus fréquent, et de type connu.
        sp::BinaryOperator::StringConcat => Ok(BinaryOp::Concat),
        // Generic custom / user-defined operators
        sp::BinaryOperator::Custom(s) => Ok(BinaryOp::Custom(s)),
        sp::BinaryOperator::PGCustomBinaryOperator(parts) => Ok(BinaryOp::Custom(parts.join("."))),

        // Tout autre opérateur connu de sqlparser se range en `Custom`, avec son
        // symbole obtenu de son propre `Display`.
        //
        // Auparavant ce bras renvoyait `Unsupported`, ce qui écartait le
        // statement entier — y compris pour des opérateurs banals que sqlparser
        // sait parfaitement lire. `BinaryOp::Custom` existait déjà pour PostGIS
        // et JSONB ; il suffisait de s'en servir jusqu'au bout.
        other => Ok(BinaryOp::Custom(other.to_string())),
    }
}

/// Appel de fonction, avec tout ce qui l'accompagne : `DISTINCT`, `ORDER BY`
/// d'agrégat, `FILTER`, `WITHIN GROUP`, `OVER`. Ce que PostgreSQL ne connaît
/// pas (syntaxes d'autres dialectes) est refusé, jamais ignoré.
fn convert_function_call(f: sp::Function) -> Result<ExprKind, ParseError> {
    if !matches!(f.parameters, sp::FunctionArguments::None) || f.null_treatment.is_some() {
        return Err(ParseError::Unsupported(format!(
            "appel de fonction hors PostgreSQL : {f}"
        )));
    }
    let call_span = sqlparser::ast::Spanned::span(&f);
    let name = convert_object_name(f.name);
    let (distinct, order_by) = match &f.args {
        sp::FunctionArguments::List(list) => {
            let distinct = matches!(
                list.duplicate_treatment,
                Some(sp::DuplicateTreatment::Distinct)
            );
            let mut order_by = Vec::new();
            for clause in &list.clauses {
                match clause {
                    sp::FunctionArgumentClause::OrderBy(items) => {
                        for o in items {
                            order_by.push(convert_order_by_expr(o.clone())?);
                        }
                    }
                    other => {
                        return Err(ParseError::Unsupported(format!(
                            "clause d'appel de fonction hors PostgreSQL : {other}"
                        )));
                    }
                }
            }
            (distinct, order_by)
        }
        _ => (false, Vec::new()),
    };
    let mut args = convert_function_args(f.args)?;
    for a in &mut args {
        if matches!(a.kind, ExprKind::Wildcard) && a.span == Span::DUMMY {
            a.span = SPAN_CTX.with(|c| {
                c.borrow()
                    .as_ref()
                    .map(|ctx| ctx.star_in_call(call_span))
                    .unwrap_or(Span::DUMMY)
            });
        }
    }
    let filter = f
        .filter
        .map(|e| convert_expr(*e).map(Box::new))
        .transpose()?;
    let within_group = f
        .within_group
        .into_iter()
        .map(convert_order_by_expr)
        .collect::<Result<Vec<_>, _>>()?;
    let over = f
        .over
        .map(|w| match w {
            sp::WindowType::NamedWindow(n) => Ok(Window::Named(convert_ident(n))),
            sp::WindowType::WindowSpec(spec) => convert_window_spec(spec).map(Window::Spec),
        })
        .transpose()?
        .map(Box::new);
    Ok(ExprKind::FunctionCall {
        name,
        args,
        order_by,
        filter,
        distinct,
        within_group,
        over,
    })
}

pub(crate) fn convert_window_spec(spec: sp::WindowSpec) -> Result<WindowSpec, ParseError> {
    let bound = |b: sp::WindowFrameBound| -> Result<FrameBound, ParseError> {
        Ok(match b {
            sp::WindowFrameBound::CurrentRow => FrameBound::CurrentRow,
            sp::WindowFrameBound::Preceding(None) => FrameBound::UnboundedPreceding,
            sp::WindowFrameBound::Preceding(Some(e)) => {
                FrameBound::Preceding(Box::new(convert_expr(*e)?))
            }
            sp::WindowFrameBound::Following(None) => FrameBound::UnboundedFollowing,
            sp::WindowFrameBound::Following(Some(e)) => {
                FrameBound::Following(Box::new(convert_expr(*e)?))
            }
        })
    };
    let frame = spec
        .window_frame
        .map(|f| -> Result<WindowFrame, ParseError> {
            Ok(WindowFrame {
                units: match f.units {
                    sp::WindowFrameUnits::Rows => FrameUnits::Rows,
                    sp::WindowFrameUnits::Range => FrameUnits::Range,
                    sp::WindowFrameUnits::Groups => FrameUnits::Groups,
                },
                start: bound(f.start_bound)?,
                end: f.end_bound.map(bound).transpose()?,
            })
        })
        .transpose()?;
    Ok(WindowSpec {
        base: spec.window_name.map(convert_ident),
        partition_by: spec
            .partition_by
            .into_iter()
            .map(convert_expr)
            .collect::<Result<Vec<_>, _>>()?,
        order_by: spec
            .order_by
            .into_iter()
            .map(convert_order_by_expr)
            .collect::<Result<Vec<_>, _>>()?,
        frame,
    })
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
                sp::FunctionArg::Named {
                    name,
                    arg: sp::FunctionArgExpr::Expr(e),
                    ..
                }
                | sp::FunctionArg::ExprNamed {
                    name: sp::Expr::Identifier(name),
                    arg: sp::FunctionArgExpr::Expr(e),
                    ..
                } => convert_named_arg(name, e),
                other => Err(ParseError::Unsupported(format!(
                    "argument de fonction hors PostgreSQL : {other}"
                ))),
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
        let plain_values = q.with.is_none()
            && q.order_by.is_none()
            && q.limit_clause.is_none()
            && matches!(*q.body, sp::SetExpr::Values(_));
        if !plain_values && matches!(*q.body, sp::SetExpr::Values(_)) {
            return Err(ParseError::Unsupported(
                "VALUES avec WITH / ORDER BY / LIMIT dans un INSERT".to_string(),
            ));
        }
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
        // Pas de source : `INSERT INTO t DEFAULT VALUES`.
        InsertSource::DefaultValues
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
        with: vec![],
        with_recursive: false,
        table,
        alias: ins.table_alias.map(|a| convert_ident(a.alias)),
        columns,
        source,
        on_conflict,
        returning,
        returning_aliases: Default::default(),
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
    // Cible de `SET` : la colonne, puis les champs d'un composite
    // (`home.city`) ; jamais un nom de table.
    let path = |name: sp::ObjectName| -> Result<Vec<Ident>, ParseError> {
        let parts: Vec<Ident> = name
            .0
            .into_iter()
            .filter_map(part_to_ident)
            .map(convert_ident)
            .collect();
        if parts.is_empty() {
            return Err(ParseError::Unsupported(
                "empty assignment target".to_string(),
            ));
        }
        Ok(parts)
    };
    let (columns, subfields) = match a.target {
        sp::AssignmentTarget::ColumnName(name) => {
            let mut parts = path(name)?;
            let rest = parts.split_off(1);
            (parts, rest)
        }
        sp::AssignmentTarget::Tuple(names) => {
            let mut cols = Vec::new();
            for n in names {
                let parts = path(n)?;
                if parts.len() > 1 {
                    return Err(ParseError::Unsupported(
                        "champ de composite dans une affectation multi-colonnes".to_string(),
                    ));
                }
                cols.extend(parts);
            }
            (cols, Vec::new())
        }
    };
    Ok(Assignment {
        columns,
        subfields,
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
    if !table.joins.is_empty() {
        return Err(ParseError::Unsupported(
            "UPDATE sur une jointure".to_string(),
        ));
    }
    let tref = convert_target(table.relation)?;

    // In 0.62, BeforeSet/AfterSet wrap Vec<TableWithJoins>, not a single one
    let (from_tables, joins) = match from {
        None => (vec![], vec![]),
        Some(sp::UpdateTableFromKind::BeforeSet(tables))
        | Some(sp::UpdateTableFromKind::AfterSet(tables)) => convert_tables_with_joins(tables)?,
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
        with: vec![],
        with_recursive: false,
        table: tref,
        assignments: assigns,
        from: from_tables,
        joins,
        where_clause,
        returning: ret,
        returning_aliases: Default::default(),
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
            convert_target(tables.remove(0).relation)?
        }
    };

    let (using, joins) = convert_tables_with_joins(del.using.unwrap_or_default())?;

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
        with: vec![],
        with_recursive: false,
        table,
        using,
        joins,
        where_clause,
        returning,
        returning_aliases: Default::default(),
    })
}

/// Cible d'UPDATE / DELETE : une table nommée, éventuellement aliasée.
fn convert_target(factor: sp::TableFactor) -> Result<TargetTable, ParseError> {
    match factor {
        sp::TableFactor::Table {
            name,
            alias,
            args: None,
            ..
        } => Ok(TargetTable {
            name: convert_object_name(name),
            alias: alias.map(|a| convert_ident(a.name)),
        }),
        other => Err(ParseError::Unsupported(format!(
            "cible d'UPDATE/DELETE : {other}"
        ))),
    }
}

/// Liste `FROM` / `USING` : sources et jointures, sans rien perdre.
fn convert_tables_with_joins(
    tables: Vec<sp::TableWithJoins>,
) -> Result<(Vec<TableRef>, Vec<Join>), ParseError> {
    let mut sources = Vec::new();
    let mut joins = Vec::new();
    for twj in tables {
        sources.push(convert_table_factor(twj.relation)?);
        for j in twj.joins {
            joins.push(convert_join(j)?);
        }
    }
    Ok((sources, joins))
}

// ── Types, vues, DROP ─────────────────────────────────────────────────────────

fn last_ident(name: sp::ObjectName) -> Result<Ident, ParseError> {
    name.0
        .into_iter()
        .filter_map(part_to_ident)
        .next_back()
        .map(convert_ident)
        .ok_or_else(|| ParseError::Unsupported("nom vide".to_string()))
}

fn drop(
    kind: ObjectKind,
    names: Vec<sp::ObjectName>,
    if_exists: bool,
    cascade: bool,
    on_table: Option<sp::ObjectName>,
) -> Statement {
    Statement::Ddl(DdlStatement::Drop(DropObject {
        kind,
        names: names.into_iter().map(convert_object_name).collect(),
        if_exists,
        cascade,
        on_table: on_table.map(convert_object_name),
        signatures: Vec::new(),
    }))
}

/// `DROP FUNCTION/PROCEDURE f(types), …` : noms et signatures d'entrée.
fn drop_routines(
    kind: ObjectKind,
    descs: Vec<sp::FunctionDesc>,
    if_exists: bool,
    drop_behavior: Option<sp::DropBehavior>,
) -> Result<Statement, ParseError> {
    let mut names = Vec::new();
    let mut signatures = Vec::new();
    for d in descs {
        names.push(convert_object_name(d.name));
        signatures.push(
            d.args
                .map(|args| {
                    args.into_iter()
                        .filter(|a| !matches!(a.mode, Some(sp::ArgMode::Out)))
                        .map(|a| convert_data_type(a.data_type))
                        .collect::<Result<Vec<_>, _>>()
                })
                .transpose()?,
        );
    }
    Ok(Statement::Ddl(DdlStatement::Drop(DropObject {
        kind,
        names,
        if_exists,
        cascade: cascade(drop_behavior),
        on_table: None,
        signatures,
    })))
}

fn convert_create_type(
    name: sp::ObjectName,
    representation: Option<sp::UserDefinedTypeRepresentation>,
) -> Result<CreateType, ParseError> {
    let kind = match representation {
        Some(sp::UserDefinedTypeRepresentation::Enum { labels }) => {
            TypeDefinition::Enum(labels.into_iter().map(|l| l.value).collect())
        }
        Some(sp::UserDefinedTypeRepresentation::Composite { attributes }) => {
            TypeDefinition::Composite(
                attributes
                    .into_iter()
                    .map(|a| Ok((convert_ident(a.name), convert_data_type(a.data_type)?)))
                    .collect::<Result<_, ParseError>>()?,
            )
        }
        Some(other) => {
            return Err(ParseError::Unsupported(format!(
                "CREATE TYPE {other} non modélisé"
            )));
        }
        None => {
            return Err(ParseError::Unsupported(
                "CREATE TYPE sans définition (type shell)".to_string(),
            ));
        }
    };
    Ok(CreateType {
        name: convert_object_name(name),
        kind,
    })
}

fn convert_alter_type(at: sp::AlterType) -> AlterType {
    let operation = match at.operation {
        sp::AlterTypeOperation::Rename(r) => AlterTypeOp::RenameTo(convert_ident(r.new_name)),
        sp::AlterTypeOperation::AddValue(a) => AlterTypeOp::AddValue {
            value: a.value.value,
            if_not_exists: a.if_not_exists,
            position: a.position.map(|p| match p {
                sp::AlterTypeAddValuePosition::Before(v) => EnumValuePosition::Before(v.value),
                sp::AlterTypeAddValuePosition::After(v) => EnumValuePosition::After(v.value),
            }),
        },
        sp::AlterTypeOperation::RenameValue(r) => AlterTypeOp::RenameValue {
            from: r.from.value,
            to: r.to.value,
        },
    };
    AlterType {
        name: convert_object_name(at.name),
        operation,
    }
}

fn convert_create_view(cv: sp::CreateView) -> Result<CreateView, ParseError> {
    Ok(CreateView {
        name: convert_object_name(cv.name),
        or_replace: cv.or_replace,
        materialized: cv.materialized,
        temporary: cv.temporary,
        columns: cv
            .columns
            .into_iter()
            .map(|c| convert_ident(c.name))
            .collect(),
        query: convert_query(*cv.query)?,
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
        unlogged: false,
        on_commit: ct.on_commit.map(|c| match c {
            sp::OnCommit::DeleteRows => OnCommit::DeleteRows,
            sp::OnCommit::PreserveRows => OnCommit::PreserveRows,
            sp::OnCommit::Drop => OnCommit::Drop,
        }),
        columns,
        constraints,
        likes: vec![],
    })
}

pub(crate) fn convert_column_def(col: sp::ColumnDef) -> Result<ColumnDef, ParseError> {
    let name = convert_ident(col.name);
    let data_type = convert_data_type(col.data_type)?;

    let mut nullable = true;
    let mut default: Option<Expr> = None;
    let mut generated = None;
    let mut identity = None;
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
            sp::ColumnOption::Generated {
                generated_as,
                generation_expr,
                generation_expr_mode,
                ..
            } => match generation_expr {
                Some(e) => {
                    generated = Some(GeneratedColumn {
                        expr: convert_expr(e)?,
                        stored: !matches!(
                            generation_expr_mode,
                            Some(sp::GeneratedExpressionMode::Virtual)
                        ),
                    });
                }
                None => {
                    identity = Some(match generated_as {
                        sp::GeneratedAs::ByDefault => Identity::ByDefault,
                        _ => Identity::Always,
                    });
                    // Une colonne identité est implicitement NOT NULL.
                    nullable = false;
                }
            },
            sp::ColumnOption::Collation(_) | sp::ColumnOption::Comment(_) => {}
            other => {
                return Err(ParseError::Unsupported(format!(
                    "option de colonne non modélisée : {other}"
                )));
            }
        }
    }

    Ok(ColumnDef {
        name,
        data_type,
        nullable,
        default,
        generated,
        identity,
        constraints,
    })
}

pub(crate) fn convert_table_constraint(
    tc: sp::TableConstraint,
) -> Result<TableConstraint, ParseError> {
    match tc {
        sp::TableConstraint::PrimaryKey(pk) => Ok(TableConstraint::PrimaryKey {
            name: pk.name.map(convert_ident),
            columns: pk
                .columns
                .into_iter()
                .map(convert_index_column)
                .collect::<Result<Vec<_>, _>>()?,
        }),
        sp::TableConstraint::Unique(uc) => Ok(TableConstraint::Unique {
            name: uc.name.map(convert_ident),
            columns: uc
                .columns
                .into_iter()
                .map(convert_index_column)
                .collect::<Result<Vec<_>, _>>()?,
        }),
        sp::TableConstraint::ForeignKey(fk) => Ok(TableConstraint::ForeignKey {
            name: fk.name.map(convert_ident),
            columns: fk.columns.into_iter().map(convert_ident).collect(),
            ref_table: convert_object_name(fk.foreign_table),
            ref_columns: fk.referred_columns.into_iter().map(convert_ident).collect(),
            on_delete: fk.on_delete.map(convert_referential_action),
            on_update: fk.on_update.map(convert_referential_action),
        }),
        sp::TableConstraint::Check(cc) => Ok(TableConstraint::Check {
            name: cc.name.map(convert_ident),
            expr: convert_expr(*cc.expr)?,
        }),
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
    let name = ci.name.map(last_ident).transpose()?;
    let table = convert_object_name(ci.table_name);
    let elements = ci
        .columns
        .into_iter()
        .map(|ic| {
            let expr = convert_expr(ic.column.expr)?;
            Ok(match expr.kind {
                ExprKind::Column(c) if c.table.is_none() => IndexElement::Column(c.column),
                _ => IndexElement::Expr(expr),
            })
        })
        .collect::<Result<Vec<_>, ParseError>>()?;

    Ok(CreateIndex {
        name,
        table,
        elements,
        unique: ci.unique,
        if_not_exists: ci.if_not_exists,
        concurrently: ci.concurrently,
        using: ci.using.map(|u| u.to_string().to_lowercase()),
        where_clause: ci.predicate.map(convert_expr).transpose()?,
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

/// Convert a sqlparser DataType to pgweave_ast ReturnType.
pub(crate) fn convert_return_type_from_dt(dt: sp::DataType) -> Result<ReturnType, ParseError> {
    match &dt {
        sp::DataType::Trigger => Ok(ReturnType::Trigger),
        _ => Ok(ReturnType::Scalar(convert_data_type(dt)?)),
    }
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

    let referencing = ct
        .referencing
        .into_iter()
        .map(|r| {
            Ok(TransitionTable {
                new: matches!(r.refer_type, sp::TriggerReferencingType::NewTable),
                name: last_ident(r.transition_relation_name)?,
            })
        })
        .collect::<Result<Vec<_>, ParseError>>()?;

    Ok(CreateTrigger {
        name: tname,
        or_replace: ct.or_replace,
        table,
        timing,
        events: evts,
        for_each,
        referencing,
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
        .map(|owner| match owner {
            sp::Owner::Ident(id) => convert_ident(id),
            // Mots-clés de rôle : non quotés, PostgreSQL les relit comme tels.
            sp::Owner::CurrentRole => Ident::new("current_role"),
            sp::Owner::CurrentUser => Ident::new("current_user"),
            sp::Owner::SessionUser => Ident::new("session_user"),
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

fn convert_alter_table(at: sp::AlterTable) -> Result<AlterTable, ParseError> {
    let operations = at
        .operations
        .into_iter()
        .map(convert_alter_table_op)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(AlterTable {
        name: convert_object_name(at.name),
        if_exists: at.if_exists,
        only: at.only,
        operations,
    })
}

pub(crate) fn convert_alter_table_op(
    op: sp::AlterTableOperation,
) -> Result<AlterTableOp, ParseError> {
    match op {
        sp::AlterTableOperation::EnableRowLevelSecurity => Ok(AlterTableOp::EnableRowLevelSecurity),
        sp::AlterTableOperation::DisableRowLevelSecurity => {
            Ok(AlterTableOp::DisableRowLevelSecurity)
        }
        sp::AlterTableOperation::AddColumn {
            column_def,
            if_not_exists,
            column_position,
            ..
        } => {
            if column_position.is_some() {
                return Err(ParseError::Unsupported(
                    "position de colonne (FIRST / AFTER) hors PostgreSQL".to_string(),
                ));
            }
            Ok(AlterTableOp::AddColumn {
                column: convert_column_def(column_def)?,
                if_not_exists,
            })
        }
        sp::AlterTableOperation::DropColumn {
            column_names,
            if_exists,
            drop_behavior,
            ..
        } => {
            let [name] = <[sp::Ident; 1]>::try_from(column_names).map_err(|_| {
                ParseError::Unsupported("DROP COLUMN : une colonne par clause".to_string())
            })?;
            Ok(AlterTableOp::DropColumn {
                name: convert_ident(name),
                if_exists,
                cascade: cascade(drop_behavior),
            })
        }
        sp::AlterTableOperation::AlterColumn { column_name, op } => Ok(AlterTableOp::AlterColumn {
            name: convert_ident(column_name),
            action: convert_alter_column_op(op)?,
        }),
        // In 0.62, AddConstraint is a struct variant
        sp::AlterTableOperation::AddConstraint {
            constraint,
            not_valid,
        } => Ok(AlterTableOp::AddConstraint {
            constraint: convert_table_constraint(constraint)?,
            not_valid,
        }),
        sp::AlterTableOperation::DropConstraint {
            name,
            if_exists,
            drop_behavior,
        } => Ok(AlterTableOp::DropConstraint {
            name: convert_ident(name),
            if_exists,
            cascade: cascade(drop_behavior),
        }),
        sp::AlterTableOperation::RenameColumn {
            old_column_name,
            new_column_name,
        } => Ok(AlterTableOp::RenameColumn {
            from: convert_ident(old_column_name),
            to: convert_ident(new_column_name),
        }),
        sp::AlterTableOperation::RenameTable { table_name } => {
            let (sp::RenameTableNameKind::To(n) | sp::RenameTableNameKind::As(n)) = table_name;
            Ok(AlterTableOp::RenameTo(last_ident(n)?))
        }
        sp::AlterTableOperation::ValidateConstraint { name } => {
            Ok(AlterTableOp::ValidateConstraint(convert_ident(name)))
        }
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
        sp::AlterColumnOperation::SetDataType {
            data_type, using, ..
        } => Ok(AlterColumnAction::SetDataType {
            data_type: convert_data_type(data_type)?,
            using: using.map(convert_expr).transpose()?,
        }),
        other => Err(ParseError::Unsupported(format!(
            "unsupported ALTER COLUMN operation: {}",
            other
        ))),
    }
}

/// `CASCADE` explicite ; `RESTRICT` est le comportement par défaut.
fn cascade(behavior: Option<sp::DropBehavior>) -> bool {
    matches!(behavior, Some(sp::DropBehavior::Cascade))
}

// ── Data type conversion ──────────────────────────────────────────────────────

pub(crate) fn convert_data_type(dt: sp::DataType) -> Result<DataType, ParseError> {
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
        sp::DataType::Time(p, tz) => match tz {
            sp::TimezoneInfo::WithTimeZone | sp::TimezoneInfo::Tz => Ok(DataType::TimeTz(p)),
            _ => Ok(DataType::Time(p)),
        },
        sp::DataType::Timestamp(p, tz) => match tz {
            sp::TimezoneInfo::WithTimeZone | sp::TimezoneInfo::Tz => Ok(DataType::TimestampTz(p)),
            _ => Ok(DataType::Timestamp(p)),
        },
        sp::DataType::Interval {
            fields: None,
            precision: None,
        } => Ok(DataType::Interval),
        sp::DataType::Interval { .. } => Err(ParseError::Unsupported(
            "INTERVAL avec champs ou précision".to_string(),
        )),
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
        sp::DataType::Custom(name, modifiers) => {
            let name = name
                .0
                .into_iter()
                .filter_map(|p| match p {
                    sp::ObjectNamePart::Identifier(id) => Some(convert_ident(id).value),
                    sp::ObjectNamePart::Function(_) => None,
                })
                .collect::<Vec<_>>()
                .join(".");
            if modifiers.is_empty() {
                Ok(DataType::Custom(name))
            } else {
                Ok(DataType::Modified { name, modifiers })
            }
        }
        // Trigger return type
        sp::DataType::Trigger => Ok(DataType::Custom("trigger".to_string())),
        // Types intégrés sans variante dédiée : nommés comme dans pg_catalog,
        // résolus par le catalogue.
        sp::DataType::Regclass => Ok(DataType::Custom("regclass".to_string())),
        sp::DataType::TsVector => Ok(DataType::Custom("tsvector".to_string())),
        sp::DataType::TsQuery => Ok(DataType::Custom("tsquery".to_string())),
        sp::DataType::GeometricType(kind) => Ok(DataType::Custom(kind.to_string().to_lowercase())),
        other => Err(ParseError::Unsupported(format!(
            "unsupported data type: {}",
            other
        ))),
    }
}

// ── Identifier helpers ────────────────────────────────────────────────────────

/// Identifiant tel que PostgreSQL le comprend : non quoté, il est replié en
/// minuscules (ASCII seulement, comme `downcase_identifier` en UTF-8) ;
/// quoté, il est conservé tel quel. `"MenuComponent"` et `MenuComponent`
/// désignent deux objets distincts.
pub(crate) fn convert_ident(id: sp::Ident) -> Ident {
    match id.quote_style {
        Some(_) => Ident {
            value: id.value,
            quoted: true,
        },
        None => Ident {
            value: id.value.to_ascii_lowercase(),
            quoted: false,
        },
    }
}

/// In 0.62, ObjectName holds Vec<ObjectNamePart> instead of Vec<Ident>.
/// We filter out any Function parts (dialect-specific) and keep Identifiers.
pub(crate) fn convert_object_name(name: sp::ObjectName) -> ObjectName {
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
    /// Offset du statement dans le texte soumis au parser.
    stmt_offset: usize,
    /// Comment traduire un offset local vers le repère du document.
    offsets: crate::Offsets,
}

impl SpanCtx {
    fn new(source: &str, stmt_offset: usize, offsets: crate::Offsets) -> Self {
        let mut line_starts = vec![0usize];
        for (i, b) in source.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(i + 1);
            }
        }
        Self {
            line_starts,
            source: source.to_string(),
            stmt_offset,
            offsets,
        }
    }

    /// `Location` (ligne/colonne 1-basées, colonne en caractères) → offset octet
    /// **local au texte soumis**. La traduction vers le document se fait dans
    /// [`Self::resolve`], qui dispose de la plage complète.
    fn local_offset_of(&self, line: u64, column: u64) -> Option<usize> {
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

        Some(self.stmt_offset + line_start + byte_in_line)
    }

    /// Position du `*` de `f(*)` : premier caractère significatif après la
    /// première parenthèse qui suit le début de l'appel. sqlparser ne la
    /// conserve pas, et le span qu'il donne à l'appel s'arrête alors au nom.
    fn star_in_call(&self, call: sqlparser::tokenizer::Span) -> Span {
        let Some(start) = self.local_offset_of(call.start.line, call.start.column) else {
            return Span::DUMMY;
        };
        let Some(text) = self.source.get(start - self.stmt_offset..) else {
            return Span::DUMMY;
        };
        let Some(paren) = text.find('(') else {
            return Span::DUMMY;
        };
        let after = &text[paren + 1..];
        let skipped = after.len() - after.trim_start().len();
        if !after[skipped..].starts_with('*') {
            return Span::DUMMY;
        }
        let at = start + paren + 1 + skipped;
        let (s, e) = self.offsets.span(at, at + 1);
        Span {
            start: s as u32,
            end: e as u32,
        }
    }

    /// Plage sqlparser → span dans le repère du document.
    fn resolve(&self, sp_span: sqlparser::tokenizer::Span) -> Span {
        let Some(start) = self.local_offset_of(sp_span.start.line, sp_span.start.column) else {
            return Span::DUMMY;
        };
        let Some(end) = self.local_offset_of(sp_span.end.line, sp_span.end.column) else {
            return Span::DUMMY;
        };
        if end < start {
            return Span::DUMMY;
        }
        let (s, e) = self.offsets.span(start, end);
        Span {
            start: s as u32,
            end: e as u32,
        }
    }
}

thread_local! {
    static SPAN_CTX: std::cell::RefCell<Option<SpanCtx>> =
        const { std::cell::RefCell::new(None) };
}

/// Garde RAII : restaure le contexte de position précédent à la sortie de
/// portée. Les contextes s'empilent : le corps d'une routine est parsé à
/// l'intérieur du parse de son fichier, et le fichier doit retrouver son
/// contexte ensuite.
pub(crate) struct SpanCtxGuard {
    previous: Option<SpanCtx>,
}

impl Drop for SpanCtxGuard {
    fn drop(&mut self) {
        let previous = self.previous.take();
        SPAN_CTX.with(|c| *c.borrow_mut() = previous);
    }
}

/// Entre dans le contexte de position d'un texte : `source` est le texte
/// soumis au tokenizer, `stmt_offset` sa position dans le texte soumis au
/// parse englobant, `offsets` la traduction vers le document.
pub(crate) fn enter_span_ctx(
    source: &str,
    stmt_offset: usize,
    offsets: crate::Offsets,
) -> SpanCtxGuard {
    let previous = SPAN_CTX.with(|c| c.replace(Some(SpanCtx::new(source, stmt_offset, offsets))));
    SpanCtxGuard { previous }
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
        c.borrow()
            .as_ref()
            .map(|ctx| ctx.resolve(sp_span))
            .unwrap_or(Span::DUMMY)
    })
}
