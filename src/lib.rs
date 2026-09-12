//! Pointerses compiler toolchain library.
//!
//! Shared front-end and back-end used by all four binaries:
//!   * `pss`  — the runner (runs `.project` packages and `.psp` source).
//!   * `pssc` — the packager (produces `.project` / native executables).
//!   * `pssp` — the decompiler (`.project` -> source / bytecode listing).
//!   * `pssl` — the dynamic-library packager (whole project -> `.psdl`).

#![allow(dead_code)]

pub mod arch;
pub mod bootstrap;
pub mod cli;
pub mod codegen;
pub mod compiler;
pub mod concurrency;
pub mod daemon;
#[path = "../runtime/errorscreen.rs"]
pub mod errorscreen;
pub mod ffi;
pub mod lexer;
pub mod packaging;
pub mod parser;
pub mod project;
pub mod semantic;
pub mod vm;
pub mod watchdog;