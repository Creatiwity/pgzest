use crate::plugin::Plugin;
use pgsql_checker::schema::SchemaProvider;

pub struct PrismaPlugin;

impl Plugin for PrismaPlugin {
    fn name(&self) -> &'static str {
        "prisma"
    }

    fn schema_provider(&self, options: &serde_json::Value) -> Option<Box<dyn SchemaProvider>> {
        let schema_path = options.get("schema")?.as_str()?;
        match super::PrismaSchemaProvider::from_file(std::path::Path::new(schema_path)) {
            Ok(provider) => Some(Box::new(provider)),
            Err(e) => {
                eprintln!("pgsql[prisma]: failed to load {}: {}", schema_path, e);
                None
            }
        }
    }
}
