use std::path::{Path, PathBuf};

use crate::role::{FileRole, sequence_from_path};

// ── Origin ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct SourceOrigin {
    pub path: PathBuf,
    pub byte_range: std::ops::Range<usize>,
}

// ── SqlChunk ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct SqlChunk {
    pub origin: SourceOrigin,
    pub sql: String,
    pub role: FileRole,
}

// ── SchemaSource trait ────────────────────────────────────────────────────────

pub trait SchemaSource: Send + Sync {
    fn chunks(&self) -> Vec<SqlChunk>;
}

// ── FileSource ────────────────────────────────────────────────────────────────

/// A single SQL file with an explicit role.
pub struct FileSource {
    pub path: PathBuf,
    pub role: FileRole,
}

impl FileSource {
    pub fn new(path: impl Into<PathBuf>, role: FileRole) -> Self {
        Self {
            path: path.into(),
            role,
        }
    }
}

impl SchemaSource for FileSource {
    fn chunks(&self) -> Vec<SqlChunk> {
        let sql = match std::fs::read_to_string(&self.path) {
            Ok(s) => s,
            Err(_) => return vec![],
        };
        let len = sql.len();
        vec![SqlChunk {
            origin: SourceOrigin {
                path: self.path.clone(),
                byte_range: 0..len,
            },
            sql,
            role: self.role.clone(),
        }]
    }
}

// ── DirSource ─────────────────────────────────────────────────────────────────

/// All `.sql` files in a directory, sorted lexicographically, with a fixed role.
pub struct DirSource {
    pub root: PathBuf,
    pub role: FileRole,
}

impl DirSource {
    pub fn new(root: impl Into<PathBuf>, role: FileRole) -> Self {
        Self {
            root: root.into(),
            role,
        }
    }
}

impl SchemaSource for DirSource {
    fn chunks(&self) -> Vec<SqlChunk> {
        sorted_sql_files(&self.root)
            .into_iter()
            .flat_map(|path| FileSource::new(path, self.role.clone()).chunks())
            .collect()
    }
}

// ── MigrationDirSource ────────────────────────────────────────────────────────

/// All `.sql` files in a migrations directory.
/// Each file gets `FileRole::Migration { sequence }` derived from its filename.
pub struct MigrationDirSource {
    pub root: PathBuf,
}

impl MigrationDirSource {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

impl SchemaSource for MigrationDirSource {
    fn chunks(&self) -> Vec<SqlChunk> {
        sorted_sql_files(&self.root)
            .into_iter()
            .flat_map(|path| {
                let seq = sequence_from_path(&path);
                FileSource::new(path, FileRole::Migration { sequence: seq }).chunks()
            })
            .collect()
    }
}

// ── MultiSource ───────────────────────────────────────────────────────────────

/// Ordered composition of sources — migrations first (by sequence), then schema.
pub struct MultiSource {
    pub sources: Vec<Box<dyn SchemaSource>>,
}

impl MultiSource {
    pub fn new(sources: Vec<Box<dyn SchemaSource>>) -> Self {
        Self { sources }
    }

    pub fn empty() -> Self {
        Self { sources: vec![] }
    }
}

impl SchemaSource for MultiSource {
    fn chunks(&self) -> Vec<SqlChunk> {
        let mut all: Vec<SqlChunk> = self.sources.iter().flat_map(|s| s.chunks()).collect();

        // Stable sort: migrations first (by sequence), then schema, then query.
        all.sort_by_key(|c| match &c.role {
            FileRole::Migration { sequence } => (0u8, *sequence),
            FileRole::Schema => (1, 0),
            FileRole::Query => (2, 0),
        });

        all
    }
}

// ── InlineSource ──────────────────────────────────────────────────────────────

/// A SQL string with an explicit role — for tests and LSP text buffers.
pub struct InlineSource {
    pub sql: String,
    pub role: FileRole,
    pub label: String,
}

impl InlineSource {
    pub fn new(label: impl Into<String>, sql: impl Into<String>, role: FileRole) -> Self {
        Self {
            label: label.into(),
            sql: sql.into(),
            role,
        }
    }
}

impl SchemaSource for InlineSource {
    fn chunks(&self) -> Vec<SqlChunk> {
        let len = self.sql.len();
        vec![SqlChunk {
            origin: SourceOrigin {
                path: PathBuf::from(&self.label),
                byte_range: 0..len,
            },
            sql: self.sql.clone(),
            role: self.role.clone(),
        }]
    }
}

// ── FragmentSource ────────────────────────────────────────────────────────────

/// Un fragment SQL extrait d'un document qui n'est pas un fichier `.sql`.
///
/// C'est le point de branchement pour tout ce qui héberge du SQL sans être du
/// SQL : littéral de template dans un `.ts`, bloc dans un Markdown, résultat
/// d'une expansion de macro. À la différence de [`FileSource`] et
/// [`InlineSource`] — qui posent `byte_range: 0..len` parce que le fichier
/// *est* le SQL — celui-ci porte l'offset réel du fragment dans son document.
///
/// Cet offset traverse [`crate::SchemaResolver`] jusqu'à
/// `pgsql_parser::parse_at_with_outcome`, donc les spans des diagnostics sont
/// exprimés dans le repère du document hôte : l'éditeur souligne le bon endroit
/// dans le `.ts`, pas une position relative au fragment.
///
/// ```ignore
/// // const q = sql`SELECT id FROM users`;
/// //               ^-- offset 14 dans le .ts
/// FragmentSource::new("app/query.ts", "SELECT id FROM users", 14, FileRole::Query)
/// ```
pub struct FragmentSource {
    pub path: PathBuf,
    pub sql: String,
    /// Offset octet du début de `sql` dans le document désigné par `path`.
    pub offset: usize,
    pub role: FileRole,
}

impl FragmentSource {
    pub fn new(
        path: impl Into<PathBuf>,
        sql: impl Into<String>,
        offset: usize,
        role: FileRole,
    ) -> Self {
        Self {
            path: path.into(),
            sql: sql.into(),
            offset,
            role,
        }
    }
}

impl SchemaSource for FragmentSource {
    fn chunks(&self) -> Vec<SqlChunk> {
        vec![SqlChunk {
            origin: SourceOrigin {
                path: self.path.clone(),
                byte_range: self.offset..self.offset + self.sql.len(),
            },
            sql: self.sql.clone(),
            role: self.role.clone(),
        }]
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn sorted_sql_files(dir: &Path) -> Vec<PathBuf> {
    let mut entries: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.extension()
                    .and_then(|s| s.to_str())
                    .map(|ext| ext.eq_ignore_ascii_case("sql"))
                    .unwrap_or(false)
            })
            .collect(),
        Err(_) => return vec![],
    };
    entries.sort();
    entries
}
