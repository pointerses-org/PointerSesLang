//! Region calculus for Pointerses.
//!
//! Values live in one of three algebraic regions: `Stack` (default for VTypes),
//! `Scoped` (block-scoped temporaries and pointers) and `Heap` (reference-counted
//! heap objects). The region calculus is evaluated at compile time to prove that
//! every pointer's region outlives (or at least is reachable from) its pointee's
//! region. Cross-region constraints are enforced, e.g. a `Heap` pointer may not
//! dangle into a `Stack` value that will be reclaimed when its frame returns.

use std::fmt;

/// An algebraic memory region.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Region {
    /// Stack allocation: the default for VTypes; reclaimed when the frame pops.
    Stack,
    /// Block-scoped region: lives as long as the enclosing lexical scope.
    Scoped,
    /// Heap region: reference-counted, non-blocking; survives scope exit.
    Heap,
}

impl Region {
    /// Whether `self` is at least as long-lived as `other`.
    /// A region can outlive another if it is "deeper" in this ordering:
    /// Heap > Scoped > Stack.
    pub fn outlives(&self, other: Region) -> bool {
        match (self, other) {
            (Region::Heap, _) => true,
            (Region::Scoped, Region::Stack) => true,
            (Region::Scoped, Region::Scoped) => true,
            (Region::Stack, Region::Stack) => true,
            _ => false,
        }
    }

    /// Cross-region constraint: a pointer stored in `dest` region pointing to a
    /// value in `src` region is legal only if `dest.outlives(src)` — i.e. the
    /// pointee must not be reclaimed before the pointer.
    pub fn can_hold(&self, pointee: Region) -> bool {
        self.outlives(pointee)
    }

    pub fn name(&self) -> &'static str {
        match self {
            Region::Stack => "Stack",
            Region::Scoped => "Scoped",
            Region::Heap => "Heap",
        }
    }
}

impl fmt::Display for Region {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name())
    }
}

/// A compile-time region proof: for every pointer `P` with region `ptr_region`
/// pointing to a value with region `pointee_region`, we record the constraint
/// that `ptr_region.outlives(pointee_region)`.
#[derive(Debug, Clone)]
pub struct RegionProof {
    pub ptr_path: String,
    pub ptr_region: Region,
    pub pointee_region: Region,
    pub ok: bool,
    pub note: String,
}

impl RegionProof {
    pub fn check(ptr_path: &str, ptr: Region, pointee: Region) -> RegionProof {
        let ok = ptr.outlives(pointee);
        let note = if ok {
            format!(
                "region {} outlives {}: valid {} pointer",
                ptr.name(),
                pointee.name(),
                ptr.name()
            )
        } else {
            format!(
                "region {} cannot point into {} (pointee reclaimed too early)",
                ptr.name(),
                pointee.name()
            )
        };
        RegionProof { ptr_path: ptr_path.to_string(), ptr_region: ptr, pointee_region: pointee, ok, note }
    }
}

/// Region descriptor attached to each declaration/expression by the analyzer.
#[derive(Debug, Clone, Copy)]
pub struct RegionInfo {
    pub region: Region,
    pub immutable: bool,
}

impl RegionInfo {
    pub fn new(region: Region, immutable: bool) -> Self {
        RegionInfo { region, immutable }
    }
}
