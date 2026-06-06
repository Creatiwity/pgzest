use std::collections::HashMap;

use pgsql_ast::types::DataType;
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
    /// Look up a column by name (case-insensitive).
    pub fn get_column(&self, name: &str) -> Option<&ColumnInfo> {
        let lower = name.to_lowercase();
        self.columns.iter().find(|c| c.name.to_lowercase() == lower)
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

// ── DdlSchemaProvider ─────────────────────────────────────────────────────────

/// Builds a schema from a slice of already-parsed AST statements.
pub struct DdlSchemaProvider {
    tables: HashMap<String, TableSchema>,
    functions: HashMap<String, FunctionSignature>,
    /// Set of table names (lowercased) that have RLS enabled via ALTER TABLE.
    rls_enabled: std::collections::HashSet<String>,
}

impl DdlSchemaProvider {
    pub fn from_statements(stmts: &[pgsql_ast::Statement]) -> Self {
        let mut tables: HashMap<String, TableSchema> = HashMap::new();
        let mut functions: HashMap<String, FunctionSignature> = HashMap::new();
        let mut rls_enabled: std::collections::HashSet<String> = std::collections::HashSet::new();

        for stmt in stmts {
            if let pgsql_ast::Statement::Ddl(ddl) = stmt {
                match ddl {
                    pgsql_ast::DdlStatement::CreateTable(ct) => {
                        let table_name = object_name_to_string(&ct.name);
                        let columns = ct
                            .columns
                            .iter()
                            .map(|col| ColumnInfo {
                                name: col.name.value.clone(),
                                data_type: col.data_type.clone(),
                                nullable: col.nullable,
                            })
                            .collect();
                        tables.insert(
                            table_name.to_lowercase(),
                            TableSchema {
                                name: table_name,
                                columns,
                            },
                        );
                    }
                    pgsql_ast::DdlStatement::CreateFunction(cf) => {
                        let func_name = object_name_to_string(&cf.name);
                        let params: Vec<DataType> = cf
                            .params
                            .iter()
                            .map(|p| p.data_type.clone())
                            .collect();
                        let return_type = match &cf.return_type {
                            pgsql_ast::ReturnType::Scalar(dt) => dt.clone(),
                            pgsql_ast::ReturnType::SetOf(dt) => dt.clone(),
                            pgsql_ast::ReturnType::Table(_) => DataType::Custom("TABLE".into()),
                            pgsql_ast::ReturnType::Trigger => DataType::Custom("TRIGGER".into()),
                            pgsql_ast::ReturnType::Void => DataType::Custom("VOID".into()),
                        };
                        functions.insert(
                            func_name.to_lowercase(),
                            FunctionSignature {
                                name: func_name,
                                params,
                                return_type,
                            },
                        );
                    }
                    pgsql_ast::DdlStatement::AlterTable(at) => {
                        let table_name = object_name_to_string(&at.name).to_lowercase();
                        for op in &at.operations {
                            match op {
                                pgsql_ast::AlterTableOp::EnableRowLevelSecurity => {
                                    rls_enabled.insert(table_name.clone());
                                }
                                pgsql_ast::AlterTableOp::DisableRowLevelSecurity => {
                                    rls_enabled.remove(&table_name);
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        Self { tables, functions, rls_enabled }
    }

    /// Parse `sql` and build the schema from the resulting DDL statements.
    pub fn from_sql(sql: &str) -> Result<Self, String> {
        let stmts = pgsql_parser::parse(sql).map_err(|e| e.to_string())?;
        Ok(Self::from_statements(&stmts))
    }
}

impl SchemaProvider for DdlSchemaProvider {
    fn get_table(&self, name: &str) -> Option<&TableSchema> {
        self.tables.get(&name.to_lowercase())
    }

    fn get_function(&self, name: &str) -> Option<&FunctionSignature> {
        self.functions.get(&name.to_lowercase())
    }

    fn all_table_names(&self) -> Vec<&str> {
        self.tables.keys().map(|s| s.as_str()).collect()
    }

    fn as_rls_aware(&self) -> Option<&dyn RlsAwareSchemaProvider> {
        Some(self)
    }
}

impl RlsAwareSchemaProvider for DdlSchemaProvider {
    fn table_has_rls(&self, table_name: &str) -> bool {
        self.rls_enabled.contains(&table_name.to_lowercase())
    }
}

// ── IntrospectionJsonProvider ─────────────────────────────────────────────────

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
        "time" => DataType::Time,
        "timestamp" => DataType::Timestamp,
        "timestamptz" | "timestamp with time zone" => DataType::TimestampTz,
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
        Self { providers: Vec::new() }
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
        self.providers.iter().find_map(|p| p.get_operator(op, left, right))
    }

    fn all_table_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.providers.iter()
            .flat_map(|p| p.all_table_names())
            .collect();
        names.sort();
        names.dedup();
        names
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

pub fn object_name_to_string(name: &pgsql_ast::ObjectName) -> String {
    name.0
        .iter()
        .map(|i| i.value.clone())
        .collect::<Vec<_>>()
        .join(".")
}
