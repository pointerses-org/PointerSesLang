//! `zigcc` — minimal wrapper turning `zig` into a C-compiler-driver invocation.
//!
//! rustc invokes the configured linker as `<linker> <args...>` with no room for
//! a subcommand. Zig 0.17+ no longer treats a bare `zig` invocation as `zig cc`,
//! so this shim re-invokes `zig cc <args...>`. It is used both by cargo (via
//! `.cargo/config.toml` `linker = "zigcc"`) and by `pssc` when cross-compiling
//! native executables (which calls `zig cc` directly through the same lookup).
//!
//! rustc passes some target-default linker flags that the zig 0.17-dev `lld`
//! driver rejects (`--fix-cortex-a53-843419` on aarch64-linux,
//! `--large-address-aware` on windows-gnu); those are dropped. rustc also hands
//! a `.def` export-list file to Windows-GNU DLL links as `-Wl,<path.def>`; zig
//! rejects that wrapped form, so it is rewritten to a bare input path (zig's
//! lld accepts `.def` files as linker inputs).

use std::process::Command;

/// Flags that must be dropped when they arrive as `-Wl,<flag>` (the form rustc
/// uses). Kept as a small explicit list so unknown flags still pass through.
///
/// * `--fix-cortex-a53-843419` — rejected by zig's lld on aarch64-linux.
/// * `--large-address-aware`   — rejected by zig's lld on windows-gnu.
/// * `--dynamicbase` / `--disable-auto-image-base` — zig 0.16 lld reports
///   "auto-image-base options are unimplemented and ignored" (harmless layout
///   noise; zig enables ASLR by default).
/// * `-O1` — rustc's linker-optimization hint; zig 0.16 lld warns "ignoring
///   deprecated linker optimization setting '1'". Zig applies its own default
///   optimizations, so the flag can be dropped.
const DROP_AS_WL: &[&str] = &[
    "--fix-cortex-a53-843419",
    "--large-address-aware",
    "--dynamicbase",
    "--disable-auto-image-base",
    "-O1",
];

fn should_drop(arg: &str) -> bool {
    let flag = match arg.strip_prefix("-Wl,") {
        Some(wl) => wl,
        None => arg,
    };
    DROP_AS_WL.iter().any(|f| flag == *f || flag.starts_with(&format!("{f}=")))
}

/// Windows-GNU cross targets (i686 / aarch64). Against the fully self-contained
/// rust host sysroot these link only if `-nodefaultlibs` is set: without it zig
/// treats the explicit `-lmsvcrt` as a "dynamic system library" under its
/// `no_fallback` strategy and only searches for `msvcrt.dll`, never falling back
/// to the `libmsvcrt.a` import lib supplied on `-L`. rustc already passes every
/// needed library explicitly, so suppressing zig's default libs is safe here.
/// Read the `-target <name>` value, if any.
fn target_of(args: &[String]) -> String {
    let mut iter = args.iter();
    while let Some(a) = iter.next() {
        if a == "-target" {
            if let Some(t) = iter.next() {
                return t.clone();
            }
        }
    }
    String::new()
}

/// i686 windows-gnu: rustc emits `-nodefaultlibs` there and `-lmsvcrt` cannot be
/// resolved (zig 0.16 ships `.def` files for mingw and generates import libs on
/// demand); `-lc` selects zig's bundled windows libc instead.
fn is_i686_win(target: &str) -> bool {
    target.starts_with("x86-") && target.contains("windows")
}

/// aarch64 windows-gnullvm: rustc passes `-nolibc --unwindlib=none` plus an
/// explicit `-lmsvcrt`, which fail under zig's no-fallback libc resolution.
/// Dropping them lets zig supply its default mingw libc (which works).
fn is_arm64_win(target: &str) -> bool {
    target.starts_with("aarch64-") && target.contains("windows")
}

fn main() {
    // Filter flags; rewrite `-Wl,<path.def>` to a bare input path.
    let mut args: Vec<String> = Vec::new();
    for a in std::env::args().skip(1) {
        if should_drop(&a) {
            continue;
        }
        if let Some(wl) = a.strip_prefix("-Wl,") {
            if wl.to_ascii_lowercase().ends_with(".def") {
                args.push(wl.to_string());
                continue;
            }
        }
        args.push(a);
    }

    let target = target_of(&args);
    let i686 = is_i686_win(&target);
    let arm64 = is_arm64_win(&target);
    let linux = target.contains("linux");
    // 64-bit Linux targets only: statically link with musl. zig 0.16 supports
    // only dynamic glibc (it rejects `-static` for glibc), so the `-target` is
    // retargeted to `*-linux-musl` and `-static` is added. rustc's GNU std
    // objects reference a few glibc-only symbols (`open64`/`fstat64`/`mmap64`/
    // `stat64`/`lseek64`/`gnu_get_libc_version`/`__res_init`) that musl lacks;
    // those are satisfied by tools/musl_shim_<arch>.o. The resulting binaries
    // carry no dynamic-linker dependency and run on systems without glibc —
    // Android Termux (bionic), minimal containers, etc. PIE flags are removed:
    // plain static (non-PIE) links are the most portable. (i686-linux keeps the
    // dynamic glibc link: its 32-bit `stat64` ABI would need a separate shim.)
    let linux64 = linux && (target.starts_with("aarch64-") || target.starts_with("x86_64-"));

    // gui.rs declares `#[link(name = "X11")]`, so every Linux link carries
    // `-lX11`. zig 0.16 resolves `-l<name>` through its own system-library
    // search (raw-dylibs/rustlib) and never consults `-L` paths, so a build
    // machine without libX11 dies with `unable to find dynamic system library
    // 'X11'`. The `-L x11_stub/` path added below is therefore not enough on
    // its own; the `-lX11` itself is rewritten per link kind in the block after
    // the musl retarget below: static executables bake the no-op `libX11.o`
    // (GUI fails gracefully), dynamic links use `-l:libX11.so` whose SONAME
    // `libX11.so.6` lets the loader bind the real libX11 at run time, and
    // shared libraries (cdylib) are left alone (zig records the `DT_NEEDED`
    // and defers the symbols to run time).
    if linux {
        let sub = if target.starts_with("aarch64-") {
            "aarch64-linux"
        } else if target.starts_with("i686-") || target.starts_with("x86-") {
            "i686-linux"
        } else {
            "x86_64-linux"
        };
        if let Some(dir) = std::env::current_exe()
            .ok()
            .and_then(|me| me.parent().map(|d| d.to_path_buf()))
        {
            let d = dir.join("x11_stub").join(sub);
            if d.is_dir() {
                // Insert before the first `-l` so ld has the search path when
                // it resolves `-lX11`.
                let flag = format!("-L{}", d.to_string_lossy());
                let at = args
                    .iter()
                    .position(|a| a.starts_with("-l"))
                    .unwrap_or(args.len());
                args.insert(at, flag);
            } else {
                eprintln!(
                    "zigcc: warning: missing {}/x11_stub/{sub}; -lX11 may fail to resolve",
                    dir.display()
                );
            }
        }
    }

    if linux64 {
        args.retain(|a| a != "-pie" && a != "-Wl,-pie" && a != "-Wl,-no-pie");
        let mut out: Vec<String> = Vec::new();
        let mut it = args.into_iter().peekable();
        while let Some(a) = it.next() {
            if a == "-target" {
                if let Some(t) = it.next() {
                    out.push(a);
                    out.push(t.replace("linux-gnu", "linux-musl"));
                    continue;
                }
            }
            out.push(a);
        }
        out.push("-static".to_string());
        // Inject the glibc-compat shim object (sibling of this shim).
        let shim = if target.starts_with("aarch64-") {
            "musl_shim_aarch64.o"
        } else {
            "musl_shim_x86_64.o"
        };
        if let Some(dir) = std::env::current_exe()
            .ok()
            .and_then(|me| me.parent().map(|d| d.to_path_buf()))
        {
            let s = dir.join(shim);
            if s.is_file() {
                out.push(s.to_string_lossy().into_owned());
            } else {
                eprintln!("zigcc: warning: missing {shim}; glibc-only symbols may remain undefined");
            }
        }
        args = out;
    }

    // Resolve `-lX11` per link kind (zig's `-l` resolution ignores `-L`, see
    // the comment above the `-L x11_stub` insertion).
    //
    // * shared library (cdylib, `-shared`): leave `-lX11` alone. For a `.so`
    //   zig records `DT_NEEDED libX11.so.6` and leaves the symbols undefined,
    //   so the real libX11 binds them at run time on a Linux host.
    // * static executable (`-static`, no `-shared` — the 64-bit musl bin): a
    //   static binary cannot carry undefined symbols, so inject the no-op stub
    //   `tools/x11_stub/<arch>/libX11.o` directly and drop `-lX11`. The 21
    //   stub bodies are baked in; GUI calls fail gracefully (`XOpenDisplay`
    //   returns 0 -> "cannot open X display") rather than crash. This is the
    //   trade-off of the portable static build: CLI everywhere, GUI only where
    //   a dynamic build can bind X11.
    // * dynamic executable (no `-static`, no `-shared` — i686-linux): replace
    //   `-lX11` with the stub `.so` as a direct input path. lld reads its
    //   symbol table and records the SONAME `libX11.so.6` as `DT_NEEDED`, so
    //   the real libX11 binds at run time and the GUI works on a Linux host.
    //   (`-l:libX11.so` would be idiomatic, but zig's lld won't find it even
    //   with `-L` set, so a direct path input is used instead.)
    if linux && !args.iter().any(|a| a == "-shared") {
        let sub = if target.starts_with("aarch64-") {
            "aarch64-linux"
        } else if target.starts_with("i686-") || target.starts_with("x86-") {
            "i686-linux"
        } else {
            "x86_64-linux"
        };
        if let Some(dir) = std::env::current_exe()
            .ok()
            .and_then(|me| me.parent().map(|d| d.to_path_buf()))
        {
            let stub_dir = dir.join("x11_stub").join(sub);
            if args.iter().any(|a| a == "-static") {
                // static executable: bake the no-op `libX11.o`, drop `-lX11`.
                args.retain(|a| a != "-lX11");
                let o = stub_dir.join("libX11.o");
                if o.is_file() {
                    args.push(o.to_string_lossy().into_owned());
                } else {
                    eprintln!(
                        "zigcc: warning: missing x11_stub/{sub}/libX11.o; X11 symbols may remain undefined"
                    );
                }
            } else {
                // dynamic executable: replace `-lX11` with the stub `.so` as a
                // direct input path (lld reads its symbol table and records the
                // SONAME `libX11.so.6` as DT_NEEDED; the real libX11 binds at
                // run time). The idiomatic `-l:libX11.so` form is not used:
                // zig's lld won't find it even with `-L` set.
                let so = stub_dir.join("libX11.so");
                if so.is_file() {
                    let so_str = so.to_string_lossy().into_owned();
                    for a in args.iter_mut() {
                        if a == "-lX11" {
                            *a = so_str.clone();
                        }
                    }
                } else {
                    eprintln!(
                        "zigcc: warning: missing x11_stub/{sub}/libX11.so; -lX11 may fail to resolve"
                    );
                }
            }
        }
    }

    // aarch64 windows-gnullvm: rustc passes `-nolibc --unwindlib=none` plus an
    // explicit `-lmsvcrt`. Under zig 0.16 those fail (`no_fallback` cannot
    // resolve msvcrt); dropping them lets zig's default mingw libc link it.
    if arm64 {
        args.retain(|a| a != "-nolibc" && a != "--unwindlib=none" && a != "-lmsvcrt");
    }

    // i686 windows-gnu: rustc emits `-nodefaultlibs` and explicit mingw libs.
    // zig 0.16 ships `.def` files for mingw and generates import libs on demand,
    // so a plain `-lmsvcrt` lookup fails (`no_fallback`/`paths_first`) while
    // `-lc` selects the bundled windows libc; and the `-l:libpthread.a`
    // exact-filename form crashes the x86 driver, so use `-lpthread` instead.
    if i686 {
        let mut out: Vec<String> = Vec::new();
        for a in args {
            if a == "-lmsvcrt" {
                out.push("-lc".to_string());
            } else if a == "-l:libpthread.a" {
                out.push("-lpthread".to_string());
            } else {
                out.push(a);
            }
        }
        args = out;
    }

    // i686-pc-windows-gnu uses Dwarf unwinding, so rust's `libstd` references
    // the mingw frame-registration symbols `___register_frame_info` /
    // `___deregister_frame_info`. zig/llvm use SEH for this target and do not
    // export them, so link the bundled no-op stubs (tools/dwarf_stub.o) that
    // satisfy the references. rustc also emits `-Wl,--gc-sections`, which would
    // garbage-collect those stub symbols away; drop it for this target so the
    // stubs survive the link (only i686 needs either fix).
    if i686 {
        args.retain(|a| a != "-Wl,--gc-sections");
        let stub = if let Some(dir) = std::env::current_exe()
            .ok()
            .and_then(|me| me.parent().map(|d| d.to_path_buf()))
        {
            let s = dir.join("dwarf_stub.o");
            s.is_file().then_some(s)
        } else {
            None
        };
        if let Some(s) = stub {
            args.push(s.to_string_lossy().into_owned());
        } else {
            eprintln!("zigcc: warning: missing dwarf_stub.o; i686 dwarf unwind symbols may remain undefined");
        }
    }

    // `zig` must be discoverable: on PATH, or via the `ZIG` environment
    // variable, or as a sibling `zig.exe` / `zig` next to this shim.
    let zig = std::env::var("ZIG").ok().filter(|p| !p.is_empty()).unwrap_or_else(|| {
        let sibling = std::env::current_exe()
            .ok()
            .and_then(|me| me.parent().map(|d| d.to_path_buf()))
            .map(|d| d.join("zig.exe"))
            .filter(|p| p.is_file())
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| "zig".to_string());
        sibling
    });
    if std::env::var("ZIGCC_DEBUG").is_ok() {
        use std::io::Write;
        let p = std::env::var("ZIGCC_LOG").unwrap_or_else(|_| "D:\\Pointerses\\target\\zigcc_debug.log".into());
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&p).unwrap();
        writeln!(f, "{}", args.join(" ")).unwrap();
    }
    let status = Command::new(&zig).arg("cc").args(&args).status();
    let code = match status {
        Ok(s) => s.code().unwrap_or(1),
        Err(e) => {
            eprintln!("zigcc: cannot run `{zig}`: {e}");
            1
        }
    };
    std::process::exit(code);
}