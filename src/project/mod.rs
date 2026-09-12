//! Project and dependency management for `.pspc` files.

pub mod yaml;
pub mod deps;
pub mod mangle;

use std::path::Path;

/// Watchdog policy: auto-restart the program when it exits abnormally.
///
/// This is honoured by the *host* (the `pss` runner and the native driver
/// embedded by `pssc --file-exe`): the parent process relaunches the program
/// (a real child process) up to `max_auto_restarts` times, waiting
/// `auto_restarts_sleep_time` seconds between attempts, until it exits with
/// status 0 or the retry budget is exhausted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Watchdog {
    pub auto_restart: bool,
    pub max_auto_restarts: u32,
    /// Restart delay in seconds.
    pub auto_restarts_sleep_time: u32,
}

/// A resolved project model.
#[derive(Debug, Clone, Default)]
pub struct Project {
    pub name: Option<String>,
    pub version: Option<String>,
    pub dependencies: Vec<deps::Dep>,
    pub pspc_path: Option<String>,
    /// Watchdog policy from the `watchdog:` block (None = not configured).
    pub watchdog: Option<Watchdog>,
    /// `no-api-check: true` — skip the build-time "does the called function
    /// exist" check (only valid for `.project` builds, so a host Rust program
    /// can provide the API at runtime).
    pub no_api_check: bool,
}

impl Project {
    pub fn is_empty(&self) -> bool {
        self.pspc_path.is_none()
    }
}

impl Watchdog {
    /// Encode as 3 little-endian u32s: (auto_restart as 0/1, max, sleep_seconds).
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(12);
        out.extend_from_slice(&(self.auto_restart as u32).to_le_bytes());
        out.extend_from_slice(&self.max_auto_restarts.to_le_bytes());
        out.extend_from_slice(&self.auto_restarts_sleep_time.to_le_bytes());
        out
    }

    /// Decode from [`Watchdog::to_bytes`]; returns None on a malformed payload.
    pub fn from_bytes(b: &[u8]) -> Option<Watchdog> {
        if b.len() < 12 {
            return None;
        }
        let u = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        Some(Watchdog {
            auto_restart: u(0) != 0,
            max_auto_restarts: u(4),
            auto_restarts_sleep_time: u(8),
        })
    }
}

/// Load the `.pspc` project file for a `.psp` source file, if one exists next to
/// it (same basename, e.g. `hello.psp` <-> `hello.pspc`).
pub fn load_for(ps_file: &str) -> Project {
    let path = Path::new(ps_file);
    let dir = path.parent().unwrap_or(Path::new("."));
    let base = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();

    // candidate: <base>.pspc in the same directory
    let candidates = vec![dir.join(format!("{base}.pspc"))];

    for cand in candidates {
        if let Ok(src) = std::fs::read_to_string(&cand) {
            return match yaml::PspcDoc::parse(&src) {
                Ok(doc) => {
                    let mut deps = Vec::new();
                    for (name, spec) in &doc.dependencies {
                        let mut d = deps::parse_dep(name, spec);
                        let _ = deps::resolve_dep(&mut d);
                        deps.push(d);
                    }
                    Project {
                        name: doc.name,
                        version: doc.version,
                        dependencies: deps,
                        watchdog: doc.watchdog.map(|w| Watchdog {
                            auto_restart: w.auto_restart,
                            max_auto_restarts: w.max_auto_restarts,
                            auto_restarts_sleep_time: w.auto_restarts_sleep_time,
                        }),
                        no_api_check: doc.no_api_check,
                        pspc_path: Some(cand.to_string_lossy().to_string()),
                    }
                }
                Err(_) => Project { pspc_path: Some(cand.to_string_lossy().to_string()), ..Default::default() },
            };
        }
    }
    Project::default()
}

/// Format a project summary for diagnostics.
pub fn describe(proj: &Project) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "project: {} v{}\n",
        proj.name.as_deref().unwrap_or("(unnamed)"),
        proj.version.as_deref().unwrap_or("0.0.0")
    ));
    for d in &proj.dependencies {
        s.push_str(&format!(
            "  dep {} <- {} {} ({})\n",
            d.name,
            d.url,
            d.revision.clone().unwrap_or_else(|| "HEAD".into()),
            if d.resolved { "cached" } else { "unresolved" }
        ));
    }
    if let Some(wd) = &proj.watchdog {
        s.push_str(&format!(
            "  watchdog: auto_restart={} max_auto_restarts={} sleep={}s\n",
            wd.auto_restart, wd.max_auto_restarts, wd.auto_restarts_sleep_time
        ));
    }
    if proj.no_api_check {
        s.push_str("  no-api-check: true (skip build-time call-existence check)\n");
    }
    s
}

