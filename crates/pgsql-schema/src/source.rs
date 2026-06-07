use std::path::{Path, PathBuf};

use crate::role::FileRole;

// ── Origin ────────────────────────────────────────────────────────────────────

/// Where a SQL chunk came from.
#[derive(Debug, Clone)]
pub struct SourceOrigin {
    pub path: PathBuf,
    pub byte_range: std::ops::Range<usize>,
}

// ── SqlChunk ──────────────────────────────────────────────────────────────────

/// One SQL fragment to index/resolve, with its provenance and role.
#[derive(Debug, Clone)]
pub struct SqlChunk {
    pub origin: SourceOrigin,
    pub sql: String,
    pub role: FileRole,
}

// ── SchemaSource trait ────────────────────────────────────────────────────────

/// Anything that can produce SQL chunks to be indexed.
pub trait SchemaSource: Send + Sync {
    fn chunks(&self) -> Vec<SqlChunk>;
}

// ── FileSource ────────────────────────────────────────────────────────────────

/// A single SQL file.
pub struct FileSource {
    pub path: PathBuf,
    /// Override role; if None, detected from filename.
    pub role_override: Option<FileRole>,
}

impl FileSource {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into(), role_override: None }
    }

    pub fn with_role(path: impl Into<PathBuf>, role: FileRole) -> Self {
        Self { path: path.into(), role_override: Some(role) }
    }
}

impl SchemaSource for FileSource {
    fn chunks(&self) -> Vec<SqlChunk> {
        let sql = match std::fs::read_to_string(&self.path) {
            Ok(s) => s,
            Err(_) => return vec![],
        };
        let role = self.role_override.clone()
            .unwrap_or_else(|| FileRole::detect(&self.path));
        let len = sql.len();
        vec![SqlChunk {
            origin: SourceOrigin { path: self.path.clone(), byte_range: 0..len },
            sql,
            role,
        }]
    }
}

// ── DirSource ─────────────────────────────────────────────────────────────────

/// All `.sql` files in a directory, sorted (lexicographic = migration order).
pub struct DirSource {
    pub root: PathBuf,
    /// Override role for every file in this directory; if None, per-file detection.
    pub role_override: Option<FileRole>,
}

impl DirSource {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into(), role_override: None }
    }

    pub fn with_role(root: impl Into<PathBuf>, role: FileRole) -> Self {
        Self { root: root.into(), role_override: Some(role) }
    }
}

impl SchemaSource for DirSource {
    fn chunks(&self) -> Vec<SqlChunk> {
        let mut entries: Vec<PathBuf> = match std::fs::read_dir(&self.root) {
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
            .into_iter()
            .flat_map(|path| {
                let src = FileSource {
                    path,
                    role_override: self.role_override.clone(),
                };
                src.chunks()
            })
            .collect()
    }
}

// ── MultiSource ───────────────────────────────────────────────────────────────

/// Ordered composition of multiple sources.
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
        // Sort migrations by sequence within each source, then concatenate.
        // Within a source, files are already sorted by `DirSource`.
        let mut all: Vec<SqlChunk> = self.sources.iter()
            .flat_map(|s| s.chunks())
            .collect();

        // Stable sort: migrations first (by sequence), then schema, then query.
        all.sort_by_key(|c| match &c.role {
            FileRole::Migration { sequence } => (0u8, *sequence),
            FileRole::Schema => (1, 0),
            FileRole::Query  => (2, 0),
        });

        all
    }
}

// ── Inline source (for tests / LSP text buffer) ───────────────────────────────

/// A SQL string with an explicit role (no file backing).
pub struct InlineSource {
    pub sql: String,
    pub role: FileRole,
    pub label: String,
}

impl InlineSource {
    pub fn new(label: impl Into<String>, sql: impl Into<String>, role: FileRole) -> Self {
        Self { label: label.into(), sql: sql.into(), role }
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

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Build a `MultiSource` from a directory path. Convenience for the common case.
pub fn dir_source(root: impl AsRef<Path>) -> MultiSource {
    MultiSource::new(vec![Box::new(DirSource::new(root.as_ref()))])
}
