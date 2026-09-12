//! Dependency resolution and a local cache for git-backed `.pspc` dependencies.
//!
//! A dependency spec is a git repository URL plus an optional `#commit`/`@tag`
//! pin, e.g.:
//!   * `git+https://github.com/x/foo.git#v1.0.0`
//!   * `https://github.com/x/foo.git@abc123`
//!
//! The resolver clones (or reuses) the repository in a local cache directory
//! keyed by `name + revision`, so the same dependency/version pair is fetched at
//! most once. When `git` or the network is unavailable the dependency is
//! recorded as unresolved rather than failing the whole build, and version
//! isolation is still applied through symbol mangling.

use std::path::{Path, PathBuf};

/// A parsed dependency.
#[derive(Debug, Clone)]
pub struct Dep {
    pub name: String,
    pub url: String,
    pub revision: Option<String>,
    pub resolved: bool,
    pub cache_path: Option<PathBuf>,
}

/// Parse a dependency spec string (`name: spec`) into a [`Dep`].
pub fn parse_dep(name: &str, spec: &str) -> Dep {
    let mut url = spec.to_string();
    let mut revision = None;

    // `git+` prefix is stripped.
    if let Some(rest) = url.strip_prefix("git+") {
        url = rest.to_string();
    }
    // `#revision` or `@revision` suffix.
    for sep in ['#', '@'] {
        if let Some(pos) = url.rfind(sep) {
            // Only treat as revision if it appears after the last '/'
            let last_slash = url.rfind('/').map(|p| p).unwrap_or(0);
            if pos > last_slash {
                let rev = url[pos + 1..].to_string();
                if !rev.is_empty() {
                    revision = Some(rev);
                }
                url.truncate(pos);
                break;
            }
        }
    }
    // trim trailing .git for cache naming
    let name = name.to_string();
    Dep { name, url, revision, resolved: false, cache_path: None }
}

/// Directory used for the local dependency cache.
pub fn cache_root() -> PathBuf {
    let base = std::env::var("PSS_CACHE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var("USERPROFILE")
                .or_else(|_| std::env::var("HOME"))
                .map(|h| Path::new(&h).join(".pointerses"))
                .unwrap_or_else(|_| PathBuf::from(".pointerses"))
        });
    base.join("cache")
}

/// A stable cache key for a dependency + revision.
pub fn cache_key(dep: &Dep) -> String {
    let rev = dep.revision.clone().unwrap_or_else(|| "HEAD".into());
    let mut key = format!("{}-{}", dep.name, rev);
    // sanitize filesystem-hostile characters
    key = key
        .replace(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_', "_");
    key
}

/// Resolve a dependency by cloning (or reusing) its git repository in the cache.
pub fn resolve_dep(dep: &mut Dep) -> Result<(), String> {
    let root = cache_root();
    std::fs::create_dir_all(&root).map_err(|e| format!("cannot create cache: {e}"))?;
    let dir = root.join(cache_key(dep));
    if dir.join(".git").exists() {
        // already cached
        dep.cache_path = Some(dir);
        dep.resolved = true;
        return Ok(());
    }
    // clone
    let status = std::process::Command::new("git")
        .args(["clone", "--quiet", &dep.url, dir.to_str().unwrap_or(".")])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    match status {
        Ok(s) if s.success() => {
            if let Some(rev) = &dep.revision {
                let checkout = std::process::Command::new("git")
                    .current_dir(&dir)
                    .args(["checkout", "--quiet", rev])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
                if let Ok(s2) = checkout {
                    if !s2.success() {
                        return Err(format!(
                            "revision `{rev}` not found in `{}`",
                            dep.url
                        ));
                    }
                }
            }
            dep.cache_path = Some(dir);
            dep.resolved = true;
            Ok(())
        }
        _ => {
            // git or network unavailable: record unresolved (does not abort build)
            dep.resolved = false;
            Ok(())
        }
    }
}
