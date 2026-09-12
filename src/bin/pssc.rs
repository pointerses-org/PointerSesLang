//! `pssc` — the Pointerses packager / compiler driver.
//!
//! Packages a compiled program:
//!   * default — a `.project` binary (run by `pss`; never run directly).
//!   * `--file-exe` — a native Windows executable.
//!   * `--file-x`   — a native Unix executable.
//!   * `--project`  — package the current directory as a project.
//!
//! Also exposes the migrated `--snapshot` and `--selfhost` diagnostics from the
//! old `pss build` surface. All tools accept `--fram-*` architecture flags.

use std::process::ExitCode;

use pointerses::arch::{self, Arch};
use pointerses::bootstrap;
use pointerses::codegen::native;
use pointerses::project;
use pointerses::{compiler, daemon, packaging};

fn main() -> ExitCode {
    let full: Vec<String> = std::env::args().collect();
    if matches!(full.get(1).map(|s| s.as_str()), Some("-v") | Some("--version")) {
        println!("pssc {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(code) => ExitCode::from(code),
        Err(msg) => {
            eprintln!("pssc: error: {msg}");
            ExitCode::from(2)
        }
    }
}

fn run(args: &[String]) -> Result<u8, String> {
    let (fram, rest) = arch::parse_fram(args);

    let mut input: Option<String> = None;
    let mut out: Option<String> = None;
    let mut file_exe = false;
    let mut file_x = false;
    let mut as_project = false;
    let mut backend = "native".to_string();
    let mut emit_ir = false;
    let mut snapshot = false;
    let mut selfhost = false;
    let mut no_api_check = false;

    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "-o" | "--out" => {
                i += 1;
                out = Some(rest.get(i).ok_or("missing value for -o")?.clone());
            }
            "--file-exe" => file_exe = true,
            "--file-x" => file_x = true,
            "--project" => as_project = true,
            "--no-api-check" => no_api_check = true,
            "-b" | "--backend" => {
                i += 1;
                backend = rest.get(i).ok_or("missing value for -b")?.clone();
                if backend != "native" && backend != "llvm" {
                    return Err(format!("unknown backend `{backend}` (native|llvm)"));
                }
            }
            "--emit-ir" => emit_ir = true,
            "--snapshot" => snapshot = true,
            "--selfhost" => selfhost = true,
            s if s.starts_with('-') => return Err(format!("unknown option `{s}`")),
            s => {
                if input.is_none() {
                    input = Some(s.to_string());
                } else {
                    return Err("more than one input file given".into());
                }
            }
        }
        i += 1;
    }

    if as_project {
        // Current directory is the project; no positional input required.
        if input.is_some() {
            return Err("--project packages the current directory; do not pass an input file".into());
        }
    } else if input.is_none() {
        return Err("missing input file: `pssc <file.psp>` (or `pssc --project`)".into());
    }

    let entry = if as_project {
        resolve_project_entry()?
    } else {
        input.unwrap()
    };
    // Watchdog + no-api-check policy from the entry's `.pspc` (same basename).
    let proj = project::load_for(&entry);
    let watchdog = proj.watchdog;
    // `--no-api-check` (CLI) OR the `.pspc` `no-api-check: true` config.
    let no_api_check = no_api_check || proj.no_api_check;
    // `no-api-check` is only valid for `.project` packaging: native
    // executables have no host to provide the API at runtime.
    if no_api_check && (file_exe || file_x) {
        return Err(
            "`--no-api-check` / `no-api-check: true` is not available for native \
             executable builds (--file-exe / --file-x); it is only valid for \
             `.project` packaging"
                .into(),
        );
    }
    let compiled = if no_api_check {
        compiler::compile_file_nac(&entry, &backend)?
    } else {
        compiler::compile_file(&entry, &backend)?
    };

    if snapshot {
        write_snapshot(&entry, &compiled)?;
    }
    if selfhost {
        bootstrap::emit_selfhost(&compiled.meta, &entry)?;
    }
    if emit_ir && backend == "llvm" {
        let ll = native::emit_driver_llvm(&compiled.bytes);
        let ll_path = format!("{}.ll", strip_ext(&entry));
        std::fs::write(&ll_path, &ll).map_err(|e| format!("cannot write IR: {e}"))?;
        println!("wrote LLVM IR to {ll_path}");
    }

    let out = out.unwrap_or_else(|| default_out(&entry, &backend, as_project, file_exe, file_x));

    if file_exe || file_x {
        // Native executable(s): one real native binary per target architecture
        // in the fram set. Cross targets are compiled with `rustc --target` and
        // linked by `zig cc` (through the `zigcc` shim), so the output really
        // is an ELF/COFF binary for the requested architecture.
        let os_windows = file_exe; // --file-exe => Windows; --file-x => Unix
        let archs: Vec<Arch> = Arch::ALL.iter().copied().filter(|a| fram.contains(*a)).collect();
        if archs.is_empty() {
            return Err("no target architecture selected (--fram-*)".into());
        }
        let single = archs.len() == 1;
        for a in &archs {
            let out_path = if single { out.clone() } else { with_arch_suffix(&out, *a) };
            if backend == "llvm" {
                if *a == Arch::host() {
                    let exe = native::link_llvm_executable(&compiled.bytes, &out_path, watchdog.as_ref())?;
                    println!("built `{exe}` from `{entry}` ({}, llvm backend)", a.label());
                } else {
                    return Err(format!(
                        "the `llvm` backend is host-only (no cross LLVM-C); use the default \
                         `native` backend for cross-architecture executables"
                    ));
                }
            } else {
                let (tri, zig) = exe_target(os_windows, *a);
                let cross = tri != Arch::host_triple();
                let opt = native::LinkOpt {
                    rust_triple: if cross { Some(tri.to_string()) } else { None },
                    zig_target: if cross { Some(zig.to_string()) } else { None },
                };
                let exe = native::link_native_executable_opt(&compiled.bytes, &out_path, &opt, watchdog.as_ref())?;
                println!(
                    "built `{exe}` from `{entry}` ({}/{})",
                    if os_windows { "windows" } else { "linux" },
                    a.label()
                );
            }
        }
        return Ok(0);
    }

    // Package as `.project`.
    let meta_json = bootstrap::meta_to_json(&compiled.meta).into_bytes();
    let mut pkg = packaging::Package::new(fram, entry.clone(), compiled.source, compiled.bytes, meta_json);
    pkg.watchdog = watchdog;
    std::fs::write(&out, pkg.serialize(&packaging::PROJECT_MAGIC))
        .map_err(|e| format!("cannot write package `{out}`: {e}"))?;
    println!("packaged `{out}` from `{entry}` (arch: {})", pkg.arch_label());
    Ok(0)
}

/// When `--project` is given, find the project entry source in the current
/// directory: `main.psp`, else a lone `.psp`, else an error.
fn resolve_project_entry() -> Result<String, String> {
    let dir = std::env::current_dir().map_err(|e| format!("cannot get cwd: {e}"))?;
    let main = dir.join("main.psp");
    if main.is_file() {
        return Ok(main.to_string_lossy().to_string());
    }
    let mut psp: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            if e.path().extension().map(|x| x == "psp").unwrap_or(false) {
                psp.push(e.path());
            }
        }
    }
    match psp.len() {
        1 => Ok(psp[0].to_string_lossy().to_string()),
        0 => Err("--project: no `.psp` source found in the current directory".into()),
        _ => Err(
            "--project: multiple `.psp` files found and no `main.psp`; specify the entry explicitly".into()
        ),
    }
}

fn write_snapshot(file: &str, compiled: &compiler::Compiled) -> Result<(), String> {
    let snap = daemon::build_snapshot(&compiled.file, &compiled.bytes, &compiled.meta);
    let json = daemon::snapshot_json(&snap)?;
    let out = format!("{}.snap.json", strip_ext(file));
    std::fs::write(&out, json).map_err(|e| format!("cannot write snapshot: {e}"))?;
    println!("wrote snapshot to `{out}`");
    Ok(())
}

fn default_out(entry: &str, _backend: &str, as_project: bool, file_exe: bool, file_x: bool) -> String {
    if as_project {
        return if file_exe {
            "project.exe".to_string()
        } else if file_x {
            // Unix executables carry no extension, even when built on Windows.
            "project".to_string()
        } else {
            "project.project".to_string()
        };
    }
    let base = strip_ext(entry);
    if file_exe {
        #[cfg(windows)]
        {
            format!("{base}.exe")
        }
        #[cfg(not(windows))]
        {
            base
        }
    } else if file_x {
        base
    } else {
        format!("{base}.project")
    }
}

/// Rust target triple + zig `-target` string for `(os, arch)`. Zig names the
/// 32-bit x86 architecture `x86` (not `i686`).
fn exe_target(windows: bool, a: Arch) -> (&'static str, &'static str) {
    match (windows, a) {
        (true, Arch::X64) => ("x86_64-pc-windows-gnu", "x86_64-windows-gnu"),
        (true, Arch::X32) => ("i686-pc-windows-gnu", "x86-windows-gnu"),
        (true, Arch::Arm64) => ("aarch64-pc-windows-gnullvm", "aarch64-windows-gnu"),
        (false, Arch::X64) => ("x86_64-unknown-linux-gnu", "x86_64-linux-gnu"),
        (false, Arch::X32) => ("i686-unknown-linux-gnu", "x86-linux-gnu"),
        (false, Arch::Arm64) => ("aarch64-unknown-linux-gnu", "aarch64-linux-gnu"),
    }
}

/// `hello.exe` + arm64 -> `hello_arm64.exe`; `hello` + x32 -> `hello_x32`.
fn with_arch_suffix(out: &str, a: Arch) -> String {
    match out.rfind('.') {
        Some(i) if i > 0 => format!("{}_{}{}", &out[..i], a.label(), &out[i..]),
        _ => format!("{}_{}", out, a.label()),
    }
}

fn strip_ext(file: &str) -> String {
    match file.rfind('.') {
        Some(i) if i > 0 => file[..i].to_string(),
        _ => file.to_string(),
    }
}

// Keep `Arch` referenced from this crate's surface.
pub fn _host_arch() -> Arch {
    Arch::host()
}
