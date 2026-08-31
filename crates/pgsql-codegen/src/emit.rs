use pgsql_ast::expr::ExprKind;
use pgsql_ast::*;

// ── Public API ────────────────────────────────────────────────────────────────

pub fn emit_statement(stmt: &Statement) -> String {
    match stmt {
        Statement::Select(sel) => emit_select(sel),
        Statement::Insert(ins) => emit_insert(ins),
        Statement::Update(upd) => emit_update(upd),
        Statement::Delete(del) => emit_delete(del),
        Statement::Ddl(ddl) => emit_ddl(ddl),
    }
}

// ── SELECT ────────────────────────────────────────────────────────────────────

pub fn emit_select(sel: &SelectStmt) -> String {
    let mut out = String::new();

    // WITH (CTEs)
    if !sel.with.is_empty() {
        out.push_str("WITH ");
        let ctes: Vec<String> = sel
            .with
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
                    emit_select(&cte.query)
                )
            })
            .collect();
        out.push_str(&ctes.join(", "));
        out.push(' ');
    }

    out.push_str("SELECT ");
    if sel.distinct {
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
    let name = emit_object_name(&tr.name);
    match &tr.alias {
        Some(a) => format!("{} AS {}", name, emit_ident(a)),
        None => name,
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
    let constraint = match &join.constraint {
        JoinConstraint::On(expr) => format!(" ON {}", emit_expr(expr)),
        JoinConstraint::Using(cols) => {
            let cols: Vec<String> = cols.iter().map(emit_ident).collect();
            format!(" USING ({})", cols.join(", "))
        }
        JoinConstraint::Natural => String::new(),
    };
    format!("{} {}{}", kind, table, constraint)
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

pub fn emit_expr(expr: &Expr) -> String {
    match &expr.kind {
        ExprKind::Literal(lit) => emit_literal(lit),
        ExprKind::Column(col_ref) => {
            let col = emit_ident(&col_ref.column);
            match &col_ref.table {
                Some(t) => format!("{}.{}", emit_ident(t), col),
                None => col,
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
        ExprKind::UnaryOp { op, expr } => match op {
            UnaryOp::Not => format!("(NOT {})", emit_expr(expr)),
            UnaryOp::Neg => format!("(-{})", emit_expr(expr)),
            UnaryOp::IsNull => format!("({} IS NULL)", emit_expr(expr)),
            UnaryOp::IsNotNull => format!("({} IS NOT NULL)", emit_expr(expr)),
        },
        ExprKind::FunctionCall {
            name,
            args,
            filter,
            distinct,
        } => {
            let fn_name = emit_object_name(name);
            let distinct_kw = if *distinct { "DISTINCT " } else { "" };
            let args_str: Vec<String> = args.iter().map(emit_expr).collect();
            let filter_str = match filter {
                Some(f) => format!(" FILTER (WHERE {})", emit_expr(f)),
                None => String::new(),
            };
            format!(
                "{}({}{}){}",
                fn_name,
                distinct_kw,
                args_str.join(", "),
                filter_str
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
    }
}

fn emit_literal(lit: &Literal) -> String {
    match lit {
        Literal::Number(n) => n.clone(),
        Literal::String(s) => format!("'{}'", s.replace('\'', "''")),
        Literal::Boolean(b) => if *b { "TRUE" } else { "FALSE" }.to_string(),
        Literal::Null => "NULL".to_string(),
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
    }
}

pub fn emit_create_table(ct: &CreateTable) -> String {
    let if_not_exists = if ct.if_not_exists {
        "IF NOT EXISTS "
    } else {
        ""
    };
    let mut parts: Vec<String> = ct.columns.iter().map(emit_column_def).collect();
    for constraint in &ct.constraints {
        parts.push(emit_table_constraint(constraint));
    }
    format!(
        "CREATE TABLE {}{} (\n  {}\n);",
        if_not_exists,
        emit_object_name(&ct.name),
        parts.join(",\n  ")
    )
}

pub fn emit_column_def(col: &ColumnDef) -> String {
    let mut parts = vec![emit_ident(&col.name), emit_data_type(&col.data_type)];
    if !col.nullable {
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

fn emit_table_constraint(tc: &TableConstraint) -> String {
    match tc {
        TableConstraint::PrimaryKey { columns } => {
            let cols: Vec<String> = columns.iter().map(emit_ident).collect();
            format!("PRIMARY KEY ({})", cols.join(", "))
        }
        TableConstraint::Unique { columns } => {
            let cols: Vec<String> = columns.iter().map(emit_ident).collect();
            format!("UNIQUE ({})", cols.join(", "))
        }
        TableConstraint::ForeignKey {
            columns,
            ref_table,
            ref_columns,
            on_delete,
            on_update,
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
        TableConstraint::Check(expr) => format!("CHECK ({})", emit_expr(expr)),
    }
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
    let cols: Vec<String> = ci.columns.iter().map(emit_ident).collect();
    format!(
        "CREATE {}INDEX {}{}ON {} ({});",
        unique,
        if_not_exists,
        name,
        emit_object_name(&ci.table),
        cols.join(", ")
    )
}

pub fn emit_create_function(cf: &CreateFunction) -> String {
    let or_replace = if cf.or_replace { "OR REPLACE " } else { "" };
    let params: Vec<String> = cf.params.iter().map(emit_function_param).collect();
    let returns = emit_return_type(&cf.return_type);
    let lang = emit_function_language(&cf.language);
    let body = emit_function_body(&cf.body);
    let security = if cf.security_definer {
        "\nSECURITY DEFINER"
    } else {
        ""
    };
    let volatility = match cf.volatility {
        Volatility::Volatile => "",
        Volatility::Stable => "\nSTABLE",
        Volatility::Immutable => "\nIMMUTABLE",
    };
    format!(
        "CREATE {}FUNCTION {}({})\nRETURNS {}\nLANGUAGE {}{}{}\nAS {};",
        or_replace,
        emit_object_name(&cf.name),
        params.join(", "),
        returns,
        lang,
        security,
        volatility,
        body
    )
}

pub fn emit_create_procedure(cp: &CreateProcedure) -> String {
    let or_replace = if cp.or_replace { "OR REPLACE " } else { "" };
    let params: Vec<String> = cp.params.iter().map(emit_function_param).collect();
    let lang = emit_function_language(&cp.language);
    let body = emit_function_body(&cp.body);
    format!(
        "CREATE {}PROCEDURE {}({})\nLANGUAGE {}\nAS {};",
        or_replace,
        emit_object_name(&cp.name),
        params.join(", "),
        lang,
        body
    )
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

fn emit_return_type(rt: &ReturnType) -> String {
    match rt {
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
    }
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
        FunctionBody::Raw(s) => format!("$${}$$", s),
        FunctionBody::SqlQuery(sel) => format!("$${}\n$$", emit_select(sel)),
        FunctionBody::Parsed(_) => "$$/* plpgsql body */$$".to_string(),
    }
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
    format!(
        "CREATE TRIGGER {}\n{} {} ON {}\nFOR EACH {}{}\nEXECUTE FUNCTION {}();",
        emit_ident(&ct.name),
        timing,
        events.join(" OR "),
        emit_object_name(&ct.table),
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
        "ALTER TABLE {}\n  {};",
        emit_object_name(&at.name),
        ops.join(",\n  ")
    )
}

fn emit_alter_table_op(op: &AlterTableOp) -> String {
    match op {
        AlterTableOp::EnableRowLevelSecurity => "ENABLE ROW LEVEL SECURITY".to_string(),
        AlterTableOp::DisableRowLevelSecurity => "DISABLE ROW LEVEL SECURITY".to_string(),
        AlterTableOp::ForceRowLevelSecurity => "FORCE ROW LEVEL SECURITY".to_string(),
        AlterTableOp::AddColumn(col) => format!("ADD COLUMN {}", emit_column_def(col)),
        AlterTableOp::DropColumn { name, if_exists } => {
            let ie = if *if_exists { "IF EXISTS " } else { "" };
            format!("DROP COLUMN {}{}", ie, emit_ident(name))
        }
        AlterTableOp::AlterColumn { name, action } => {
            let action_str = match action {
                AlterColumnAction::SetDefault(expr) => {
                    format!("SET DEFAULT {}", emit_expr(expr))
                }
                AlterColumnAction::DropDefault => "DROP DEFAULT".to_string(),
                AlterColumnAction::SetNotNull => "SET NOT NULL".to_string(),
                AlterColumnAction::DropNotNull => "DROP NOT NULL".to_string(),
                AlterColumnAction::SetDataType(dt) => {
                    format!("SET DATA TYPE {}", emit_data_type(dt))
                }
            };
            format!("ALTER COLUMN {} {}", emit_ident(name), action_str)
        }
        AlterTableOp::AddConstraint(tc) => {
            format!("ADD {}", emit_table_constraint(tc))
        }
        AlterTableOp::DropConstraint { name, if_exists } => {
            let ie = if *if_exists { "IF EXISTS " } else { "" };
            format!("DROP CONSTRAINT {}{}", ie, emit_ident(name))
        }
    }
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
    };
    let on_conflict = ins
        .on_conflict
        .as_ref()
        .map(emit_on_conflict)
        .unwrap_or_default();
    let returning = emit_returning(&ins.returning);
    format!(
        "INSERT INTO {}{} {}{}{}",
        emit_object_name(&ins.table),
        cols,
        source,
        on_conflict,
        returning
    )
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
            let assigns: Vec<String> = assignments
                .iter()
                .map(|a| format!("{} = {}", emit_ident(&a.column), emit_expr(&a.value)))
                .collect();
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
    let assigns: Vec<String> = upd
        .assignments
        .iter()
        .map(|a| format!("{} = {}", emit_ident(&a.column), emit_expr(&a.value)))
        .collect();
    let from = if upd.from.is_empty() {
        String::new()
    } else {
        let tables: Vec<String> = upd.from.iter().map(emit_table_ref).collect();
        format!(" FROM {}", tables.join(", "))
    };
    let where_str = upd
        .where_clause
        .as_ref()
        .map(|w| format!(" WHERE {}", emit_expr(w)))
        .unwrap_or_default();
    let returning = emit_returning(&upd.returning);
    format!(
        "UPDATE {} SET {}{}{}{}",
        emit_table_ref(&upd.table),
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
        format!(" USING {}", tables.join(", "))
    };
    let where_str = del
        .where_clause
        .as_ref()
        .map(|w| format!(" WHERE {}", emit_expr(w)))
        .unwrap_or_default();
    let returning = emit_returning(&del.returning);
    format!(
        "DELETE FROM {}{}{}{}",
        emit_table_ref(&del.table),
        using,
        where_str,
        returning
    )
}

fn emit_returning(items: &[SelectItem]) -> String {
    if items.is_empty() {
        return String::new();
    }
    let cols: Vec<String> = items.iter().map(emit_select_item).collect();
    format!(" RETURNING {}", cols.join(", "))
}

// ── Shared helpers ────────────────────────────────────────────────────────────

pub fn emit_ident(ident: &Ident) -> String {
    if ident.quoted {
        format!("\"{}\"", ident.value.replace('"', "\"\""))
    } else {
        ident.value.clone()
    }
}

pub fn emit_object_name(name: &ObjectName) -> String {
    name.0.iter().map(emit_ident).collect::<Vec<_>>().join(".")
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
        DataType::Time => "TIME".to_string(),
        DataType::Timestamp => "TIMESTAMP".to_string(),
        DataType::TimestampTz => "TIMESTAMPTZ".to_string(),
        DataType::Interval => "INTERVAL".to_string(),
        DataType::Boolean => "BOOLEAN".to_string(),
        DataType::Bytea => "BYTEA".to_string(),
        DataType::Json => "JSON".to_string(),
        DataType::Jsonb => "JSONB".to_string(),
        DataType::Uuid => "UUID".to_string(),
        DataType::Array(inner) => format!("{}[]", emit_data_type(inner)),
        DataType::Custom(name) => name.clone(),
    }
}
