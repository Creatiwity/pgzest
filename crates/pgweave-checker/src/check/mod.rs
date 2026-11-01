pub(crate) mod plpgsql;
pub(crate) mod select;

use pgweave_ast::Statement;
use pgweave_ast::ddl::CreatePolicy;

use pgweave_ast::expr::ExprKind;
use pgweave_diagnostics::Diagnostic;
use pgweave_diagnostics::codes;
use pgweave_schema::provider::{SchemaProvider, object_name_to_string};

/// Parse `sql` and type-check all resulting statements.
pub fn check_sql(sql: &str, schema: &dyn SchemaProvider) -> Vec<Diagnostic> {
    match pgweave_parser::parse(sql) {
        Ok(stmts) => check_statements(&stmts, schema),
        Err(e) => vec![Diagnostic::error(
            codes::PARSE_ERROR,
            format!("parse error: {e}"),
        )],
    }
}

/// Parse `sql`, extrait le schéma des statements DDL qu'il contient,
/// puis type-check tous les statements contre ce schéma.
/// C'est le mode utilisé quand on vérifie un fichier sans schéma externe séparé.
pub fn check_sql_self_contained(sql: &str) -> Vec<Diagnostic> {
    let stmts = match pgweave_parser::parse(sql) {
        Ok(s) => s,
        Err(e) => {
            return vec![Diagnostic::error(
                codes::PARSE_ERROR,
                format!("parse error: {e}"),
            )];
        }
    };

    // Première passe : construire le schéma depuis le DDL du fichier lui-même
    let schema = pgweave_schema::provider::DdlSchemaProvider::from_statements(&stmts);

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
    expr: &pgweave_ast::Expr,
    clause: &str,
    policy_name: &str,
    table_schema: &pgweave_schema::provider::TableSchema,
    schema: &dyn SchemaProvider,
    diags: &mut Vec<Diagnostic>,
) {
    match &expr.kind {
        ExprKind::Column(col_ref) => {
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

        ExprKind::FunctionCall { name, .. } => {
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
            for child in expr.children() {
                check_policy_expr(child, clause, policy_name, table_schema, schema, diags);
            }
        }

        ExprKind::BinaryOp { left, right, .. } | ExprKind::Quantified { left, right, .. } => {
            check_policy_expr(left, clause, policy_name, table_schema, schema, diags);
            check_policy_expr(right, clause, policy_name, table_schema, schema, diags);
        }

        ExprKind::UnaryOp { expr, .. } => {
            check_policy_expr(expr, clause, policy_name, table_schema, schema, diags);
        }

        ExprKind::Cast { expr, .. } => {
            check_policy_expr(expr, clause, policy_name, table_schema, schema, diags);
        }

        ExprKind::Case {
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

        ExprKind::InList { expr, list, .. } => {
            check_policy_expr(expr, clause, policy_name, table_schema, schema, diags);
            for item in list {
                check_policy_expr(item, clause, policy_name, table_schema, schema, diags);
            }
        }

        ExprKind::Between {
            expr, low, high, ..
        } => {
            check_policy_expr(expr, clause, policy_name, table_schema, schema, diags);
            check_policy_expr(low, clause, policy_name, table_schema, schema, diags);
            check_policy_expr(high, clause, policy_name, table_schema, schema, diags);
        }

        // Subqueries in policy expressions are uncommon but legal; skip for now.
        ExprKind::Subquery(_) | ExprKind::InSubquery { .. } => {}

        // Literals and wildcards carry no column or function references.
        // Idem pour les expressions de policy RLS.
        ExprKind::AtTimeZone {
            timestamp,
            time_zone,
        } => {
            check_policy_expr(timestamp, clause, policy_name, table_schema, schema, diags);
            check_policy_expr(time_zone, clause, policy_name, table_schema, schema, diags);
        }
        ExprKind::Collate { expr, .. } => {
            check_policy_expr(expr, clause, policy_name, table_schema, schema, diags)
        }
        ExprKind::Tuple(items) => {
            for e in items {
                check_policy_expr(e, clause, policy_name, table_schema, schema, diags);
            }
        }
        ExprKind::FieldAccess { root, .. } => {
            check_policy_expr(root, clause, policy_name, table_schema, schema, diags)
        }
        ExprKind::NamedArgument { value, .. } => {
            check_policy_expr(value, clause, policy_name, table_schema, schema, diags)
        }

        // Une sous-requête de policy a sa propre portée : hors périmètre ici.
        ExprKind::Exists { .. } => {}

        ExprKind::Literal(_)
        | ExprKind::Wildcard
        | ExprKind::QualifiedWildcard(_)
        | ExprKind::Interval(_)
        | ExprKind::Parameter(_)
        | ExprKind::Unsupported(_) => {}
    }
}
