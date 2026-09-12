//! Pointer legality proof.
//!
//! Every pointer is an *algebraic path* — a sequence of field selections from a
//! root value, carrying the byte offset derived from the target type's layout.
//! The legality proof verifies at compile time that:
//!   * the path targets a real field of a real struct type,
//!   * the computed offset stays within the bounds of the type's byte length,
//!   * a mutable pointer never aliases an existing live pointer,
//!   * the pointer's region may legally hold its pointee's region.

use crate::parser::ast::Type;
use super::region::{Region, RegionProof};
use super::types::{StructLayout, type_size};
use std::collections::HashMap;

/// Result of proving a pointer expression legal.
#[derive(Debug, Clone)]
pub struct PointerProof {
    /// The algebraic path, e.g. `g.prefix`.
    pub path: String,
    /// Byte offset into the target object.
    pub offset: u64,
    /// The type pointed at.
    pub target: Type,
    /// Whether the pointer is mutable (`&mut`).
    pub mutable: bool,
    /// Region the pointer lives in.
    pub ptr_region: Region,
    /// Region of the pointee.
    pub pointee_region: Region,
    /// Whether the offset is within bounds of the containing type.
    pub in_bounds: bool,
    /// The region proof.
    pub region_proof: RegionProof,
}

/// The pointer checker maintains the set of live (addressable) pointers in a
/// scope to reject aliasing of mutable pointers.
#[derive(Debug, Clone, Default)]
pub struct PointerChecker {
    pub live_immutable: Vec<String>,
    pub live_mutable: Vec<String>,
    pub proofs: Vec<PointerProof>,
}

impl PointerChecker {
    /// Prove a pointer into `root_type` along `steps`. Returns the proof.
    pub fn prove(
        &mut self,
        root_type: &Type,
        steps: &[String],
        structs: &HashMap<String, StructLayout>,
        mutable: bool,
        ptr_region: Region,
        pointee_region: Region,
    ) -> Result<PointerProof, String> {
        let path = steps.join(".");
        let mut offset = 0u64;
        let mut cur: &Type = root_type;
        let mut in_bounds = true;

        for (i, step) in steps.iter().enumerate() {
            let cur_size = type_size(cur, structs).unwrap_or(0);
            match cur {
                Type::Named(name) => {
                    let layout = structs
                        .get(name)
                        .ok_or_else(|| format!("pointer path `{path}`: unknown struct `{name}`"))?;
                    let f = layout
                        .fields
                        .iter()
                        .find(|f| &f.name == step)
                        .ok_or_else(|| {
                            format!("pointer path `{path}`: `{name}` has no field `{step}`")
                        })?;
                    offset += f.offset;
                    if offset > layout.size {
                        in_bounds = false;
                    }
                    cur = &f.ty;
                }
                Type::Ptr { target, .. } => {
                    // auto-deref while descending the path
                    cur = target;
                    // field steps continue on the pointee
                    if i == 0 {
                        // fall through to process step on deref'd type
                        let layout = match cur {
                            Type::Named(n) => structs
                                .get(n)
                                .ok_or_else(|| format!("unknown struct `{n}`"))?,
                            _ => {
                                return Err(format!(
                                    "pointer path `{path}`: cannot index into `{}`",
                                    cur.display()
                                ))
                            }
                        };
                        let f = layout.fields.iter().find(|f| &f.name == step).ok_or_else(|| {
                            format!("pointer path `{path}`: no field `{step}`")
                        })?;
                        offset += f.offset;
                        cur = &f.ty;
                    } else {
                        return Err(format!("pointer path `{path}`: nested pointer path not supported"));
                    }
                }
                _ => {
                    return Err(format!(
                        "pointer path `{path}`: cannot select field `{step}` of `{}`",
                        cur.display()
                    ))
                }
            }
            let _ = cur_size;
        }

        // Final bounds check against the containing type's total length.
        let target = cur.clone();
        let target_size = type_size(&target, structs).unwrap_or(0);
        if offset > target_size {
            in_bounds = false;
        }

        let proof = RegionProof::check(&path, ptr_region, pointee_region);

        // Aliasing check: reject a second mutable pointer to the same path.
        if mutable {
            if self.live_mutable.iter().any(|p| p == &path) {
                return Err(format!(
                    "pointer proof failed: `&mut {path}` aliases an already-live mutable pointer"
                ));
            }
            self.live_mutable.push(path.clone());
        } else {
            self.live_immutable.push(path.clone());
        }

        let p = PointerProof {
            path,
            offset,
            target,
            mutable,
            ptr_region,
            pointee_region,
            in_bounds,
            region_proof: proof,
        };
        if !p.in_bounds {
            return Err(format!(
                "pointer proof failed: offset {} exceeds the byte length of `{}`",
                p.offset,
                p.target.display()
            ));
        }
        if !p.region_proof.ok {
            return Err(p.region_proof.note.clone());
        }
        self.proofs.push(p.clone());
        Ok(p)
    }

    /// Release a pointer (e.g. at end of scope) from the live sets.
    pub fn release(&mut self, path: &str) {
        self.live_mutable.retain(|p| p != path);
        self.live_immutable.retain(|p| p != path);
    }
}

/// Compute an algebraic path for a "rooted" expression. Returns the list of
/// field steps and the root identifier, or `None` if the expression is not a
/// statically-provable path (e.g. `&(a + b)`).
pub fn root_path(expr: &crate::parser::ast::Expr) -> Option<(String, Vec<String>)> {
    let mut steps: Vec<String> = Vec::new();
    let mut cur = expr;
    loop {
        match cur {
            crate::parser::ast::Expr::Field(obj, name, _) => {
                steps.insert(0, name.clone());
                cur = obj;
            }
            crate::parser::ast::Expr::Ident(name, _) => {
                return Some((name.clone(), steps));
            }
            _ => return None,
        }
    }
}
