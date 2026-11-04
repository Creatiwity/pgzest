use pgzest_ast::types::DataType;
use pgzest_schema::provider::{ColumnInfo, FunctionSignature, SchemaProvider, TableSchema};
use std::collections::HashMap;
use std::path::Path;

/// Schema provider that reads from a Prisma schema.prisma file.
/// Parses model definitions and exposes them as TableSchema entries.
pub struct PrismaSchemaProvider {
    tables: HashMap<String, TableSchema>,
}

impl PrismaSchemaProvider {
    /// Load from a schema.prisma file.
    pub fn from_file(path: &Path) -> Result<Self, PrismaError> {
        let content = std::fs::read_to_string(path)?;
        Ok(Self::parse(&content))
    }

    /// Parse a Prisma schema string. Basic regex-free hand parser.
    ///
    /// Not `FromStr`: parsing is infallible (unknown lines are skipped).
    pub fn parse(content: &str) -> Self {
        let mut tables = HashMap::new();
        let mut current_model: Option<(String, Vec<ColumnInfo>)> = None;

        for line in content.lines() {
            let trimmed = line.trim();

            // model User {
            if trimmed.starts_with("model ") && trimmed.ends_with('{') {
                let name = trimmed[6..trimmed.len() - 1].trim().to_string();
                current_model = Some((name, Vec::new()));
                continue;
            }

            // Closing brace
            if trimmed == "}" {
                if let Some((name, columns)) = current_model.take() {
                    // Prisma uses PascalCase model names; map to snake_case table name
                    let table_name = pascal_to_snake(&name);
                    tables.insert(
                        table_name.clone(),
                        TableSchema {
                            name: table_name,
                            columns,
                        },
                    );
                }
                continue;
            }

            // Field line inside a model: "  id    String  @id"
            if let Some((_, ref mut cols)) = current_model
                && let Some(col) = parse_prisma_field(trimmed)
            {
                cols.push(col);
            }
        }

        Self { tables }
    }
}

/// Convert PascalCase to snake_case.
fn pascal_to_snake(s: &str) -> String {
    let mut result = String::new();
    for (i, c) in s.chars().enumerate() {
        if c.is_uppercase() && i > 0 {
            result.push('_');
        }
        result.push(c.to_lowercase().next().unwrap());
    }
    result
}

/// Parse a single Prisma field line. Returns None for directives, annotations, etc.
fn parse_prisma_field(line: &str) -> Option<ColumnInfo> {
    // Skip directives, empty lines, comments
    if line.is_empty() || line.starts_with("@@") || line.starts_with("//") || line.starts_with('@')
    {
        return None;
    }
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 2 {
        return None;
    }
    let field_name = parts[0].to_string();
    let type_raw = parts[1];
    let nullable = type_raw.ends_with('?');
    let prisma_type = type_raw.trim_end_matches('?').trim_end_matches("[]");
    let data_type = prisma_type_to_pg(prisma_type);
    Some(ColumnInfo {
        name: field_name,
        data_type,
        nullable,
    })
}

/// Map Prisma scalar types to PG DataType (best effort).
fn prisma_type_to_pg(t: &str) -> DataType {
    use DataType::*;
    match t {
        "String" => Text,
        "Int" => Integer,
        "BigInt" => BigInt,
        "Float" => DoublePrecision,
        "Decimal" => Numeric {
            precision: None,
            scale: None,
        },
        "Boolean" => Boolean,
        // Prisma generates `TIMESTAMP(3)`, without time zone.
        "DateTime" => Timestamp(Some(3)),
        "Json" => Jsonb,
        "Bytes" => Bytea,
        _ => Text, // fallback for relations, custom types
    }
}

impl SchemaProvider for PrismaSchemaProvider {
    fn get_table(&self, name: &str) -> Option<&TableSchema> {
        self.tables.get(&name.to_lowercase())
    }
    fn get_function(&self, _name: &str) -> Option<&FunctionSignature> {
        None
    }
    fn all_table_names(&self) -> Vec<&str> {
        self.tables.keys().map(String::as_str).collect()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PrismaError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub mod plugin;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic_prisma_schema() {
        let schema = r#"
            model User {
              id        String   @id
              email     String   @unique
              name      String?
              createdAt DateTime
            }
        "#;
        let provider = PrismaSchemaProvider::parse(schema);
        let table = provider.get_table("user").expect("user table");
        assert!(table.get_column("email").is_some());
        assert!(
            table.get_column("created_at").is_some()
                || table.get_column("createdat").is_some()
                || table.get_column("createdAt").is_some()
        );
    }

    #[test]
    fn pascal_to_snake_conversion() {
        assert_eq!(pascal_to_snake("User"), "user");
        assert_eq!(pascal_to_snake("BlogPost"), "blog_post");
        assert_eq!(pascal_to_snake("UserProfile"), "user_profile");
    }

    #[test]
    fn prisma_type_mapping() {
        use DataType::*;
        assert_eq!(prisma_type_to_pg("String"), Text);
        assert_eq!(prisma_type_to_pg("Int"), Integer);
        assert_eq!(prisma_type_to_pg("Boolean"), Boolean);
        assert_eq!(prisma_type_to_pg("DateTime"), Timestamp(Some(3)));
        assert_eq!(prisma_type_to_pg("Json"), Jsonb);
        assert_eq!(prisma_type_to_pg("Bytes"), Bytea);
    }
}
