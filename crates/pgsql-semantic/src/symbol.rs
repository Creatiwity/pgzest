use pgsql_ast::types::DataType;
use pgsql_ast::{NodeId, Span};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq)]
pub enum SymbolKind {
    Table,
    Column { table: String },
    Function,
    Cte,
    PlVar,   // variable DECLARE dans PL/pgSQL
    PlParam, // paramètre de fonction/procédure
}

#[derive(Debug, Clone)]
pub struct Symbol {
    pub name: String, // lower-case
    pub kind: SymbolKind,
    pub data_type: DataType,
    pub node_id: NodeId,
    pub span: Span,
}

/// Maps lowercase name → Symbol.
/// Lookup is case-insensitive (names stored lowercase).
#[derive(Debug, Default)]
pub struct SymbolTable {
    // global symbols (tables, functions from SchemaProvider)
    tables: HashMap<String, Symbol>,
    functions: HashMap<String, Symbol>,
}

impl SymbolTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_table(&mut self, name: impl Into<String>, sym: Symbol) {
        self.tables.insert(name.into().to_lowercase(), sym);
    }

    pub fn add_function(&mut self, name: impl Into<String>, sym: Symbol) {
        self.functions.insert(name.into().to_lowercase(), sym);
    }

    pub fn get_table(&self, name: &str) -> Option<&Symbol> {
        self.tables.get(&name.to_lowercase())
    }

    pub fn get_function(&self, name: &str) -> Option<&Symbol> {
        self.functions.get(&name.to_lowercase())
    }

    /// All table names (for completion).
    pub fn table_names(&self) -> impl Iterator<Item = &str> {
        self.tables.keys().map(String::as_str)
    }
}
