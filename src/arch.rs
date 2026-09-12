//! Target-architecture model and the shared `--fram-*` flag parsing.
//!
//! Every tool understands `--fram-x64`, `--fram-x32`, `--fram-arm64` and
//! `--fram-all`. The first three are combinable (not mutually exclusive);
//! `--fram-all` is a shorthand for all three. When no fram flag is given the
//! host architecture is used by default. Architecture is recorded as a bitmask
//! so a package may target several architectures at once.

use std::collections::BTreeSet;

/// A target CPU architecture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Arch {
    X64,
    X32,
    Arm64,
}

impl Arch {
    pub const ALL: [Arch; 3] = [Arch::X64, Arch::X32, Arch::Arm64];

    /// The single-bit flag for this architecture.
    pub fn bit(self) -> u32 {
        match self {
            Arch::X64 => 1,
            Arch::X32 => 2,
            Arch::Arm64 => 4,
        }
    }

    /// Recover an architecture from its single-bit flag.
    pub fn from_bit(bit: u32) -> Option<Arch> {
        match bit {
            1 => Some(Arch::X64),
            2 => Some(Arch::X32),
            4 => Some(Arch::Arm64),
            _ => None,
        }
    }

    /// Short CLI label.
    pub fn label(self) -> &'static str {
        match self {
            Arch::X64 => "x64",
            Arch::X32 => "x32",
            Arch::Arm64 => "arm64",
        }
    }

    /// The architecture of the host this binary is running on.
    pub fn host() -> Arch {
        #[cfg(target_arch = "x86_64")]
        {
            Arch::X64
        }
        #[cfg(target_arch = "x86")]
        {
            Arch::X32
        }
        #[cfg(target_arch = "aarch64")]
        {
            Arch::Arm64
        }
        #[cfg(not(any(target_arch = "x86_64", target_arch = "x86", target_arch = "aarch64")))]
        {
            // Unknown hosts default to x64.
            Arch::X64
        }
    }

    /// The Rust target triple of the host `pss` itself is running on.
    pub fn host_triple() -> &'static str {
        #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
        {
            "x86_64-pc-windows-gnu"
        }
        #[cfg(all(target_os = "windows", target_arch = "x86"))]
        {
            "i686-pc-windows-gnu"
        }
        #[cfg(all(target_os = "windows", target_arch = "aarch64"))]
        {
            "aarch64-pc-windows-gnullvm"
        }
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        {
            "x86_64-unknown-linux-gnu"
        }
        #[cfg(all(target_os = "linux", target_arch = "x86"))]
        {
            "i686-unknown-linux-gnu"
        }
        #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
        {
            "aarch64-unknown-linux-gnu"
        }
        #[cfg(not(any(
            all(target_os = "windows", target_arch = "x86_64"),
            all(target_os = "windows", target_arch = "x86"),
            all(target_os = "windows", target_arch = "aarch64"),
            all(target_os = "linux", target_arch = "x86_64"),
            all(target_os = "linux", target_arch = "x86"),
            all(target_os = "linux", target_arch = "aarch64"),
        )))]
        {
            "unknown"
        }
    }
}

/// A set of target architectures, stored as a bitmask.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FramSet(pub u32);

impl FramSet {
    /// The bitmask with every known architecture set.
    pub const ALL: u32 = 1 | 2 | 4;

    pub fn contains(&self, a: Arch) -> bool {
        self.0 & a.bit() != 0
    }

    /// `true` if the host architecture is in this set.
    pub fn ok_on_host(&self) -> bool {
        self.contains(Arch::host())
    }

    /// Human-readable list of the contained architectures.
    pub fn labels(&self) -> String {
        let mut set = BTreeSet::new();
        for a in Arch::ALL {
            if self.contains(a) {
                set.insert(a.label());
            }
        }
        if set.is_empty() {
            "(none)".to_string()
        } else {
            set.into_iter().collect::<Vec<_>>().join(",")
        }
    }
}

/// Parse the `--fram-*` flags out of an argument list, returning the resulting
/// [`FramSet`] and the remaining (non-fram) arguments.
///
/// If no fram flag is present the set defaults to the host architecture.
pub fn parse_fram(args: &[String]) -> (FramSet, Vec<String>) {
    let mut mask = 0u32;
    let mut rest: Vec<String> = Vec::new();
    for a in args {
        match a.as_str() {
            "--fram-all" => mask |= FramSet::ALL,
            "--fram-x64" => mask |= Arch::X64.bit(),
            "--fram-x32" => mask |= Arch::X32.bit(),
            "--fram-arm64" => mask |= Arch::Arm64.bit(),
            other => rest.push(other.to_string()),
        }
    }
    if mask == 0 {
        mask = Arch::host().bit();
    }
    (FramSet(mask), rest)
}
