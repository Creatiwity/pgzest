use pgsql_ast::NodeId;
use pgsql_ast::types::DataType;
use pgsql_ast::expr::{Expr, Literal, BinaryOp};
use crate::scope::{ScopeId, ScopeTree};
use crate::symbol::SymbolTable;
use pgsql_checker::schema::SchemaProvider;

/// A resolved output column from a SELECT.
#[derive(Debug, Clone)]
pub struct OutputColumn {
    /// Alias if specified (AS name), otherwise derived from expression.
    pub name: Option<String>,
    pub data_type: DataType,
    pub node_id: NodeId,
}

impl OutputColumn {
    /// TypeScript type string for this column.
    pub fn ts_type(&self) -> &'static str {
        data_type_to_ts(&self.data_type)
    }
}

/// Infer the DataType of an expression given the current scope context.
/// Returns None if inference is not possible (unknown function, unresolved ref, etc.).
pub fn infer_expr_type(
    expr: &Expr,
    scope_id: ScopeId,
    scopes: &ScopeTree,
    symbols: &SymbolTable,
    schema: &dyn SchemaProvider,
) -> Option<DataType> {
    match expr {
        // Literals — always known
        Expr::Literal(lit) => Some(match lit {
            Literal::Number(s) => {
                // Distinguish integer vs float by presence of '.'
                if s.contains('.') {
                    DataType::DoublePrecision
                } else {
                    DataType::Integer
                }
            }
            Literal::String(_) => DataType::Text,
            Literal::Boolean(_) => DataType::Boolean,
            Literal::Null => DataType::Text, // NULL is polymorphic; use Text as placeholder
        }),

        // Column reference — look up in scope
        Expr::Column(col_ref) => {
            let col_lower = col_ref.column.value.to_lowercase();
            // Try qualified reference first (table.col)
            if let Some(tbl) = &col_ref.table {
                let tbl_lower = tbl.value.to_lowercase();
                let key = format!("{}.{}", tbl_lower, col_lower);
                if let Some(scope) = scopes.get(scope_id) {
                    if let Some(binding) = scope.columns.get(&key) {
                        return Some(binding.data_type.clone());
                    }
                }
            }
            // Unqualified: search all columns in scope chain
            let cols = scopes.available_columns(scope_id);
            for col in cols {
                if col.name.to_lowercase() == col_lower {
                    return Some(col.data_type.clone());
                }
            }
            None
        }

        // Cast — the cast target type is always known
        Expr::Cast { data_type, .. } => Some(data_type.clone()),

        // Binary operators
        Expr::BinaryOp { op, left, right } => {
            match op {
                // Comparison operators → Boolean
                BinaryOp::Eq | BinaryOp::NotEq | BinaryOp::Lt | BinaryOp::Lte
                | BinaryOp::Gt | BinaryOp::Gte | BinaryOp::And | BinaryOp::Or
                | BinaryOp::Like | BinaryOp::NotLike | BinaryOp::ILike
                | BinaryOp::Is | BinaryOp::IsNot | BinaryOp::In | BinaryOp::NotIn => {
                    Some(DataType::Boolean)
                }
                // Arithmetic — use left operand type as proxy
                BinaryOp::Plus | BinaryOp::Minus | BinaryOp::Multiply
                | BinaryOp::Divide | BinaryOp::Modulo => {
                    infer_expr_type(left, scope_id, scopes, symbols, schema)
                }
                // Custom / extension operators — ask the schema provider (e.g. PostGIS plugin)
                BinaryOp::Custom(op_sym) => {
                    let left_ty = infer_expr_type(left, scope_id, scopes, symbols, schema)
                        .unwrap_or(DataType::Custom("unknown".into()));
                    let right_ty = infer_expr_type(right, scope_id, scopes, symbols, schema)
                        .unwrap_or(DataType::Custom("unknown".into()));
                    schema.get_operator(op_sym, &left_ty, &right_ty)
                }
            }
        }

        // Unary
        Expr::UnaryOp { expr, .. } => {
            infer_expr_type(expr, scope_id, scopes, symbols, schema)
        }

        // Case — use type of first THEN clause
        Expr::Case { when_clauses, .. } => {
            when_clauses.first()
                .and_then(|(_, then)| infer_expr_type(then, scope_id, scopes, symbols, schema))
        }

        // Function calls — look up return type in symbol table
        Expr::FunctionCall { name, .. } => {
            let func_name = crate::schema_name(name);
            // Known built-ins with fixed return types
            match func_name.to_lowercase().as_str() {
                "count" | "count_distinct" => Some(DataType::BigInt),
                "sum" | "avg" => Some(DataType::Numeric { precision: None, scale: None }),
                "min" | "max" => None, // depends on argument — skip for now
                "now" | "current_timestamp" => Some(DataType::TimestampTz),
                "lower" | "upper" | "trim" | "concat" | "substr" | "replace" => Some(DataType::Text),
                "length" | "char_length" => Some(DataType::Integer),
                "round" | "ceil" | "floor" | "abs" => Some(DataType::Numeric { precision: None, scale: None }),
                "coalesce" | "nullif" => None, // polymorphic — infer from args
                "uuid_generate_v4" | "gen_random_uuid" => Some(DataType::Uuid),
                _ => {
                    // Look up user-defined function return type
                    symbols.get_function(&func_name)
                        .map(|s| s.data_type.clone())
                }
            }
        }

        // Subquery — would require recursive analysis; return None for now
        Expr::Subquery(_) | Expr::InSubquery { .. } => None,

        // IN list — Boolean
        Expr::InList { .. } => Some(DataType::Boolean),

        // BETWEEN — Boolean
        Expr::Between { .. } => Some(DataType::Boolean),

        // Wildcards don't have a single type
        Expr::Wildcard | Expr::QualifiedWildcard(_) => None,
    }
}

/// Map DataType to TypeScript type string.
pub fn data_type_to_ts(dt: &DataType) -> &'static str {
    use DataType::*;
    match dt {
        Boolean => "boolean",
        SmallInt | Integer | BigInt => "number",
        Real | DoublePrecision => "number",
        Numeric { .. } => "number",
        Text => "string",
        Varchar(_) | Char(_) => "string",
        Uuid => "string",
        Date | Time | Timestamp | TimestampTz => "string", // ISO 8601
        Json | Jsonb => "unknown",
        Bytea => "Uint8Array",
        Array(inner) => match inner.as_ref() {
            Text | Varchar(_) | Char(_) => "string[]",
            SmallInt | Integer | BigInt => "number[]",
            _ => "unknown[]",
        },
        Interval => "string",
        Custom(_) => "unknown",
    }
}
