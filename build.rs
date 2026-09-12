//! Bundles the LLVM runtime next to `pss` so the `llvm` backend is fully
//! self-contained at runtime — it does not need a separate LLVM install. `pss`
//! finds the library alongside its own executable first, then falls back to
//! `PSS_LLVM_DIR` / `PATH`.
//!
//! The source is `libs/LLVM-C.dll` vendored in this repository: that is the only
//! required location, so no machine-specific install path is hardcoded. A host
//! without it can point `PSS_LLVM_DIR` (or `PATH`) at any LLVM install. On
//! Windows the DLL only depends on standard system libraries and loads on any
//! Windows host. On Linux the LLVM-C library (`libLLVM-C.so`) is normally
//! provided by the distro package, so this script bundles nothing and `pss`
//! locates it at runtime via its search paths. macOS is intentionally not
//! supported.

fn main() {
    #[cfg(target_os = "windows")]
    bundle_llvm_windows();

    // Resolved at build time: `.cargo/config.toml` cannot express this portably.
    link_mingw_import_libs();
}

/// Emit `-L` for the mingw import libraries of the target being compiled.
///
/// Cross sysroots ship only crt2.o/dllcrt2.o, so zig needs the per-target
/// rustlib dir to resolve `-lmsvcrt` (libmsvcrt.a and friends). This used to be
/// a hardcoded path in `.cargo/config.toml`, but cargo does not expand
/// `${env:VAR}` inside `rustflags`, so it is looked up from the toolchain here
/// instead. Applies to the lib and to every bin of the package.
fn link_mingw_import_libs() {
    let Ok(target) = std::env::var("TARGET") else { return; };
    let Ok(host) = std::env::var("HOST") else { return; };
    let Ok(rustc) = std::env::var("RUSTC") else { return; };

    // Only for cross-compiling to a windows-gnu target: the host triple must be
    // skipped (it resolves its own CRT), and on Linux hosts the import libs are
    // ordinary shared objects that zig resolves on its own.
    if target == host {
        return;
    }
    if !(target.contains("windows") && target.contains("gnu")) {
        return;
    }

    let Ok(out) = std::process::Command::new(rustc)
        .args(["--print", "sysroot"])
        .output()
    else {
        return;
    };
    if !out.status.success() {
        return;
    }
    let sysroot = std::str::from_utf8(&out.stdout).unwrap_or("").trim().to_string();
    if sysroot.is_empty() {
        return;
    }

    let dir = std::path::Path::new(&sysroot)
        .join("lib")
        .join("rustlib")
        .join(target)
        .join("lib");
    if !dir.is_dir() {
        return;
    }

    // Note: `cargo:rustc-link-arg=bin=...` is not parseable — cargo splits the
    // value on the first `=` and forwards `bin=-L...` to the linker verbatim,
    // which zig rejects. The plain form reaches every link of the package.
    println!("cargo:rerun-if-env-changed=RUSTC");
    println!("cargo:rustc-link-arg=-L{}", dir.display());
}

#[cfg(target_os = "windows")]
fn bundle_llvm_windows() {
    println!("cargo:rerun-if-changed=build.rs");

    // Resolution order — no hardcoded install paths:
    //   1. libs/LLVM-C.dll vendored in this repository (the only required place)
    //   2. PSS_LLVM_DIR/bin and PSS_LLVM_DIR/lib
    //   3. every directory on PATH
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    candidates.push(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("libs").join("LLVM-C.dll"),
    );
    if let Ok(dir) = std::env::var("PSS_LLVM_DIR") {
        let dir = std::path::Path::new(&dir);
        candidates.push(dir.join("bin").join("LLVM-C.dll"));
        candidates.push(dir.join("lib").join("LLVM-C.dll"));
    }
    if let Ok(path) = std::env::var("PATH") {
        for p in std::env::split_paths(&path) {
            candidates.push(p.join("LLVM-C.dll"));
        }
    }

    let Some(src) = candidates.iter().find(|c| c.is_file()) else {
        println!(
            "cargo:warning=LLVM-C.dll not found in libs/, PSS_LLVM_DIR or PATH; \
             pss will keep searching for it at run time"
        );
        return;
    };
    println!("cargo:rerun-if-changed={}", src.display());

    // OUT_DIR = <target>/<profile>/build/<pkg>/out -> 3 pops = <target>/<profile>,
    // which is exactly where pss.exe is emitted.
    let Ok(out_dir) = std::env::var("OUT_DIR") else {
        return;
    };
    let mut exe_dir = std::path::PathBuf::from(&out_dir);
    exe_dir.pop();
    exe_dir.pop();
    exe_dir.pop();
    let dest = exe_dir.join("LLVM-C.dll");
    match std::fs::copy(src, &dest) {
        Ok(_) => println!("bundled LLVM-C.dll -> {}", dest.display()),
        Err(e) => println!("cargo:warning=could not bundle LLVM-C.dll next to pss.exe: {e}"),
    }
}
