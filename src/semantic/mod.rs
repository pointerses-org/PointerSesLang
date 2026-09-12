//! Semantic analysis for Pointerses.
//!
//! Provides strict type inference, lexical scope validation, region-calculus
//! lifetime checking and pointer-legality proving. The entry point is
//! [`analyze`], which turns a parsed [`Program`] into a [`TypedProgram`].

pub mod types;
pub mod region;
pub mod pointer;
pub mod analyzer;

pub use analyzer::{analyze, analyze_nac, SemanticError, TypedProgram};
pub use region::Region;

use crate::parser::Program;
use crate::concurrency::Schedule;

/// Convenience wrapper: analyze a program and return the typed program.
pub fn analyze_program(prog: &mut Program) -> Result<TypedProgram, SemanticError> {
    analyze(prog)
}

/// Convenience wrapper honouring the `no-api-check` flag (skip the build-time
/// "does the called function exist" check).
pub fn analyze_program_nac(prog: &mut Program, no_api_check: bool) -> Result<TypedProgram, SemanticError> {
    analyze_nac(prog, no_api_check)
}

/// Look up the region assigned to a function name (defaults to `Scoped`).
pub fn function_region(typed: &TypedProgram, name: &str) -> Region {
    typed.regions.get(name).copied().unwrap_or(Region::Scoped)
}

/// Look up the scheduling annotation assigned to a function name.
pub fn function_schedule(typed: &TypedProgram, name: &str) -> Schedule {
    typed.schedules.get(name).cloned().unwrap_or(Schedule::Single)
}
