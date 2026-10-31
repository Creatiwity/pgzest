pub mod builtin;
pub mod plugin;
pub mod postgis;
pub mod prisma;
pub mod registry;

pub use plugin::Plugin;
pub use postgis::PostGisPlugin;
pub use prisma::PrismaSchemaProvider;
pub use prisma::plugin::PrismaPlugin;
pub use registry::PluginRegistry;
