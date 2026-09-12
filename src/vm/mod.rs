//! Pointerses virtual machine (bytecode interpreter).
//!
//! The interpreter itself lives in a single self-contained file
//! (`runtime/runtime.rs`) so the exact same code is embedded into the native
//! executables produced by `pss build`. This module re-exports it for use by
//! `pss run` and exposes a small convenience wrapper.

#[path = "../../runtime/runtime.rs"]
pub mod runtime;

/// Execute compiled bytecode, returning the program exit code as a `u8`.
pub fn execute_bytes(bytes: &[u8], args: &[String]) -> Result<u8, String> {
    let code = runtime::execute_bytes(bytes, args)?;
    Ok(code as u8)
}
