//! Symbol mangling for version isolation.
//!
//! When a project depends on two different versions of the same library, their
//! symbols are renamed so they do not collide. Every public symbol is mangled
//! with the package name and version, so version conflicts are resolved at the
//! symbol level. The mangled name also encodes the algebraic path of the symbol
//! for the pointer system.

/// Produce a mangled symbol name from package identity + a symbol path.
///
/// Format: `<pkg>_<version>_<hash>__<path-with-dots-replaced>`.
pub fn mangle(pkg: &str, version: &str, symbol_path: &str) -> String {
    // A short, stable hash of the version isolates differing versions even when
    // the version strings are similar.
    let vhash = fnv1a(&format!("{pkg}:{version}"));
    let path = symbol_path.replace('.', "_");
    format!("{pkg}_{version}_{vhash}__{path}")
}

/// A stable 32-bit FNV-1a hash (used for the version fingerprint).
pub fn fnv1a(s: &str) -> u32 {
    let mut h: u32 = 0x811c9dc5;
    for b in s.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x01000193);
    }
    h
}

/// Demangle (best-effort) a mangled name back to its parts.
pub fn demangle(mangled: &str) -> (String, String, String) {
    // Find the `__` separator.
    if let Some(pos) = mangled.rfind("__") {
        let head = &mangled[..pos];
        let path = &mangled[pos + 2..];
        // head is `<pkg>_<version>_<hash>`
        let mut parts = head.rsplitn(2, '_');
        let _hash = parts.next().unwrap_or("");
        let rest = parts.next().unwrap_or(head);
        let mut vparts = rest.rsplitn(2, '_');
        let version = vparts.next().unwrap_or("").to_string();
        let pkg = vparts.next().unwrap_or(rest).to_string();
        return (pkg, version, path.replace('_', "."));
    }
    (String::new(), String::new(), mangled.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_isolated() {
        let a = mangle("foo", "1.0.0", "add");
        let b = mangle("foo", "2.0.0", "add");
        assert_ne!(a, b);
        let (pkg, ver, path) = demangle(&a);
        assert_eq!(pkg, "foo");
        assert_eq!(ver, "1.0.0");
        assert_eq!(path, "add");
    }
}
