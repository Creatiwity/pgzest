/// Classification of a SQL file that drives which DDL operations are permitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileRole {
    /// Numbered migration file — all DDL operations allowed.
    Migration { sequence: u64 },
    /// Static DDL reference file — only CREATE statements; structural ALTER is an error.
    Schema,
    /// Application query file — DDL forbidden.
    Query,
    /// Fragments file (`[macros] directory`, P6): pieces of SQL inserted
    /// elsewhere, never executed nor analyzed on their own.
    Fragments,
}

impl FileRole {
    /// Parse a role from a config string (`"migration"` | `"schema"` | `"query"`).
    ///
    /// Not `FromStr`: returns `Option` rather than `Result`, no error type.
    pub fn from_name(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "migration" => Some(FileRole::Migration { sequence: 0 }),
            "schema" => Some(FileRole::Schema),
            "query" => Some(FileRole::Query),
            _ => None,
        }
    }

    pub fn is_migration(&self) -> bool {
        matches!(self, FileRole::Migration { .. })
    }
    pub fn is_schema(&self) -> bool {
        matches!(self, FileRole::Schema)
    }
    pub fn is_query(&self) -> bool {
        matches!(self, FileRole::Query)
    }
}

/// Extract the leading decimal sequence from a filename stem.
/// `000010_init` → `Some(10)`, `schema` → `None`.
pub fn sequence_from_stem(stem: &str) -> Option<u64> {
    let digits: String = stem.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let after = &stem[digits.len()..];
    if !after.is_empty() && !after.starts_with('_') && !after.starts_with('-') {
        return None;
    }
    digits.parse().ok()
}

/// Parse a migration sequence number from a file path: the one in the file
/// name (`000010_init.sql`), otherwise the one of its directory
/// (Prisma's `20240101120000_init/migration.sql`, Diesel's `…/up.sql`).
pub fn sequence_from_path(path: &std::path::Path) -> u64 {
    let stem = |p: &std::path::Path| {
        p.file_stem()
            .and_then(|s| s.to_str())
            .and_then(sequence_from_stem)
    };
    stem(path)
        .or_else(|| {
            path.parent()
                .and_then(|d| d.file_name())
                .and_then(|s| s.to_str())
                .and_then(sequence_from_stem)
        })
        .unwrap_or(0)
}
