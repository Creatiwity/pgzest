use std::collections::HashMap;

use pgweave_ast::types::DataType;
use serde::Deserialize;

// ── Public schema types ───────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ColumnInfo {
    pub name: String,
    pub data_type: DataType,
    pub nullable: bool,
}

#[derive(Debug, Clone)]
pub struct TableSchema {
    pub name: String,
    pub columns: Vec<ColumnInfo>,
}

impl TableSchema {
    /// Colonne de ce nom, casse exacte (principe 8 : la casse est fixée au
    /// parse).
    pub fn get_column(&self, name: &str) -> Option<&ColumnInfo> {
        self.columns.iter().find(|c| c.name == name)
    }
}

#[derive(Debug, Clone)]
pub struct FunctionSignature {
    pub name: String,
    pub params: Vec<DataType>,
    pub return_type: DataType,
}

// ── SchemaProvider trait ──────────────────────────────────────────────────────

pub trait SchemaProvider: Send + Sync {
    fn get_table(&self, name: &str) -> Option<&TableSchema>;
    fn get_function(&self, name: &str) -> Option<&FunctionSignature>;
    fn all_table_names(&self) -> Vec<&str>;

    /// Look up the return type of a custom binary operator (e.g. PostGIS `<->`, `&&`).
    /// `op` is the operator symbol, `left` and `right` are the inferred operand types.
    /// Returns `None` by default — only extension providers need to implement this.
    fn get_operator(&self, _op: &str, _left: &DataType, _right: &DataType) -> Option<DataType> {
        None
    }

    /// Downcast hook for the optional RLS-aware extension.
    /// Returns `None` by default — safe for all existing providers.
    fn as_rls_aware(&self) -> Option<&dyn RlsAwareSchemaProvider> {
        None
    }
}

/// Optional extension for SchemaProvider: RLS awareness.
/// Implement this on your provider if you track ALTER TABLE ... ENABLE ROW LEVEL SECURITY.
pub trait RlsAwareSchemaProvider: SchemaProvider {
    fn table_has_rls(&self, table_name: &str) -> bool;
}

// ── Colonnes d'un CREATE TABLE ────────────────────────────────────────────────

/// Qualifie un nom pointé avec `default_schema` s'il ne l'est pas. Casse
/// conservée : elle a été fixée au parse (principe 8).
pub fn qualify_str(name: &str, default_schema: &str) -> String {
    if name.contains('.') {
        name.to_string()
    } else {
        format!("{default_schema}.{name}")
    }
}

/// Wraps an external JSON introspection blob.
///
/// Expected format:
/// ```json
/// {
///   "tables": [
///     {
///       "name": "my_table",
///       "columns": [
///         { "name": "id", "type": "uuid", "nullable": false }
///       ]
///     }
///   ]
/// }
/// ```
pub struct IntrospectionJsonProvider {
    tables: HashMap<String, TableSchema>,
    functions: HashMap<String, FunctionSignature>,
}

// ── JSON deserialization helpers (internal) ───────────────────────────────────

#[derive(Deserialize)]
struct JsonRoot {
    tables: Vec<JsonTable>,
}

#[derive(Deserialize)]
struct JsonTable {
    name: String,
    columns: Vec<JsonColumn>,
}

#[derive(Deserialize)]
struct JsonColumn {
    name: String,
    #[serde(rename = "type")]
    col_type: String,
    nullable: bool,
}

fn parse_data_type(s: &str) -> DataType {
    match s.to_lowercase().trim() {
        "smallint" | "int2" => DataType::SmallInt,
        "integer" | "int" | "int4" => DataType::Integer,
        "bigint" | "int8" => DataType::BigInt,
        "real" | "float4" => DataType::Real,
        "double precision" | "float8" => DataType::DoublePrecision,
        "text" => DataType::Text,
        "varchar" | "character varying" => DataType::Varchar(None),
        "char" | "character" => DataType::Char(None),
        "boolean" | "bool" => DataType::Boolean,
        "date" => DataType::Date,
        "time" | "time without time zone" => DataType::Time(None),
        "timetz" | "time with time zone" => DataType::TimeTz(None),
        "timestamp" => DataType::Timestamp(None),
        "timestamptz" | "timestamp with time zone" => DataType::TimestampTz(None),
        "interval" => DataType::Interval,
        "bytea" => DataType::Bytea,
        "json" => DataType::Json,
        "jsonb" => DataType::Jsonb,
        "uuid" => DataType::Uuid,
        "numeric" | "decimal" => DataType::Numeric {
            precision: None,
            scale: None,
        },
        other => DataType::Custom(other.to_string()),
    }
}

impl IntrospectionJsonProvider {
    pub fn from_json(json: &str) -> Result<Self, String> {
        let root: JsonRoot =
            serde_json::from_str(json).map_err(|e| format!("JSON parse error: {e}"))?;

        let mut tables: HashMap<String, TableSchema> = HashMap::new();
        for jt in root.tables {
            let columns = jt
                .columns
                .into_iter()
                .map(|col| ColumnInfo {
                    name: col.name,
                    data_type: parse_data_type(&col.col_type),
                    nullable: col.nullable,
                })
                .collect();
            tables.insert(
                jt.name.to_lowercase(),
                TableSchema {
                    name: jt.name,
                    columns,
                },
            );
        }

        Ok(Self {
            tables,
            functions: HashMap::new(),
        })
    }
}

impl SchemaProvider for IntrospectionJsonProvider {
    fn get_table(&self, name: &str) -> Option<&TableSchema> {
        self.tables.get(&name.to_lowercase())
    }

    fn get_function(&self, name: &str) -> Option<&FunctionSignature> {
        self.functions.get(&name.to_lowercase())
    }

    fn all_table_names(&self) -> Vec<&str> {
        self.tables.keys().map(|s| s.as_str()).collect()
    }
}

// ── CompositeSchemaProvider ───────────────────────────────────────────────────

/// Merges multiple SchemaProviders into one.
/// The first provider that has a result wins (priority order = insertion order).
pub struct CompositeSchemaProvider {
    providers: Vec<Box<dyn SchemaProvider>>,
}

impl CompositeSchemaProvider {
    pub fn new() -> Self {
        Self {
            providers: Vec::new(),
        }
    }

    pub fn add(&mut self, provider: Box<dyn SchemaProvider>) {
        self.providers.push(provider);
    }

    pub fn from_providers(providers: Vec<Box<dyn SchemaProvider>>) -> Self {
        Self { providers }
    }
}

impl Default for CompositeSchemaProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl SchemaProvider for CompositeSchemaProvider {
    fn get_table(&self, name: &str) -> Option<&TableSchema> {
        self.providers.iter().find_map(|p| p.get_table(name))
    }

    fn get_function(&self, name: &str) -> Option<&FunctionSignature> {
        self.providers.iter().find_map(|p| p.get_function(name))
    }

    fn get_operator(&self, op: &str, left: &DataType, right: &DataType) -> Option<DataType> {
        self.providers
            .iter()
            .find_map(|p| p.get_operator(op, left, right))
    }

    fn all_table_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self
            .providers
            .iter()
            .flat_map(|p| p.all_table_names())
            .collect();
        names.sort();
        names.dedup();
        names
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

pub fn object_name_to_string(name: &pgweave_ast::ObjectName) -> String {
    name.0
        .iter()
        .map(|i| i.value.clone())
        .collect::<Vec<_>>()
        .join(".")
}
