//! Binary package formats: `.project` and `.psdl`.
//!
//! * `.project` — a compiled Pointerses program, runnable by `pss`. It bundles
//!   the serialized bytecode, the original source text, bootstrap metadata and
//!   the set of target architectures it was built for.
//! * `.psdl`    — a "PointerSes Dymatic Library": the whole project packaged as
//!   a library (same container structure, different magic).
//!
//! Both formats carry an architecture bitmask. At run time `pss` refuses to
//! execute a package whose architecture set does not include the host.

use crate::arch::{Arch, FramSet};
use crate::project::Watchdog;

/// Magic header for a `.project` package.
pub const PROJECT_MAGIC: [u8; 8] = *b"PSPROJ\x00\x00";
/// Magic header for a `.psdl` dynamic-library package.
pub const PSDL_MAGIC: [u8; 8] = *b"PSDL\x00\x00\x00\x00";

const VERSION: u32 = 2;

/// A serialized Pointerses package (either a program or a library).
#[derive(Debug, Clone)]
pub struct Package {
    /// Bitmask of target architectures (see [`crate::arch`]).
    pub arch_mask: u32,
    /// The original source file name (relative, for diagnostics / `--out-psp`).
    pub source_name: String,
    /// The original source text, so `pssp --out-psp` can restore it faithfully.
    pub source: String,
    /// Serialized bytecode program.
    pub bytecode: Vec<u8>,
    /// Bootstrap metadata as JSON.
    pub meta_json: Vec<u8>,
    /// Watchdog policy (None = not configured). Present from package version 2.
    pub watchdog: Option<Watchdog>,
}

impl Package {
    pub fn new(
        fram: FramSet,
        source_name: String,
        source: String,
        bytecode: Vec<u8>,
        meta_json: Vec<u8>,
    ) -> Package {
        Package {
            arch_mask: fram.0,
            source_name,
            source,
            bytecode,
            meta_json,
            watchdog: None,
        }
    }

    /// The target-architecture set recorded in this package.
    pub fn fram(&self) -> FramSet {
        FramSet(self.arch_mask)
    }

    /// Whether this package may run on the host.
    pub fn ok_on_host(&self) -> bool {
        self.fram().ok_on_host()
    }

    /// Human-readable description of the target architectures.
    pub fn arch_label(&self) -> String {
        self.fram().labels()
    }

    /// Serialize with the given magic header.
    pub fn serialize(&self, magic: &[u8; 8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(magic);
        push_u32(&mut out, VERSION);
        push_u32(&mut out, self.arch_mask);
        push_str(&mut out, &self.source_name);
        push_str(&mut out, &self.source);
        push_bytes(&mut out, &self.bytecode);
        push_bytes(&mut out, &self.meta_json);
        // Watchdog policy (length-prefixed; 0 = not configured).
        match &self.watchdog {
            Some(wd) => {
                let b = wd.to_bytes();
                push_u32(&mut out, b.len() as u32);
                out.extend_from_slice(&b);
            }
            None => push_u32(&mut out, 0),
        }
        out
    }

    /// Deserialize a package, validating the magic header is `.project` or `.psdl`.
    pub fn deserialize(data: &[u8]) -> Result<Package, String> {
        let mut c = data;
        if c.len() < 8 {
            return Err("not a Pointerses package (too short)".into());
        }
        let magic: [u8; 8] = [c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]];
        c = &c[8..];
        if magic != PROJECT_MAGIC && magic != PSDL_MAGIC {
            return Err("not a Pointerses package (bad magic header)".into());
        }
        let version = read_u32(&mut c)?;
        let arch_mask = read_u32(&mut c)?;
        let source_name = read_str(&mut c)?;
        let source = read_str(&mut c)?;
        let bytecode = read_bytes(&mut c)?;
        let meta_json = read_bytes(&mut c)?;
        // Watchdog segment exists from version 2; older packages carry none.
        let watchdog = if version >= 2 {
            let n = read_u32(&mut c)? as usize;
            if n == 0 {
                None
            } else if n == 12 && c.len() >= 12 {
                Watchdog::from_bytes(&c[..12])
            } else {
                return Err("malformed watchdog segment in package".into());
            }
        } else {
            None
        };
        Ok(Package {
            arch_mask,
            source_name,
            source,
            bytecode,
            meta_json,
            watchdog,
        })
    }
}

// ---------------------------------------------------------------------------
// Little-endian wire helpers
// ---------------------------------------------------------------------------

fn push_u32(v: &mut Vec<u8>, x: u32) {
    v.extend_from_slice(&x.to_le_bytes());
}

fn push_bytes(v: &mut Vec<u8>, b: &[u8]) {
    push_u32(v, b.len() as u32);
    v.extend_from_slice(b);
}

fn push_str(v: &mut Vec<u8>, s: &str) {
    push_bytes(v, s.as_bytes());
}

fn read_u32(c: &mut &[u8]) -> Result<u32, String> {
    if c.len() < 4 {
        return Err("package truncated".into());
    }
    let b = [c[0], c[1], c[2], c[3]];
    *c = &c[4..];
    Ok(u32::from_le_bytes(b))
}

fn read_bytes(c: &mut &[u8]) -> Result<Vec<u8>, String> {
    let n = read_u32(c)? as usize;
    if c.len() < n {
        return Err("package truncated".into());
    }
    let (head, tail) = c.split_at(n);
    *c = tail;
    Ok(head.to_vec())
}

fn read_str(c: &mut &[u8]) -> Result<String, String> {
    let bytes = read_bytes(c)?;
    String::from_utf8(bytes).map_err(|_| "package contains invalid UTF-8".into())
}

/// Validate that a package's architecture set includes `arch`; used by tools
/// that must not run a package built for a different architecture.
pub fn check_arch(pkg: &Package, arch: Arch) -> Result<(), String> {
    if pkg.fram().contains(arch) {
        Ok(())
    } else {
        Err(format!(
            "package targets architecture `{}` but this host is `{}`",
            pkg.arch_label(),
            arch.label()
        ))
    }
}
