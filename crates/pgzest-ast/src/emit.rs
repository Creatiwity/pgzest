//! SQL rendering of the AST: the emitted text parses back into an equivalent AST.

use crate::expr::ExprKind;
use crate::*;

// ── Public API ────────────────────────────────────────────────────────────────

pub fn emit_statement(stmt: &Statement) -> String {
    match stmt {
        Statement::Select(sel) => emit_select(sel),
        Statement::Insert(ins) => emit_insert(ins),
        Statement::Update(upd) => emit_update(upd),
        Statement::Delete(del) => emit_delete(del),
        Statement::Ddl(ddl) => emit_ddl(ddl),
        Statement::Utility(u) => emit_utility(&u.kind),
    }
}

// ── Utilities ─────────────────────────────────────────────────────────────────

pub fn emit_utility(u: &UtilityKind) -> String {
    use crate::utility::{CommentTarget, CopyOption, CopySource, CopyTarget, TransactionCommand};
    match u {
        UtilityKind::Do { language, body, .. } => format!(
            "DO LANGUAGE {} {};",
            emit_function_language(language),
            emit_function_body(body)
        ),
        UtilityKind::Call { name, args } => format!(
            "CALL {}({});",
            emit_object_name(name),
            args.iter().map(emit_expr).collect::<Vec<_>>().join(", ")
        ),
        UtilityKind::Copy(c) => {
            let source = match &c.source {
                CopySource::Table { name, columns } if columns.is_empty() => emit_object_name(name),
                CopySource::Table { name, columns } => format!(
                    "{} ({})",
                    emit_object_name(name),
                    columns
                        .iter()
                        .map(emit_ident)
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                CopySource::Query(q) => format!("({})", emit_select(q)),
            };
            let target = match &c.target {
                CopyTarget::Stdin => "STDIN".to_string(),
                CopyTarget::Stdout => "STDOUT".to_string(),
                CopyTarget::File(f) => emit_string_literal(f),
                CopyTarget::Program(p) => format!("PROGRAM {}", emit_string_literal(p)),
            };
            let char_lit = |ch: &char| emit_string_literal(&ch.to_string());
            let idents = |v: &Vec<Ident>| v.iter().map(emit_ident).collect::<Vec<_>>().join(", ");
            let options: Vec<String> = c
                .options
                .iter()
                .map(|o| match o {
                    CopyOption::Format(f) => format!("FORMAT {f}"),
                    CopyOption::Header(b) => format!("HEADER {b}"),
                    CopyOption::Delimiter(ch) => format!("DELIMITER {}", char_lit(ch)),
                    CopyOption::Null(n) => format!("NULL {}", emit_string_literal(n)),
                    CopyOption::Quote(ch) => format!("QUOTE {}", char_lit(ch)),
                    CopyOption::Escape(ch) => format!("ESCAPE {}", char_lit(ch)),
                    CopyOption::Encoding(e) => format!("ENCODING {}", emit_string_literal(e)),
                    CopyOption::Freeze(b) => format!("FREEZE {b}"),
                    CopyOption::ForceQuote(v) => format!("FORCE_QUOTE ({})", idents(v)),
                    CopyOption::ForceNotNull(v) => format!("FORCE_NOT_NULL ({})", idents(v)),
                    CopyOption::ForceNull(v) => format!("FORCE_NULL ({})", idents(v)),
                })
                .collect();
            let options = if options.is_empty() {
                String::new()
            } else {
                format!(" WITH ({})", options.join(", "))
            };
            format!(
                "COPY {} {} {}{};",
                source,
                if c.to { "TO" } else { "FROM" },
                target,
                options
            )
        }
        UtilityKind::Set { local, name, value } => match value {
            SettingValue::FromCurrent => format!("SET {name} FROM CURRENT;"),
            SettingValue::Values(v) if v.is_empty() => {
                format!(
                    "SET {}{} TO DEFAULT;",
                    if *local { "LOCAL " } else { "" },
                    name
                )
            }
            SettingValue::Values(v) => format!(
                "SET {}{} TO {};",
                if *local { "LOCAL " } else { "" },
                name,
                v.iter()
                    .map(|x| emit_string_literal(x))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        },
        UtilityKind::Transaction(t) => match t {
            TransactionCommand::Begin => "BEGIN;".to_string(),
            TransactionCommand::Commit => "COMMIT;".to_string(),
            TransactionCommand::Rollback => "ROLLBACK;".to_string(),
            TransactionCommand::Savepoint(n) => format!("SAVEPOINT {};", emit_ident(n)),
            TransactionCommand::ReleaseSavepoint(n) => {
                format!("RELEASE SAVEPOINT {};", emit_ident(n))
            }
            TransactionCommand::RollbackToSavepoint(n) => {
                format!("ROLLBACK TO SAVEPOINT {};", emit_ident(n))
            }
        },
        UtilityKind::Comment { target, name, text } => {
            let kind = match target {
                CommentTarget::Table => "TABLE",
                CommentTarget::Column => "COLUMN",
                CommentTarget::View => "VIEW",
                CommentTarget::MaterializedView => "MATERIALIZED VIEW",
                CommentTarget::Index => "INDEX",
                CommentTarget::Sequence => "SEQUENCE",
                CommentTarget::Type => "TYPE",
                CommentTarget::Domain => "DOMAIN",
                CommentTarget::Function => "FUNCTION",
                CommentTarget::Procedure => "PROCEDURE",
                CommentTarget::Schema => "SCHEMA",
                CommentTarget::Extension => "EXTENSION",
            };
            format!(
                "COMMENT ON {} {} IS {};",
                kind,
                emit_object_name(name),
                text.as_deref()
                    .map(emit_string_literal)
                    .unwrap_or_else(|| "NULL".to_string())
            )
        }
        UtilityKind::Privilege {
            grant,
            privileges,
            objects,
            grantees,
        } => format!(
            "{} {} ON {} {} {};",
            if *grant { "GRANT" } else { "REVOKE" },
            privileges,
            objects,
            if *grant { "TO" } else { "FROM" },
            grantees.join(", ")
        ),
        UtilityKind::Truncate {
            tables,
            restart_identity,
            cascade,
        } => format!(
            "TRUNCATE TABLE {}{}{};",
            tables
                .iter()
                .map(emit_object_name)
                .collect::<Vec<_>>()
                .join(", "),
            if *restart_identity {
                " RESTART IDENTITY"
            } else {
                ""
            },
            if *cascade { " CASCADE" } else { "" }
        ),
        UtilityKind::Maintenance {
            command,
            options,
            tables,
        } => {
            let mut out = match command {
                crate::utility::MaintenanceCommand::Analyze => "ANALYZE".to_string(),
                crate::utility::MaintenanceCommand::Vacuum => "VACUUM".to_string(),
            };
            let (legacy, modern): (Vec<_>, Vec<_>) = options.iter().partition(|o| o.legacy);
            for o in legacy {
                out.push(' ');
                out.push_str(&o.name);
            }
            if !modern.is_empty() {
                let opts: Vec<String> = modern
                    .iter()
                    .map(|o| match &o.value {
                        Some(v) => format!("{} {}", o.name, v),
                        None => o.name.clone(),
                    })
                    .collect();
                out.push_str(&format!(" ({})", opts.join(", ")));
            }
            let targets: Vec<String> = tables
                .iter()
                .map(|t| {
                    if t.columns.is_empty() {
                        emit_object_name(&t.table)
                    } else {
                        format!(
                            "{} ({})",
                            emit_object_name(&t.table),
                            t.columns
                                .iter()
                                .map(emit_ident)
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    }
                })
                .collect();
            if !targets.is_empty() {
                out.push(' ');
                out.push_str(&targets.join(", "));
            }
            out.push(';');
            out
        }
    }
}

// ── SELECT ────────────────────────────────────────────────────────────────────

/// `WITH` clause followed by a space, or nothing.
fn emit_with(with: &[Cte], recursive: bool) -> String {
    if with.is_empty() {
        return String::new();
    }
    let ctes: Vec<String> = with
        .iter()
        .map(|cte| {
            let cols = if cte.columns.is_empty() {
                String::new()
            } else {
                format!(
                    " ({})",
                    cte.columns
                        .iter()
                        .map(emit_ident)
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            format!(
                "{}{} AS ({})",
                emit_ident(&cte.name),
                cols,
                match &cte.query {
                    CteQuery::Select(q) => emit_select(q),
                    CteQuery::Modify(s) => emit_statement(s),
                }
            )
        })
        .collect();
    format!(
        "{}{} ",
        if recursive {
            "WITH RECURSIVE "
        } else {
            "WITH "
        },
        ctes.join(", ")
    )
}

pub fn emit_select(sel: &SelectStmt) -> String {
    let mut out = String::new();

    out.push_str(&emit_with(&sel.with, sel.with_recursive));

    // Body: SELECT, then set operations. They are applied left to right
    // (see `SelectStmt::set_ops`); since INTERSECT binds tighter than
    // UNION/EXCEPT in SQL, the prefix is parenthesized when an INTERSECT
    // follows a UNION or an EXCEPT.
    let mut body = emit_select_core(sel);
    let mut loose_prefix = false;
    for op in &sel.set_ops {
        if op.op == SetOperator::Intersect && loose_prefix {
            body = format!("({body})");
            loose_prefix = false;
        }
        if op.op != SetOperator::Intersect {
            loose_prefix = true;
        }
        let keyword = match op.op {
            SetOperator::Union => "UNION",
            SetOperator::Intersect => "INTERSECT",
            SetOperator::Except => "EXCEPT",
        };
        let operand = emit_select(&op.select);
        let o = &op.select;
        let operand = if !o.set_ops.is_empty()
            || !o.with.is_empty()
            || !o.order_by.is_empty()
            || o.limit.is_some()
            || o.offset.is_some()
        {
            format!("({operand})")
        } else {
            operand
        };
        body = format!(
            "{body} {keyword}{} {operand}",
            if op.all { " ALL" } else { "" }
        );
    }
    out.push_str(&body);

    // ORDER BY
    if !sel.order_by.is_empty() {
        out.push_str(" ORDER BY ");
        let items: Vec<String> = sel.order_by.iter().map(emit_order_by).collect();
        out.push_str(&items.join(", "));
    }

    // LIMIT
    if let Some(l) = &sel.limit {
        out.push_str(" LIMIT ");
        out.push_str(&emit_expr(l));
    }

    // OFFSET
    if let Some(o) = &sel.offset {
        out.push_str(" OFFSET ");
        out.push_str(&emit_expr(o));
    }

    out
}

/// `SELECT … FROM … WHERE … GROUP BY … HAVING …`, without set operations
/// or ORDER BY / LIMIT.
fn emit_select_core(sel: &SelectStmt) -> String {
    if !sel.values.is_empty() {
        let rows: Vec<String> = sel
            .values
            .iter()
            .map(|r| {
                format!(
                    "({})",
                    r.iter().map(emit_expr).collect::<Vec<_>>().join(", ")
                )
            })
            .collect();
        return format!("VALUES {}", rows.join(", "));
    }
    let mut out = String::new();
    out.push_str("SELECT ");
    if !sel.distinct_on.is_empty() {
        let on: Vec<String> = sel.distinct_on.iter().map(emit_expr).collect();
        out.push_str(&format!("DISTINCT ON ({}) ", on.join(", ")));
    } else if sel.distinct {
        out.push_str("DISTINCT ");
    }

    // columns
    let cols: Vec<String> = sel.columns.iter().map(emit_select_item).collect();
    out.push_str(&cols.join(", "));

    // FROM
    if !sel.from.is_empty() {
        out.push_str(" FROM ");
        let tables: Vec<String> = sel.from.iter().map(emit_table_ref).collect();
        out.push_str(&tables.join(", "));
    }

    // JOINs
    for join in &sel.joins {
        out.push(' ');
        out.push_str(&emit_join(join));
    }

    // WHERE
    if let Some(w) = &sel.where_clause {
        out.push_str(" WHERE ");
        out.push_str(&emit_expr(w));
    }

    // GROUP BY
    if !sel.group_by.is_empty() {
        out.push_str(" GROUP BY ");
        let exprs: Vec<String> = sel.group_by.iter().map(emit_expr).collect();
        out.push_str(&exprs.join(", "));
    }

    // HAVING
    if let Some(h) = &sel.having {
        out.push_str(" HAVING ");
        out.push_str(&emit_expr(h));
    }

    // WINDOW
    if !sel.windows.is_empty() {
        let defs: Vec<String> = sel
            .windows
            .iter()
            .map(|w| format!("{} AS ({})", emit_ident(&w.name), emit_window_spec(&w.spec)))
            .collect();
        out.push_str(" WINDOW ");
        out.push_str(&defs.join(", "));
    }

    out
}

/// Contents of a window's parentheses.
fn emit_window_spec(spec: &WindowSpec) -> String {
    let mut parts = Vec::new();
    if let Some(base) = &spec.base {
        parts.push(emit_ident(base));
    }
    if !spec.partition_by.is_empty() {
        let p: Vec<String> = spec.partition_by.iter().map(emit_expr).collect();
        parts.push(format!("PARTITION BY {}", p.join(", ")));
    }
    if !spec.order_by.is_empty() {
        let o: Vec<String> = spec.order_by.iter().map(emit_order_by).collect();
        parts.push(format!("ORDER BY {}", o.join(", ")));
    }
    if let Some(frame) = &spec.frame {
        let units = match frame.units {
            FrameUnits::Rows => "ROWS",
            FrameUnits::Range => "RANGE",
            FrameUnits::Groups => "GROUPS",
        };
        let bound = |b: &FrameBound| match b {
            FrameBound::UnboundedPreceding => "UNBOUNDED PRECEDING".to_string(),
            FrameBound::Preceding(e) => format!("{} PRECEDING", emit_expr(e)),
            FrameBound::CurrentRow => "CURRENT ROW".to_string(),
            FrameBound::Following(e) => format!("{} FOLLOWING", emit_expr(e)),
            FrameBound::UnboundedFollowing => "UNBOUNDED FOLLOWING".to_string(),
        };
        parts.push(match &frame.end {
            Some(end) => format!("{units} BETWEEN {} AND {}", bound(&frame.start), bound(end)),
            None => format!("{units} {}", bound(&frame.start)),
        });
    }
    parts.join(" ")
}

fn emit_select_item(item: &SelectItem) -> String {
    match item {
        SelectItem::Wildcard => "*".to_string(),
        SelectItem::QualifiedWildcard(ident) => format!("{}.*", emit_ident(ident)),
        SelectItem::Expr { expr, alias } => {
            let e = emit_expr(expr);
            match alias {
                Some(a) => format!("{} AS {}", e, emit_ident(a)),
                None => e,
            }
        }
    }
}

pub fn emit_table_ref(tr: &TableRef) -> String {
    let source = match &tr.source {
        TableSource::Table(name) => emit_object_name(name),
        TableSource::Subquery { lateral, query } => format!(
            "{}({})",
            if *lateral { "LATERAL " } else { "" },
            emit_select(query)
        ),
        TableSource::Function {
            lateral,
            name,
            args,
            with_ordinality,
        } => format!(
            "{}{}({}){}",
            if *lateral { "LATERAL " } else { "" },
            emit_object_name(name),
            args.iter().map(emit_expr).collect::<Vec<_>>().join(", "),
            if *with_ordinality {
                " WITH ORDINALITY"
            } else {
                ""
            }
        ),
    };
    let alias = match &tr.alias {
        Some(a) if tr.column_aliases.is_empty() => format!(" AS {}", emit_ident(a)),
        Some(a) => format!(
            " AS {} ({})",
            emit_ident(a),
            tr.column_aliases
                .iter()
                .enumerate()
                .map(|(i, c)| match tr.column_types.get(i) {
                    Some(t) => format!("{} {}", emit_ident(c), emit_data_type(t)),
                    None => emit_ident(c),
                })
                .collect::<Vec<_>>()
                .join(", ")
        ),
        None => String::new(),
    };
    format!("{source}{alias}")
}

fn emit_target(t: &TargetTable) -> String {
    match &t.alias {
        Some(a) => format!("{} AS {}", emit_object_name(&t.name), emit_ident(a)),
        None => emit_object_name(&t.name),
    }
}

fn emit_join(join: &Join) -> String {
    let kind = match join.kind {
        JoinKind::Inner => "INNER JOIN",
        JoinKind::Left => "LEFT JOIN",
        JoinKind::Right => "RIGHT JOIN",
        JoinKind::Full => "FULL JOIN",
        JoinKind::Cross => "CROSS JOIN",
    };
    let table = emit_table_ref(&join.table);
    let (natural, constraint) = match &join.constraint {
        JoinConstraint::On(expr) => ("", format!(" ON {}", emit_expr(expr))),
        JoinConstraint::Using(cols) => {
            let cols: Vec<String> = cols.iter().map(emit_ident).collect();
            ("", format!(" USING ({})", cols.join(", ")))
        }
        JoinConstraint::Natural => ("NATURAL ", String::new()),
        JoinConstraint::None => ("", String::new()),
    };
    format!("{}{} {}{}", natural, kind, table, constraint)
}

fn emit_order_by(ob: &OrderByExpr) -> String {
    let expr = emit_expr(&ob.expr);
    let dir = match ob.asc {
        Some(true) => " ASC",
        Some(false) => " DESC",
        None => "",
    };
    let nulls = match ob.nulls_first {
        Some(true) => " NULLS FIRST",
        Some(false) => " NULLS LAST",
        None => "",
    };
    format!("{}{}{}", expr, dir, nulls)
}

// ── EXPRESSIONS ───────────────────────────────────────────────────────────────

/// SQL functions called without parentheses ("SQL value functions").
const SQL_VALUE_FUNCTIONS: &[&str] = &[
    "current_catalog",
    "current_date",
    "current_role",
    "current_schema",
    "current_time",
    "current_timestamp",
    "current_user",
    "localtime",
    "localtimestamp",
    "session_user",
    "system_user",
    "user",
];

pub fn emit_expr(expr: &Expr) -> String {
    match &expr.kind {
        ExprKind::Literal(lit) => emit_literal(lit),
        ExprKind::Column(col_ref) => {
            let col = emit_ident(&col_ref.column);
            match (&col_ref.schema, &col_ref.table) {
                (Some(s), Some(t)) => format!("{}.{}.{}", emit_ident(s), emit_ident(t), col),
                (_, Some(t)) => format!("{}.{}", emit_ident(t), col),
                (_, None) => col,
            }
        }
        ExprKind::BinaryOp { left, op, right } => {
            format!(
                "({} {} {})",
                emit_expr(left),
                emit_binary_op(op),
                emit_expr(right)
            )
        }
        ExprKind::Quantified {
            left,
            op,
            quantifier,
            right,
        } => format!(
            "({} {} {} ({}))",
            emit_expr(left),
            emit_binary_op(op),
            match quantifier {
                Quantifier::Any => "ANY",
                Quantifier::All => "ALL",
            },
            emit_expr(right)
        ),
        ExprKind::UnaryOp { op, expr } => match op {
            UnaryOp::Not => format!("(NOT {})", emit_expr(expr)),
            UnaryOp::Prefix(op) => format!("({op} {})", emit_expr(expr)),
            UnaryOp::Neg => format!("(-{})", emit_expr(expr)),
            UnaryOp::IsNull => format!("({} IS NULL)", emit_expr(expr)),
            UnaryOp::IsNotNull => format!("({} IS NOT NULL)", emit_expr(expr)),
        },
        ExprKind::FunctionCall {
            name,
            args,
            order_by,
            filter,
            distinct,
            within_group,
            over,
        } => {
            let fn_name = emit_routine_name(name);
            // `ARRAY[…]` is represented as a call to `array`.
            if name.0.len() == 1 && !name.0[0].quoted && name.0[0].value == "array" {
                let items: Vec<String> = args.iter().map(emit_expr).collect();
                return format!("ARRAY[{}]", items.join(", "));
            }
            // SQL value functions (`current_user`, `current_date`…): keywords,
            // which PostgreSQL rejects when followed by `()`.
            if args.is_empty()
                && over.is_none()
                && filter.is_none()
                && name.0.len() == 1
                && !name.0[0].quoted
                && SQL_VALUE_FUNCTIONS.contains(&name.0[0].value.as_str())
            {
                return fn_name;
            }
            let distinct_kw = if *distinct { "DISTINCT " } else { "" };
            let args_str: Vec<String> = args.iter().map(emit_expr).collect();
            let order_str = if order_by.is_empty() {
                String::new()
            } else {
                let o: Vec<String> = order_by.iter().map(emit_order_by).collect();
                format!(" ORDER BY {}", o.join(", "))
            };
            let within_str = if within_group.is_empty() {
                String::new()
            } else {
                let o: Vec<String> = within_group.iter().map(emit_order_by).collect();
                format!(" WITHIN GROUP (ORDER BY {})", o.join(", "))
            };
            let filter_str = match filter {
                Some(f) => format!(" FILTER (WHERE {})", emit_expr(f)),
                None => String::new(),
            };
            let over_str = match over.as_deref() {
                None => String::new(),
                Some(Window::Named(n)) => format!(" OVER {}", emit_ident(n)),
                Some(Window::Spec(spec)) => format!(" OVER ({})", emit_window_spec(spec)),
            };
            format!(
                "{}({}{}{}){}{}{}",
                fn_name,
                distinct_kw,
                args_str.join(", "),
                order_str,
                within_str,
                filter_str,
                over_str
            )
        }
        ExprKind::Cast { expr, data_type } => {
            format!("CAST({} AS {})", emit_expr(expr), emit_data_type(data_type))
        }
        ExprKind::Case {
            operand,
            when_clauses,
            else_clause,
        } => {
            let mut s = "CASE".to_string();
            if let Some(op) = operand {
                s.push(' ');
                s.push_str(&emit_expr(op));
            }
            for (when, then) in when_clauses {
                s.push_str(&format!(
                    " WHEN {} THEN {}",
                    emit_expr(when),
                    emit_expr(then)
                ));
            }
            if let Some(el) = else_clause {
                s.push_str(&format!(" ELSE {}", emit_expr(el)));
            }
            s.push_str(" END");
            s
        }
        ExprKind::Subquery(sel) => format!("({})", emit_select(sel)),
        ExprKind::InSubquery {
            expr,
            subquery,
            negated,
        } => {
            let not = if *negated { " NOT" } else { "" };
            format!(
                "({}{} IN ({}))",
                emit_expr(expr),
                not,
                emit_select(subquery)
            )
        }
        ExprKind::InList {
            expr,
            list,
            negated,
        } => {
            let not = if *negated { " NOT" } else { "" };
            let items: Vec<String> = list.iter().map(emit_expr).collect();
            format!("({}{} IN ({}))", emit_expr(expr), not, items.join(", "))
        }
        ExprKind::Between {
            expr,
            low,
            high,
            negated,
        } => {
            let not = if *negated { " NOT" } else { "" };
            format!(
                "({}{} BETWEEN {} AND {})",
                emit_expr(expr),
                not,
                emit_expr(low),
                emit_expr(high)
            )
        }
        ExprKind::Wildcard => "*".to_string(),
        ExprKind::QualifiedWildcard(ident) => format!("{}.*", emit_ident(ident)),

        // ── Added constructs ──────────────────────────────────────────────────
        ExprKind::Exists { subquery, negated } => format!(
            "{}EXISTS ({})",
            if *negated { "NOT " } else { "" },
            emit_select(subquery)
        ),
        ExprKind::Interval(lit) => lit.clone(),
        ExprKind::AtTimeZone {
            timestamp,
            time_zone,
        } => format!(
            "{} AT TIME ZONE {}",
            emit_expr(timestamp),
            emit_expr(time_zone)
        ),
        ExprKind::Collate { expr, collation } => {
            format!("{} COLLATE {}", emit_expr(expr), collation)
        }
        ExprKind::Tuple(items) => format!(
            "({})",
            items.iter().map(emit_expr).collect::<Vec<_>>().join(", ")
        ),
        ExprKind::FieldAccess { root, path } => {
            format!("{}{}", emit_expr(root), path.join(""))
        }
        ExprKind::GroupingSets { kind, sets } => {
            let keyword = match kind {
                crate::expr::GroupingKind::Rollup => "ROLLUP",
                crate::expr::GroupingKind::Cube => "CUBE",
                crate::expr::GroupingKind::Sets => "GROUPING SETS",
            };
            let sets: Vec<String> = sets
                .iter()
                .map(|s| {
                    format!(
                        "({})",
                        s.iter().map(emit_expr).collect::<Vec<_>>().join(", ")
                    )
                })
                .collect();
            format!("{keyword} ({})", sets.join(", "))
        }
        ExprKind::Parameter(p) => p.clone(),
        ExprKind::NamedArgument { name, value } => {
            format!("{} => {}", emit_ident(name), emit_expr(value))
        }

        // An unmodeled expression cannot be re-emitted faithfully.
        //
        // This emitter feeds migration generation: producing approximate SQL
        // here would be deployed as is. So we emit a deliberately INVALID
        // construct, which breaks on re-parse rather than passing for
        // correct SQL. `emit_contains_unsupported` lets the caller detect it
        // before writing.
        ExprKind::Unsupported(what) => {
            format!("{UNSUPPORTED_MARKER}/* {what} */")
        }
    }
}

/// Marker emitted in place of an unmodeled expression.
///
/// Deliberately syntactically invalid: a migration containing it must
/// fail loudly, not be deployed.
pub const UNSUPPORTED_MARKER: &str = "$$PGSQL_UNSUPPORTED$$";

/// True if `sql` contains an expression the AST did not model.
///
/// To check before writing a migration file: the produced SQL is then
/// not deployable.
pub fn emit_contains_unsupported(sql: &str) -> bool {
    sql.contains(UNSUPPORTED_MARKER)
}

fn emit_literal(lit: &Literal) -> String {
    match lit {
        Literal::Number(n) => n.clone(),
        Literal::String(s) => format!("'{}'", s.replace('\'', "''")),
        Literal::Boolean(b) => if *b { "TRUE" } else { "FALSE" }.to_string(),
        Literal::Null => "NULL".to_string(),
        Literal::BitString(b) => match b.split_at(1) {
            ("x", digits) => format!("X'{digits}'"),
            (_, digits) => format!("B'{digits}'"),
        },
    }
}

fn emit_binary_op(op: &BinaryOp) -> std::borrow::Cow<'static, str> {
    match op {
        BinaryOp::Eq => "=".into(),
        BinaryOp::NotEq => "<>".into(),
        BinaryOp::Lt => "<".into(),
        BinaryOp::Lte => "<=".into(),
        BinaryOp::Gt => ">".into(),
        BinaryOp::Gte => ">=".into(),
        BinaryOp::And => "AND".into(),
        BinaryOp::Concat => "||".into(),
        BinaryOp::Or => "OR".into(),
        BinaryOp::Plus => "+".into(),
        BinaryOp::Minus => "-".into(),
        BinaryOp::Multiply => "*".into(),
        BinaryOp::Divide => "/".into(),
        BinaryOp::Modulo => "%".into(),
        BinaryOp::In => "IN".into(),
        BinaryOp::NotIn => "NOT IN".into(),
        BinaryOp::Like => "LIKE".into(),
        BinaryOp::NotLike => "NOT LIKE".into(),
        BinaryOp::ILike => "ILIKE".into(),
        BinaryOp::Is => "IS".into(),
        BinaryOp::IsNot => "IS NOT".into(),
        BinaryOp::IsDistinctFrom => "IS DISTINCT FROM".into(),
        BinaryOp::IsNotDistinctFrom => "IS NOT DISTINCT FROM".into(),
        BinaryOp::Custom(s) => s.clone().into(),
    }
}

// ── DDL ───────────────────────────────────────────────────────────────────────

pub fn emit_ddl(ddl: &DdlStatement) -> String {
    match ddl {
        DdlStatement::CreateTable(ct) => emit_create_table(ct),
        DdlStatement::CreateIndex(ci) => emit_create_index(ci),
        DdlStatement::CreateFunction(cf) => emit_create_function(cf),
        DdlStatement::CreateProcedure(cp) => emit_create_procedure(cp),
        DdlStatement::CreateTrigger(ct) => emit_create_trigger(ct),
        DdlStatement::CreatePolicy(cp) => emit_create_policy(cp),
        DdlStatement::AlterTable(at) => emit_alter_table(at),
        DdlStatement::CreateType(ct) => emit_create_type(ct),
        DdlStatement::AlterType(at) => emit_alter_type(at),
        DdlStatement::CreateDomain(cd) => emit_create_domain(cd),
        DdlStatement::CreateExtension(ce) => emit_create_extension(ce),
        DdlStatement::AlterExtension(ae) => emit_alter_extension(ae),
        DdlStatement::CreateSchema(cs) => format!(
            "CREATE SCHEMA {}{}{};",
            if cs.if_not_exists {
                "IF NOT EXISTS "
            } else {
                ""
            },
            emit_ident(&cs.name),
            cs.authorization
                .as_ref()
                .map(|a| format!(" AUTHORIZATION {}", emit_ident(a)))
                .unwrap_or_default()
        ),
        DdlStatement::CreateView(cv) => emit_create_view(cv),
        DdlStatement::CreateSequence(cs) => emit_create_sequence(cs),
        DdlStatement::Drop(d) => emit_drop(d),
    }
}

pub fn emit_create_table(ct: &CreateTable) -> String {
    let if_not_exists = if ct.if_not_exists {
        "IF NOT EXISTS "
    } else {
        ""
    };
    let kind = if ct.temporary {
        "TEMPORARY "
    } else if ct.unlogged {
        "UNLOGGED "
    } else {
        ""
    };
    // Columns and `LIKE` interleaved: the position of a `LIKE` sets the
    // order of the table's columns.
    let mut parts: Vec<String> = Vec::new();
    for (i, col) in ct.columns.iter().enumerate() {
        for like in ct.likes.iter().filter(|l| l.position == i) {
            parts.push(emit_table_like(like));
        }
        parts.push(emit_column_def(col));
    }
    for like in ct.likes.iter().filter(|l| l.position >= ct.columns.len()) {
        parts.push(emit_table_like(like));
    }
    for constraint in &ct.constraints {
        parts.push(emit_table_constraint(constraint));
    }
    let on_commit = match ct.on_commit {
        None => "",
        Some(OnCommit::PreserveRows) => " ON COMMIT PRESERVE ROWS",
        Some(OnCommit::DeleteRows) => " ON COMMIT DELETE ROWS",
        Some(OnCommit::Drop) => " ON COMMIT DROP",
    };
    if let Some(q) = &ct.as_query {
        let names = if ct.columns.is_empty() {
            String::new()
        } else {
            format!(
                " ({})",
                ct.columns
                    .iter()
                    .map(|c| emit_ident(&c.name))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        return format!(
            "CREATE {}TABLE {}{}{}{} AS\n{};",
            kind,
            if_not_exists,
            emit_object_name(&ct.name),
            names,
            on_commit,
            emit_select(q)
        );
    }
    format!(
        "CREATE {}TABLE {}{} (\n  {}\n){};",
        kind,
        if_not_exists,
        emit_object_name(&ct.name),
        parts.join(",\n  "),
        on_commit
    )
}

fn emit_table_like(like: &TableLike) -> String {
    let mut out = format!("LIKE {}", emit_object_name(&like.source));
    for o in &like.options {
        out.push_str(if o.including {
            " INCLUDING "
        } else {
            " EXCLUDING "
        });
        out.push_str(o.feature.keyword());
    }
    out
}

pub fn emit_column_def(col: &ColumnDef) -> String {
    let mut parts = vec![emit_ident(&col.name), emit_data_type(&col.data_type)];
    if let Some(g) = &col.generated {
        parts.push(format!(
            "GENERATED ALWAYS AS ({}) {}",
            emit_expr(&g.expr),
            if g.stored { "STORED" } else { "VIRTUAL" }
        ));
    }
    match col.identity {
        None => {}
        Some(Identity::Always) => parts.push("GENERATED ALWAYS AS IDENTITY".to_string()),
        Some(Identity::ByDefault) => parts.push("GENERATED BY DEFAULT AS IDENTITY".to_string()),
    }
    // An identity column is implicitly NOT NULL: do not repeat it.
    if !col.nullable && col.identity.is_none() {
        // Only emit NOT NULL if there is no explicit PrimaryKey constraint
        // (PK implies NOT NULL).
        let has_pk = col
            .constraints
            .iter()
            .any(|c| matches!(c, ColumnConstraint::PrimaryKey));
        if !has_pk {
            parts.push("NOT NULL".to_string());
        }
    }
    for constraint in &col.constraints {
        match constraint {
            ColumnConstraint::PrimaryKey => parts.push("PRIMARY KEY".to_string()),
            ColumnConstraint::Unique => parts.push("UNIQUE".to_string()),
            ColumnConstraint::NotNull => {} // already handled above
            ColumnConstraint::Check(expr) => parts.push(format!("CHECK ({})", emit_expr(expr))),
            ColumnConstraint::References {
                table,
                column,
                on_delete,
                on_update,
            } => {
                let col_ref = match column {
                    Some(c) => format!(" ({})", emit_ident(c)),
                    None => String::new(),
                };
                let on_del = on_delete
                    .as_ref()
                    .map(|a| format!(" ON DELETE {}", emit_referential_action(a)))
                    .unwrap_or_default();
                let on_upd = on_update
                    .as_ref()
                    .map(|a| format!(" ON UPDATE {}", emit_referential_action(a)))
                    .unwrap_or_default();
                parts.push(format!(
                    "REFERENCES {}{}{}{}",
                    emit_object_name(table),
                    col_ref,
                    on_del,
                    on_upd
                ));
            }
            ColumnConstraint::Default(expr) => parts.push(format!("DEFAULT {}", emit_expr(expr))),
        }
    }
    parts.join(" ")
}

pub fn emit_table_constraint(tc: &TableConstraint) -> String {
    let named = tc
        .name()
        .map(|n| format!("CONSTRAINT {} ", emit_ident(n)))
        .unwrap_or_default();
    let body = match tc {
        TableConstraint::PrimaryKey { columns, .. } => {
            let cols: Vec<String> = columns.iter().map(emit_ident).collect();
            format!("PRIMARY KEY ({})", cols.join(", "))
        }
        TableConstraint::Unique { columns, .. } => {
            let cols: Vec<String> = columns.iter().map(emit_ident).collect();
            format!("UNIQUE ({})", cols.join(", "))
        }
        TableConstraint::ForeignKey {
            columns,
            ref_table,
            ref_columns,
            on_delete,
            on_update,
            ..
        } => {
            let cols: Vec<String> = columns.iter().map(emit_ident).collect();
            let ref_cols: Vec<String> = ref_columns.iter().map(emit_ident).collect();
            let on_del = on_delete
                .as_ref()
                .map(|a| format!(" ON DELETE {}", emit_referential_action(a)))
                .unwrap_or_default();
            let on_upd = on_update
                .as_ref()
                .map(|a| format!(" ON UPDATE {}", emit_referential_action(a)))
                .unwrap_or_default();
            format!(
                "FOREIGN KEY ({}) REFERENCES {} ({}){}{}",
                cols.join(", "),
                emit_object_name(ref_table),
                ref_cols.join(", "),
                on_del,
                on_upd
            )
        }
        TableConstraint::Check { expr, .. } => format!("CHECK ({})", emit_expr(expr)),
        TableConstraint::Exclude {
            using,
            elements,
            where_clause,
            ..
        } => {
            let using = using
                .as_ref()
                .map(|u| format!("USING {u} "))
                .unwrap_or_default();
            let elements: Vec<String> = elements
                .iter()
                .map(|e| format!("{} WITH {}", emit_index_element(&e.element), e.operator))
                .collect();
            let where_clause = where_clause
                .as_ref()
                .map(|w| format!(" WHERE ({})", emit_expr(w)))
                .unwrap_or_default();
            format!("EXCLUDE {}({}){}", using, elements.join(", "), where_clause)
        }
    };
    format!("{named}{body}")
}

fn emit_index_element(e: &IndexElement) -> String {
    match e {
        IndexElement::Column(c) => emit_ident(c),
        IndexElement::Expr(expr) => format!("({})", emit_expr(expr)),
    }
}

fn emit_index_column(c: &IndexColumn) -> String {
    let mut out = emit_index_element(&c.element);
    if let Some(op) = &c.opclass {
        out.push(' ');
        out.push_str(&emit_object_name(op));
    }
    if c.descending {
        out.push_str(" DESC");
    }
    match c.nulls_first {
        Some(true) => out.push_str(" NULLS FIRST"),
        Some(false) => out.push_str(" NULLS LAST"),
        None => {}
    }
    out
}

fn emit_referential_action(action: &ReferentialAction) -> &'static str {
    match action {
        ReferentialAction::Cascade => "CASCADE",
        ReferentialAction::SetNull => "SET NULL",
        ReferentialAction::SetDefault => "SET DEFAULT",
        ReferentialAction::Restrict => "RESTRICT",
        ReferentialAction::NoAction => "NO ACTION",
    }
}

pub fn emit_create_index(ci: &CreateIndex) -> String {
    let unique = if ci.unique { "UNIQUE " } else { "" };
    let if_not_exists = if ci.if_not_exists {
        "IF NOT EXISTS "
    } else {
        ""
    };
    let name = ci
        .name
        .as_ref()
        .map(|n| format!("{} ", emit_ident(n)))
        .unwrap_or_default();
    let concurrently = if ci.concurrently { "CONCURRENTLY " } else { "" };
    let using = ci
        .using
        .as_ref()
        .map(|u| format!(" USING {u}"))
        .unwrap_or_default();
    let elements: Vec<String> = ci.elements.iter().map(emit_index_column).collect();
    let include = if ci.include.is_empty() {
        String::new()
    } else {
        format!(
            " INCLUDE ({})",
            ci.include
                .iter()
                .map(emit_ident)
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    let nulls = if ci.nulls_not_distinct {
        " NULLS NOT DISTINCT"
    } else {
        ""
    };
    let where_clause = ci
        .where_clause
        .as_ref()
        .map(|w| format!(" WHERE {}", emit_expr(w)))
        .unwrap_or_default();
    format!(
        "CREATE {}INDEX {}{}{}ON {}{} ({}){}{}{};",
        unique,
        concurrently,
        if_not_exists,
        name,
        emit_object_name(&ci.table),
        using,
        elements.join(", "),
        include,
        nulls,
        where_clause
    )
}

pub fn emit_create_function(cf: &CreateFunction) -> String {
    let or_replace = if cf.or_replace { "OR REPLACE " } else { "" };
    let params: Vec<String> = cf.params.iter().map(emit_function_param).collect();
    let returns = emit_return_type(&cf.return_type);
    let lang = emit_function_language(&cf.language);
    let body = emit_function_body(&cf.body);
    let mut options = String::new();
    match cf.volatility {
        Volatility::Volatile => {}
        Volatility::Stable => options.push_str("\nSTABLE"),
        Volatility::Immutable => options.push_str("\nIMMUTABLE"),
    }
    if cf.strict {
        options.push_str("\nSTRICT");
    }
    if cf.leakproof {
        options.push_str("\nLEAKPROOF");
    }
    if cf.security_definer {
        options.push_str("\nSECURITY DEFINER");
    }
    match cf.parallel {
        None => {}
        Some(Parallel::Unsafe) => options.push_str("\nPARALLEL UNSAFE"),
        Some(Parallel::Restricted) => options.push_str("\nPARALLEL RESTRICTED"),
        Some(Parallel::Safe) => options.push_str("\nPARALLEL SAFE"),
    }
    if let Some(cost) = cf.cost {
        options.push_str(&format!("\nCOST {cost}"));
    }
    if let Some(rows) = cf.rows {
        options.push_str(&format!("\nROWS {rows}"));
    }
    options.push_str(&emit_settings(&cf.settings));
    format!(
        "CREATE {}FUNCTION {}({}){}\nLANGUAGE {}{}\nAS {};",
        or_replace,
        emit_object_name(&cf.name),
        params.join(", "),
        returns,
        lang,
        options,
        body
    )
}

pub fn emit_create_procedure(cp: &CreateProcedure) -> String {
    let or_replace = if cp.or_replace { "OR REPLACE " } else { "" };
    let params: Vec<String> = cp.params.iter().map(emit_function_param).collect();
    let lang = emit_function_language(&cp.language);
    let body = emit_function_body(&cp.body);
    let mut options = String::new();
    if cp.security_definer {
        options.push_str("\nSECURITY DEFINER");
    }
    options.push_str(&emit_settings(&cp.settings));
    format!(
        "CREATE {}PROCEDURE {}({})\nLANGUAGE {}{}\nAS {};",
        or_replace,
        emit_object_name(&cp.name),
        params.join(", "),
        lang,
        options,
        body
    )
}

fn emit_settings(settings: &[FunctionSetting]) -> String {
    settings
        .iter()
        .map(|s| match &s.value {
            SettingValue::FromCurrent => format!("\nSET {} FROM CURRENT", s.name),
            SettingValue::Values(v) if v.is_empty() => format!("\nSET {} TO DEFAULT", s.name),
            SettingValue::Values(v) => format!(
                "\nSET {} TO {}",
                s.name,
                v.iter()
                    .map(|x| format!("'{}'", x.replace('\'', "''")))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        })
        .collect()
}

fn emit_function_param(param: &FunctionParam) -> String {
    let mode = match param.mode {
        ParamMode::In => "",
        ParamMode::Out => "OUT ",
        ParamMode::InOut => "INOUT ",
        ParamMode::Variadic => "VARIADIC ",
    };
    let name = param
        .name
        .as_ref()
        .map(|n| format!("{} ", emit_ident(n)))
        .unwrap_or_default();
    let default = param
        .default
        .as_ref()
        .map(|d| format!(" DEFAULT {}", emit_expr(d)))
        .unwrap_or_default();
    format!(
        "{}{}{}{}",
        mode,
        name,
        emit_data_type(&param.data_type),
        default
    )
}

/// Full `RETURNS …` clause, empty without a clause (`OutParams`).
fn emit_return_type(rt: &ReturnType) -> String {
    let t = match rt {
        ReturnType::OutParams => return String::new(),
        ReturnType::Scalar(dt) => emit_data_type(dt),
        ReturnType::Table(cols) => {
            let cols: Vec<String> = cols
                .iter()
                .map(|(name, dt)| format!("{} {}", emit_ident(name), emit_data_type(dt)))
                .collect();
            format!("TABLE({})", cols.join(", "))
        }
        ReturnType::SetOf(dt) => format!("SETOF {}", emit_data_type(dt)),
        ReturnType::Trigger => "TRIGGER".to_string(),
        ReturnType::Void => "VOID".to_string(),
    };
    format!("\nRETURNS {t}")
}

fn emit_function_language(lang: &FunctionLanguage) -> &str {
    match lang {
        FunctionLanguage::Sql => "sql",
        FunctionLanguage::PlPgSql => "plpgsql",
        FunctionLanguage::C => "c",
        FunctionLanguage::Internal => "internal",
        FunctionLanguage::Other(s) => s.as_str(),
    }
}

fn emit_function_body(body: &FunctionBody) -> String {
    match body {
        FunctionBody::Raw(s) => dollar_quote(s),
        FunctionBody::Parsed(b) => dollar_quote(&b.source),
        FunctionBody::SqlQuery(sel) => dollar_quote(&format!("{}\n", emit_select(sel))),
        FunctionBody::Sql(stmts) => dollar_quote(&format!(
            "\n{}\n",
            stmts
                .iter()
                .map(emit_statement)
                .collect::<Vec<_>>()
                .join("\n")
        )),
        FunctionBody::SqlReturn(e) => dollar_quote(&format!("SELECT {}", emit_expr(e))),
    }
}

/// `$$text$$`, or `$pgzestN$text$pgzestN$` if the text already contains the
/// delimiter: the body must come out intact, whatever it contains.
fn dollar_quote(text: &str) -> String {
    let mut tag = String::new();
    let mut n = 0;
    while text.contains(&format!("${tag}$")) {
        n += 1;
        tag = format!("pgzest{n}");
    }
    format!("${tag}${text}${tag}$")
}

pub fn emit_create_trigger(ct: &CreateTrigger) -> String {
    let timing = match ct.timing {
        TriggerTiming::Before => "BEFORE",
        TriggerTiming::After => "AFTER",
        TriggerTiming::InsteadOf => "INSTEAD OF",
    };
    let events: Vec<String> = ct.events.iter().map(emit_trigger_event).collect();
    let for_each = match ct.for_each {
        TriggerForEach::Row => "ROW",
        TriggerForEach::Statement => "STATEMENT",
    };
    let when = ct
        .when
        .as_ref()
        .map(|w| format!(" WHEN ({})", emit_expr(w)))
        .unwrap_or_default();
    let referencing = if ct.referencing.is_empty() {
        String::new()
    } else {
        let items: Vec<String> = ct
            .referencing
            .iter()
            .map(|t| {
                format!(
                    "{} TABLE AS {}",
                    if t.new { "NEW" } else { "OLD" },
                    emit_ident(&t.name)
                )
            })
            .collect();
        format!("\nREFERENCING {}", items.join(" "))
    };
    format!(
        "CREATE {}TRIGGER {}\n{} {} ON {}{}\nFOR EACH {}{}\nEXECUTE FUNCTION {}();",
        if ct.or_replace { "OR REPLACE " } else { "" },
        emit_ident(&ct.name),
        timing,
        events.join(" OR "),
        emit_object_name(&ct.table),
        referencing,
        for_each,
        when,
        emit_object_name(&ct.function)
    )
}

fn emit_trigger_event(event: &TriggerEvent) -> String {
    match event {
        TriggerEvent::Insert => "INSERT".to_string(),
        TriggerEvent::Update(cols) => {
            if cols.is_empty() {
                "UPDATE".to_string()
            } else {
                let cols: Vec<String> = cols.iter().map(emit_ident).collect();
                format!("UPDATE OF {}", cols.join(", "))
            }
        }
        TriggerEvent::Delete => "DELETE".to_string(),
        TriggerEvent::Truncate => "TRUNCATE".to_string(),
    }
}

pub fn emit_create_policy(cp: &CreatePolicy) -> String {
    let permissive = if cp.permissive {
        "PERMISSIVE"
    } else {
        "RESTRICTIVE"
    };
    let command = match cp.command {
        PolicyCommand::All => "ALL",
        PolicyCommand::Select => "SELECT",
        PolicyCommand::Insert => "INSERT",
        PolicyCommand::Update => "UPDATE",
        PolicyCommand::Delete => "DELETE",
    };
    let roles = if cp.roles.is_empty() {
        String::new()
    } else {
        let roles: Vec<String> = cp.roles.iter().map(emit_ident).collect();
        format!("\n  TO {}", roles.join(", "))
    };
    let using = cp
        .using
        .as_ref()
        .map(|e| format!("\n  USING ({})", emit_expr(e)))
        .unwrap_or_default();
    let with_check = cp
        .with_check
        .as_ref()
        .map(|e| format!("\n  WITH CHECK ({})", emit_expr(e)))
        .unwrap_or_default();
    format!(
        "CREATE POLICY {} ON {}\n  AS {} FOR {}{}{}{}",
        emit_ident(&cp.name),
        emit_object_name(&cp.table),
        permissive,
        command,
        roles,
        using,
        with_check
    )
}

fn emit_alter_table(at: &AlterTable) -> String {
    let ops: Vec<String> = at.operations.iter().map(emit_alter_table_op).collect();
    format!(
        "ALTER TABLE {}{}{}\n  {};",
        if at.if_exists { "IF EXISTS " } else { "" },
        if at.only { "ONLY " } else { "" },
        emit_object_name(&at.name),
        ops.join(",\n  ")
    )
}

fn emit_alter_table_op(op: &AlterTableOp) -> String {
    let cascade_kw = |c: bool| if c { " CASCADE" } else { "" };
    match op {
        AlterTableOp::EnableRowLevelSecurity => "ENABLE ROW LEVEL SECURITY".to_string(),
        AlterTableOp::DisableRowLevelSecurity => "DISABLE ROW LEVEL SECURITY".to_string(),
        AlterTableOp::ForceRowLevelSecurity => "FORCE ROW LEVEL SECURITY".to_string(),
        AlterTableOp::NoForceRowLevelSecurity => "NO FORCE ROW LEVEL SECURITY".to_string(),
        AlterTableOp::AddColumn {
            column,
            if_not_exists,
        } => format!(
            "ADD COLUMN {}{}",
            if *if_not_exists { "IF NOT EXISTS " } else { "" },
            emit_column_def(column)
        ),
        AlterTableOp::DropColumn {
            name,
            if_exists,
            cascade,
        } => {
            let ie = if *if_exists { "IF EXISTS " } else { "" };
            format!(
                "DROP COLUMN {}{}{}",
                ie,
                emit_ident(name),
                cascade_kw(*cascade)
            )
        }
        AlterTableOp::AlterColumn { name, action } => {
            let action_str = match action {
                AlterColumnAction::SetDefault(expr) => {
                    format!("SET DEFAULT {}", emit_expr(expr))
                }
                AlterColumnAction::DropDefault => "DROP DEFAULT".to_string(),
                AlterColumnAction::SetNotNull => "SET NOT NULL".to_string(),
                AlterColumnAction::DropNotNull => "DROP NOT NULL".to_string(),
                AlterColumnAction::SetDataType { data_type, using } => match using {
                    Some(u) => format!(
                        "SET DATA TYPE {} USING {}",
                        emit_data_type(data_type),
                        emit_expr(u)
                    ),
                    None => format!("SET DATA TYPE {}", emit_data_type(data_type)),
                },
            };
            format!("ALTER COLUMN {} {}", emit_ident(name), action_str)
        }
        AlterTableOp::AddConstraint {
            constraint,
            not_valid,
        } => format!(
            "ADD {}{}",
            emit_table_constraint(constraint),
            if *not_valid { " NOT VALID" } else { "" }
        ),
        AlterTableOp::DropConstraint {
            name,
            if_exists,
            cascade,
        } => {
            let ie = if *if_exists { "IF EXISTS " } else { "" };
            format!(
                "DROP CONSTRAINT {}{}{}",
                ie,
                emit_ident(name),
                cascade_kw(*cascade)
            )
        }
        AlterTableOp::RenameColumn { from, to } => {
            format!("RENAME COLUMN {} TO {}", emit_ident(from), emit_ident(to))
        }
        AlterTableOp::RenameTo(name) => format!("RENAME TO {}", emit_ident(name)),
        AlterTableOp::SetSchema(schema) => format!("SET SCHEMA {}", emit_ident(schema)),
        AlterTableOp::ValidateConstraint(name) => {
            format!("VALIDATE CONSTRAINT {}", emit_ident(name))
        }
    }
}

// ── Types, domains, extensions, views, sequences, DROP ────────────────────────

fn emit_string_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

pub fn emit_create_type(ct: &CreateType) -> String {
    let body = match &ct.kind {
        TypeDefinition::Enum(values) => format!(
            "ENUM ({})",
            values
                .iter()
                .map(|v| emit_string_literal(v))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        TypeDefinition::Composite(fields) => format!(
            "({})",
            fields
                .iter()
                .map(|(n, t)| format!("{} {}", emit_ident(n), emit_data_type(t)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };
    format!("CREATE TYPE {} AS {};", emit_object_name(&ct.name), body)
}

pub fn emit_alter_type(at: &AlterType) -> String {
    let op = match &at.operation {
        AlterTypeOp::AddValue {
            value,
            if_not_exists,
            position,
        } => {
            let position = match position {
                None => String::new(),
                Some(EnumValuePosition::Before(v)) => format!(" BEFORE {}", emit_string_literal(v)),
                Some(EnumValuePosition::After(v)) => format!(" AFTER {}", emit_string_literal(v)),
            };
            format!(
                "ADD VALUE {}{}{}",
                if *if_not_exists { "IF NOT EXISTS " } else { "" },
                emit_string_literal(value),
                position
            )
        }
        AlterTypeOp::RenameValue { from, to } => format!(
            "RENAME VALUE {} TO {}",
            emit_string_literal(from),
            emit_string_literal(to)
        ),
        AlterTypeOp::RenameTo(name) => format!("RENAME TO {}", emit_ident(name)),
        AlterTypeOp::SetSchema(schema) => format!("SET SCHEMA {}", emit_ident(schema)),
    };
    format!("ALTER TYPE {} {};", emit_object_name(&at.name), op)
}

pub fn emit_create_domain(cd: &CreateDomain) -> String {
    let mut out = format!(
        "CREATE DOMAIN {} AS {}",
        emit_object_name(&cd.name),
        emit_data_type(&cd.data_type)
    );
    if let Some(d) = &cd.default {
        out.push_str(&format!(" DEFAULT {}", emit_expr(d)));
    }
    if cd.not_null {
        out.push_str(" NOT NULL");
    }
    for check in &cd.checks {
        if let Some(n) = &check.name {
            out.push_str(&format!(" CONSTRAINT {}", emit_ident(n)));
        }
        out.push_str(&format!(" CHECK ({})", emit_expr(&check.expr)));
    }
    out.push(';');
    out
}

pub fn emit_create_extension(ce: &CreateExtension) -> String {
    let mut out = format!(
        "CREATE EXTENSION {}{}",
        if ce.if_not_exists {
            "IF NOT EXISTS "
        } else {
            ""
        },
        emit_ident(&ce.name)
    );
    if let Some(s) = &ce.schema {
        out.push_str(&format!(" SCHEMA {}", emit_ident(s)));
    }
    if let Some(v) = &ce.version {
        out.push_str(&format!(" VERSION {}", emit_string_literal(v)));
    }
    if ce.cascade {
        out.push_str(" CASCADE");
    }
    out.push(';');
    out
}

pub fn emit_alter_extension(ae: &AlterExtension) -> String {
    let op = match &ae.operation {
        AlterExtensionOp::Update(None) => "UPDATE".to_string(),
        AlterExtensionOp::Update(Some(v)) => format!("UPDATE TO {}", emit_string_literal(v)),
        AlterExtensionOp::SetSchema(s) => format!("SET SCHEMA {}", emit_ident(s)),
    };
    format!("ALTER EXTENSION {} {};", emit_ident(&ae.name), op)
}

pub fn emit_create_view(cv: &CreateView) -> String {
    let columns = if cv.columns.is_empty() {
        String::new()
    } else {
        format!(
            " ({})",
            cv.columns
                .iter()
                .map(emit_ident)
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    format!(
        "CREATE {}{}{}VIEW {}{} AS\n{};",
        if cv.or_replace { "OR REPLACE " } else { "" },
        if cv.temporary { "TEMPORARY " } else { "" },
        if cv.materialized { "MATERIALIZED " } else { "" },
        emit_object_name(&cv.name),
        columns,
        emit_select(&cv.query)
    )
}

pub fn emit_create_sequence(cs: &CreateSequence) -> String {
    let mut out = format!(
        "CREATE {}SEQUENCE {}{}",
        if cs.temporary { "TEMPORARY " } else { "" },
        if cs.if_not_exists {
            "IF NOT EXISTS "
        } else {
            ""
        },
        emit_object_name(&cs.name)
    );
    if let Some(t) = &cs.data_type {
        out.push_str(&format!(" AS {}", emit_data_type(t)));
    }
    let numbers = [
        ("INCREMENT BY", cs.increment),
        ("MINVALUE", cs.min_value),
        ("MAXVALUE", cs.max_value),
        ("START WITH", cs.start),
        ("CACHE", cs.cache),
    ];
    for (kw, value) in numbers {
        if let Some(v) = value {
            out.push_str(&format!(" {kw} {v}"));
        }
    }
    if cs.cycle {
        out.push_str(" CYCLE");
    }
    if let Some(owner) = &cs.owned_by {
        out.push_str(&format!(" OWNED BY {}", emit_object_name(owner)));
    }
    out.push(';');
    out
}

pub fn emit_drop(d: &DropObject) -> String {
    let kind = match d.kind {
        ObjectKind::Table => "TABLE",
        ObjectKind::View => "VIEW",
        ObjectKind::MaterializedView => "MATERIALIZED VIEW",
        ObjectKind::Index => "INDEX",
        ObjectKind::Sequence => "SEQUENCE",
        ObjectKind::Type => "TYPE",
        ObjectKind::Domain => "DOMAIN",
        ObjectKind::Function => "FUNCTION",
        ObjectKind::Procedure => "PROCEDURE",
        ObjectKind::Trigger => "TRIGGER",
        ObjectKind::Policy => "POLICY",
        ObjectKind::Schema => "SCHEMA",
        ObjectKind::Extension => "EXTENSION",
    };
    let on = d
        .on_table
        .as_ref()
        .map(|t| format!(" ON {}", emit_object_name(t)))
        .unwrap_or_default();
    format!(
        "DROP {} {}{}{}{};",
        kind,
        if d.if_exists { "IF EXISTS " } else { "" },
        d.names
            .iter()
            .enumerate()
            .map(|(i, n)| match d.signatures.get(i) {
                Some(Some(types)) => format!(
                    "{}({})",
                    emit_object_name(n),
                    types
                        .iter()
                        .map(emit_data_type)
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                _ => emit_object_name(n),
            })
            .collect::<Vec<_>>()
            .join(", "),
        on,
        if d.cascade { " CASCADE" } else { "" }
    )
}

// ── INSERT / UPDATE / DELETE ──────────────────────────────────────────────────

fn emit_insert(ins: &InsertStmt) -> String {
    let cols = if ins.columns.is_empty() {
        String::new()
    } else {
        let cols: Vec<String> = ins.columns.iter().map(emit_ident).collect();
        format!(" ({})", cols.join(", "))
    };
    let source = match &ins.source {
        InsertSource::Values(rows) => {
            let rows: Vec<String> = rows
                .iter()
                .map(|row| {
                    let vals: Vec<String> = row.iter().map(emit_expr).collect();
                    format!("({})", vals.join(", "))
                })
                .collect();
            format!("VALUES {}", rows.join(", "))
        }
        InsertSource::Select(sel) => emit_select(sel),
        InsertSource::DefaultValues => "DEFAULT VALUES".to_string(),
    };
    let on_conflict = ins
        .on_conflict
        .as_ref()
        .map(emit_on_conflict)
        .unwrap_or_default();
    let returning = emit_returning(&ins.returning, &ins.returning_aliases);
    format!(
        "{}INSERT INTO {}{}{} {}{}{}",
        emit_with(&ins.with, ins.with_recursive),
        emit_object_name(&ins.table),
        ins.alias
            .as_ref()
            .map(|a| format!(" AS {}", emit_ident(a)))
            .unwrap_or_default(),
        cols,
        source,
        on_conflict,
        returning
    )
}

fn emit_assignment(a: &Assignment) -> String {
    let target = match a.columns.as_slice() {
        [one] => std::iter::once(one)
            .chain(&a.subfields)
            .map(emit_ident)
            .collect::<Vec<_>>()
            .join("."),
        many => format!(
            "({})",
            many.iter().map(emit_ident).collect::<Vec<_>>().join(", ")
        ),
    };
    format!("{} = {}", target, emit_expr(&a.value))
}

fn emit_on_conflict(oc: &OnConflict) -> String {
    let target = match &oc.target {
        OnConflictTarget::Column(cols) => {
            let cols: Vec<String> = cols.iter().map(emit_ident).collect();
            format!(" ({})", cols.join(", "))
        }
        OnConflictTarget::Constraint(name) => format!(" ON CONSTRAINT {}", emit_ident(name)),
        OnConflictTarget::None => String::new(),
    };
    let action = match &oc.action {
        OnConflictAction::DoNothing => "DO NOTHING".to_string(),
        OnConflictAction::Update {
            assignments,
            where_clause,
        } => {
            let assigns: Vec<String> = assignments.iter().map(emit_assignment).collect();
            let where_str = where_clause
                .as_ref()
                .map(|w| format!(" WHERE {}", emit_expr(w)))
                .unwrap_or_default();
            format!("DO UPDATE SET {}{}", assigns.join(", "), where_str)
        }
    };
    format!(" ON CONFLICT{} {}", target, action)
}

fn emit_update(upd: &UpdateStmt) -> String {
    let assigns: Vec<String> = upd.assignments.iter().map(emit_assignment).collect();
    let from = if upd.from.is_empty() {
        String::new()
    } else {
        let tables: Vec<String> = upd.from.iter().map(emit_table_ref).collect();
        let joins: String = upd
            .joins
            .iter()
            .map(|j| format!(" {}", emit_join(j)))
            .collect();
        format!(" FROM {}{}", tables.join(", "), joins)
    };
    let where_str = upd
        .where_clause
        .as_ref()
        .map(|w| format!(" WHERE {}", emit_expr(w)))
        .unwrap_or_default();
    let returning = emit_returning(&upd.returning, &upd.returning_aliases);
    format!(
        "{}UPDATE {} SET {}{}{}{}",
        emit_with(&upd.with, upd.with_recursive),
        emit_target(&upd.table),
        assigns.join(", "),
        from,
        where_str,
        returning
    )
}

fn emit_delete(del: &DeleteStmt) -> String {
    let using = if del.using.is_empty() {
        String::new()
    } else {
        let tables: Vec<String> = del.using.iter().map(emit_table_ref).collect();
        let joins: String = del
            .joins
            .iter()
            .map(|j| format!(" {}", emit_join(j)))
            .collect();
        format!(" USING {}{}", tables.join(", "), joins)
    };
    let where_str = del
        .where_clause
        .as_ref()
        .map(|w| format!(" WHERE {}", emit_expr(w)))
        .unwrap_or_default();
    let returning = emit_returning(&del.returning, &del.returning_aliases);
    format!(
        "{}DELETE FROM {}{}{}{}",
        emit_with(&del.with, del.with_recursive),
        emit_target(&del.table),
        using,
        where_str,
        returning
    )
}

fn emit_returning(items: &[SelectItem], aliases: &ReturningAliases) -> String {
    if items.is_empty() {
        return String::new();
    }
    let cols: Vec<String> = items.iter().map(emit_select_item).collect();
    let mut with: Vec<String> = Vec::new();
    if let Some(o) = &aliases.old {
        with.push(format!("OLD AS {}", emit_ident(o)));
    }
    if let Some(n) = &aliases.new {
        with.push(format!("NEW AS {}", emit_ident(n)));
    }
    let with = if with.is_empty() {
        String::new()
    } else {
        format!("WITH ({}) ", with.join(", "))
    };
    format!(" RETURNING {}{}", with, cols.join(", "))
}

// ── Shared helpers ────────────────────────────────────────────────────────────

/// A quoted identifier stays quoted; an identifier PostgreSQL would not read
/// back identically without quotes (uppercase, special characters) is
/// quoted, so as never to designate another object.
///
/// An unquoted identifier equal to a keyword can only have been read at a
/// position where the keyword is allowed (`current_user` as a role): it stays
/// as is. Names that do not come from parsing (catalog export) go through
/// `is_plain_identifier`, which also quotes keywords.
pub fn emit_ident(ident: &Ident) -> String {
    if ident.quoted || !has_plain_shape(&ident.value) {
        quote_ident(&ident.value)
    } else {
        ident.value.clone()
    }
}

fn quote_ident(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

/// `[a-z_][a-z0-9_$]*` and not a keyword: read back as is without quotes.
pub fn is_plain_identifier(value: &str) -> bool {
    has_plain_shape(value) && !crate::keywords::needs_quoting(value)
}

/// `[a-z_][a-z0-9_$]*`. Enough for a function or type name, where keywords
/// (`coalesce`, `bit`) are written without quotes — quoted, they would
/// designate another object.
pub fn has_plain_shape(value: &str) -> bool {
    let mut chars = value.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '$')
}

/// Function or type name: quoted only if it was, or if its form
/// requires it.
fn emit_routine_name(name: &ObjectName) -> String {
    name.0
        .iter()
        .map(|i| {
            if i.quoted || !has_plain_shape(&i.value) {
                quote_ident(&i.value)
            } else {
                i.value.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(".")
}

pub fn emit_object_name(name: &ObjectName) -> String {
    name.0.iter().map(emit_ident).collect::<Vec<_>>().join(".")
}

/// `(n)` of a fractional-seconds precision, or nothing.
fn precision(p: Option<u64>) -> String {
    p.map(|n| format!("({n})")).unwrap_or_default()
}

pub fn emit_data_type(dt: &DataType) -> String {
    match dt {
        DataType::SmallInt => "SMALLINT".to_string(),
        DataType::Integer => "INTEGER".to_string(),
        DataType::BigInt => "BIGINT".to_string(),
        DataType::Numeric { precision, scale } => match (precision, scale) {
            (Some(p), Some(s)) => format!("NUMERIC({}, {})", p, s),
            (Some(p), None) => format!("NUMERIC({})", p),
            _ => "NUMERIC".to_string(),
        },
        DataType::Real => "REAL".to_string(),
        DataType::DoublePrecision => "DOUBLE PRECISION".to_string(),
        DataType::Text => "TEXT".to_string(),
        DataType::Varchar(n) => match n {
            Some(len) => format!("VARCHAR({})", len),
            None => "VARCHAR".to_string(),
        },
        DataType::Char(n) => match n {
            Some(len) => format!("CHAR({})", len),
            None => "CHAR".to_string(),
        },
        DataType::Date => "DATE".to_string(),
        DataType::Time(p) => format!("TIME{}", precision(*p)),
        DataType::TimeTz(p) => format!("TIME{} WITH TIME ZONE", precision(*p)),
        DataType::Timestamp(p) => format!("TIMESTAMP{}", precision(*p)),
        DataType::TimestampTz(p) => format!("TIMESTAMP{} WITH TIME ZONE", precision(*p)),
        DataType::Interval => "INTERVAL".to_string(),
        DataType::Boolean => "BOOLEAN".to_string(),
        DataType::Bytea => "BYTEA".to_string(),
        DataType::Json => "JSON".to_string(),
        DataType::Jsonb => "JSONB".to_string(),
        DataType::Uuid => "UUID".to_string(),
        DataType::Array(inner) => format!("{}[]", emit_data_type(inner)),
        // Dotted name where each part already has its PostgreSQL case: only
        // the parts that would not read back identically are quoted.
        // (A quoted name containing a dot cannot be represented here.)
        DataType::Custom(name) => name
            .split('.')
            .map(|part| {
                if has_plain_shape(part) {
                    part.to_string()
                } else {
                    quote_ident(part)
                }
            })
            .collect::<Vec<_>>()
            .join("."),
        DataType::Modified { name, modifiers } => format!(
            "{}({})",
            emit_data_type(&DataType::Custom(name.clone())),
            modifiers.join(", ")
        ),
    }
}
