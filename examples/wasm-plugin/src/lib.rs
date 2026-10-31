use std::ffi::CString;
use std::os::raw::c_char;

// ── ABI ───────────────────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn pgweave_plugin_name() -> *mut c_char {
    CString::new("example-wasm-plugin").unwrap().into_raw()
}

#[no_mangle]
pub extern "C" fn pgweave_plugin_version() -> i32 {
    100
}

#[no_mangle]
pub extern "C" fn pgweave_alloc(size: i32) -> *mut u8 {
    let mut v = Vec::with_capacity(size as usize);
    let ptr = v.as_mut_ptr();
    std::mem::forget(v);
    ptr
}

#[no_mangle]
pub unsafe extern "C" fn pgweave_free(ptr: *mut u8) {
    // Minimal: just drop (real impl would track size)
    drop(Box::from_raw(ptr));
}

// ── Check hooks ───────────────────────────────────────────────────────────────

/// Example: warn on SELECT with no FROM clause (bare SELECT).
#[no_mangle]
pub unsafe extern "C" fn pgweave_check_select(ptr: *const u8, len: i32) -> *mut c_char {
    let json = std::str::from_utf8(std::slice::from_raw_parts(ptr, len as usize))
        .unwrap_or("{}");

    // Minimal check: if "from":[] in the JSON, it might be a bare SELECT
    let mut diags: Vec<serde_json::Value> = vec![];
    if json.contains(r#""from":[]"#) {
        diags.push(serde_json::json!({
            "code": "W099",
            "message": "SELECT with no FROM clause",
            "severity": "warning",
            "hint": "add a FROM clause or use a literal expression"
        }));
    }

    let result = serde_json::to_string(&diags).unwrap_or_else(|_| "[]".to_string());
    CString::new(result).unwrap().into_raw()
}
