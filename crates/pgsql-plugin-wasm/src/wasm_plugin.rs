use std::path::Path;
use wasmi::{AsContext, Engine, Linker, Module, Store};
use crate::abi::WasmDiagnostic;
use pgsql_checker::Diagnostic;
use pgsql_checker::schema::SchemaProvider;

/// A plugin loaded from a WASM file.
/// Implements pgsql_plugin::Plugin.
pub struct WasmPlugin {
    /// Display name (from pgsql_plugin_name export).
    plugin_name: String,
    /// Path to the .wasm file.
    wasm_path: std::path::PathBuf,
    /// WASM bytes (loaded once at init).
    wasm_bytes: Vec<u8>,
}

impl WasmPlugin {
    /// Load a WASM plugin from disk.
    pub fn from_file(path: &Path) -> Result<Self, WasmPluginError> {
        let wasm_bytes = std::fs::read(path)?;
        // We'll get the name by instantiating once
        let plugin_name = Self::extract_name(&wasm_bytes).unwrap_or_else(|| {
            path.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown")
                .to_string()
        });
        Ok(Self {
            plugin_name,
            wasm_path: path.to_path_buf(),
            wasm_bytes,
        })
    }

    fn extract_name(wasm_bytes: &[u8]) -> Option<String> {
        let engine = Engine::default();
        let module = Module::new(&engine, &mut &wasm_bytes[..]).ok()?;
        let mut store: Store<()> = Store::new(&engine, ());
        let linker: Linker<()> = Linker::new(&engine);
        // Try to instantiate with empty linker (may fail if imports needed)
        // wasmi 1.x combines instantiate + start into one call
        let instance = linker
            .instantiate_and_start(&mut store, &module)
            .ok()?;
        let name_fn = instance
            .get_typed_func::<(), i32>(&store, "pgsql_plugin_name")
            .ok()?;
        let ptr = name_fn.call(&mut store, ()).ok()?;
        // Read null-terminated UTF-8 from linear memory
        let mem = instance.get_memory(&store, "memory")?;
        let data = mem.data(&store);
        let bytes: Vec<u8> = data
            .iter()
            .skip(ptr as usize)
            .take_while(|&&b| b != 0)
            .copied()
            .collect();
        String::from_utf8(bytes).ok()
    }

    /// Run pgsql_check_select with a JSON-serialized SelectStmt.
    /// Returns diagnostics from the WASM plugin.
    pub fn check_select_json(
        &self,
        select_json: &str,
        schema: &dyn SchemaProvider,
    ) -> Vec<Diagnostic> {
        self.call_check_fn("pgsql_check_select", select_json, schema)
    }

    pub fn check_table_json(
        &self,
        table_json: &str,
        schema: &dyn SchemaProvider,
    ) -> Vec<Diagnostic> {
        self.call_check_fn("pgsql_check_table", table_json, schema)
    }

    fn call_check_fn(
        &self,
        fn_name: &str,
        input_json: &str,
        schema: &dyn SchemaProvider,
    ) -> Vec<Diagnostic> {
        match self.try_call_check_fn(fn_name, input_json, schema) {
            Ok(diags) => diags,
            Err(e) => {
                eprintln!(
                    "pgsql[wasm:{}]: {} failed: {}",
                    self.plugin_name, fn_name, e
                );
                vec![]
            }
        }
    }

    fn try_call_check_fn(
        &self,
        fn_name: &str,
        input_json: &str,
        _schema: &dyn SchemaProvider,
    ) -> Result<Vec<Diagnostic>, WasmPluginError> {
        let engine = Engine::default();
        let module = Module::new(&engine, &mut &self.wasm_bytes[..])?;

        // Host state
        struct HostState;
        let mut store: Store<HostState> = Store::new(&engine, HostState);
        let mut linker: Linker<HostState> = Linker::new(&engine);

        // Define host imports
        linker
            .func_wrap(
                "env",
                "pgsql_log",
                |caller: wasmi::Caller<'_, HostState>, ptr: i32, len: i32| {
                    if let Some(mem) = caller.get_export("memory").and_then(|e| e.into_memory()) {
                        let data = mem.data(caller.as_context());
                        let start = ptr as usize;
                        let end = (start + len as usize).min(data.len());
                        if let Ok(msg) = std::str::from_utf8(&data[start..end]) {
                            eprintln!("[wasm plugin] {}", msg);
                        }
                    }
                },
            )
            .map_err(wasmi::Error::from)?;

        // Stub for pgsql_get_table — returns 0 (null) for now
        linker
            .func_wrap(
                "env",
                "pgsql_get_table",
                |_: wasmi::Caller<'_, HostState>, _ptr: i32, _len: i32| -> i32 { 0 },
            )
            .map_err(wasmi::Error::from)?;

        let instance = linker.instantiate_and_start(&mut store, &module)?;

        // Get alloc and the check function
        let alloc = instance.get_typed_func::<i32, i32>(&store, "pgsql_alloc")?;
        let check_fn = instance.get_typed_func::<(i32, i32), i32>(&store, fn_name)?;
        let mem = instance
            .get_memory(&store, "memory")
            .ok_or(WasmPluginError::NoMemory)?;

        // Write input JSON to WASM memory
        let input_bytes = input_json.as_bytes();
        let input_len = input_bytes.len() as i32;
        let input_ptr = alloc
            .call(&mut store, input_len)
            .map_err(wasmi::Error::from)?;

        {
            let data = mem.data_mut(&mut store);
            let start = input_ptr as usize;
            data[start..start + input_bytes.len()].copy_from_slice(input_bytes);
        }

        // Call the check function → ptr to JSON response
        let result_ptr = check_fn
            .call(&mut store, (input_ptr, input_len))
            .map_err(wasmi::Error::from)?;

        // Read response JSON (null-terminated)
        let result_json = {
            let data = mem.data(&store);
            let bytes: Vec<u8> = data
                .iter()
                .skip(result_ptr as usize)
                .take_while(|&&b| b != 0)
                .copied()
                .collect();
            String::from_utf8(bytes).map_err(|_| WasmPluginError::Utf8)?
        };

        // Deserialize diagnostics
        let wasm_diags: Vec<WasmDiagnostic> =
            serde_json::from_str(&result_json).unwrap_or_default();

        Ok(wasm_diags.into_iter().map(|d| d.into()).collect())
    }

    /// Return the plugin's display name.
    pub fn name(&self) -> &str {
        &self.plugin_name
    }

    /// Return the path to the WASM file.
    pub fn wasm_path(&self) -> &Path {
        &self.wasm_path
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WasmPluginError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("wasmi: {0}")]
    Wasmi(#[from] wasmi::Error),
    #[error("wasm module has no 'memory' export")]
    NoMemory,
    #[error("invalid utf-8 from wasm")]
    Utf8,
}
