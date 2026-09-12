//! `pssl` — the Pointerses dynamic-library packager.
//!
//! Packages a whole project as a `.psdl` ("PointerSes Dymatic Library") file.
//! Only whole-project packaging is supported (no single-file packaging).

use std::process::ExitCode;

use pointerses::arch;
use pointerses::bootstrap;
use pointerses::{compiler, packaging, project};

fn main() -> ExitCode {
    let full: Vec<String> = std::env::args().collect();
    if matches!(full.get(1).map(|s| s.as_str()), Some("-v") | Some("--version")) {
        println!("pssl {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(code) => ExitCode::from(code),
        Err(msg) => {
            eprintln!("pssl: error: {msg}");
            ExitCode::from(2)
        }
    }
}

fn run(args: &[String]) -> Result<u8, String> {
    let (fram, rest) = arch::parse_fram(args);

    let mut dir: Option<String> = None;
    let mut out: Option<String> = None;
    let mut backend = "native".to_string();

    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "-o" | "--out" => {
                i += 1;
                out = Some(rest.get(i).ok_or("missing value for -o")?.clone());
            }
            "-b" | "--backend" => {
                i += 1;
                backend = rest.get(i).ok_or("missing value for -b")?.clone();
                if backend != "native" && backend != "llvm" {
                    return Err(format!("unknown backend `{backend}` (native|llvm)"));
                }
            }
            s if s.starts_with('-') => return Err(format!("unknown option `{s}`")),
            s => {
                if dir.is_none() {
                    dir = Some(s.to_string());
                } else {
                    return Err("more than one input directory given".into());
                }
            }
        }
        i += 1;
    }

    let dir = dir.unwrap_or_else(|| ".".to_string());
    let entry = resolve_project_entry(&dir)?;
    let compiled = compiler::compile_file(&entry, &backend)?;

    let out = out.unwrap_or_else(|| {
        // Default name from the project name, else the directory basename.
        let proj = project::load_for(&entry);
        if let Some(name) = proj.name {
            format!("{name}.psdl")
        } else {
            let base = std::path::Path::new(&dir)
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "project".to_string());
            format!("{base}.psdl")
        }
    });

    let meta_json = bootstrap::meta_to_json(&compiled.meta).into_bytes();
    let pkg = packaging::Package::new(fram, entry.clone(), compiled.source, compiled.bytes, meta_json);
    std::fs::write(&out, pkg.serialize(&packaging::PSDL_MAGIC))
        .map_err(|e| format!("cannot write library `{out}`: {e}"))?;
    println!(
        "packaged dynamic library `{out}` from project `{dir}` (arch: {})",
        pkg.arch_label()
    );
    Ok(0)
}

/// Find the project entry source in `dir`: `main.psp`, else a lone `.psp`.
fn resolve_project_entry(dir: &str) -> Result<String, String> {
    let dir_path = std::path::Path::new(dir);
    let main = dir_path.join("main.psp");
    if main.is_file() {
        return Ok(main.to_string_lossy().to_string());
    }
    let mut psp: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir_path) {
        for e in rd.flatten() {
            if e.path().extension().map(|x| x == "psp").unwrap_or(false) {
                psp.push(e.path());
            }
        }
    }
    match psp.len() {
        1 => Ok(psp[0].to_string_lossy().to_string()),
        0 => Err(format!("`{dir}`: no `.psp` source found")),
        _ => Err(format!(
            "`{dir}`: multiple `.psp` files and no `main.psp`; add a `main.psp` entry"
        )),
    }
}
