use std::collections::HashMap;

use pgsql_ast::{Expr, SelectItem, SelectStmt, TableRef};

use crate::diagnostic::Diagnostic;
use crate::schema::{SchemaProvider, TableSchema};

/// Check a single SELECT statement against the schema.
pub fn check_select(stmt: &SelectStmt, schema: &dyn SchemaProvider) -> Vec<Diagnostic> {
    let mut diags = Vec::new();

    // Build the column context from FROM + JOINs.
    // Maps alias (or table name) → TableSchema.
    let mut ctx: HashMap<String, &TableSchema> = HashMap::new();

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
                // table.* — verify the table alias/name exists in context.
                let key = ident.value.to_lowercase();
                if !ctx.contains_key(&key) {
                    diags.push(Diagnostic::error(
                        "E002",
                        format!("table or alias '{}' not found in FROM clause", ident.value),
                    ));
                }
            }
            SelectItem::Expr { expr, .. } => {
                check_expr(expr, &ctx, schema, &mut diags);
            }
        }
    }

    // Check WHERE clause.
    if let Some(where_expr) = &stmt.where_clause {
        check_expr(where_expr, &ctx, schema, &mut diags);
    }

    // Check GROUP BY.
    for expr in &stmt.group_by {
        check_expr(expr, &ctx, schema, &mut diags);
    }

    // Check HAVING.
    if let Some(having) = &stmt.having {
        check_expr(having, &ctx, schema, &mut diags);
    }

    // Check ORDER BY.
    for order in &stmt.order_by {
        check_expr(&order.expr, &ctx, schema, &mut diags);
    }

    diags
}

/// Resolve a table reference against the schema and add it to the context map.
fn resolve_table_ref<'a>(
    table_ref: &TableRef,
    schema: &'a dyn SchemaProvider,
    ctx: &mut HashMap<String, &'a TableSchema>,
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
            ctx.insert(alias_or_name, ts);
            // Also register under unaliased table name for qualified references.
            if table_ref.alias.is_some() {
                ctx.insert(key, ts);
            }
        }
        None => {
            diags.push(Diagnostic::error(
                "E003",
                format!("table '{}' does not exist", table_name),
            ));
        }
    }
}

/// Recursively check an expression for column/function validity.
fn check_expr(
    expr: &Expr,
    ctx: &HashMap<String, &TableSchema>,
    schema: &dyn SchemaProvider,
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
                        Some(table_schema) => {
                            if table_schema.get_column(col_name).is_none() {
                                diags.push(
                                    Diagnostic::error(
                                        "E001",
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
                        None => {
                            diags.push(Diagnostic::error(
                                "E002",
                                format!(
                                    "table or alias '{}' not found in FROM clause",
                                    tbl_ident.value
                                ),
                            ));
                        }
                    }
                }
                None => {
                    // Unqualified reference — search all tables in context.
                    let found = ctx
                        .values()
                        .any(|ts| ts.get_column(col_name).is_some());

                    if !found && !ctx.is_empty() {
                        // Build a hint listing all available columns across tables.
                        let all_cols: Vec<String> = ctx
                            .values()
                            .flat_map(|ts| ts.columns.iter().map(|c| c.name.clone()))
                            .collect();
                        let hint = if all_cols.is_empty() {
                            None
                        } else {
                            Some(format!("available columns: {}", all_cols.join(", ")))
                        };
                        let mut d = Diagnostic::error(
                            "E001",
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
                    "W001",
                    format!("function '{}' is not defined in the known schema", func_name),
                ));
            }
            for arg in args {
                check_expr(arg, ctx, schema, diags);
            }
        }

        Expr::BinaryOp { left, right, .. } => {
            check_expr(left, ctx, schema, diags);
            check_expr(right, ctx, schema, diags);
        }

        Expr::UnaryOp { expr, .. } => {
            check_expr(expr, ctx, schema, diags);
        }

        Expr::Cast { expr, .. } => {
            check_expr(expr, ctx, schema, diags);
        }

        Expr::Case {
            operand,
            when_clauses,
            else_clause,
        } => {
            if let Some(op) = operand {
                check_expr(op, ctx, schema, diags);
            }
            for (when, then) in when_clauses {
                check_expr(when, ctx, schema, diags);
                check_expr(then, ctx, schema, diags);
            }
            if let Some(el) = else_clause {
                check_expr(el, ctx, schema, diags);
            }
        }

        Expr::Subquery(sub) => {
            let sub_diags = check_select(sub, schema);
            diags.extend(sub_diags);
        }

        Expr::InSubquery { expr, subquery, .. } => {
            check_expr(expr, ctx, schema, diags);
            let sub_diags = check_select(subquery, schema);
            diags.extend(sub_diags);
        }

        Expr::InList { expr, list, .. } => {
            check_expr(expr, ctx, schema, diags);
            for item in list {
                check_expr(item, ctx, schema, diags);
            }
        }

        Expr::Between {
            expr, low, high, ..
        } => {
            check_expr(expr, ctx, schema, diags);
            check_expr(low, ctx, schema, diags);
            check_expr(high, ctx, schema, diags);
        }

        // Literals and wildcards carry no references to resolve.
        Expr::Literal(_) | Expr::Wildcard | Expr::QualifiedWildcard(_) => {}
    }
}
