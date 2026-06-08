use std::collections::{HashMap, HashSet};

use pgsql_ast::{Expr, SelectItem, SelectStmt, TableRef};

use crate::codes;
use crate::diagnostic::Diagnostic;
use crate::schema::{SchemaProvider, TableSchema};

/// Check a single SELECT statement against the schema.
pub fn check_select(stmt: &SelectStmt, schema: &dyn SchemaProvider) -> Vec<Diagnostic> {
    check_select_inner(stmt, schema, &HashSet::new(), &HashSet::new())
}

/// Like `check_select`, but treats any identifier in `plvars` (lower-cased) as a
/// bound PL/pgSQL variable or parameter rather than a column reference — suppressing
/// false-positive E001 errors for names like `p_task_id` or `v_org_id`.
pub fn check_select_with_plvars(
    stmt: &SelectStmt,
    schema: &dyn SchemaProvider,
    plvars: &HashSet<String>,
) -> Vec<Diagnostic> {
    check_select_inner(stmt, schema, plvars, &HashSet::new())
}

/// Like `check_select_with_plvars`, but also accepts `phantom_qualifiers`: a set
/// of lowercase table qualifiers (e.g. `"new"`, `"old"`) that are known to be valid
/// row-variable aliases in the current context but whose schema is unavailable.
/// References to these qualifiers suppress E002 and skip column validation.
pub fn check_select_with_plvars_and_phantoms(
    stmt: &SelectStmt,
    schema: &dyn SchemaProvider,
    plvars: &HashSet<String>,
    phantom_qualifiers: &HashSet<String>,
) -> Vec<Diagnostic> {
    check_select_inner(stmt, schema, plvars, phantom_qualifiers)
}

fn check_select_inner(
    stmt: &SelectStmt,
    schema: &dyn SchemaProvider,
    plvars: &HashSet<String>,
    phantom_qualifiers: &HashSet<String>,
) -> Vec<Diagnostic> {
    let mut diags = Vec::new();

    // Build the column context from FROM + JOINs.
    // Maps alias (or table name) → Option<&TableSchema>.
    // None means the table fired E003 (unknown) but the alias is still registered
    // so qualified references like `p.col` don't cascade into a spurious E002.
    let mut ctx: HashMap<String, Option<&TableSchema>> = HashMap::new();

    for table_ref in &stmt.from {
        resolve_table_ref(table_ref, schema, &mut ctx, &mut diags);
    }
    for join in &stmt.joins {
        resolve_table_ref(&join.table, schema, &mut ctx, &mut diags);
    }

    // Check SELECT columns.
    for item in &stmt.columns {
        match item {
            SelectItem::Wildcard => {
                // SELECT * is always fine.
            }
            SelectItem::QualifiedWildcard(ident) => {
                // table.* — only fire E002 if the alias is genuinely absent from
                // the FROM clause; if it's present but None (E003 already fired), skip.
                let key = ident.value.to_lowercase();
                if !ctx.contains_key(&key) && !phantom_qualifiers.contains(&key) {
                    diags.push(Diagnostic::error(
                        codes::TABLE_NOT_IN_SCOPE,
                        format!("table or alias '{}' not found in FROM clause", ident.value),
                    ));
                }
            }
            SelectItem::Expr { expr, .. } => {
                check_expr(expr, &ctx, schema, plvars, phantom_qualifiers, &mut diags);
            }
        }
    }

    // Check WHERE clause.
    if let Some(where_expr) = &stmt.where_clause {
        check_expr(where_expr, &ctx, schema, plvars, phantom_qualifiers, &mut diags);
    }

    // Check GROUP BY.
    for expr in &stmt.group_by {
        check_expr(expr, &ctx, schema, plvars, phantom_qualifiers, &mut diags);
    }

    // Check HAVING.
    if let Some(having) = &stmt.having {
        check_expr(having, &ctx, schema, plvars, phantom_qualifiers, &mut diags);
    }

    // Check ORDER BY.
    for order in &stmt.order_by {
        check_expr(&order.expr, &ctx, schema, plvars, phantom_qualifiers, &mut diags);
    }

    diags
}

/// Resolve a table reference against the schema and add it to the context map.
///
/// If the table exists, inserts `Some(ts)` under its alias/name.
/// If the table is unknown, emits E003 and inserts `None` — this prevents
/// qualified references like `p.col` from also firing E002 (cascade noise).
fn resolve_table_ref<'a>(
    table_ref: &TableRef,
    schema: &'a dyn SchemaProvider,
    ctx: &mut HashMap<String, Option<&'a TableSchema>>,
    diags: &mut Vec<Diagnostic>,
) {
    let table_name = crate::schema::object_name_to_string(&table_ref.name);
    let key = table_name.to_lowercase();

    match schema.get_table(&key) {
        Some(ts) => {
            // Register under the alias if present, otherwise under the table name.
            let alias_or_name = table_ref
                .alias
                .as_ref()
                .map(|a| a.value.to_lowercase())
                .unwrap_or_else(|| key.clone());
            ctx.insert(alias_or_name, Some(ts));
            // Also register under unaliased table name for qualified references.
            if table_ref.alias.is_some() {
                ctx.insert(key, Some(ts));
            }
        }
        None => {
            diags.push(Diagnostic::error(
                codes::TABLE_NOT_FOUND,
                format!("table '{}' does not exist", table_name),
            ));
            // Register alias (or table name) as None so qualified refs don't
            // cascade into table-not-in-scope on top of the table-not-found already emitted.
            let alias_or_name = table_ref
                .alias
                .as_ref()
                .map(|a| a.value.to_lowercase())
                .unwrap_or(key);
            ctx.insert(alias_or_name, None);
        }
    }
}

/// Recursively check an expression for column/function validity.
fn check_expr(
    expr: &Expr,
    ctx: &HashMap<String, Option<&TableSchema>>,
    schema: &dyn SchemaProvider,
    plvars: &HashSet<String>,
    phantom_qualifiers: &HashSet<String>,
    diags: &mut Vec<Diagnostic>,
) {
    match expr {
        Expr::Column(col_ref) => {
            let col_name = &col_ref.column.value;

            match &col_ref.table {
                Some(tbl_ident) => {
                    // Qualified reference: table.column
                    let tbl_key = tbl_ident.value.to_lowercase();
                    match ctx.get(&tbl_key) {
                        Some(Some(table_schema)) => {
                            if table_schema.get_column(col_name).is_none() {
                                diags.push(
                                    Diagnostic::error(
                                        codes::COLUMN_NOT_FOUND,
                                        format!(
                                            "column '{}' does not exist in table '{}'",
                                            col_name, tbl_ident.value
                                        ),
                                    )
                                    .with_hint(format!(
                                        "available columns: {}",
                                        table_schema
                                            .columns
                                            .iter()
                                            .map(|c| c.name.as_str())
                                            .collect::<Vec<_>>()
                                            .join(", ")
                                    )),
                                );
                            }
                        }
                        // Table alias is known but the table itself fired E003 — skip
                        // to avoid cascading noise on every `alias.col` reference.
                        Some(None) => {}
                        None => {
                            // Phantom qualifiers (e.g. NEW/OLD in trigger bodies without
                            // a known trigger table) suppress both E002 and column checks.
                            if phantom_qualifiers.contains(&tbl_key) {
                                // Known alias but schema unavailable — skip silently.
                            } else {
                                // Not in the FROM-clause context — fall back to the schema
                                // provider. This handles implicit row variables like NEW and
                                // OLD in PL/pgSQL trigger bodies when the trigger table IS
                                // known and registered via BodySchemaProvider.
                                match schema.get_table(&tbl_key) {
                                    Some(ts) => {
                                        if ts.get_column(col_name).is_none() {
                                            diags.push(
                                                Diagnostic::error(
                                                    codes::COLUMN_NOT_FOUND,
                                                    format!(
                                                        "column '{}' does not exist in table '{}'",
                                                        col_name, tbl_ident.value
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
                                    None => {
                                        diags.push(Diagnostic::error(
                                            codes::TABLE_NOT_IN_SCOPE,
                                            format!(
                                                "table or alias '{}' not found in FROM clause",
                                                tbl_ident.value
                                            ),
                                        ));
                                    }
                                }
                            }
                        }
                    }
                }
                None => {
                    // Unqualified reference — skip if it's a known PL/pgSQL variable/param.
                    let col_lower = col_name.to_lowercase();
                    if plvars.contains(&col_lower) {
                        return;
                    }

                    // Search all resolved tables in context.
                    let found = ctx
                        .values()
                        .filter_map(|v| *v)
                        .any(|ts| ts.get_column(col_name).is_some());

                    if !found && ctx.values().any(|v| v.is_some()) {
                        // Build a hint listing all available columns across resolved tables.
                        let all_cols: Vec<String> = ctx
                            .values()
                            .filter_map(|v| *v)
                            .flat_map(|ts| ts.columns.iter().map(|c| c.name.clone()))
                            .collect();
                        let hint = if all_cols.is_empty() {
                            None
                        } else {
                            Some(format!("available columns: {}", all_cols.join(", ")))
                        };
                        let mut d = Diagnostic::error(
                            codes::COLUMN_NOT_FOUND,
                            format!("column '{}' does not exist", col_name),
                        );
                        if let Some(h) = hint {
                            d = d.with_hint(h);
                        }
                        diags.push(d);
                    }
                }
            }
        }

        Expr::FunctionCall { name, args, .. } => {
            let func_name = crate::schema::object_name_to_string(name);
            // Emit a warning for unknown functions (but only for single-part names;
            // schema-qualified calls like pg_catalog.now() are trusted).
            // Built-in PostgreSQL / SQL-standard functions never require a declaration.
            if name.0.len() == 1
                && !crate::builtins::is_builtin_function(&func_name)
                && schema.get_function(&func_name).is_none()
            {
                diags.push(Diagnostic::warning(
                    codes::FUNCTION_NOT_FOUND,
                    format!("function '{}' is not defined in the known schema", func_name),
                ));
            }
            for arg in args {
                check_expr(arg, ctx, schema, plvars, phantom_qualifiers, diags);
            }
        }

        Expr::BinaryOp { left, right, .. } => {
            check_expr(left, ctx, schema, plvars, phantom_qualifiers, diags);
            check_expr(right, ctx, schema, plvars, phantom_qualifiers, diags);
        }

        Expr::UnaryOp { expr, .. } => {
            check_expr(expr, ctx, schema, plvars, phantom_qualifiers, diags);
        }

        Expr::Cast { expr, .. } => {
            check_expr(expr, ctx, schema, plvars, phantom_qualifiers, diags);
        }

        Expr::Case {
            operand,
            when_clauses,
            else_clause,
        } => {
            if let Some(op) = operand {
                check_expr(op, ctx, schema, plvars, phantom_qualifiers, diags);
            }
            for (when, then) in when_clauses {
                check_expr(when, ctx, schema, plvars, phantom_qualifiers, diags);
                check_expr(then, ctx, schema, plvars, phantom_qualifiers, diags);
            }
            if let Some(el) = else_clause {
                check_expr(el, ctx, schema, plvars, phantom_qualifiers, diags);
            }
        }

        Expr::Subquery(sub) => {
            let sub_diags = check_select_inner(sub, schema, plvars, phantom_qualifiers);
            diags.extend(sub_diags);
        }

        Expr::InSubquery { expr, subquery, .. } => {
            check_expr(expr, ctx, schema, plvars, phantom_qualifiers, diags);
            let sub_diags = check_select_inner(subquery, schema, plvars, phantom_qualifiers);
            diags.extend(sub_diags);
        }

        Expr::InList { expr, list, .. } => {
            check_expr(expr, ctx, schema, plvars, phantom_qualifiers, diags);
            for item in list {
                check_expr(item, ctx, schema, plvars, phantom_qualifiers, diags);
            }
        }

        Expr::Between {
            expr, low, high, ..
        } => {
            check_expr(expr, ctx, schema, plvars, phantom_qualifiers, diags);
            check_expr(low, ctx, schema, plvars, phantom_qualifiers, diags);
            check_expr(high, ctx, schema, plvars, phantom_qualifiers, diags);
        }

        // Literals and wildcards carry no references to resolve.
        Expr::Literal(_) | Expr::Wildcard | Expr::QualifiedWildcard(_) => {}
    }
}
