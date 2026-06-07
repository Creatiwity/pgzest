use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use pgsql_ast::{DdlStatement, Statement};

use crate::role::FileRole;
use crate::source::SqlChunk;

// ── IndexEntry ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct SourceOrigin {
    pub path: PathBuf,
    /// Byte range within the file where the defining statement starts/ends.
    pub byte_range: std::ops::Range<usize>,
}

#[derive(Debug, Clone)]
pub struct IndexEntry {
    pub origin: SourceOrigin,
    pub role: FileRole,
    /// The raw SQL of the chunk that defines this object (for lazy resolution).
    pub chunk_sql: String,
}

// ── SchemaIndex ───────────────────────────────────────────────────────────────

/// Fast name-only index. Built by scanning all chunks without deep expression parsing.
/// Keys are always `schema.name` lowercase (e.g. `"public.users"`).
/// Unqualified names default to the `public` schema.
#[derive(Debug, Default)]
pub struct SchemaIndex {
    pub tables:     HashMap<String, IndexEntry>,
    pub functions:  HashMap<String, IndexEntry>,
    pub procedures: HashMap<String, IndexEntry>,
    pub types:      HashMap<String, IndexEntry>,
    pub sequences:  HashMap<String, IndexEntry>,
    pub schemas:    HashSet<String>,
}

impl SchemaIndex {
    /// Build from already-sorted chunks.
    pub fn from_chunks(chunks: &[SqlChunk]) -> Self {
        let mut idx = SchemaIndex::default();
        idx.schemas.insert("public".to_string());

        for chunk in chunks {
            let stmts = match pgsql_parser::parse(&chunk.sql) {
                Ok(s) => s,
                Err(_) => continue,
            };
            for stmt in &stmts {
                idx.index_statement(stmt, chunk);
            }
        }
        idx
    }

    fn index_statement(&mut self, stmt: &Statement, chunk: &SqlChunk) {
        use pgsql_checker::schema::object_name_to_string;

        let make_entry = |chunk: &SqlChunk| IndexEntry {
            origin: SourceOrigin {
                path: chunk.origin.path.clone(),
                byte_range: chunk.origin.byte_range.clone(),
            },
            role: chunk.role.clone(),
            chunk_sql: chunk.sql.clone(),
        };

        if let Statement::Ddl(ddl) = stmt {
            match ddl {
                DdlStatement::CreateTable(ct) => {
                    let name = qualify(object_name_to_string(&ct.name));
                    self.tables.entry(name).or_insert_with(|| make_entry(chunk));
                }
                DdlStatement::CreateFunction(cf) => {
                    let name = qualify(object_name_to_string(&cf.name));
                    self.functions.entry(name).or_insert_with(|| make_entry(chunk));
                }
                DdlStatement::CreateProcedure(cp) => {
                    let name = qualify(object_name_to_string(&cp.name));
                    self.procedures.entry(name).or_insert_with(|| make_entry(chunk));
                }
                _ => {}
            }
        }
    }

    /// All table names known to the index.
    pub fn all_table_names(&self) -> impl Iterator<Item = &str> {
        self.tables.keys().map(|s| s.as_str())
    }

    /// Check if a file path belongs to any schema source (by role).
    pub fn role_of_path(&self, path: &std::path::Path) -> Option<&FileRole> {
        self.tables.values()
            .chain(self.functions.values())
            .chain(self.procedures.values())
            .find(|e| e.origin.path == path)
            .map(|e| &e.role)
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Ensure a name is schema-qualified with `public.` if it has no dot.
pub(crate) fn qualify(name: String) -> String {
    let lower = name.to_lowercase();
    if lower.contains('.') { lower } else { format!("public.{lower}") }
}
