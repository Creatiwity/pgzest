//! The plugin ABI end to end on a real WASM module (written in WAT): name
//! export, host imports, input written through `pgzest_alloc`, JSON
//! diagnostics read back from linear memory.

use pgzest_diagnostics::Severity;
use pgzest_plugin::WasmPlugin;
use pgzest_schema::provider::CompositeSchemaProvider;

/// Answers `pgzest_check_table` with one diagnostic whose message says
/// whether the input it received starts with `{`, after calling both host
/// imports.
const PLUGIN: &str = r#"
(module
  (import "env" "pgzest_log" (func $log (param i32 i32)))
  (import "env" "pgzest_get_table" (func $get_table (param i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 16) "abi-test\00")
  (data (i32.const 32) "called from the plugin")
  (data (i32.const 64) "[{\"code\":\"abi-test/json-object\",\"message\":\"input is a JSON object\",\"severity\":\"error\",\"hint\":\"from WASM\"}]\00")
  (data (i32.const 256) "[{\"code\":\"abi-test/not-json\",\"message\":\"input is not a JSON object\",\"severity\":\"info\",\"hint\":null}]\00")
  (func (export "pgzest_plugin_name") (result i32) (i32.const 16))
  (func (export "pgzest_plugin_version") (result i32) (i32.const 100))
  (func (export "pgzest_alloc") (param i32) (result i32) (i32.const 1024))
  (func (export "pgzest_free") (param i32))
  (func (export "pgzest_check_table") (param $ptr i32) (param $len i32) (result i32)
    (call $log (i32.const 32) (i32.const 22))
    (drop (call $get_table (i32.const 16) (i32.const 8)))
    (if (result i32) (i32.eq (i32.load8_u (local.get $ptr)) (i32.const 123))
      (then (i32.const 64))
      (else (i32.const 256))))
)
"#;

/// The module written to its own file: tests run in parallel.
fn plugin(test: &str) -> WasmPlugin {
    let dir = std::env::temp_dir().join(format!("pgzest-wasm-abi-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{test}.wasm"));
    std::fs::write(&path, wat::parse_str(PLUGIN).unwrap()).unwrap();
    WasmPlugin::from_file(&path).unwrap()
}

#[test]
fn the_name_comes_from_the_module() {
    assert_eq!(plugin("name").name(), "abi-test");
}

#[test]
fn input_goes_in_and_diagnostics_come_back() {
    let plugin = plugin("diagnostics");
    let schema = CompositeSchemaProvider::new();

    let diags = plugin.check_table_json(r#"{"name":"t"}"#, &schema);
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0].code, "abi-test/json-object");
    assert_eq!(diags[0].message, "input is a JSON object");
    assert_eq!(diags[0].severity, Severity::Error);
    assert_eq!(diags[0].hint.as_deref(), Some("from WASM"));

    let diags = plugin.check_table_json("not json", &schema);
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0].code, "abi-test/not-json");
    assert_eq!(diags[0].severity, Severity::Info);
    assert_eq!(diags[0].hint, None);
}

#[test]
fn a_missing_export_yields_no_diagnostic() {
    // No `pgzest_check_select` in the module: reported on stderr, not a
    // crash and not a made-up diagnostic.
    let diags = plugin("missing").check_select_json("{}", &CompositeSchemaProvider::new());
    assert!(diags.is_empty(), "{diags:?}");
}
