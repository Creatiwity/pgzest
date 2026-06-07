pub mod config;
pub mod index;
pub mod lint;
pub mod role;
pub mod source;
pub mod resolver;

pub use config::ProjectConfig;
pub use index::SchemaIndex;
pub use lint::check_ddl_role;
pub use role::FileRole;
pub use resolver::SchemaResolver;
pub use source::{
    dir_source, DirSource, FileSource, InlineSource, MultiSource, SchemaSource, SqlChunk,
    SourceOrigin,
};
