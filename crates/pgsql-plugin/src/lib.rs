pub mod builtin;
pub mod config;
pub mod plugin;
pub mod postgis;
pub mod prisma;
pub mod registry;

pub use config::PluginConfig;
pub use plugin::Plugin;
pub use postgis::PostGisPlugin;
pub use prisma::plugin::PrismaPlugin;
pub use prisma::PrismaSchemaProvider;
pub use registry::PluginRegistry;
