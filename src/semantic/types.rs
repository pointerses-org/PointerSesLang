//! Value types and their machine layout.
//!
//! `VType` carries a byte length used by the pointer-offset algebra: a pointer
//! into a field at algebraic path `P` has offset = sum of the byte lengths of
//! all preceding sibling fields. Illegal offsets (fields that do not exist, or
//! an offset that would run past the end of a type) are rejected at compile time.

use crate::parser::ast::Type;

/// Primitive value sizes in bytes (matching the runtime VM ABI).
pub const SZ_INT: u64 = 8;
pub const SZ_FLOAT: u64 = 8;
pub const SZ_BOOL: u64 = 1;
pub const SZ_STR: u64 = 8; // heap handle

/// The resolved layout of a struct: field name, its type, and its byte offset.
#[derive(Debug, Clone)]
pub struct FieldLayout {
    pub name: String,
    pub ty: Type,
    pub offset: u64,
}

/// Resolved struct type with a computed layout.
#[derive(Debug, Clone)]
pub struct StructLayout {
    pub name: String,
    pub fields: Vec<FieldLayout>,
    pub size: u64,
    pub align: u64,
}

/// Compute the byte length of a resolved type.
pub fn type_size(t: &Type, structs: &std::collections::HashMap<String, StructLayout>) -> Option<u64> {
    match t {
        Type::Int => Some(SZ_INT),
        Type::Float => Some(SZ_FLOAT),
        Type::Bool => Some(SZ_BOOL),
        Type::Str => Some(SZ_STR),
        Type::Void => Some(0),
        Type::Named(n) => structs.get(n).map(|s| s.size),
        // Generic instantiations and type variables are erased at runtime; the
        // VM stores all fields as dynamic values, so they occupy a machine word.
        Type::Generic(..) | Type::Var(..) | Type::Trait(..) => Some(SZ_INT),
        Type::Ptr { .. } => Some(SZ_INT), // a pointer is a machine word
        Type::Fn { .. } => Some(SZ_INT),  // a closure handle is a machine word
        Type::Array(_) | Type::List(_) | Type::Map(..) => Some(SZ_INT), // container handle
    }
}

/// Compute the alignment of a type.
pub fn type_align(t: &Type, structs: &std::collections::HashMap<String, StructLayout>) -> u64 {
    match t {
        Type::Int | Type::Float | Type::Ptr { .. } | Type::Fn { .. } | Type::Str
        | Type::Array(_) | Type::List(_) | Type::Map(..) | Type::Generic(..) | Type::Var(..)
        | Type::Trait(..) => 8,
        Type::Bool => 1,
        Type::Void => 1,
        Type::Named(n) => structs.get(n).map(|s| s.align).unwrap_or(8),
    }
}

/// Build the layout for a struct from its field declarations, computing each
/// field's byte offset via the algebraic length of its type.
pub fn build_layout(
    name: &str,
    fields: &[(String, Type)],
    structs: &std::collections::HashMap<String, StructLayout>,
) -> Result<StructLayout, String> {
    let mut layout = Vec::new();
    let mut offset = 0u64;
    let mut max_align = 1u64;
    for (fname, fty) in fields {
        let sz = type_size(fty, structs)
            .ok_or_else(|| format!("cannot size type `{}` of field `{fname}` in `{name}`", fty.display()))?;
        let al = type_align(fty, structs);
        // align the offset to the field's alignment
        if al > 1 {
            offset = align_up(offset, al);
        }
        layout.push(FieldLayout { name: fname.clone(), ty: fty.clone(), offset });
        offset += sz;
        if al > max_align {
            max_align = al;
        }
    }
    let size = align_up(offset, max_align);
    Ok(StructLayout { name: name.to_string(), fields: layout, size, align: max_align })
}

fn align_up(v: u64, a: u64) -> u64 {
    if a <= 1 {
        v
    } else {
        (v + a - 1) / a * a
    }
}

/// An algebraic path through a value: a sequence of field names.
#[derive(Debug, Clone, Default)]
pub struct Path {
    pub steps: Vec<String>,
}

impl Path {
    pub fn push(&mut self, field: &str) {
        self.steps.push(field.to_string());
    }
    pub fn display(&self) -> String {
        self.steps.join(".")
    }
}

/// Given a struct layout and a field name, return the byte offset of that field
/// (the algebraic offset used to prove pointer legality).
pub fn field_offset(layout: &StructLayout, field: &str) -> Option<u64> {
    layout.fields.iter().find(|f| f.name == field).map(|f| f.offset)
}
