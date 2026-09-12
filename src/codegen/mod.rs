//! Code generation backends.
//!
//! Two backends are provided:
//!   * `native` — the default bytecode VM backend (see [`bytecode`]).
//!   * `llvm`   — a pure-Rust LLVM IR text emitter (see [`llvm`], feature-gated).

pub mod codegen_error;
pub mod bytecode;
pub mod native;
pub mod llvm_embedded;

#[cfg(feature = "llvm")]
pub mod llvm;

use crate::parser::Program;
use crate::semantic::TypedProgram;

/// Generate code for a program under a named backend.
///
/// Returns `(bytes, ir_text, listing)` where `bytes` is the serialized bytecode
/// (native backend) or empty (llvm), `ir_text` is the LLVM IR (llvm backend) or
/// empty, and `listing` is a human-readable form for diagnostics.
pub fn generate(
    backend: &str,
    prog: &Program,
    typed: &TypedProgram,
    no_api_check: bool,
) -> Result<(Vec<u8>, String, String), String> {
    match backend {
        "native" => {
            let bytes = bytecode::Compiler::compile(prog, typed, no_api_check).map_err(|e| e.msg)?;
            let listing = bytecode::disassemble(&bytes);
            Ok((bytes, String::new(), listing))
        }
        "llvm" => {
            // The bytecode is always produced (the `llvm` backend embeds it in an
            // LLVM driver and links the complete VM runtime, so it runs any
            // program with all features — no cargo feature is required to build).
            let bytes = bytecode::Compiler::compile(prog, typed, no_api_check).map_err(|e| e.msg)?;
            let listing = bytecode::disassemble(&bytes);
            #[cfg(feature = "llvm")]
            let ir = llvm::emit_llvm(prog, typed, no_api_check).map_err(|e| e.msg)?;
            #[cfg(not(feature = "llvm"))]
            let ir = String::new();
            Ok((bytes, ir, listing))
        }
        other => Err(format!("unknown backend `{other}`")),
    }
}
