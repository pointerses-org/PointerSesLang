//! `pssp` — the Pointerses decompiler.
//!
//! Reads a `.project` package and restores either the original `.psp` source
//! (`--out-psp`) or a human-readable bytecode listing (`--out-list`). With no
//! output flag the bytecode listing is printed to stdout.

use std::process::ExitCode;

use pointerses::arch;
use pointerses::codegen::bytecode;
use pointerses::packaging;

fn main() -> ExitCode {
    let full: Vec<String> = std::env::args().collect();
    if matches!(full.get(1).map(|s| s.as_str()), Some("-v") | Some("--version")) {
        println!("pssp {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(code) => ExitCode::from(code),
        Err(msg) => {
            eprintln!("pssp: error: {msg}");
            ExitCode::from(2)
        }
    }
}

fn run(args: &[String]) -> Result<u8, String> {
    // `--fram-*` flags are accepted for interface consistency; they do not change
    // decompilation but let a caller verify the package's target architecture.
    let (_fram, rest) = arch::parse_fram(args);

    let mut file: Option<String> = None;
    let mut out_psp: Option<String> = None;
    let mut out_list: Option<String> = None;

    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--out-psp" => {
                i += 1;
                out_psp = Some(rest.get(i).ok_or("missing value for --out-psp")?.clone());
            }
            "--out-list" => {
                i += 1;
                out_list = Some(rest.get(i).ok_or("missing value for --out-list")?.clone());
            }
            s if s.starts_with('-') => return Err(format!("unknown option `{s}`")),
            s => {
                if file.is_none() {
                    file = Some(s.to_string());
                } else {
                    return Err("more than one input file given".into());
                }
            }
        }
        i += 1;
    }
    let file = file.ok_or("missing input file: `pssp <file.project>`")?;

    let data = std::fs::read(&file).map_err(|e| format!("cannot read `{file}`: {e}"))?;
    let pkg = packaging::Package::deserialize(&data)?;
    println!(
        "{}: source `{}`, arch: {}, {} bytes bytecode",
        file,
        pkg.source_name,
        pkg.arch_label(),
        pkg.bytecode.len()
    );

    let mut wrote_any = false;
    if let Some(p) = out_psp {
        std::fs::write(&p, &pkg.source).map_err(|e| format!("cannot write source: {e}"))?;
        println!("wrote source to `{p}`");
        wrote_any = true;
    }
    if let Some(l) = out_list {
        let list = bytecode::disassemble(&pkg.bytecode);
        std::fs::write(&l, &list).map_err(|e| format!("cannot write listing: {e}"))?;
        println!("wrote bytecode listing to `{l}`");
        wrote_any = true;
    }
    if !wrote_any {
        println!("--- bytecode listing ---");
        print!("{}", bytecode::disassemble(&pkg.bytecode));
    }
    Ok(0)
}
