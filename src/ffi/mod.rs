//! Pointerses <-> Rust interoperability layer (FFI).
//!
//! `.psp` files may declare `extern` functions and call Rust libraries. Data is
//! passed between the two languages through Arrow-format shared-memory
//! descriptors (see [`arrow`]) for zero-copy transfer. The runtime resolves
//! extern calls against a built-in Rust interop library and, optionally, a
//! dynamically loaded Rust `cdylib` configured via `PSS_RUST_LIB`.

pub mod arrow;

use crate::parser::ast::{Program, Type};

/// A resolved extern declaration.
#[derive(Debug, Clone)]
pub struct ExternDecl {
    pub name: String,
    pub params: Vec<Type>,
    pub ret: Type,
    pub abi: String,
}

/// Collect the extern declarations from a parsed program.
pub fn collect_externs(prog: &Program) -> Result<Vec<ExternDecl>, String> {
    let mut out = Vec::new();
    for e in &prog.externs {
        let params = e.params.iter().map(|p| p.ty.clone().unwrap_or(Type::Int)).collect();
        let ret = e.ret.clone().unwrap_or(Type::Void);
        out.push(ExternDecl { name: e.name.clone(), params, ret, abi: e.abi.clone() });
    }
    Ok(out)
}

/// Build an Arrow schema describing an extern's parameters.
pub fn schema_for(externs: &[ExternDecl]) -> Vec<arrow::ArrowSchema> {
    externs
        .iter()
        .map(|e| {
            let fields = e
                .params
                .iter()
                .enumerate()
                .map(|(i, t)| arrow::ArrowField {
                    name: format!("arg{i}"),
                    ty: type_to_arrow(t),
                })
                .collect();
            arrow::ArrowSchema { fields }
        })
        .collect()
}

fn type_to_arrow(t: &Type) -> arrow::ArrowType {
    match t {
        Type::Int => arrow::ArrowType::Int,
        Type::Float => arrow::ArrowType::Float,
        Type::Bool => arrow::ArrowType::Bool,
        Type::Str => arrow::ArrowType::Utf8,
        Type::Named(_) => arrow::ArrowType::Struct,
        _ => arrow::ArrowType::Int,
    }
}

/// Convenience: derive a stable mangled symbol for an extern given a package.
pub fn ffi_symbol(pkg: &str, version: &str, name: &str) -> String {
    crate::project::mangle::mangle(pkg, version, name)
}

