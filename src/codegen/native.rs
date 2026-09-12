//! Native executable linking (native and llvm backends).
//!
//! `pss build` produces a standalone native `.exe` with no external runtime.
//! The compiled bytecode is embedded into a small Rust driver that includes the
//! self-contained VM runtime (the same `runtime/runtime.rs` used by `pss run`)
//! and is linked with `rustc`. Because the runtime is standard-library-only, the
//! driver compiles without any network access or installed LLVM/C toolchain.
//!
//! The `llvm` backend (see [`link_llvm_executable`]) uses the same complete VM
//! runtime so that *every* `.psp` program runs with all features (pointers,
//! non-blocking refcounting, closures, collections and real worker-thread
//! concurrency). The difference from `native` is the toolchain: the program's
//! native driver object is lowered by the *embedded* LLVM library
//! (`LLVM-C.dll`, bundled with `pss` and driven through the LLVM-C API in
//! [`super::llvm_embedded`]), and the runtime is linked in via the GNU (lld)
//! toolchain.

use std::path::PathBuf;
use std::process::Command;

/// The self-contained VM runtime source, embedded at build time so the driver
/// does not depend on the working directory.
const RUNTIME_SRC: &str = include_str!("../../runtime/runtime.rs");

/// The self-contained error-screen source, embedded the same way so a packaged
/// program shows the identical popup as `pss`.
const ERRORSCREEN_SRC: &str = include_str!("../../runtime/errorscreen.rs");

/// The GUI submodule source, embedded the same way.
const GUI_SRC: &str = include_str!("../../runtime/gui.rs");

/// `RUNTIME_SRC` with its GUI submodule inlined.
///
/// `runtime.rs` pulls the GUI in with `#[path = "gui.rs"]`, which only resolves
/// when compiling from `runtime/`. Both drivers embed `runtime.rs` inside their
/// own `mod runtime { ... }`, where that relative path would point at the
/// driver's directory instead, so the module body is spliced in verbatim.
fn runtime_src() -> String {
    const MARKER: &str = "#[path = \"gui.rs\"]";
    match RUNTIME_SRC.find(MARKER) {
        Some(pos) => format!(
            "{}mod gui {{\n{GUI_SRC}\n}}",
            &RUNTIME_SRC[..pos]
        ),
        None => RUNTIME_SRC.to_string(),
    }
}

/// Emit the LLVM driver IR for the `llvm` backend.
///
/// The module embeds the serialized bytecode as a constant byte array and
/// defines `ps_program_main`, which hands the bytecode to the complete VM
/// runtime (`ps_run`) and returns its exit code. Because it is plain LLVM IR it
/// lowers cleanly with `clang` (no C headers required) and covers the whole
/// language via the linked runtime.
pub fn emit_driver_llvm(bytes: &[u8]) -> String {
    let mut items = String::new();
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 {
            items.push_str(", ");
        }
        items.push_str(&format!("i8 {}", *b as i8));
    }
    let n = bytes.len();
    let triple = crate::codegen::llvm_embedded::target_triple();
    format!(
        "; Pointerses -> LLVM driver IR (lowered by clang/LLVM)\n\
target triple = \"{triple}\"\n\n\
@bytecode = private constant [{n} x i8] [{items}]\n\
declare i64 @ps_run(i8*, i64)\n\
define i64 @ps_program_main() {{\nentry:\n\
  %r = call i64 @ps_run(i8* getelementptr inbounds ([{n} x i8], [{n} x i8]* @bytecode, i64 0, i64 0), i64 {n})\n\
  ret i64 %r\n}}\n"
    )
}

/// Link a native executable for the `llvm` backend.
///
/// Pipeline: emit the driver IR -> lower it to a native COFF object with the
/// *embedded* LLVM library (no external `clang`; see [`super::llvm_embedded`])
/// -> build a Rust driver that embeds the complete VM runtime and exposes
/// `ps_run` -> link the object together with the driver using `rustc` (GNU/lld
/// toolchain). Returns the produced executable path.
pub fn link_llvm_executable(
    bytes: &[u8],
    out_path: &str,
    watchdog: Option<&crate::project::Watchdog>,
) -> Result<String, String> {
    let out = std::path::Path::new(out_path);
    let out_dir = out
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));

    // 1. Emit the driver IR and lower it to a native object with embedded LLVM.
    let obj_ext = crate::codegen::llvm_embedded::object_extension();
    let ll_path = out_dir.join("__pss_llvm.ll");
    let obj_path = out_dir.join(format!("__pss_llvm.{obj_ext}"));
    let ir = emit_driver_llvm(bytes);
    std::fs::write(&ll_path, &ir).map_err(|e| format!("cannot write LLVM IR: {e}"))?;

    crate::codegen::llvm_embedded::emit_object(&ir, &obj_path.to_string_lossy())?;

    // 2. Build the Rust driver (embeds the complete VM runtime).
    let driver = build_llvm_driver_source(watchdog);
    let driver_path = out_dir.join("__pss_llvm_driver.rs");
    std::fs::write(&driver_path, &driver)
        .map_err(|e| format!("cannot write driver: {e}"))?;

    // 3. Link the LLVM object together with the driver via rustc (GNU/lld).
    let status = Command::new("rustc")
        .arg("--edition=2021")
        .arg("-O")
        .arg("-C")
        .arg("codegen-units=1")
        .arg("-C")
        .arg(format!("link-arg={}", obj_path.display()))
        .arg(&driver_path)
        .arg("-o")
        .arg(out_path)
        .status()
        .map_err(|e| format!("failed to invoke `rustc`: {e} (is the Rust toolchain on PATH?)"))?;
    if !status.success() {
        let _ = std::fs::remove_file(&ll_path);
        let _ = std::fs::remove_file(&obj_path);
        let _ = std::fs::remove_file(&driver_path);
        return Err("`rustc` failed to link the LLVM object with the runtime; see error above".into());
    }

    // 4. Clean up temporary artifacts.
    let _ = std::fs::remove_file(&ll_path);
    let _ = std::fs::remove_file(&obj_path);
    let _ = std::fs::remove_file(&driver_path);

    Ok(out_path.to_string())
}

/// Build the Rust driver source for the `llvm` backend.
///
/// Unlike the `native` driver this does *not* embed the bytecode (that lives in
/// the clang-lowered object). Instead it embeds the complete VM runtime and
/// exposes `ps_run`, which decodes the bytecode passed in from the LLVM driver
/// and executes it with the full runtime.
fn build_llvm_driver_source(watchdog: Option<&crate::project::Watchdog>) -> String {
    let (wd_ar, wd_max, wd_sleep) = match watchdog {
        Some(wd) => (wd.auto_restart, wd.max_auto_restarts, wd.auto_restarts_sleep_time),
        None => (false, 0, 0),
    };
    format!(
        r#"// Auto-generated Pointerses LLVM native driver.
#![allow(dead_code)]
mod runtime {{
{RUNTIME_SRC}
}}
use std::slice;
use std::sync::OnceLock;

static PS_ARGS: OnceLock<Vec<String>> = OnceLock::new();

#[no_mangle]
pub extern "C" fn ps_run(ptr: *const u8, len: usize) -> i64 {{
    if ptr.is_null() {{ eprintln!("pointerses runtime: null bytecode"); return 2; }}
    let code: &[u8] = unsafe {{ slice::from_raw_parts(ptr, len) }};
    let args: Vec<String> = PS_ARGS.get().cloned().unwrap_or_default();
    let prog = match runtime::decode_program(code) {{
        Ok(p) => p,
        Err(e) => {{ eprintln!("pointerses runtime: {{e}}"); return 2; }}
    }};
    match runtime::run_program(&prog, &args) {{
        Ok(c) => c as i64,
        Err(e) => {{ eprintln!("pointerses runtime error: {{e}}"); return 1; }}
    }}
}}

extern "C" {{ fn ps_program_main() -> i64; }}

fn main() {{
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Watchdog: relaunch this executable as a real child process whenever it
    // exits abnormally (non-zero), up to the configured budget.
    const AUTO_RESTART: bool = {WD_AR};
    const MAX_RESTARTS: u32 = {WD_MAX};
    const SLEEP_SECS: u32 = {WD_SLEEP};
    let is_child = std::env::var("PSS_WATCHDOG_CHILD").is_ok();
    if AUTO_RESTART && !is_child {{
        let mut restarts: u32 = 0;
        loop {{
            let exe = match std::env::current_exe() {{
                Ok(e) => e,
                Err(err) => {{ eprintln!("watchdog: cannot locate self: {{err}}"); std::process::exit(1); }}
            }};
            let status = match std::process::Command::new(&exe)
                .args(&args)
                .env("PSS_WATCHDOG_CHILD", "1")
                .status()
            {{
                Ok(s) => s,
                Err(err) => {{
                    eprintln!("watchdog: cannot relaunch: {{err}}");
                    std::process::exit(1);
                }}
            }};
            let code = status.code().unwrap_or(1);
            if code == 0 {{ std::process::exit(0); }}
            restarts += 1;
            if restarts > MAX_RESTARTS {{
                eprintln!("watchdog: program failed {{restarts}} time(s); giving up (max_auto_restarts={{MAX_RESTARTS}})");
                std::process::exit(code);
            }}
            eprintln!("watchdog: program exited abnormally (code {{code}}); restarting in {{SLEEP_SECS}}s ({{restarts}}/{{MAX_RESTARTS}})");
            std::thread::sleep(std::time::Duration::from_secs(SLEEP_SECS as u64));
        }}
    }}

    let _ = PS_ARGS.set(args);
    let code = unsafe {{ ps_program_main() }};
    std::process::exit(code as i32);
}}
"#,
        RUNTIME_SRC = runtime_src(),
        WD_AR = wd_ar,
        WD_MAX = wd_max,
        WD_SLEEP = wd_sleep,
    )
}

/// Link options for producing a native executable.
///
/// With no options set, `rustc` links for the host (the plain build path used
/// by `pssc --file-exe` on its own platform). Setting `rust_triple` +
/// `zig_target` cross-compiles: `rustc --target <triple>` with `zig cc`
/// (through the bundled `zigcc` shim) as the linker, which yields real native
/// ELF/COFF/Mach-O-free binaries for the requested architecture.
#[derive(Debug, Clone, Default)]
pub struct LinkOpt {
    /// Rust target triple for `--target`; `None` = host compilation.
    pub rust_triple: Option<String>,
    /// Zig `-target` string for `zig cc`; `None` = use rustc's default linker.
    pub zig_target: Option<String>,
}

impl LinkOpt {
    pub fn host() -> LinkOpt {
        LinkOpt::default()
    }
}

/// Locate the `zigcc` shim that re-invokes `zig cc` (zig 0.17+ requires the
/// explicit `cc` subcommand; rustc itself invokes the linker with no
/// subcommand, so the shim is required for cross-linking).
///
/// Resolution order: `PSS_ZIGCC` env var, the `pss` executable's directory,
/// then `PATH`.
fn find_zigcc() -> Option<String> {
    if let Ok(p) = std::env::var("PSS_ZIGCC") {
        if !p.is_empty() {
            return Some(p);
        }
    }
    let name = if cfg!(windows) { "zigcc.exe" } else { "zigcc" };
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let cand = dir.join(name);
            if cand.is_file() {
                return Some(cand.to_string_lossy().to_string());
            }
        }
    }
    if let Ok(path) = std::env::var("PATH") {
        for d in std::env::split_paths(&path) {
            let cand = d.join(name);
            if cand.is_file() {
                return Some(cand.to_string_lossy().to_string());
            }
        }
    }
    None
}

/// Link a native executable from compiled bytecode (host build).
///
/// Returns the path of the produced executable.
pub fn link_native_executable(
    bytes: &[u8],
    out_path: &str,
    watchdog: Option<&crate::project::Watchdog>,
) -> Result<String, String> {
    link_native_executable_opt(bytes, out_path, &LinkOpt::host(), watchdog)
}

/// Link a native executable from compiled bytecode, optionally cross-compiled
/// via `rustc --target` + `zig cc`.
///
/// Returns the path of the produced executable.
pub fn link_native_executable_opt(
    bytes: &[u8],
    out_path: &str,
    opt: &LinkOpt,
    watchdog: Option<&crate::project::Watchdog>,
) -> Result<String, String> {
    // 1. Build the driver source.
    let driver = build_driver_source(bytes, watchdog);

    // 2. Write it to a temporary file in the same directory as the output.
    let driver_dir = std::path::Path::new(out_path)
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let driver_path = driver_dir.join("__pss_driver.rs");
    std::fs::write(&driver_path, &driver)
        .map_err(|e| format!("cannot write driver: {e}"))?;

    // 3. Invoke rustc to produce the native executable.
    let cross = opt.rust_triple.is_some() || opt.zig_target.is_some();
    let mut cmd = Command::new("rustc");
    cmd.arg("--edition=2021")
        .arg("-O")
        .arg("-C")
        .arg("codegen-units=1");
    if let Some(triple) = &opt.rust_triple {
        cmd.arg("--target").arg(triple);
    }
    if let Some(zig) = &opt.zig_target {
        let shim = find_zigcc().ok_or(
            "cross-compilation requires the `zigcc` shim and `zig`; set PSS_ZIGCC \
             or put zigcc(.exe) next to `pss` (e.g. in a dist/ tools directory)",
        )?;
        cmd.arg("-C").arg(format!("linker={shim}"));
        cmd.arg("-C").arg("link-arg=-target").arg("-C").arg(format!("link-arg={zig}"));
    }
    let status = cmd
        .arg(&driver_path)
        .arg("-o")
        .arg(out_path)
        .status()
        .map_err(|e| format!("failed to invoke `rustc`: {e} (is the Rust toolchain on PATH?)"))?;

    if !status.success() && !cross {
        // Retry without extra flags on failure for broader toolchain tolerance.
        // (Cross builds must keep the target/linker flags, so no retry.)
        let status2 = Command::new("rustc")
            .arg(&driver_path)
            .arg("-o")
            .arg(out_path)
            .status()
            .map_err(|e| format!("failed to invoke `rustc`: {e}"))?;
        if !status2.success() {
            return Err("`rustc` failed to compile the driver; see error above".into());
        }
    } else if !status.success() {
        return Err("`rustc` failed to cross-compile the driver; see error above".into());
    }

    // 4. Clean up the temporary driver source.
    let _ = std::fs::remove_file(&driver_path);

    Ok(out_path.to_string())
}

/// Build the Rust driver source embedding the bytecode and the VM runtime.
fn build_driver_source(bytes: &[u8], watchdog: Option<&crate::project::Watchdog>) -> String {
    // Emit the bytecode as a byte-array literal.
    let mut lit = String::with_capacity(bytes.len() * 6 + 16);
    lit.push('[');
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 {
            lit.push(',');
            if i % 24 == 0 {
                lit.push('\n');
            }
        }
        lit.push_str(&format!("{}u8", b));
    }
    lit.push(']');

    let (wd_ar, wd_max, wd_sleep) = match watchdog {
        Some(wd) => (wd.auto_restart, wd.max_auto_restarts, wd.auto_restarts_sleep_time),
        None => (false, 0, 0),
    };

    format!(
        r#"// Auto-generated Pointerses native driver.
#![allow(dead_code)]
mod runtime {{
{RUNTIME_SRC}
}}

mod errorscreen {{
{ERRORSCREEN_SRC}
}}

fn run_once(prog: &runtime::Program, args: &[String]) -> i32 {{
    match runtime::run_program(prog, args) {{
        Ok(c) => c as i32,
        Err(e) => {{
            eprintln!("pointerses runtime error: {{e}}");
            errorscreen::show_error(&format!("运行出错：{{e}}"));
            1
        }}
    }}
}}

fn main() {{
    let code: &[u8] = &{LIT};
    let args: Vec<String> = std::env::args().skip(1).collect();
    let prog = match runtime::decode_program(code) {{
        Ok(p) => p,
        Err(e) => {{
            eprintln!("pointerses runtime: {{e}}");
            errorscreen::show_error(&format!("无法加载程序字节码：{{e}}"));
            std::process::exit(2);
        }}
    }};

    // Watchdog: relaunch this executable as a real child process whenever it
    // exits abnormally (non-zero), up to the configured budget.
    const AUTO_RESTART: bool = {WD_AR};
    const MAX_RESTARTS: u32 = {WD_MAX};
    const SLEEP_SECS: u32 = {WD_SLEEP};
    let is_child = std::env::var("PSS_WATCHDOG_CHILD").is_ok();
    if AUTO_RESTART && !is_child {{
        let mut restarts: u32 = 0;
        loop {{
            let exe = match std::env::current_exe() {{
                Ok(e) => e,
                Err(err) => {{ eprintln!("watchdog: cannot locate self: {{err}}"); std::process::exit(1); }}
            }};
            let status = match std::process::Command::new(&exe)
                .args(&args)
                .env("PSS_WATCHDOG_CHILD", "1")
                .status()
            {{
                Ok(s) => s,
                Err(err) => {{
                    eprintln!("watchdog: cannot relaunch: {{err}}");
                    std::process::exit(1);
                }}
            }};
            let code = status.code().unwrap_or(1);
            if code == 0 {{ std::process::exit(0); }}
            restarts += 1;
            if restarts > MAX_RESTARTS {{
                eprintln!("watchdog: program failed {{restarts}} time(s); giving up (max_auto_restarts={{MAX_RESTARTS}})");
                std::process::exit(code);
            }}
            eprintln!("watchdog: program exited abnormally (code {{code}}); restarting in {{SLEEP_SECS}}s ({{restarts}}/{{MAX_RESTARTS}})");
            std::thread::sleep(std::time::Duration::from_secs(SLEEP_SECS as u64));
        }}
    }}

    std::process::exit(run_once(&prog, &args));
}}
"#,
        RUNTIME_SRC = runtime_src(),
        ERRORSCREEN_SRC = ERRORSCREEN_SRC,
        LIT = lit,
        WD_AR = wd_ar,
        WD_MAX = wd_max,
        WD_SLEEP = wd_sleep,
    )
}
