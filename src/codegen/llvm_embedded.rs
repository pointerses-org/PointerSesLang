//! Embedded LLVM driver for the `llvm` backend.
//!
//! The `llvm` backend lowers the emitted LLVM driver IR to a native object
//! *inside* `pss` — the LLVM-C shared library is loaded at runtime and driven
//! through its C API. This means the backend needs no external `clang`, no
//! MSVC headers, and no fixed LLVM install: as long as the LLVM-C library sits
//! next to `pss` (or is found via `PSS_LLVM_DIR` / `PATH` / well-known paths),
//! `pss build --backend llvm` produces a real native executable.
//!
//! Cross-platform loading:
//!   * Windows — `LLVM-C.dll` via Win32 `LoadLibraryW`/`GetProcAddress`.
//!   * Linux   — `libLLVM-C.so` via `dlopen`/`dlsym`/`dlerror` from libc
//!                (no third-party deps). macOS is intentionally not supported.
//!
//! Only the small set of LLVM-C functions needed to parse LLVM IR and emit a
//! native object are resolved; the complete VM runtime is linked in afterwards
//! via `rustc` (see [`super::native::link_llvm_executable`]), giving full
//! language coverage (pointers, non-blocking refcounts, closures, collections
//! and real worker-thread concurrency).

use std::ffi::{c_char, c_void, CStr, CString};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

// ---------------------------------------------------------------------------
// Platform constants
// ---------------------------------------------------------------------------

/// Name of the bundled LLVM-C shared library.
#[cfg(windows)]
const LLVM_LIB: &str = "LLVM-C.dll";
#[cfg(not(windows))]
const LLVM_LIB: &str = "libLLVM-C.so";

/// Target triple the emitted object must be built for (matches the toolchain
/// used to link the final executable).
#[cfg(windows)]
const TRIPLE: &str = "x86_64-pc-windows-gnu";
#[cfg(not(windows))]
const TRIPLE: &str = "x86_64-unknown-linux-gnu";

/// Native object file extension produced by the LLVM target machine.
#[cfg(windows)]
const OBJ_EXT: &str = "obj";
#[cfg(not(windows))]
const OBJ_EXT: &str = "o";

/// The host target triple, as a string usable in emitted LLVM IR.
pub fn target_triple() -> &'static str {
    TRIPLE
}

/// The host native-object file extension (without the leading dot).
pub fn object_extension() -> &'static str {
    OBJ_EXT
}

/// File type selector for `LLVMTargetMachineEmitToFile` (object, not text).
const LLVM_OBJECT_FILE: i32 = 1;
/// `LLVMCodeGenOptLevel::LLVMCodeGenLevelDefault`.
const OPT_DEFAULT: i32 = 2;
/// `LLVMRelocMode::LLVMRelocDefault`.
const RELOC_DEFAULT: i32 = 0;
/// `LLVMCodeModel::LLVMCodeModelDefault`.
const CODE_MODEL_DEFAULT: i32 = 0;

type HModule = *mut c_void;

// ---------------------------------------------------------------------------
// Platform shared-library loading
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod platform {
    use super::*;

    extern "system" {
        fn LoadLibraryW(name: *const u16) -> HModule;
        fn GetProcAddress(h: HModule, name: *const c_char) -> *mut c_void;
    }

    /// Load a shared library by absolute path.
    pub fn load(path: &Path) -> Result<HModule, String> {
        let wpath: Vec<u16> = path
            .to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let h = unsafe { LoadLibraryW(wpath.as_ptr()) };
        if h.is_null() {
            Err(format!("failed to load `{}`", path.display()))
        } else {
            Ok(h)
        }
    }

    /// Resolve one exported symbol from the loaded library.
    pub fn sym(h: HModule, name: &str) -> Result<*mut c_void, String> {
        let c = CString::new(name).map_err(|_| format!("bad symbol name `{name}`"))?;
        let p = unsafe { GetProcAddress(h, c.as_ptr()) };
        if p.is_null() {
            Err(format!("missing symbol `{name}`"))
        } else {
            Ok(p)
        }
    }
}

#[cfg(unix)]
mod platform {
    use super::*;

    #[cfg_attr(target_os = "linux", link(name = "dl"))]
    extern "C" {
        fn dlopen(filename: *const c_char, flags: i32) -> HModule;
        fn dlsym(h: HModule, name: *const c_char) -> *mut c_void;
        fn dlerror() -> *mut c_char;
    }

    /// `RTLD_NOW` (2) on Linux.
    const RTLD_NOW: i32 = 2;

    /// Load a shared library by absolute path.
    pub fn load(path: &Path) -> Result<HModule, String> {
        let bytes = path.to_string_lossy().as_bytes().to_vec();
        let c = CString::new(bytes)
            .map_err(|_| format!("path contains a NUL byte: {}", path.display()))?;
        let h = unsafe { dlopen(c.as_ptr(), RTLD_NOW) };
        if h.is_null() {
            let msg = unsafe { CStr::from_ptr(dlerror()) }.to_string_lossy();
            Err(format!("failed to load `{}`: {msg}", path.display()))
        } else {
            Ok(h)
        }
    }

    /// Resolve one exported symbol from the loaded library.
    pub fn sym(h: HModule, name: &str) -> Result<*mut c_void, String> {
        let c = CString::new(name).map_err(|_| format!("bad symbol name `{name}`"))?;
        // Clear any stale error before the lookup so a null result is a real miss.
        unsafe {
            dlerror();
        }
        let p = unsafe { dlsym(h, c.as_ptr()) };
        if p.is_null() {
            Err(format!("missing symbol `{name}`"))
        } else {
            Ok(p)
        }
    }
}

/// Keep the loaded library alive for the whole process (we never unload it).
/// Stored as `usize` (not a raw pointer) so the static is `Send + Sync`.
static HANDLE: OnceLock<usize> = OnceLock::new();

/// Locate the LLVM-C shared library.
///
/// Resolution order: alongside `pss`, `PSS_LLVM_DIR/bin` and `PSS_LLVM_DIR/lib`,
/// well-known platform install dirs (Linux), then every directory on `PATH`.
///
/// There is deliberately no hardcoded install path on Windows: `build.rs` copies
/// the vendored `libs/LLVM-C.dll` next to the built tools, so rule 1 covers it.
fn locate_library() -> Result<PathBuf, String> {
    let mut candidates: Vec<PathBuf> = Vec::new();

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join(LLVM_LIB));
        }
    }
    if let Ok(dir) = std::env::var("PSS_LLVM_DIR") {
        let dir = PathBuf::from(dir);
        candidates.push(dir.join("bin").join(LLVM_LIB));
        candidates.push(dir.join("lib").join(LLVM_LIB));
    }

    #[cfg(target_os = "linux")]
    {
        candidates.push(PathBuf::from("/usr/lib/x86_64-linux-gnu").join(LLVM_LIB));
        candidates.push(PathBuf::from("/usr/local/lib").join(LLVM_LIB));
        candidates.push(PathBuf::from("/usr/lib").join(LLVM_LIB));
    }
    if let Ok(path) = std::env::var("PATH") {
        for p in std::env::split_paths(&path) {
            candidates.push(p.join(LLVM_LIB));
        }
    }

    for c in &candidates {
        if c.is_file() {
            return Ok(c.clone());
        }
    }
    Err(format!(
        "cannot find {LLVM_LIB} (the `llvm` backend is bundled with it). \
         Put it next to `pss` or set PSS_LLVM_DIR=<dir containing bin/ or lib/{LLVM_LIB}>. \
         Looked in: {}",
        candidates
            .iter()
            .map(|c| c.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

/// The resolved LLVM-C function pointers used to compile IR -> native object.
struct Llvm {
    init_target: unsafe extern "C" fn() -> i32,
    init_target_info: unsafe extern "C" fn() -> i32,
    init_target_mc: unsafe extern "C" fn() -> i32,
    init_asm_parser: unsafe extern "C" fn() -> i32,
    init_asm_printer: unsafe extern "C" fn() -> i32,
    context_create: unsafe extern "C" fn() -> *mut c_void,
    membuf: unsafe extern "C" fn(*const c_char, usize, *const c_char) -> *mut c_void,
    parse_ir: unsafe extern "C" fn(*mut c_void, *mut c_void, *mut *mut c_void, *mut *mut c_char) -> i32,
    get_target: unsafe extern "C" fn(*const c_char, *mut *mut c_void, *mut *mut c_char) -> i32,
    create_tm: unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char, *const c_char, i32, i32, i32) -> *mut c_void,
    emit_to_file: unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_char, i32, *mut *mut c_char) -> i32,
    dispose_tm: unsafe extern "C" fn(*mut c_void),
    dispose_module: unsafe extern "C" fn(*mut c_void),
}

fn load() -> Result<Llvm, String> {
    // Only load once; cache the handle so the library stays resident.
    if let Some(h) = HANDLE.get() {
        return resolve(*h as HModule);
    }
    let lib = locate_library()?;
    let handle = platform::load(&lib)?;
    let _ = HANDLE.set(handle as usize);
    resolve(handle)
}

fn resolve(h: HModule) -> Result<Llvm, String> {
    macro_rules! get {
        ($n:literal) => {
            unsafe { std::mem::transmute::<*mut c_void, _>(platform::sym(h, $n)?) }
        };
    }
    Ok(Llvm {
        init_target: get!("LLVMInitializeX86Target"),
        init_target_info: get!("LLVMInitializeX86TargetInfo"),
        init_target_mc: get!("LLVMInitializeX86TargetMC"),
        init_asm_parser: get!("LLVMInitializeX86AsmParser"),
        init_asm_printer: get!("LLVMInitializeX86AsmPrinter"),
        context_create: get!("LLVMContextCreate"),
        membuf: get!("LLVMCreateMemoryBufferWithMemoryRangeCopy"),
        parse_ir: get!("LLVMParseIRInContext"),
        get_target: get!("LLVMGetTargetFromTriple"),
        create_tm: get!("LLVMCreateTargetMachine"),
        emit_to_file: get!("LLVMTargetMachineEmitToFile"),
        dispose_tm: get!("LLVMDisposeTargetMachine"),
        dispose_module: get!("LLVMDisposeModule"),
    })
}

/// Turn an LLVM-C `char**` error message into a Rust string (empty on null).
fn read_message(p: *mut c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

/// Compile the given LLVM IR text to a native object file.
///
/// This runs entirely inside `pss` via the loaded LLVM-C library — no external
/// `clang` is spawned and no C headers are required.
pub fn emit_object(ir: &str, obj_path: &str) -> Result<(), String> {
    let ll = load()?;

    // 1. Initialize the X86 backend (we always target x86_64).
    unsafe {
        (ll.init_target_info)();
        (ll.init_target)();
        (ll.init_target_mc)();
        (ll.init_asm_parser)();
        (ll.init_asm_printer)();
    }

    // 2. Parse the IR text into an in-memory module.
    let ctx = unsafe { (ll.context_create)() };
    if ctx.is_null() {
        return Err("LLVMContextCreate failed".into());
    }
    let name = CString::new("ps_driver.ll").unwrap();
    let buf = unsafe { (ll.membuf)(ir.as_ptr() as *const c_char, ir.len(), name.as_ptr()) };
    if buf.is_null() {
        return Err("LLVMCreateMemoryBufferWithMemoryRangeCopy failed".into());
    }
    let mut module: *mut c_void = std::ptr::null_mut();
    let mut err: *mut c_char = std::ptr::null_mut();
    let r = unsafe { (ll.parse_ir)(ctx, buf, &mut module, &mut err) };
    if r != 0 {
        let m = read_message(err);
        return Err(format!("LLVM failed to parse the driver IR: {m}"));
    }
    // NOTE: a successful parse takes ownership of the memory buffer, so we must
    // NOT dispose `buf` here (doing so double-frees). We also skip explicit
    // module/target-machine disposal: `pss build` is a short-lived process and
    // the small leak is not worth risking an access violation in dispose paths.

    // 3. Select the host target and create a target machine.
    let triple = CString::new(TRIPLE).unwrap();
    let mut target: *mut c_void = std::ptr::null_mut();
    let mut terr: *mut c_char = std::ptr::null_mut();
    let r = unsafe { (ll.get_target)(triple.as_ptr(), &mut target, &mut terr) };
    if r != 0 {
        let m = read_message(terr);
        return Err(format!("LLVM could not find target `{TRIPLE}`: {m}"));
    }
    let cpu = CString::new("").unwrap();
    let features = CString::new("").unwrap();
    let tm = unsafe {
        (ll.create_tm)(
            target,
            triple.as_ptr(),
            cpu.as_ptr(),
            features.as_ptr(),
            OPT_DEFAULT,
            RELOC_DEFAULT,
            CODE_MODEL_DEFAULT,
        )
    };
    if tm.is_null() {
        return Err("LLVMCreateTargetMachine failed".into());
    }

    // 4. Emit the module as a native object file.
    let out = CString::new(obj_path).map_err(|_| "object path contains a NUL byte".to_string())?;
    let mut oerr: *mut c_char = std::ptr::null_mut();
    let r = unsafe {
        (ll.emit_to_file)(tm, module, out.as_ptr() as *mut c_char, LLVM_OBJECT_FILE, &mut oerr)
    };
    if r != 0 {
        let m = read_message(oerr);
        return Err(format!("LLVM failed to emit the native object: {m}"));
    }

    unsafe {
        (ll.dispose_tm)(tm);
        (ll.dispose_module)(module);
    }
    Ok(())
}