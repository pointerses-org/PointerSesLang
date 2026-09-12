//! Minimal YAML subset parser for `.pspc` project files.
//!
//! `.pspc` is a small declarative format (a YAML subset) with the fields
//! `name`, `version`, and `dependencies`. We parse exactly this subset without
//! pulling in an external YAML crate (keeps the toolchain std-only and offline).
//! The parser understands:
//!   * scalar `key: value` pairs
//!   * a `dependencies:` map of `name: spec` entries
//!   * comments (`# ...`)

/// A parsed `.pspc` document.
#[derive(Debug, Clone, Default)]
pub struct PspcDoc {
    pub name: Option<String>,
    pub version: Option<String>,
    pub dependencies: Vec<(String, String)>,
    /// Watchdog policy, if a `watchdog:` block is present.
    pub watchdog: Option<WatchdogYaml>,
    /// `no-api-check: true` skips the build-time "does the called function
    /// exist" check, so a `.psp` library may call functions that a host Rust
    /// program provides at runtime (via `PSS_RUST_LIB`). Only valid for
    /// `.project` builds (not `--file-exe` / `--file-x`).
    pub no_api_check: bool,
    /// Any additional top-level keys we do not interpret.
    pub extra: Vec<(String, String)>,
}

/// Watchdog policy parsed from a `watchdog:` block.
#[derive(Debug, Clone, Default)]
pub struct WatchdogYaml {
    pub auto_restart: bool,
    pub max_auto_restarts: u32,
    /// Restart delay in seconds.
    pub auto_restarts_sleep_time: u32,
}

impl PspcDoc {
    pub fn parse(src: &str) -> Result<PspcDoc, String> {
        let mut doc = PspcDoc::default();
        let mut section: Option<String> = None;
        let mut line_no = 0usize;
        for raw in src.lines() {
            line_no += 1;
            let line = strip_comment(raw).trim().to_string();
            if line.is_empty() {
                continue;
            }
            let indent = raw.len() - raw.trim_start().len();
            // A `key:` alone opens a section (e.g. `dependencies:`).
            if indent == 0 && line.ends_with(':') {
                section = Some(line.trim_end_matches(':').to_string());
                continue;
            }
            let (key, value) = match split_key_value(&line) {
                Some(kv) => kv,
                None => {
                    return Err(format!("line {line_no}: expected `key: value`, got `{line}`"));
                }
            };
            match section.as_deref() {
                Some("dependencies") => {
                    doc.dependencies.push((key, value));
                }
                Some("watchdog") => {
                    let wd = doc.watchdog.get_or_insert_with(WatchdogYaml::default);
                    match key.as_str() {
                        "auto_restart" => {
                            wd.auto_restart =
                                parse_bool(&value, line_no).map_err(|e| format!("{e} (line {line_no})"))?;
                        }
                        "max_auto_restarts" => {
                            wd.max_auto_restarts =
                                value.trim().parse().map_err(|_| {
                                    format!("line {line_no}: `max_auto_restarts` must be an integer, got `{value}`")
                                })?;
                        }
                        "auto_restarts_sleep_time" => {
                            wd.auto_restarts_sleep_time =
                                value.trim().parse().map_err(|_| {
                                    format!(
                                        "line {line_no}: `auto_restarts_sleep_time` must be an integer (seconds), got `{value}`"
                                    )
                                })?;
                        }
                        _ => doc.extra.push((key, value)),
                    }
                }
                _ => match key.as_str() {
                    "name" => doc.name = Some(value),
                    "version" => doc.version = Some(value),
                    "no-api-check" => {
                        doc.no_api_check =
                            parse_bool(&value, line_no).map_err(|e| format!("{e} (line {line_no})"))?;
                    }
                    _ => doc.extra.push((key, value)),
                },
            }
        }
        Ok(doc)
    }
}

fn parse_bool(value: &str, line_no: usize) -> Result<bool, String> {
    match value.trim() {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(format!("line {line_no}: expected `true` or `false`, got `{value}`")),
    }
}

fn strip_comment(line: &str) -> &str {
    // A `#` that is not inside quotes starts a comment.
    let mut in_q: Option<char> = None;
    for (i, c) in line.char_indices() {
        match c {
            '\'' | '"' => {
                if in_q == Some(c) {
                    in_q = None;
                } else if in_q.is_none() {
                    in_q = Some(c);
                }
            }
            '#' if in_q.is_none() => return &line[..i],
            _ => {}
        }
    }
    line
}

fn split_key_value(line: &str) -> Option<(String, String)> {
    let idx = line.find(':')?;
    let key = line[..idx].trim().to_string();
    let value = line[idx + 1..].trim().to_string();
    Some((key, value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_pspc() {
        let src = "name: demo\nversion: 1.0.0\ndependencies:\n  foo: git+https://github.com/x/foo.git#v1\n";
        let doc = PspcDoc::parse(src).unwrap();
        assert_eq!(doc.name.as_deref(), Some("demo"));
        assert_eq!(doc.version.as_deref(), Some("1.0.0"));
        assert_eq!(doc.dependencies.len(), 1);
        assert_eq!(doc.dependencies[0].0, "foo");
        assert!(doc.watchdog.is_none());
    }

    #[test]
    fn parses_watchdog_block() {
        let src = "name: svc\nwatchdog:\n  auto_restart: true\n  max_auto_restarts: 3\n  auto_restarts_sleep_time: 20\n";
        let doc = PspcDoc::parse(src).unwrap();
        let wd = doc.watchdog.expect("watchdog parsed");
        assert!(wd.auto_restart);
        assert_eq!(wd.max_auto_restarts, 3);
        assert_eq!(wd.auto_restarts_sleep_time, 20);
    }

    #[test]
    fn watchdog_defaults_when_disabled() {
        let src = "name: svc\nwatchdog:\n  auto_restart: false\n";
        let doc = PspcDoc::parse(src).unwrap();
        let wd = doc.watchdog.expect("watchdog parsed");
        assert!(!wd.auto_restart);
        assert_eq!(wd.max_auto_restarts, 0);
        assert_eq!(wd.auto_restarts_sleep_time, 0);
    }

    #[test]
    fn parses_no_api_check_true() {
        let src = "name: api\nno-api-check: true\n";
        let doc = PspcDoc::parse(src).unwrap();
        assert!(doc.no_api_check);
    }

    #[test]
    fn parses_no_api_check_false_and_default() {
        let src = "name: api\nno-api-check: false\n";
        let doc = PspcDoc::parse(src).unwrap();
        assert!(!doc.no_api_check);
        let def = PspcDoc::parse("name: api\n").unwrap();
        assert!(!def.no_api_check);
    }

    #[test]
    fn bad_no_api_check_bool_rejected() {
        let src = "no-api-check: maybe\n";
        assert!(PspcDoc::parse(src).is_err());
    }

    #[test]
    fn bad_watchdog_bool_rejected() {
        let src = "watchdog:\n  auto_restart: yes\n";
        assert!(PspcDoc::parse(src).is_err());
    }
}
