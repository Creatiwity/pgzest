//! Evaluation of conditions over the catalog, for replaying `DO` blocks.
//!
//! Idempotent migrations guard their DDL behind conditions such as
//! `IF NOT EXISTS (SELECT 1 FROM pg_type WHERE typname = 'x')`. To reproduce
//! their effect, these conditions are evaluated over **virtual tables** built
//! from the catalog (`pg_type`, `pg_enum`, `information_schema.columns`…).
//!
//! Evaluation has three outcomes: true, false, or **unknown** — as soon as a
//! table, a column, a function or a construct is not covered. An unknown is
//! never settled by default: the caller reports it.

use std::collections::HashMap;

use pgweave_ast::Expr;
use pgweave_ast::expr::{BinaryOp, ExprKind, Literal, ObjectName, UnaryOp};
use pgweave_ast::stmt::{JoinConstraint, JoinKind, SelectStmt, TableSource};

use crate::catalog::Catalog;
use crate::model::{ConstraintKind, RelationKind, TypeKind, TypeRef};

#[derive(Debug, Clone, PartialEq)]
enum Value {
    Text(String),
    Bool(bool),
    Null,
}

type Row = HashMap<&'static str, Value>;

/// Rows bound to an alias, from the innermost level to the outermost.
struct Bindings<'a> {
    rows: Vec<(&'a str, &'a Row)>,
    outer: Option<&'a Bindings<'a>>,
}

fn text(s: impl Into<String>) -> Value {
    Value::Text(s.into())
}

/// Rows of a virtual table, or `None` if the table is not covered.
fn table_rows(cat: &Catalog, name: &ObjectName) -> Option<Vec<Row>> {
    let parts: Vec<&str> = name.0.iter().map(|i| i.value.as_str()).collect();
    let (schema, table) = match parts.as_slice() {
        [t] => ("pg_catalog", *t),
        [s, t] => (*s, *t),
        _ => return None,
    };
    let mut rows = Vec::new();
    match (schema, table) {
        ("pg_catalog", "pg_namespace") => {
            for s in cat.schemas.keys() {
                rows.push(Row::from([
                    ("oid", text(format!("n:{s}"))),
                    ("nspname", text(s)),
                ]));
            }
        }
        ("pg_catalog", "pg_type") => {
            for t in cat.types.values() {
                let typtype = match t.kind {
                    TypeKind::Enum { .. } => "e",
                    TypeKind::Domain { .. } => "d",
                    TypeKind::Composite { .. } => "c",
                    TypeKind::Pseudo => "p",
                    TypeKind::Range { .. } => "r",
                    TypeKind::Multirange { .. } => "m",
                    TypeKind::Base | TypeKind::Array { .. } => "b",
                };
                rows.push(Row::from([
                    ("oid", text(format!("t:{}", t.name))),
                    ("typname", text(&t.name.name)),
                    ("typnamespace", text(format!("n:{}", t.name.schema))),
                    ("typtype", text(typtype)),
                ]));
            }
            // Row type of each relation.
            for r in cat.relations.values() {
                rows.push(Row::from([
                    ("oid", text(format!("t:{}", r.name))),
                    ("typname", text(&r.name.name)),
                    ("typnamespace", text(format!("n:{}", r.name.schema))),
                    ("typtype", text("c")),
                ]));
            }
        }
        ("pg_catalog", "pg_enum") => {
            for t in cat.types.values() {
                if let TypeKind::Enum { labels } = &t.kind {
                    for (i, l) in labels.iter().enumerate() {
                        rows.push(Row::from([
                            ("enumtypid", text(format!("t:{}", t.name))),
                            ("enumlabel", text(l)),
                            ("enumsortorder", text((i + 1).to_string())),
                        ]));
                    }
                }
            }
        }
        ("pg_catalog", "pg_extension") => {
            for e in cat.extensions.values() {
                rows.push(Row::from([
                    ("extname", text(&e.name)),
                    (
                        "extversion",
                        e.version.clone().map(Value::Text).unwrap_or(Value::Null),
                    ),
                    ("extnamespace", text(format!("n:{}", e.schema))),
                ]));
            }
        }
        ("pg_catalog", "pg_class") => {
            for r in cat.relations.values() {
                let relkind = match r.kind {
                    RelationKind::Table => "r",
                    RelationKind::PartitionedTable => "p",
                    RelationKind::View => "v",
                    RelationKind::MaterializedView => "m",
                    RelationKind::ForeignTable => "f",
                };
                rows.push(Row::from([
                    ("oid", text(format!("r:{}", r.name))),
                    ("relname", text(&r.name.name)),
                    ("relnamespace", text(format!("n:{}", r.name.schema))),
                    ("relkind", text(relkind)),
                ]));
            }
            for i in cat.indexes.values() {
                rows.push(Row::from([
                    ("oid", text(format!("r:{}", i.name))),
                    ("relname", text(&i.name.name)),
                    ("relnamespace", text(format!("n:{}", i.name.schema))),
                    ("relkind", text("i")),
                ]));
            }
            for s in cat.sequences.values() {
                rows.push(Row::from([
                    ("oid", text(format!("r:{}", s.name))),
                    ("relname", text(&s.name.name)),
                    ("relnamespace", text(format!("n:{}", s.name.schema))),
                    ("relkind", text("S")),
                ]));
            }
        }
        ("pg_catalog", "pg_indexes") => {
            for i in cat.indexes.values() {
                rows.push(Row::from([
                    ("schemaname", text(&i.table.schema)),
                    ("tablename", text(&i.table.name)),
                    ("indexname", text(&i.name.name)),
                ]));
            }
        }
        ("information_schema", "schemata") => {
            for s in cat.schemas.keys() {
                rows.push(Row::from([("schema_name", text(s))]));
            }
        }
        ("information_schema", "tables") => {
            for r in cat.relations.values() {
                let kind = match r.kind {
                    RelationKind::View | RelationKind::MaterializedView => "VIEW",
                    RelationKind::ForeignTable => "FOREIGN",
                    _ => "BASE TABLE",
                };
                rows.push(Row::from([
                    ("table_schema", text(&r.name.schema)),
                    ("table_name", text(&r.name.name)),
                    ("table_type", text(kind)),
                ]));
            }
        }
        ("information_schema", "columns") => {
            for r in cat.relations.values() {
                for c in &r.columns {
                    let (udt_schema, udt_name) = match &c.ty {
                        TypeRef::Named { name, .. } => (text(&name.schema), text(&name.name)),
                        TypeRef::Array(inner) => match inner.as_ref() {
                            TypeRef::Named { name, .. } => {
                                (text(&name.schema), text(format!("_{}", name.name)))
                            }
                            _ => continue,
                        },
                        // Unknown type: the row is omitted rather than
                        // filled in at random — a condition looking for it
                        // cannot wrongly conclude on its columns.
                        _ => continue,
                    };
                    rows.push(Row::from([
                        ("table_schema", text(&r.name.schema)),
                        ("table_name", text(&r.name.name)),
                        ("column_name", text(&c.name)),
                        ("is_nullable", text(if c.not_null { "NO" } else { "YES" })),
                        ("udt_schema", udt_schema),
                        ("udt_name", udt_name),
                    ]));
                }
            }
        }
        ("information_schema", "table_constraints") => {
            for r in cat.relations.values() {
                for c in &r.constraints {
                    let kind = match c.kind {
                        ConstraintKind::PrimaryKey { .. } => "PRIMARY KEY",
                        ConstraintKind::Unique { .. } => "UNIQUE",
                        ConstraintKind::ForeignKey { .. } => "FOREIGN KEY",
                        ConstraintKind::Check { .. } => "CHECK",
                        // Absent from information_schema.table_constraints.
                        ConstraintKind::Exclude { .. } => continue,
                    };
                    rows.push(Row::from([
                        ("constraint_schema", text(&r.name.schema)),
                        ("constraint_name", text(&c.name)),
                        ("table_schema", text(&r.name.schema)),
                        ("table_name", text(&r.name.name)),
                        ("constraint_type", text(kind)),
                    ]));
                }
            }
        }
        _ => return None,
    }
    Some(rows)
}

/// Condition of an `IF`: `Some(true/false)`, or `None` if not evaluable.
pub fn eval_condition(cat: &Catalog, expr: &Expr) -> Option<bool> {
    let empty = Bindings {
        rows: Vec::new(),
        outer: None,
    };
    match eval(cat, expr, &empty)? {
        Value::Bool(b) => Some(b),
        Value::Null => Some(false),
        Value::Text(_) => None,
    }
}

fn lookup(b: &Bindings, table: Option<&str>, column: &str) -> Option<Value> {
    for (alias, row) in &b.rows {
        if table.is_some_and(|t| t != *alias) {
            continue;
        }
        if let Some(v) = row.get(column) {
            return Some(v.clone());
        }
    }
    b.outer.and_then(|o| lookup(o, table, column))
}

fn eval(cat: &Catalog, expr: &Expr, b: &Bindings) -> Option<Value> {
    Some(match &expr.kind {
        ExprKind::Literal(Literal::String(s)) => Value::Text(s.clone()),
        ExprKind::Literal(Literal::Number(n)) => Value::Text(n.clone()),
        ExprKind::Literal(Literal::Boolean(v)) => Value::Bool(*v),
        ExprKind::Literal(Literal::Null) => Value::Null,
        ExprKind::Literal(Literal::BitString(_)) => return None,
        ExprKind::Column(c) => lookup(
            b,
            c.table.as_ref().map(|t| t.value.as_str()),
            &c.column.value,
        )?,
        // `'x'::text`: the value; any other cast (regclass…) is unknown.
        ExprKind::Cast {
            expr,
            data_type: pgweave_ast::types::DataType::Text | pgweave_ast::types::DataType::Varchar(_),
        } => eval(cat, expr, b)?,
        ExprKind::UnaryOp { op, expr } => {
            let v = eval(cat, expr, b)?;
            match op {
                UnaryOp::Not => match v {
                    Value::Bool(x) => Value::Bool(!x),
                    Value::Null => Value::Null,
                    Value::Text(_) => return None,
                },
                UnaryOp::IsNull => Value::Bool(v == Value::Null),
                UnaryOp::IsNotNull => Value::Bool(v != Value::Null),
                UnaryOp::Neg | UnaryOp::Prefix(_) => return None,
            }
        }
        ExprKind::BinaryOp { left, op, right } => match op {
            BinaryOp::And | BinaryOp::Or => {
                let l = eval(cat, left, b)?;
                let r = eval(cat, right, b)?;
                let as_tri = |v: Value| match v {
                    Value::Bool(x) => Some(Some(x)),
                    Value::Null => Some(None),
                    Value::Text(_) => None,
                };
                let (l, r) = (as_tri(l)?, as_tri(r)?);
                let res = if *op == BinaryOp::And {
                    match (l, r) {
                        (Some(false), _) | (_, Some(false)) => Some(false),
                        (Some(true), Some(true)) => Some(true),
                        _ => None,
                    }
                } else {
                    match (l, r) {
                        (Some(true), _) | (_, Some(true)) => Some(true),
                        (Some(false), Some(false)) => Some(false),
                        _ => None,
                    }
                };
                res.map(Value::Bool).unwrap_or(Value::Null)
            }
            BinaryOp::Eq | BinaryOp::NotEq => {
                let l = eval(cat, left, b)?;
                let r = eval(cat, right, b)?;
                match (&l, &r) {
                    (Value::Null, _) | (_, Value::Null) => Value::Null,
                    _ => Value::Bool((l == r) == (*op == BinaryOp::Eq)),
                }
            }
            _ => return None,
        },
        ExprKind::InList {
            expr,
            list,
            negated,
        } => {
            let v = eval(cat, expr, b)?;
            if v == Value::Null {
                return Some(Value::Null);
            }
            let mut found = false;
            for item in list {
                if eval(cat, item, b)? == v {
                    found = true;
                }
            }
            Value::Bool(found != *negated)
        }
        ExprKind::Exists { subquery, negated } => {
            Value::Bool(exists(cat, subquery, b)? != *negated)
        }
        _ => return None,
    })
}

/// `EXISTS (query)`: does the query produce at least one row?
fn exists(cat: &Catalog, sel: &SelectStmt, outer: &Bindings) -> Option<bool> {
    if !sel.set_ops.is_empty() || sel.having.is_some() || !sel.with.is_empty() {
        return None;
    }
    // Sources: (alias, rows), with the join condition of each one.
    let mut refs: Vec<(&pgweave_ast::stmt::TableRef, Option<&Expr>, bool)> =
        sel.from.iter().map(|t| (t, None, false)).collect();
    for j in &sel.joins {
        let on = match &j.constraint {
            JoinConstraint::On(e) => Some(e),
            JoinConstraint::None => None,
            _ => return None,
        };
        let left = match j.kind {
            JoinKind::Inner | JoinKind::Cross => false,
            JoinKind::Left => true,
            _ => return None,
        };
        refs.push((&j.table, on, left));
    }
    let mut sources: Vec<(String, Vec<Row>, Option<&Expr>, bool)> = Vec::new();
    for (t, on, left) in refs {
        let TableSource::Table(name) = &t.source else {
            return None;
        };
        let rows = table_rows(cat, name)?;
        sources.push((t.visible_name()?.to_string(), rows, on, left));
    }
    if sources.is_empty() {
        return None;
    }

    // Filtering before the join: a `WHERE` condition (top-level conjunction)
    // that reads a single source discards upfront the rows where it is not
    // true. A `LEFT JOIN` source is not filtered (its missing rows become
    // NULLs). An unknown result keeps the row: the full test decides later.
    if let Some(w) = &sel.where_clause {
        let mut conjuncts = Vec::new();
        split_and(w, &mut conjuncts);
        for c in conjuncts {
            let Some(i) = single_source(c, &sources) else {
                continue;
            };
            if sources[i].3 {
                continue;
            }
            let alias = sources[i].0.clone();
            sources[i].1.retain(|row| {
                let b = Bindings {
                    rows: vec![(alias.as_str(), row)],
                    outer: Some(outer),
                };
                !matches!(eval(cat, c, &b), Some(v) if v != Value::Bool(true))
            });
        }
    }

    // Nested loops: every combination of rows is tested.
    let mut unknown = false;
    let found = combine(
        cat,
        &sources,
        0,
        &mut Vec::new(),
        outer,
        sel.where_clause.as_ref(),
        &mut unknown,
    );
    if found {
        Some(true)
    } else if unknown {
        None
    } else {
        Some(false)
    }
}

/// Top-level conjunctions of a condition.
fn split_and<'e>(e: &'e Expr, out: &mut Vec<&'e Expr>) {
    match &e.kind {
        ExprKind::BinaryOp {
            left,
            op: pgweave_ast::expr::BinaryOp::And,
            right,
        } => {
            split_and(left, out);
            split_and(right, out);
        }
        _ => out.push(e),
    }
}

/// The only source `e` reads, if it reads just one (and no nested query):
/// reference qualified by its alias, or column name only that source has.
fn single_source(e: &Expr, sources: &[(String, Vec<Row>, Option<&Expr>, bool)]) -> Option<usize> {
    fn walk(
        e: &Expr,
        sources: &[(String, Vec<Row>, Option<&Expr>, bool)],
        found: &mut Option<usize>,
    ) -> bool {
        match &e.kind {
            ExprKind::Subquery(_) | ExprKind::Exists { .. } | ExprKind::InSubquery { .. } => false,
            ExprKind::Column(c) => {
                let owners: Vec<usize> = sources
                    .iter()
                    .enumerate()
                    .filter(|(_, (alias, rows, _, _))| match &c.table {
                        Some(t) => t.value == *alias,
                        None => rows
                            .first()
                            .is_some_and(|r| r.contains_key(c.column.value.as_str())),
                    })
                    .map(|(i, _)| i)
                    .collect();
                match (owners.as_slice(), *found) {
                    ([i], None) => {
                        *found = Some(*i);
                        true
                    }
                    ([i], Some(f)) => *i == f,
                    _ => false,
                }
            }
            _ => e.children().into_iter().all(|c| walk(c, sources, found)),
        }
    }
    let mut found = None;
    if walk(e, sources, &mut found) {
        found
    } else {
        None
    }
}

fn combine<'r>(
    cat: &Catalog,
    sources: &'r [(String, Vec<Row>, Option<&Expr>, bool)],
    i: usize,
    bound: &mut Vec<(String, &'r Row)>,
    outer: &Bindings,
    where_clause: Option<&Expr>,
    unknown: &mut bool,
) -> bool {
    let test = |bound: &Vec<(String, &Row)>, cond: Option<&Expr>, unknown: &mut bool| -> bool {
        let Some(cond) = cond else {
            return true;
        };
        let b = Bindings {
            rows: bound.iter().rev().map(|(a, r)| (a.as_str(), *r)).collect(),
            outer: Some(outer),
        };
        match eval(cat, cond, &b) {
            Some(Value::Bool(true)) => true,
            Some(_) => false,
            None => {
                *unknown = true;
                false
            }
        }
    };
    if i == sources.len() {
        return test(bound, where_clause, unknown);
    }
    let (alias, rows, on, left) = &sources[i];
    let mut matched = false;
    for row in rows {
        bound.push((alias.clone(), row));
        if test(bound, *on, unknown) {
            matched = true;
            if combine(cat, sources, i + 1, bound, outer, where_clause, unknown) {
                bound.pop();
                return true;
            }
        }
        bound.pop();
    }
    // LEFT JOIN without a match: a row of NULLs. The source's columns are
    // then missing from the row — a condition reading them becomes unknown,
    // not false.
    if *left && !matched {
        static EMPTY: std::sync::LazyLock<Row> = std::sync::LazyLock::new(Row::new);
        bound.push((alias.clone(), &EMPTY));
        let r = combine(cat, sources, i + 1, bound, outer, where_clause, unknown);
        bound.pop();
        return r;
    }
    false
}
