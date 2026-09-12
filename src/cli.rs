//! Command-line interface for the `pss` runner.
//!
//! `pss` runs Pointerses programs:
//!   * `pss <file.project>` — run a compiled `.project` package (with arch check).
//!   * `pss <file.psp>`     — compile and run a source file directly.
//!   * `pss run <file.psp>` — compile and run a source file (daemon-accelerated).
//!   * `pss daemon ...`     — internal resident daemon used by `pss run`.
//!   * `pss version | help`.

use crate::arch::Arch;
use crate::compiler;
use crate::daemon;
use crate::packaging;
use crate::project;
use crate::vm;
use crate::watchdog;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Run the CLI. Returns an exit code (0 = ok, 1 = program error, 2 = usage error).
pub fn run(args: &[String]) -> Result<u8, String> {
    if args.len() < 2 {
        print_help();
        return Ok(0);
    }
    let sub = args[1].as_str();
    let rest = &args[2..];
    match sub {
        "run" => cmd_run(rest),
        "daemon" => cmd_daemon(rest),
        "task" => cmd_task(rest),
        "-c" | "--command" => {
            let code = rest
                .get(0)
                .ok_or("missing code for -c (usage: pss -c '<code>' [args...])")?;
            let prog_args = &rest[1..];
            cmd_command(code, prog_args)
        }
        "version" | "--version" | "-V" => {
            println!("pss {} (Pointerses compiler)", VERSION);
            Ok(0)
        }
        "help" | "--help" | "-h" => {
            print_help();
            Ok(0)
        }
        // Bare file invocation: `pss <file.psp | file.project> [args...]`.
        // `--no-daemon` is a runner flag, never a program argument.
        other if !other.starts_with('-') => {
            let (no_daemon, prog_args) = split_runner_flags(rest);
            run_file(other, &prog_args, no_daemon)
        }
        other => Err(format!("unknown sub-command `{other}` (try `pss help`)")),
    }
}

/// Show the PointerSes error screen for a program/compile failure. Skipped in
/// the watchdog's supervised child so a restart loop is not blocked by a modal
/// dialog (the popup is meant for ordinary users running a program, who do not
/// use auto-restart).
fn screen_err(msg: &str) {
    if !watchdog::is_child() {
        crate::errorscreen::show_error(msg);
    }
}

// ---------------------------------------------------------------------------
// task
// ---------------------------------------------------------------------------

/// `pss task <name> [args...]` — built-in tasks.
///
/// Currently:
///   `pss task pointerses-error-screen -e "<报错内容>"`
///   Pop up the PointerSes error screen with the given content. The error text
///   should be quoted (double quotes) so spaces / special characters arrive as
///   a single argument; everything after `-e` is joined with spaces anyway, so
///   even an unquoted multi-word message survives.
fn cmd_task(args: &[String]) -> Result<u8, String> {
    let name = args
        .first()
        .ok_or("missing task name (usage: pss task <name> [args...])")?;
    match name.as_str() {
        "pointerses-error-screen" => {
            let mut msg = String::new();
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "-e" | "--error" => {
                        if i + 1 >= args.len() {
                            return Err(
                                "missing error content after -e (usage: pss task \
                                 pointerses-error-screen -e \"<报错内容>\")"
                                    .to_string(),
                            );
                        }
                        msg = args[i + 1..].join(" ");
                        i = args.len();
                    }
                    s => {
                        return Err(format!(
                            "unknown option `{s}` for pointerses-error-screen (usage: pss task \
                             pointerses-error-screen -e \"<报错内容>\")"
                        ))
                    }
                }
            }
            if msg.is_empty() {
                return Err("missing -e \"<报错内容>\"".to_string());
            }
            crate::errorscreen::show_error(&msg);
            Ok(0)
        }
        other => Err(format!(
            "unknown task `{other}` (available: pointerses-error-screen)"
        )),
    }
}

/// Run a single line of Pointerses source directly (like `python -c`).
fn cmd_command(code: &str, prog_args: &[String]) -> Result<u8, String> {
    // Pseudo-path "<command>": no real file, so `import` resolves against the
    // current directory. No daemon involvement (there is no file to snapshot).
    let toks = crate::lexer::tokenize(code)
        .map_err(|e| format!("lex error in <command>: {e}"))?;
    let wrapped = match crate::parser::parse(&toks) {
        // A complete program that already defines `main`: run it as-is.
        Ok(prog) if prog.funcs.iter().any(|f| f.module.is_none() && f.name == "main") => None,
        // A parseable program without `main` (e.g. only `func` declarations):
        // wrapping declarations inside a function body would be invalid.
        Ok(_) => {
            return Err(
                "`-c` code defines declarations but no `func main`; add a `func main() -> int { ... }` \
                 or use bare statements (e.g. `pss -c 'say(\"hi\")'`)"
                    .to_string(),
            )
        }
        // Bare statements / expressions: wrap in a synthetic `main`
        // (python -c semantics).
        Err(_) => Some(format!("func main() -> int {{\n{code}\n  return 0\n}}")),
    };
    let src = wrapped.as_deref().unwrap_or(code);
    let pipe = match compiler::compile_source("<command>", src, "native", false) {
        Ok(p) => p,
        Err(e) => {
            screen_err(&format!("执行 `-c` 代码时发生错误：{e}"));
            return Err(e);
        }
    };
    match vm::execute_bytes(&pipe.bytes, prog_args) {
        Ok(c) => Ok(c),
        Err(e) => {
            screen_err(&format!("执行 `-c` 代码时发生错误：{e}"));
            Err(e)
        }
    }
}

fn print_help() {
    println!(
        "Pointerses runner v{VERSION}

USAGE:
    pss <file.project> [args...]      Run a compiled .project package.
    pss <file.psp> [--no-daemon]      Compile and run a .psp source file.
    pss run <file.psp> [--no-daemon]  Same as above.
    pss -c '<code>' [args...]         Run code directly from the command line
                                      (like `python -c`).
    pss task pointerses-error-screen -e \"报错内容\"
                                      Pop up the PointerSes error screen.
    pss daemon [--port N]             Start the resident Pointerses daemon (used internally).
    pss -v | --version | help

TOOLS:
    pssc    packager (produces .project / native executables)
    pssp    decompiler (.project -> source / bytecode listing)
    pssl    dynamic-library packager (whole project -> .psdl)

ERROR SCREEN:
    When a Pointerses program fails at runtime (or a packaged executable built
    with `pssc` does), an error screen pops up so the message is easy to see and
    report. Set PSS_NO_ERROR_SCREEN=1 to disable the popup (CI / scripts).
"
    );
}

// ---------------------------------------------------------------------------
// run
// ---------------------------------------------------------------------------

fn cmd_run(args: &[String]) -> Result<u8, String> {
    let mut file: Option<String> = None;
    let mut no_daemon = false;
    let mut prog_args: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--no-daemon" => no_daemon = true,
            s if s.starts_with('-') && s != "-" => return Err(format!("unknown option `{s}`")),
            s => {
                if file.is_none() {
                    file = Some(s.to_string());
                } else {
                    prog_args.push(s.to_string());
                }
            }
        }
        i += 1;
    }
    let file = file.ok_or("missing input file: `pss run <file.psp>`")?;
    run_file(&file, &prog_args, no_daemon)
}

/// Split runner flags out of a bare-file argument list.
///
/// `--no-daemon` is a *runner* option, not a program argument: a bare
/// `pss <file.psp> --no-daemon` invocation must not leak the flag into the
/// program's `args()`, and must actually skip the daemon fast-start path.
/// Everything else is passed through untouched, so real program arguments still
/// reach the program.
fn split_runner_flags(args: &[String]) -> (bool, Vec<String>) {
    let mut no_daemon = false;
    let mut prog_args = Vec::new();
    for a in args {
        match a.as_str() {
            "--no-daemon" => no_daemon = true,
            s => prog_args.push(s.to_string()),
        }
    }
    (no_daemon, prog_args)
}

/// Run either a `.project` package or a `.psp` source file.
fn run_file(file: &str, prog_args: &[String], no_daemon: bool) -> Result<u8, String> {
    if file.ends_with(".project") || file.ends_with(".psdl") {
        return run_package(file, prog_args);
    }
    run_source(file, prog_args, no_daemon)
}

/// Run a compiled `.project` / `.psdl` package with an architecture check.
fn run_package(file: &str, prog_args: &[String]) -> Result<u8, String> {
    let data = std::fs::read(file).map_err(|e| format!("cannot read `{file}`: {e}"))?;
    let pkg = packaging::Package::deserialize(&data)?;
    packaging::check_arch(&pkg, Arch::host())?;
    if let Some(wd) = pkg.watchdog {
        if wd.auto_restart && !watchdog::is_child() {
            return watchdog::supervise(&wd, || spawn_supervised(file, prog_args, false));
        }
    }
    match vm::execute_bytes(&pkg.bytecode, prog_args) {
        Ok(code) => Ok(code),
        Err(e) => {
            screen_err(&format!("运行 `{file}` 时发生错误：{e}"));
            Err(e)
        }
    }
}

/// Compile and run a `.psp` source file (daemon-accelerated unless disabled).
fn run_source(file: &str, prog_args: &[String], no_daemon: bool) -> Result<u8, String> {
    // Watchdog: when the project configures auto-restart and we are not the
    // supervised child, act as the supervisor by relaunching this program as a
    // child process (real process-level restarts on any abnormal exit).
    if !watchdog::is_child() {
        if let Some(wd) = project::load_for(file).watchdog {
            if wd.auto_restart {
                return watchdog::supervise(&wd, || spawn_supervised(file, prog_args, true));
            }
        }
    }

    // Fast-start path: try the resident daemon's memory snapshot.
    if !no_daemon {
        if let Some(bytes) = daemon::try_fast_start(file) {
            return match vm::execute_bytes(&bytes, prog_args) {
                Ok(code) => Ok(code),
                Err(e) => {
                    screen_err(&format!("运行 `{file}` 时发生错误：{e}"));
                    Err(e)
                }
            };
        }
    }

    // Cold path: full compile, then (if permitted) warm the daemon snapshot.
    let pipe = match compiler::compile_file(file, "native") {
        Ok(p) => p,
        Err(e) => {
            screen_err(&format!("编译 `{file}` 时发生错误：{e}"));
            return Err(e);
        }
    };
    if !no_daemon {
        daemon::warm_cache(file, pipe.bytes.clone());
    }
    match vm::execute_bytes(&pipe.bytes, prog_args) {
        Ok(code) => Ok(code),
        Err(e) => {
            screen_err(&format!("运行 `{file}` 时发生错误：{e}"));
            Err(e)
        }
    }
}

/// Launch this same program (`pss` / the packaged exe) as a supervised child
/// process, preserving program arguments. Uses the `run` sub-command so
/// `--no-daemon` is parsed by the CLI (a bare `<file> --no-daemon` invocation
/// would leak it into the program arguments).
fn spawn_supervised(
    file: &str,
    prog_args: &[String],
    source: bool,
) -> Result<std::process::ExitStatus, String> {
    let exe =
        std::env::current_exe().map_err(|e| format!("cannot locate this executable: {e}"))?;
    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("run").arg(file);
    if source {
        cmd.arg("--no-daemon");
    }
    cmd.args(prog_args);
    cmd.env(watchdog::WATCHDOG_CHILD_ENV, "1");
    cmd.status().map_err(|e| format!("failed to run `{}`: {e}", exe.display()))
}

// ---------------------------------------------------------------------------
// daemon
// ---------------------------------------------------------------------------

fn cmd_daemon(args: &[String]) -> Result<u8, String> {
    let mut port: u16 = daemon::DEFAULT_PORT;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--port" => {
                i += 1;
                let v = args.get(i).ok_or("missing value for --port")?;
                port = v.parse().map_err(|_| format!("bad port `{v}`"))?;
            }
            s => return Err(format!("unknown daemon option `{s}`")),
        }
        i += 1;
    }
    daemon::serve(port)
}
