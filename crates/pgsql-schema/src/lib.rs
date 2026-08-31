pub mod config;
pub mod index;
pub mod lint;
pub mod provider;
pub mod resolver;
pub mod role;
pub mod source;

pub use config::ProjectConfig;
pub use index::SchemaIndex;
pub use lint::check_ddl_role;
pub use provider::{
    ColumnInfo, CompositeSchemaProvider, DdlSchemaProvider, FunctionSignature,
    IntrospectionJsonProvider, RlsAwareSchemaProvider, SchemaProvider, TableSchema,
    detect_default_schema, detect_search_path, object_name_to_string, qualify_object_name,
    qualify_str,
};
pub use resolver::SchemaResolver;
pub use role::{FileRole, sequence_from_path};
pub use source::{
    DirSource, FileSource, InlineSource, MigrationDirSource, MultiSource, SchemaSource,
    SourceOrigin, SqlChunk,
};
