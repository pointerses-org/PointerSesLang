//! Concurrency annotations and scheduling.
//!
//! Pointerses supports two concurrency annotations on functions:
//!   * `@Auto`              - the function runs under the M:N coroutine scheduler.
//!   * `@Manual(fixed=N)`   - the function runs on a fixed-size thread pool of N
//!                            OS threads.
//!
//! The annotation parser extracts the arguments into the AST (`Annotation`
//! nodes), the semantic analyzer stores the resolved [`Schedule`] per function,
//! and the code generator emits the corresponding scheduling metadata into the
//! compiled artifact. The runtime `Scheduler` provides the actual execution
//! model.

pub mod schedule;

pub use schedule::Schedule;

use crate::parser::ast::Annotation;

/// Parse a `@Auto` / `@Manual(fixed=N)` annotation into a [`Schedule`].
pub fn schedule_from_annotation(ann: &Annotation) -> Option<Schedule> {
    match ann.name.as_str() {
        "Auto" => Some(Schedule::Auto),
        "Manual" => {
            let n = ann
                .args
                .iter()
                .find(|a| a.name.as_deref() == Some("fixed"))
                .and_then(|a| a.value.as_int())
                .unwrap_or(4);
            Some(Schedule::Manual(n.max(1) as usize))
        }
        _ => None,
    }
}

/// Return the first scheduling annotation from a list, if any.
pub fn find_schedule(anns: &[Annotation]) -> Option<Schedule> {
    anns.iter().find_map(schedule_from_annotation)
}
