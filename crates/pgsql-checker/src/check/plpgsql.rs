use std::collections::HashMap;

use pgsql_ast::ddl::ReturnType;
use pgsql_ast::plpgsql::{
    AssignTarget, Block, ElsIf, PlExpr, PlStatement, PlpgsqlBody, RaiseLevel,
};
use pgsql_ast::types::DataType;

use crate::diagnostic::Diagnostic;
use crate::schema::{SchemaProvider, TableSchema};

// ── Type context ──────────────────────────────────────────────────────────────

struct TypeContext<'a> {
    /// Variables declared in DECLARE
    vars: HashMap<String, DataType>,
    /// Function parameters
    params: HashMap<String, DataType>,
    /// Trigger table (for NEW/OLD)
    trigger_table: Option<&'a TableSchema>,
    /// Schema access
    schema: &'a dyn SchemaProvider,
    /// Declared return type (reserved for future type-compatibility checks)
    #[allow(dead_code)]
    return_type: &'a ReturnType,
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Structural checker: operates on the parsed AST.
pub fn check_plpgsql_ast(
    body: &PlpgsqlBody,
    function_name: &str,
    return_type: &ReturnType,
    trigger_table: Option<&str>,
    schema: &dyn SchemaProvider,
    diags: &mut Vec<Diagnostic>,
) {
    // W002 — empty block (no statements and no exception handlers).
    if body.block.statements.is_empty() && body.block.exception.is_none() {
        diags.push(
            Diagnostic::warning(
                "W002",
                format!("function '{}' has an empty body", function_name),
            )
            .with_hint("add a BEGIN … END block or a RETURN statement"),
        );
        return;
    }

    // Resolve the trigger table schema if provided.
    let trigger_table_schema: Option<&TableSchema> =
        trigger_table.and_then(|t| schema.get_table(t));

    // Build vars from DECLARE.
    let mut vars: HashMap<String, DataType> = HashMap::new();
    for decl in &body.declare {
        vars.insert(decl.name.value.to_lowercase(), decl.data_type.clone());
    }

    let ctx = TypeContext {
        vars,
        params: HashMap::new(), // params could be populated from CreateFunction if passed
        trigger_table: trigger_table_schema,
        schema,
        return_type,
    };

    check_block(&body.block, &ctx, diags);
}

/// Text-based checker (fallback when the body is not parsed).
///
/// Diagnostic codes:
/// - W002  empty body
/// - E001  column referenced via OLD./NEW. does not exist on the trigger table
pub fn check_plpgsql_body(
    body: &str,
    function_name: &str,
    trigger_table: Option<&str>,
    schema: &dyn SchemaProvider,
    diags: &mut Vec<Diagnostic>,
) {
    // W002 — empty body.
    if body.trim().is_empty() {
        diags.push(
            Diagnostic::warning(
                "W002",
                format!("function '{}' has an empty body", function_name),
            )
            .with_hint("add a BEGIN … END block or a RETURN statement"),
        );
        return;
    }

    let trigger_table_schema: Option<&TableSchema> =
        trigger_table.and_then(|t| schema.get_table(t));

    for line in body.lines() {
        let trimmed = line.trim();
        let upper = trimmed.to_uppercase();

        // --- RAISE EXCEPTION / WARNING / NOTICE — always valid, skip.
        if upper.starts_with("RAISE") {
            continue;
        }

        // --- Embedded SQL queries: attempt to parse and check them.
        if contains_dml_keyword(&upper) {
            if let Some(sql_fragment) = extract_sql_fragment(trimmed) {
                match pgsql_parser::parse(sql_fragment) {
                    Ok(stmts) => {
                        let inner = super::check_statements(&stmts, schema);
                        for mut d in inner {
                            d.message = format!("[in {}] {}", function_name, d.message);
                            diags.push(d);
                        }
                    }
                    // Fragment parse failures are expected for multi-line or
                    // PL/pgSQL-variable-containing queries — ignore silently.
                    Err(_) => {}
                }
            }
        }

        // --- OLD.column / NEW.column references in trigger bodies.
        if let Some(ts) = trigger_table_schema {
            check_old_new_refs(trimmed, ts, function_name, diags);
        }
    }
}

// ── AST checker internals ─────────────────────────────────────────────────────

fn check_block(block: &Block, ctx: &TypeContext<'_>, diags: &mut Vec<Diagnostic>) {
    for stmt in &block.statements {
        check_statement(stmt, ctx, diags);
    }

    // Also check exception handlers.
    if let Some(exc) = &block.exception {
        for handler in &exc.handlers {
            check_block(&handler.block, ctx, diags);
        }
    }
}

fn check_statement(stmt: &PlStatement, ctx: &TypeContext<'_>, diags: &mut Vec<Diagnostic>) {
    match stmt {
        // NEW.col as assignment target — verify the column exists on the trigger table.
        PlStatement::Assign {
            target: AssignTarget::NewField(col),
            value,
        } => {
            if let Some(ts) = ctx.trigger_table {
                if ts.get_column(&col.value).is_none() {
                    diags.push(
                        Diagnostic::error(
                            "E001",
                            format!(
                                "column '{}' does not exist on trigger table '{}'",
                                col.value, ts.name
                            ),
                        )
                        .with_hint(format!(
                            "available columns: {}",
                            ts.columns
                                .iter()
                                .map(|c| c.name.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        )),
                    );
                }
            }
            check_plexpr(value, ctx, diags);
        }

        // OLD.col as assignment target — always an error.
        PlStatement::Assign {
            target: AssignTarget::OldField(col),
            value,
        } => {
            diags.push(Diagnostic::error(
                "E003",
                format!("cannot assign to OLD.{}", col.value),
            ));
            check_plexpr(value, ctx, diags);
        }

        // Simple variable or field assignment.
        PlStatement::Assign { value, .. } => {
            check_plexpr(value, ctx, diags);
        }

        // SELECT … INTO: check the query.
        PlStatement::SelectInto { query, .. } => {
            let sub_diags = super::select::check_select(query, ctx.schema);
            diags.extend(sub_diags);
        }

        // IF statement.
        PlStatement::If {
            condition,
            then_block,
            elsif_clauses,
            else_block,
        } => {
            check_plexpr(condition, ctx, diags);
            check_block(then_block, ctx, diags);
            for ElsIf { condition, block } in elsif_clauses {
                check_plexpr(condition, ctx, diags);
                check_block(block, ctx, diags);
            }
            if let Some(eb) = else_block {
                check_block(eb, ctx, diags);
            }
        }

        // RAISE EXCEPTION with no message — validate USING options.
        PlStatement::Raise {
            level: RaiseLevel::Exception,
            message: None,
            options,
            args,
        } => {
            for opt in options {
                check_plexpr(&opt.value, ctx, diags);
            }
            for arg in args {
                check_plexpr(arg, ctx, diags);
            }
        }

        // Any other RAISE level.
        PlStatement::Raise { args, options, .. } => {
            for arg in args {
                check_plexpr(arg, ctx, diags);
            }
            for opt in options {
                check_plexpr(&opt.value, ctx, diags);
            }
        }

        // RETURN expr.
        PlStatement::Return(Some(expr)) => {
            check_plexpr(expr, ctx, diags);
        }
        PlStatement::Return(None) => {}

        // RETURN NEXT expr.
        PlStatement::ReturnNext(expr) => {
            check_plexpr(expr, ctx, diags);
        }

        // RETURN QUERY select.
        PlStatement::ReturnQuery(query) => {
            let sub_diags = super::select::check_select(query, ctx.schema);
            diags.extend(sub_diags);
        }

        // Direct SQL statement (INSERT, UPDATE, DELETE).
        PlStatement::Sql(sql_stmt) => {
            let sub_diags = super::check_statements(&[sql_stmt.clone()], ctx.schema);
            diags.extend(sub_diags);
        }

        // Nested block.
        PlStatement::SubBlock(block) => {
            check_block(block, ctx, diags);
        }

        // WHILE loop.
        PlStatement::While { condition, body } => {
            check_plexpr(condition, ctx, diags);
            check_block(body, ctx, diags);
        }

        // LOOP (unconditional).
        PlStatement::Loop(body) => {
            check_block(body, ctx, diags);
        }

        // FOR … IN range.
        PlStatement::ForRange {
            from, to, by, body, ..
        } => {
            check_plexpr(from, ctx, diags);
            check_plexpr(to, ctx, diags);
            if let Some(step) = by {
                check_plexpr(step, ctx, diags);
            }
            check_block(body, ctx, diags);
        }

        // FOR … IN query.
        PlStatement::ForSelect { query, body, .. } => {
            let sub_diags = super::select::check_select(query, ctx.schema);
            diags.extend(sub_diags);
            check_block(body, ctx, diags);
        }

        // FOR … IN EXECUTE.
        PlStatement::ForExecute { sql, using, body, .. } => {
            check_plexpr(sql, ctx, diags);
            for expr in using {
                check_plexpr(expr, ctx, diags);
            }
            check_block(body, ctx, diags);
        }

        // PERFORM.
        PlStatement::Perform(expr) => {
            check_plexpr(expr, ctx, diags);
        }

        // EXECUTE.
        PlStatement::Execute { sql, using, .. } => {
            check_plexpr(sql, ctx, diags);
            for expr in using {
                check_plexpr(expr, ctx, diags);
            }
        }

        // EXIT / CONTINUE with optional WHEN.
        PlStatement::Exit { when, .. } | PlStatement::Continue { when, .. } => {
            if let Some(expr) = when {
                check_plexpr(expr, ctx, diags);
            }
        }

        // CALL.
        PlStatement::Call { args, .. } => {
            for arg in args {
                check_plexpr(arg, ctx, diags);
            }
        }

        // NULL no-op and GET DIAGNOSTICS — nothing to check.
        PlStatement::Null | PlStatement::GetDiagnostics(_) => {}
    }
}

fn check_plexpr(expr: &PlExpr, ctx: &TypeContext<'_>, diags: &mut Vec<Diagnostic>) {
    match expr {
        PlExpr::NewField(col) => {
            if let Some(ts) = ctx.trigger_table {
                if ts.get_column(&col.value).is_none() {
                    diags.push(
                        Diagnostic::error(
                            "E001",
                            format!(
                                "column '{}' does not exist on trigger table '{}'",
                                col.value, ts.name
                            ),
                        )
                        .with_hint(format!(
                            "available columns: {}",
                            ts.columns
                                .iter()
                                .map(|c| c.name.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        )),
                    );
                }
            }
        }

        PlExpr::OldField(col) => {
            if let Some(ts) = ctx.trigger_table {
                if ts.get_column(&col.value).is_none() {
                    diags.push(
                        Diagnostic::error(
                            "E001",
                            format!(
                                "column '{}' does not exist on trigger table '{}'",
                                col.value, ts.name
                            ),
                        )
                        .with_hint(format!(
                            "available columns: {}",
                            ts.columns
                                .iter()
                                .map(|c| c.name.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        )),
                    );
                }
            }
        }

        PlExpr::Var(ident) => {
            let name = &ident.value;
            let lower = name.to_lowercase();
            let upper = name.to_uppercase();
            if !ctx.vars.contains_key(&lower)
                && !ctx.params.contains_key(&lower)
                && upper != "FOUND"
            {
                // W005: variable may not be declared (warning, not error —
                // could be an untracked parameter).
                diags.push(
                    Diagnostic::warning(
                        "W005",
                        format!("variable '{}' may not be declared", name),
                    )
                    .with_hint(
                        "declare it in the DECLARE section or verify it's a function parameter",
                    ),
                );
            }
        }

        PlExpr::Found | PlExpr::Sqlstate => {}

        PlExpr::Sql(_) | PlExpr::RecordField { .. } => {}

        PlExpr::Not(inner) => check_plexpr(inner, ctx, diags),

        PlExpr::And(l, r) | PlExpr::Or(l, r) | PlExpr::BinOp(l, _, r) => {
            check_plexpr(l, ctx, diags);
            check_plexpr(r, ctx, diags);
        }
    }
}

// ── Text-based checker helpers ────────────────────────────────────────────────

/// Returns true if the (already-uppercased) line contains a DML keyword at a
/// word boundary (to avoid false-positive matches inside identifiers or strings).
fn contains_dml_keyword(upper: &str) -> bool {
    for kw in &["SELECT", "INSERT", "UPDATE", "DELETE"] {
        if let Some(pos) = upper.find(kw) {
            let before_ok = pos == 0
                || !upper
                    .as_bytes()
                    .get(pos - 1)
                    .map(|b| b.is_ascii_alphanumeric() || *b == b'_')
                    .unwrap_or(false);
            let after_pos = pos + kw.len();
            let after_ok = after_pos >= upper.len()
                || !upper
                    .as_bytes()
                    .get(after_pos)
                    .map(|b| b.is_ascii_alphanumeric() || *b == b'_')
                    .unwrap_or(false);
            if before_ok && after_ok {
                return true;
            }
        }
    }
    false
}

/// Try to extract a plausible SQL fragment from a PL/pgSQL body line.
fn extract_sql_fragment(line: &str) -> Option<&str> {
    let t = line.trim();
    // Skip comment lines.
    if t.starts_with("--") {
        return None;
    }
    // Strip trailing semicolon for the parser.
    let t = t.strip_suffix(';').unwrap_or(t).trim();
    // If the line starts with an assignment (e.g. `v_count := SELECT ...`),
    // strip the left side.
    let t = if let Some(idx) = t.find(":=") {
        t[idx + 2..].trim()
    } else {
        t
    };
    if t.is_empty() {
        None
    } else {
        Some(t)
    }
}

/// Scan a single body line for `OLD.<col>` and `NEW.<col>` references.
fn check_old_new_refs(
    line: &str,
    trigger_table_schema: &TableSchema,
    function_name: &str,
    diags: &mut Vec<Diagnostic>,
) {
    let upper_line = line.to_uppercase();

    for prefix in &["OLD.", "NEW."] {
        let mut search_start = 0;
        while let Some(rel_pos) = upper_line[search_start..].find(prefix) {
            let abs_pos = search_start + rel_pos;
            let rest = &line[abs_pos + prefix.len()..];
            let col_name: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();

            if !col_name.is_empty() && trigger_table_schema.get_column(&col_name).is_none() {
                diags.push(
                    Diagnostic::error(
                        "E001",
                        format!(
                            "[in {}] column '{}' does not exist on trigger table '{}'",
                            function_name, col_name, trigger_table_schema.name
                        ),
                    )
                    .with_hint(format!(
                        "available columns: {}",
                        trigger_table_schema
                            .columns
                            .iter()
                            .map(|c| c.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )),
                );
            }

            search_start = abs_pos + prefix.len();
        }
    }
}
