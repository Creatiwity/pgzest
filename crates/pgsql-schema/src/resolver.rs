use std::collections::{HashMap, HashSet};

use pgsql_ast::{AlterTableOp, DdlStatement, Statement};
use pgsql_checker::schema::{
    object_name_to_string, ColumnInfo, FunctionSignature, RlsAwareSchemaProvider, SchemaProvider,
    TableSchema,
};

use crate::index::{qualify, SchemaIndex};
use crate::source::{SchemaSource, SqlChunk};

// ── SchemaResolver ────────────────────────────────────────────────────────────

/// Full schema built from all sources.  Replaces `DdlSchemaProvider` as the
/// primary `SchemaProvider` implementation; additionally exposes the
/// `SchemaIndex` for tooling (go-to-definition, completion, role checks).
///
/// Construction is eager: all chunks are parsed once at build time.
/// ALTER TABLE mutations (ADD COLUMN, DROP COLUMN, RLS) are applied in
/// chunk order so migrations layer on top of the base schema correctly.
pub struct SchemaResolver {
    tables:      HashMap<String, TableSchema>,
    functions:   HashMap<String, FunctionSignature>,
    rls_enabled: HashSet<String>,
    pub index:   SchemaIndex,
}

impl SchemaResolver {
    /// Build from a `SchemaSource` (loads all chunks).
    pub fn from_source(source: &dyn SchemaSource) -> Self {
        let chunks = source.chunks();
        Self::from_chunks(&chunks)
    }

    /// Build from pre-collected chunks (already sorted by the source).
    pub fn from_chunks(chunks: &[SqlChunk]) -> Self {
        let mut tables: HashMap<String, TableSchema> = HashMap::new();
        let mut functions: HashMap<String, FunctionSignature> = HashMap::new();
        let mut rls_enabled: HashSet<String> = HashSet::new();

        for chunk in chunks {
            let stmts = match pgsql_parser::parse(&chunk.sql) {
                Ok(s) => s,
                Err(_) => continue,
            };
            apply_statements(&stmts, &mut tables, &mut functions, &mut rls_enabled);
        }

        let index = SchemaIndex::from_chunks(chunks);
        Self { tables, functions, rls_enabled, index }
    }

    /// Empty resolver (no schema loaded).
    pub fn empty() -> Self {
        Self {
            tables: HashMap::new(),
            functions: HashMap::new(),
            rls_enabled: HashSet::new(),
            index: SchemaIndex::default(),
        }
    }

    /// Build from a single SQL string (convenience for tests / LSP).
    pub fn from_sql(sql: &str) -> Result<Self, String> {
        let stmts = pgsql_parser::parse(sql).map_err(|e| e.to_string())?;
        let mut tables = HashMap::new();
        let mut functions = HashMap::new();
        let mut rls_enabled = HashSet::new();
        apply_statements(&stmts, &mut tables, &mut functions, &mut rls_enabled);
        Ok(Self { tables, functions, rls_enabled, index: SchemaIndex::default() })
    }
}

// ── SchemaProvider impl ───────────────────────────────────────────────────────

impl SchemaProvider for SchemaResolver {
    fn get_table(&self, name: &str) -> Option<&TableSchema> {
        resolve_name(name, &self.tables)
    }

    fn get_function(&self, name: &str) -> Option<&FunctionSignature> {
        resolve_name(name, &self.functions)
    }

    fn all_table_names(&self) -> Vec<&str> {
        self.tables.keys().map(|s| s.as_str()).collect()
    }

    fn as_rls_aware(&self) -> Option<&dyn RlsAwareSchemaProvider> {
        Some(self)
    }
}

impl RlsAwareSchemaProvider for SchemaResolver {
    fn table_has_rls(&self, table_name: &str) -> bool {
        self.rls_enabled.contains(&table_name.to_lowercase())
    }
}

// ── Internal statement application ───────────────────────────────────────────

fn apply_statements(
    stmts: &[Statement],
    tables: &mut HashMap<String, TableSchema>,
    functions: &mut HashMap<String, FunctionSignature>,
    rls_enabled: &mut HashSet<String>,
) {
    use pgsql_ast::ddl::AlterColumnAction;
    use pgsql_ast::types::DataType;

    for stmt in stmts {
        let Statement::Ddl(ddl) = stmt else { continue };
        match ddl {
            DdlStatement::CreateTable(ct) => {
                let key = qualify(object_name_to_string(&ct.name));
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
                    key,
                    TableSchema { name: object_name_to_string(&ct.name), columns },
                );
            }

            DdlStatement::CreateFunction(cf) => {
                let key = qualify(object_name_to_string(&cf.name));
                let params: Vec<DataType> =
                    cf.params.iter().map(|p| p.data_type.clone()).collect();
                let return_type = match &cf.return_type {
                    pgsql_ast::ReturnType::Scalar(dt) => dt.clone(),
                    pgsql_ast::ReturnType::SetOf(dt) => dt.clone(),
                    pgsql_ast::ReturnType::Table(_) => DataType::Custom("TABLE".into()),
                    pgsql_ast::ReturnType::Trigger => DataType::Custom("TRIGGER".into()),
                    pgsql_ast::ReturnType::Void => DataType::Custom("VOID".into()),
                };
                functions.insert(
                    key,
                    FunctionSignature {
                        name: object_name_to_string(&cf.name),
                        params,
                        return_type,
                    },
                );
            }

            DdlStatement::CreateProcedure(cp) => {
                let key = qualify(object_name_to_string(&cp.name));
                functions.insert(
                    key,
                    FunctionSignature {
                        name: object_name_to_string(&cp.name),
                        params: cp.params.iter().map(|p| p.data_type.clone()).collect(),
                        return_type: DataType::Custom("VOID".into()),
                    },
                );
            }

            DdlStatement::AlterTable(at) => {
                let table_key = qualify(object_name_to_string(&at.name));
                for op in &at.operations {
                    match op {
                        AlterTableOp::EnableRowLevelSecurity => {
                            rls_enabled.insert(table_key.clone());
                        }
                        AlterTableOp::DisableRowLevelSecurity => {
                            rls_enabled.remove(&table_key);
                        }
                        AlterTableOp::AddColumn(col_def) => {
                            if let Some(table) = find_table_mut(tables, &table_key) {
                                // Avoid duplicate if already present
                                let col_lower = col_def.name.value.to_lowercase();
                                if !table.columns.iter().any(|c| c.name.to_lowercase() == col_lower)
                                {
                                    table.columns.push(ColumnInfo {
                                        name: col_def.name.value.clone(),
                                        data_type: col_def.data_type.clone(),
                                        nullable: col_def.nullable,
                                    });
                                }
                            }
                        }
                        AlterTableOp::DropColumn { name, .. } => {
                            if let Some(table) = find_table_mut(tables, &table_key) {
                                let col_lower = name.value.to_lowercase();
                                table.columns.retain(|c| c.name.to_lowercase() != col_lower);
                            }
                        }
                        AlterTableOp::AlterColumn { name, action } => {
                            if let Some(table) = find_table_mut(tables, &table_key) {
                                let col_lower = name.value.to_lowercase();
                                if let Some(col) = table
                                    .columns
                                    .iter_mut()
                                    .find(|c| c.name.to_lowercase() == col_lower)
                                {
                                    match action {
                                        AlterColumnAction::SetNotNull => col.nullable = false,
                                        AlterColumnAction::DropNotNull => col.nullable = true,
                                        AlterColumnAction::SetDataType(dt) => {
                                            col.data_type = dt.clone()
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }

            _ => {}
        }
    }
}

// ── Lookup helpers ────────────────────────────────────────────────────────────

/// 3-step name resolution (exact → public.name → strip schema prefix).
fn resolve_name<'a, V>(name: &str, map: &'a HashMap<String, V>) -> Option<&'a V> {
    let lower = name.to_lowercase();
    if let Some(v) = map.get(&lower) {
        return Some(v);
    }
    let with_public = format!("public.{lower}");
    if let Some(v) = map.get(&with_public) {
        return Some(v);
    }
    if let Some(dot) = lower.find('.') {
        if let Some(v) = map.get(&lower[dot + 1..]) {
            return Some(v);
        }
    }
    None
}

/// Mutable variant of the 3-step resolver.
fn find_table_mut<'a>(
    tables: &'a mut HashMap<String, TableSchema>,
    key: &str,
) -> Option<&'a mut TableSchema> {
    let lower = key.to_lowercase();
    if tables.contains_key(&lower) {
        return tables.get_mut(&lower);
    }
    let with_public = format!("public.{lower}");
    if tables.contains_key(&with_public) {
        return tables.get_mut(&with_public);
    }
    if let Some(dot) = lower.find('.') {
        let unqualified = &lower[dot + 1..];
        if tables.contains_key(unqualified) {
            return tables.get_mut(unqualified);
        }
    }
    None
}
