pub(crate) mod plpgsql;
pub(crate) mod select;

use pgsql_ast::ddl::CreatePolicy;
use pgsql_ast::Statement;

use crate::codes;
use crate::diagnostic::Diagnostic;
use crate::schema::{object_name_to_string, SchemaProvider};

/// Parse `sql` and type-check all resulting statements.
pub fn check_sql(sql: &str, schema: &dyn SchemaProvider) -> Vec<Diagnostic> {
    match pgsql_parser::parse(sql) {
        Ok(stmts) => check_statements(&stmts, schema),
        Err(e) => vec![Diagnostic::error(codes::PARSE_ERROR, format!("parse error: {e}"))],
    }
}

/// Parse `sql`, extrait le schéma des statements DDL qu'il contient,
/// puis type-check tous les statements contre ce schéma.
/// C'est le mode utilisé quand on vérifie un fichier sans schéma externe séparé.
pub fn check_sql_self_contained(sql: &str) -> Vec<Diagnostic> {
    let stmts = match pgsql_parser::parse(sql) {
        Ok(s) => s,
        Err(e) => return vec![Diagnostic::error(codes::PARSE_ERROR, format!("parse error: {e}"))],
    };

    // Première passe : construire le schéma depuis le DDL du fichier lui-même
    let schema = crate::schema::DdlSchemaProvider::from_statements(&stmts);

    // Deuxième passe : checker tous les statements contre ce schéma
    check_statements(&stmts, &schema)
}

/// Type-check a slice of already-parsed AST statements.
pub fn check_statements(stmts: &[Statement], schema: &dyn SchemaProvider) -> Vec<Diagnostic> {
    crate::checker::Checker::with_builtin_rules().run(stmts, schema)
}

// ── RLS policy checker ────────────────────────────────────────────────────────

pub(crate) fn check_create_policy_rule(
    policy: &CreatePolicy,
    schema: &dyn SchemaProvider,
    diags: &mut Vec<Diagnostic>,
) {
    let table_name = object_name_to_string(&policy.table);

    let table_schema = match schema.get_table(&table_name) {
        Some(ts) => ts,
        None => {
            diags.push(
                Diagnostic::error(
                    codes::TABLE_NOT_FOUND,
                    format!(
                        "policy '{}' references table '{}' which does not exist",
                        policy.name.value, table_name
                    ),
                )
                .with_hint("create the table before defining an RLS policy on it"),
            );
            return;
        }
    };

    // Check USING expression.
    if let Some(using_expr) = &policy.using {
        check_policy_expr(
            using_expr,
            "USING",
            &policy.name.value,
            table_schema,
            schema,
            diags,
        );
    }

    // Check WITH CHECK expression.
    if let Some(with_check_expr) = &policy.with_check {
        check_policy_expr(
            with_check_expr,
            "WITH CHECK",
            &policy.name.value,
            table_schema,
            schema,
            diags,
        );
    }
}

/// Traverse a policy expression and validate column references and function calls.
fn check_policy_expr(
    expr: &pgsql_ast::Expr,
    clause: &str,
    policy_name: &str,
    table_schema: &crate::schema::TableSchema,
    schema: &dyn SchemaProvider,
    diags: &mut Vec<Diagnostic>,
) {
    use pgsql_ast::Expr;

    match expr {
        Expr::Column(col_ref) => {
            let col_name = &col_ref.column.value;
            if table_schema.get_column(col_name).is_none() {
                diags.push(
                    Diagnostic::error(
                        codes::COLUMN_NOT_FOUND,
                        format!(
                            "policy '{}' {} clause references column '{}' which does not exist on table '{}'",
                            policy_name, clause, col_name, table_schema.name
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

        Expr::FunctionCall { name, args, .. } => {
            let func_name = object_name_to_string(name);
            // Schema-qualified calls (e.g. auth.uid()) are trusted silently.
            if name.0.len() == 1 && schema.get_function(&func_name).is_none() {
                // Unqualified unknown function — warning.
                diags.push(Diagnostic::warning(
                    codes::FUNCTION_NOT_FOUND,
                    format!(
                        "policy '{}' {} clause calls '{}': function not found in known schema, not validated",
                        policy_name, clause, func_name
                    ),
                ));
            }
            // Recurse into arguments.
            for arg in args {
                check_policy_expr(arg, clause, policy_name, table_schema, schema, diags);
            }
        }

        Expr::BinaryOp { left, right, .. } => {
            check_policy_expr(left, clause, policy_name, table_schema, schema, diags);
            check_policy_expr(right, clause, policy_name, table_schema, schema, diags);
        }

        Expr::UnaryOp { expr, .. } => {
            check_policy_expr(expr, clause, policy_name, table_schema, schema, diags);
        }

        Expr::Cast { expr, .. } => {
            check_policy_expr(expr, clause, policy_name, table_schema, schema, diags);
        }

        Expr::Case {
            operand,
            when_clauses,
            else_clause,
        } => {
            if let Some(op) = operand {
                check_policy_expr(op, clause, policy_name, table_schema, schema, diags);
            }
            for (when, then) in when_clauses {
                check_policy_expr(when, clause, policy_name, table_schema, schema, diags);
                check_policy_expr(then, clause, policy_name, table_schema, schema, diags);
            }
            if let Some(el) = else_clause {
                check_policy_expr(el, clause, policy_name, table_schema, schema, diags);
            }
        }

        Expr::InList { expr, list, .. } => {
            check_policy_expr(expr, clause, policy_name, table_schema, schema, diags);
            for item in list {
                check_policy_expr(item, clause, policy_name, table_schema, schema, diags);
            }
        }

        Expr::Between { expr, low, high, .. } => {
            check_policy_expr(expr, clause, policy_name, table_schema, schema, diags);
            check_policy_expr(low, clause, policy_name, table_schema, schema, diags);
            check_policy_expr(high, clause, policy_name, table_schema, schema, diags);
        }

        // Subqueries in policy expressions are uncommon but legal; skip for now.
        Expr::Subquery(_) | Expr::InSubquery { .. } => {}

        // Literals and wildcards carry no column or function references.
        Expr::Literal(_) | Expr::Wildcard | Expr::QualifiedWildcard(_) => {}
    }
}
