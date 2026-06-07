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

// ── Schema qualification helpers ──────────────────────────────────────────────

/// Qualify an `ObjectName` with the given default schema.
/// If the name already has 2+ parts (e.g. `public.users`), join them lowercased.
/// If it has 1 part (e.g. `users`), prepend `default_schema`.
pub fn qualify_object_name(name: &pgsql_ast::ObjectName, default_schema: &str) -> String {
    if name.0.len() >= 2 {
        name.0.iter().map(|i| i.value.to_lowercase()).collect::<Vec<_>>().join(".")
    } else {
        let local = name.0.first().map(|i| i.value.to_lowercase()).unwrap_or_default();
        format!("{}.{}", default_schema.to_lowercase(), local)
    }
}

/// Qualify a dotted name string with the given default schema.
/// If `name` already contains a dot, return it lowercased unchanged.
/// Otherwise, prepend `default_schema`.
pub fn qualify_str(name: &str, default_schema: &str) -> String {
    let lower = name.to_lowercase();
    if lower.contains('.') { lower } else { format!("{}.{}", default_schema.to_lowercase(), lower) }
}

/// Parse the full `search_path` from raw SQL text.
/// Recognises `SET search_path TO a, b, c` / `SET search_path = a, b, c`
/// and the `-- @schema: a, b, c` comment override.
/// `$user` entries are skipped (they are session-dependent).
/// Returns `["public"]` when nothing is found.
pub fn detect_search_path(sql: &str) -> Vec<String> {
    for line in sql.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("-- @schema:") {
            let schemas: Vec<String> = rest
                .split(',')
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty())
                .collect();
            if !schemas.is_empty() {
                return schemas;
            }
        }
        let lower = trimmed.to_lowercase();
        if lower.starts_with("set search_path") {
            let after_kw = if let Some(r) = lower.split_once("to") { r.1 }
                           else if let Some(r) = lower.split_once('=') { r.1 }
                           else { continue };
            let schemas: Vec<String> = after_kw
                .split(',')
                .map(|s| s.trim().trim_matches(';').trim().to_string())
                .filter(|s| !s.is_empty() && s != "$user")
                .collect();
            if !schemas.is_empty() {
                return schemas;
            }
        }
    }
    vec!["public".to_string()]
}

/// Returns the first schema from `detect_search_path` — the schema new objects
/// are created in when an unqualified name is used.
pub fn detect_default_schema(sql: &str) -> String {
    detect_search_path(sql).into_iter().next().unwrap_or_else(|| "public".to_string())
}

// ── DdlSchemaProvider ─────────────────────────────────────────────────────────

/// Builds a schema from a slice of already-parsed AST statements.
/// All objects are stored under a fully-qualified `"schema.name"` key so that
/// lookups work regardless of whether the caller uses qualified or bare names.
///
/// Unqualified names are resolved by iterating `search_path` in order, matching
/// PostgreSQL's own rule: first schema wins, new objects go into the first schema.
pub struct DdlSchemaProvider {
    tables: HashMap<String, TableSchema>,
    functions: HashMap<String, FunctionSignature>,
    rls_enabled: std::collections::HashSet<String>,
    /// Ordered list of schemas to search for unqualified names.
    /// `search_path[0]` is also used as the schema for new object creation.
    search_path: Vec<String>,
}

impl DdlSchemaProvider {
    /// Build a schema with `["public"]` as the search path.
    pub fn from_statements(stmts: &[pgsql_ast::Statement]) -> Self {
        Self::from_statements_with_search_path(stmts, &["public"])
    }

    /// Build a schema using a single `default_schema` for unqualified names.
    pub fn from_statements_with_schema(stmts: &[pgsql_ast::Statement], default_schema: &str) -> Self {
        Self::from_statements_with_search_path(stmts, &[default_schema])
    }

    /// Build a schema respecting the full `search_path` (PostgreSQL semantics):
    /// - new unqualified objects are stored under `search_path[0]`
    /// - lookups iterate `search_path` in order, returning the first match
    pub fn from_statements_with_search_path(stmts: &[pgsql_ast::Statement], search_path: &[&str]) -> Self {
        let search_path: Vec<String> = search_path.iter().map(|s| s.to_lowercase()).collect();
        let creation_schema = search_path.first().map(String::as_str).unwrap_or("public");

        let mut tables: HashMap<String, TableSchema> = HashMap::new();
        let mut functions: HashMap<String, FunctionSignature> = HashMap::new();
        let mut rls_enabled: std::collections::HashSet<String> = std::collections::HashSet::new();

        for stmt in stmts {
            if let pgsql_ast::Statement::Ddl(ddl) = stmt {
                match ddl {
                    pgsql_ast::DdlStatement::CreateTable(ct) => {
                        let key = qualify_object_name(&ct.name, creation_schema);
                        let table_name = object_name_to_string(&ct.name);
                        let columns = ct.columns.iter().map(|col| ColumnInfo {
                            name: col.name.value.clone(),
                            data_type: col.data_type.clone(),
                            nullable: col.nullable,
                        }).collect();
                        tables.insert(key, TableSchema { name: table_name, columns });
                    }
                    pgsql_ast::DdlStatement::CreateFunction(cf) => {
                        let key = qualify_object_name(&cf.name, creation_schema);
                        let func_name = object_name_to_string(&cf.name);
                        let params: Vec<DataType> = cf.params.iter().map(|p| p.data_type.clone()).collect();
                        let return_type = match &cf.return_type {
                            pgsql_ast::ReturnType::Scalar(dt) => dt.clone(),
                            pgsql_ast::ReturnType::SetOf(dt) => dt.clone(),
                            pgsql_ast::ReturnType::Table(_) => DataType::Custom("TABLE".into()),
                            pgsql_ast::ReturnType::Trigger => DataType::Custom("TRIGGER".into()),
                            pgsql_ast::ReturnType::Void => DataType::Custom("VOID".into()),
                        };
                        functions.insert(key, FunctionSignature { name: func_name, params, return_type });
                    }
                    pgsql_ast::DdlStatement::AlterTable(at) => {
                        let key = qualify_object_name(&at.name, creation_schema);
                        for op in &at.operations {
                            match op {
                                pgsql_ast::AlterTableOp::EnableRowLevelSecurity => {
                                    rls_enabled.insert(key.clone());
                                }
                                pgsql_ast::AlterTableOp::DisableRowLevelSecurity => {
                                    rls_enabled.remove(&key);
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        Self { tables, functions, rls_enabled, search_path }
    }

    /// Parse `sql`, auto-detect its `search_path` from `SET search_path` /
    /// `-- @schema:`, and build the schema from the resulting DDL statements.
    pub fn from_sql(sql: &str) -> Result<Self, String> {
        let sp = detect_search_path(sql);
        let sp_refs: Vec<&str> = sp.iter().map(String::as_str).collect();
        let stmts = pgsql_parser::parse(sql).map_err(|e| e.to_string())?;
        Ok(Self::from_statements_with_search_path(&stmts, &sp_refs))
    }

    fn lookup_unqualified<'a, V>(
        map: &'a HashMap<String, V>,
        local: &str,
        search_path: &[String],
    ) -> Option<&'a V> {
        search_path.iter().find_map(|schema| map.get(&format!("{}.{}", schema, local)))
    }
}

impl SchemaProvider for DdlSchemaProvider {
    fn get_table(&self, name: &str) -> Option<&TableSchema> {
        let lower = name.to_lowercase();
        if lower.contains('.') {
            self.tables.get(&lower)
        } else {
            Self::lookup_unqualified(&self.tables, &lower, &self.search_path)
        }
    }

    fn get_function(&self, name: &str) -> Option<&FunctionSignature> {
        let lower = name.to_lowercase();
        if lower.contains('.') {
            self.functions.get(&lower)
        } else {
            Self::lookup_unqualified(&self.functions, &lower, &self.search_path)
        }
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
        let lower = table_name.to_lowercase();
        if lower.contains('.') {
            self.rls_enabled.contains(&lower)
        } else {
            self.search_path.iter().any(|schema| {
                self.rls_enabled.contains(&format!("{}.{}", schema, lower))
            })
        }
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
