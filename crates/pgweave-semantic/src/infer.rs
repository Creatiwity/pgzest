use crate::scope::{ScopeId, ScopeTree};
use crate::symbol::SymbolTable;
use pgweave_ast::NodeId;
use pgweave_ast::expr::{BinaryOp, Expr, ExprKind, Literal};
use pgweave_ast::types::DataType;
use pgweave_schema::provider::SchemaProvider;

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
    match &expr.kind {
        // Literals — always known
        ExprKind::Literal(lit) => Some(match lit {
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
        ExprKind::Column(col_ref) => {
            let col_lower = col_ref.column.value.to_lowercase();
            // Try qualified reference first (table.col)
            if let Some(tbl) = &col_ref.table {
                let tbl_lower = tbl.value.to_lowercase();
                let key = format!("{}.{}", tbl_lower, col_lower);
                if let Some(scope) = scopes.get(scope_id)
                    && let Some(binding) = scope.columns.get(&key)
                {
                    return Some(binding.data_type.clone());
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
        ExprKind::Cast { data_type, .. } => Some(data_type.clone()),

        // Binary operators
        ExprKind::BinaryOp { op, left, right } => {
            match op {
                // Comparison operators → Boolean
                BinaryOp::Eq
                | BinaryOp::NotEq
                | BinaryOp::Lt
                | BinaryOp::Lte
                | BinaryOp::Gt
                | BinaryOp::Gte
                | BinaryOp::And
                | BinaryOp::Or
                | BinaryOp::Like
                | BinaryOp::NotLike
                | BinaryOp::ILike
                | BinaryOp::Is
                | BinaryOp::IsNot
                | BinaryOp::IsDistinctFrom
                | BinaryOp::IsNotDistinctFrom
                | BinaryOp::In
                | BinaryOp::NotIn => Some(DataType::Boolean),
                // Arithmetic — use left operand type as proxy
                BinaryOp::Plus
                | BinaryOp::Minus
                | BinaryOp::Multiply
                | BinaryOp::Divide
                | BinaryOp::Modulo => infer_expr_type(left, scope_id, scopes, symbols, schema),
                // `||` produit toujours du texte, quels que soient les opérandes.
                BinaryOp::Concat => Some(DataType::Text),
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
        ExprKind::UnaryOp { expr, .. } => infer_expr_type(expr, scope_id, scopes, symbols, schema),

        // Case — use type of first THEN clause
        ExprKind::Case { when_clauses, .. } => when_clauses
            .first()
            .and_then(|(_, then)| infer_expr_type(then, scope_id, scopes, symbols, schema)),

        // Function calls — look up return type in symbol table
        ExprKind::FunctionCall { name, .. } => {
            let func_name = crate::schema_name(name);
            // Known built-ins with fixed return types
            match func_name.to_lowercase().as_str() {
                "count" | "count_distinct" => Some(DataType::BigInt),
                "sum" | "avg" => Some(DataType::Numeric {
                    precision: None,
                    scale: None,
                }),
                "min" | "max" => None, // depends on argument — skip for now
                "now" | "current_timestamp" => Some(DataType::TimestampTz(None)),
                "lower" | "upper" | "trim" | "concat" | "substr" | "replace" => {
                    Some(DataType::Text)
                }
                "length" | "char_length" => Some(DataType::Integer),
                "round" | "ceil" | "floor" | "abs" => Some(DataType::Numeric {
                    precision: None,
                    scale: None,
                }),
                "coalesce" | "nullif" => None, // polymorphic — infer from args
                "uuid_generate_v4" | "gen_random_uuid" => Some(DataType::Uuid),
                _ => {
                    // Look up user-defined function return type
                    symbols
                        .get_function(&func_name)
                        .map(|s| s.data_type.clone())
                }
            }
        }

        // Subquery — would require recursive analysis; return None for now
        ExprKind::Subquery(_) | ExprKind::InSubquery { .. } => None,

        // IN list — Boolean
        ExprKind::InList { .. } => Some(DataType::Boolean),

        // BETWEEN — Boolean
        ExprKind::Between { .. } => Some(DataType::Boolean),

        // ── Constructions ajoutées ────────────────────────────────────────────
        // `EXISTS (…)` est un prédicat.
        ExprKind::Exists { .. } => Some(DataType::Boolean),

        ExprKind::Interval(_) => Some(DataType::Custom("interval".into())),

        // `AT TIME ZONE` sur un timestamptz donne un timestamp, et inversement.
        // Sans suivre cette bascule finement, on garde le type de l'opérande :
        // c'est faux dans un cas sur deux, donc on préfère ne rien affirmer.
        ExprKind::AtTimeZone { .. } => None,

        // `COLLATE` ne change pas le type, seulement l'ordre de tri.
        ExprKind::Collate { expr, .. } => infer_expr_type(expr, scope_id, scopes, symbols, schema),

        // Un constructeur de ligne n'a pas de type scalaire.
        ExprKind::Tuple(_) => None,
        // Élément de `GROUP BY`, pas une valeur.
        ExprKind::GroupingSets { .. } => None,

        // L'accès à un élément de tableau ou à un champ composite demanderait de
        // résoudre le type de la base puis de le décomposer — pas encore fait.
        ExprKind::FieldAccess { .. } => None,

        // Un argument nommé a le type de sa valeur.
        ExprKind::NamedArgument { value, .. } => {
            infer_expr_type(value, scope_id, scopes, symbols, schema)
        }

        // Comparaison à chaque élément d'un ensemble : toujours booléenne.
        ExprKind::Quantified { .. } => Some(DataType::Boolean),

        // Le type d'un paramètre lié vient du site d'appel, pas de la requête.
        ExprKind::Parameter(_) => None,

        // Ne rien affirmer sur ce qu'on ne modélise pas : c'est toute la raison
        // d'être de cette variante.
        ExprKind::Unsupported(_) => None,

        // Wildcards don't have a single type
        ExprKind::Wildcard | ExprKind::QualifiedWildcard(_) => None,
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
        Date | Time(_) | TimeTz(_) | Timestamp(_) | TimestampTz(_) => "string", // ISO 8601
        Json | Jsonb => "unknown",
        Bytea => "Uint8Array",
        Array(inner) => match inner.as_ref() {
            Text | Varchar(_) | Char(_) => "string[]",
            SmallInt | Integer | BigInt => "number[]",
            _ => "unknown[]",
        },
        Interval => "string",
        Custom(_) | Modified { .. } => "unknown",
    }
}

fn with_precision(name: &str, p: Option<u64>) -> String {
    match p {
        Some(n) => format!("{name}({n})"),
        None => name.to_string(),
    }
}

/// Map DataType to its canonical SQL type name.
pub fn data_type_to_sql(dt: &DataType) -> String {
    use DataType::*;
    match dt {
        SmallInt => "smallint".into(),
        Integer => "integer".into(),
        BigInt => "bigint".into(),
        Numeric {
            precision: Some(p),
            scale: Some(s),
        } => format!("numeric({p},{s})"),
        Numeric {
            precision: Some(p),
            scale: None,
        } => format!("numeric({p})"),
        Numeric { .. } => "numeric".into(),
        Real => "real".into(),
        DoublePrecision => "double precision".into(),
        Text => "text".into(),
        Varchar(Some(n)) => format!("varchar({n})"),
        Varchar(None) => "varchar".into(),
        Char(Some(n)) => format!("char({n})"),
        Char(None) => "char".into(),
        Boolean => "boolean".into(),
        Date => "date".into(),
        Time(p) => with_precision("time", *p),
        TimeTz(p) => with_precision("timetz", *p),
        Timestamp(p) => with_precision("timestamp", *p),
        TimestampTz(p) => with_precision("timestamptz", *p),
        Interval => "interval".into(),
        Bytea => "bytea".into(),
        Json => "json".into(),
        Jsonb => "jsonb".into(),
        Uuid => "uuid".into(),
        Array(inner) => format!("{}[]", data_type_to_sql(inner)),
        Custom(s) => s.clone(),
        Modified { name, modifiers } => format!("{name}({})", modifiers.join(",")),
    }
}
