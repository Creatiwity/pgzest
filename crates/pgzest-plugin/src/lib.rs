//! Plugins: rules added to the checks, built in (`[plugins] builtin`) or
//! compiled to WebAssembly (`[plugins] wasm`, see [`abi`]).

pub mod abi;
pub mod builtin;
pub mod plugin;
pub mod registry;
pub mod wasm_plugin;
pub mod wasm_rule;

pub use plugin::Plugin;
pub use registry::PluginRegistry;
pub use wasm_plugin::WasmPlugin;
pub use wasm_rule::WasmCheckRule;
