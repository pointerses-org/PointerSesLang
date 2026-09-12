//! `pss` — the Pointerses runner.
//!
//! Runs compiled `.project` packages (with an architecture check) and compiles
//! + runs `.psp` source files directly. See `pss help` for usage.

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if matches!(args.get(1).map(|s| s.as_str()), Some("-v") | Some("--version")) {
        println!("pss {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    match pointerses::cli::run(&args) {
        Ok(code) => ExitCode::from(code),
        Err(msg) => {
            eprintln!("pss: error: {msg}");
            ExitCode::from(2)
        }
    }
}
