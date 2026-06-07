use std::path::Path;

/// Classification of a SQL file that drives which DDL operations are permitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileRole {
    /// Numbered migration file — all DDL operations allowed.
    Migration { sequence: u64 },
    /// Static DDL reference file — creation allowed, destructive DDL is warned.
    Schema,
    /// Application query file — DDL forbidden.
    Query,
}

impl FileRole {
    /// Detect role from a file path using naming conventions:
    /// - `^[0-9]+[_-].*\.sql$`          → Migration
    /// - `schema.sql` / `*.schema.sql` / parent dir is `schema/` → Schema
    /// - anything else                   → Query
    pub fn detect(path: &Path) -> Self {
        let filename = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");

        // Migration: leading digits followed by _ or -
        if let Some(seq) = parse_migration_sequence(filename) {
            return FileRole::Migration { sequence: seq };
        }

        // Schema: named `schema.sql`, ends with `.schema.sql`,
        // or lives inside a directory called `schema`
        let lower = filename.to_lowercase();
        if lower == "schema.sql" || lower.ends_with(".schema.sql") {
            return FileRole::Schema;
        }
        if path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .map(|n| n.eq_ignore_ascii_case("schema"))
            .unwrap_or(false)
        {
            return FileRole::Schema;
        }

        FileRole::Query
    }

    /// Parse a role from a config string ("migration" | "schema" | "query").
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "migration" => Some(FileRole::Migration { sequence: 0 }),
            "schema"    => Some(FileRole::Schema),
            "query"     => Some(FileRole::Query),
            _           => None,
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

/// Extract the leading decimal sequence from a migration filename.
/// `000010_init.sql` → `Some(10)`, `schema.sql` → `None`.
fn parse_migration_sequence(filename: &str) -> Option<u64> {
    let base = filename.strip_suffix(".sql").unwrap_or(filename);
    // Digits at the start of the base name
    let digits: String = base.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    // Must be followed by a separator character or be the whole name
    let after = &base[digits.len()..];
    if !after.is_empty() && !after.starts_with('_') && !after.starts_with('-') {
        return None;
    }
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn migration_detection() {
        assert!(matches!(
            FileRole::detect(Path::new("migrations/000010_add_users.sql")),
            FileRole::Migration { sequence: 10 }
        ));
        assert!(matches!(
            FileRole::detect(Path::new("20260101-baseline.sql")),
            FileRole::Migration { sequence: 20260101 }
        ));
    }

    #[test]
    fn schema_detection() {
        assert_eq!(FileRole::detect(Path::new("schema.sql")), FileRole::Schema);
        assert_eq!(FileRole::detect(Path::new("types.schema.sql")), FileRole::Schema);
        assert_eq!(FileRole::detect(Path::new("schema/types.sql")), FileRole::Schema);
    }

    #[test]
    fn query_detection() {
        assert_eq!(FileRole::detect(Path::new("queries/get_user.sql")), FileRole::Query);
    }
}
