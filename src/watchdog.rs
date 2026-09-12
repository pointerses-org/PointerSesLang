//! Watchdog supervision: auto-restart a program that exits abnormally.
//!
//! The watchdog is a *host-level* supervisor (real process-level restarts): the
//! parent process runs the actual program as a child process, and when that
//! child terminates with a non-zero status (any failure — a `main` non-zero
//! return, an uncaught exception, a panic, an abort, or even an OS signal) it
//! relaunches it after a short sleep, up to `max_auto_restarts` times.
//!
//! The same policy is honoured by `pss run`/`pss <file>` and by the native
//! executables produced by `pssc --file-exe` (the driver embeds the equivalent
//! logic). Both communicate the "child" role to avoid unbounded recursion via
//! the `PSS_WATCHDOG_CHILD` environment variable.

use std::process::ExitStatus;
use std::time::Duration;

use crate::project::Watchdog;

/// Environment variable marking the supervised child process; a child skips its
/// own watchdog so the supervisor does not recurse into itself.
pub const WATCHDOG_CHILD_ENV: &str = "PSS_WATCHDOG_CHILD";

/// Whether this process is the supervised child of a watchdog supervisor.
pub fn is_child() -> bool {
    std::env::var(WATCHDOG_CHILD_ENV).is_ok()
}

/// Run `spawn` (which must launch the program as a child and wait for it) under
/// watchdog supervision. Returns the final exit code.
///
/// * exit 0 → success, return 0.
/// * any non-zero exit → abnormal; restart up to `max_auto_restarts` times,
///   sleeping `auto_restarts_sleep_time` seconds between attempts.
/// * the retry budget is exhausted → return the last (non-zero) exit code.
pub fn supervise<F: Fn() -> Result<ExitStatus, String>>(
    wd: &Watchdog,
    spawn: F,
) -> Result<u8, String> {
    if !wd.auto_restart {
        // Watchdog block present but auto-restart disabled: run once only.
        return exit_code(spawn()?);
    }
    let mut restarts: u32 = 0;
    loop {
        let st = spawn()?;
        let code = exit_code(st)?;
        if code == 0 {
            return Ok(0);
        }
        restarts += 1;
        if restarts > wd.max_auto_restarts {
            eprintln!(
                "watchdog: program failed {restarts} time(s); giving up (max_auto_restarts={})",
                wd.max_auto_restarts
            );
            return Ok(code);
        }
        eprintln!(
            "watchdog: program exited abnormally (code {code}); restarting in {}s ({restarts}/{})",
            wd.auto_restarts_sleep_time, wd.max_auto_restarts
        );
        std::thread::sleep(Duration::from_secs(wd.auto_restarts_sleep_time as u64));
    }
}

/// Map an `ExitStatus` to a `u8` exit code (1 when terminated by a signal,
/// which has no status code).
fn exit_code(st: ExitStatus) -> Result<u8, String> {
    Ok(st.code().unwrap_or(1) as u8)
}
