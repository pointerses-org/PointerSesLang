//! Shared front-end pipeline used by `pssc`, `pssp` and `pss`.
//!
//! `compile_file` runs the full `lex -> parse -> semantic -> codegen` chain and
//! returns everything the toolchain needs (bytecode, LLVM IR, listing and
//! bootstrap metadata) for a single source file.

use crate::bootstrap::{self, MetaBundle};
use crate::codegen;
use crate::ffi::{self, ExternDecl};
use crate::lexer;
use crate::parser::{self, Program};
use crate::project::{self, Project};
use crate::semantic::{self, TypedProgram};

/// The complete result of compiling one source file.
pub struct Compiled {
    pub file: String,
    pub source: String,
    pub program: Program,
    pub typed: TypedProgram,
    /// Serialized bytecode program (native backend).
    pub bytes: Vec<u8>,
    /// LLVM IR text (llvm backend) or empty.
    pub ir_text: String,
    /// Human-readable bytecode / IR listing.
    pub listing: String,
    pub meta: MetaBundle,
    pub proj: Project,
    pub externs: Vec<ExternDecl>,
}

/// Compile a `.psp` source file through the full front-end pipeline.
///
/// `backend` is one of `"native"` (bytecode VM) or `"llvm"`.
pub fn compile_file(file: &str, backend: &str) -> Result<Compiled, String> {
    let src = std::fs::read_to_string(file)
        .map_err(|e| format!("cannot read `{file}`: {e}"))?;
    compile_source(file, &src, backend, false)
}

/// Compile a `.psp` source file, honouring the `no-api-check` flag (either the
/// CLI `--no-api-check` or the `.pspc` `no-api-check: true` config). Used by
/// `pssc` for `.project` builds where a host Rust program provides the API.
pub fn compile_file_nac(file: &str, backend: &str) -> Result<Compiled, String> {
    let src = std::fs::read_to_string(file)
        .map_err(|e| format!("cannot read `{file}`: {e}"))?;
    compile_source(file, &src, backend, true)
}

/// Compile source *text* through the full front-end pipeline (used by
/// `pss -c '<code>'`). `file` is only a label for error messages and the base
/// for resolving `import` paths — a pseudo-path such as `"<command>"` resolves
/// imports against the current working directory.
pub fn compile_source(file: &str, src: &str, backend: &str, no_api_check: bool) -> Result<Compiled, String> {
    let mut prog = parse_with_imports(file, src)?;
    let proj = project::load_for(file);
    // The `.pspc` config (`no-api-check: true`) ORs with the caller's flag.
    let no_api_check = no_api_check || proj.no_api_check;
    let typed = semantic::analyze_nac(&mut prog, no_api_check)?;
    let externs = ffi::collect_externs(&prog)?;
    let (bytes, ir_text, listing) = codegen::generate(backend, &prog, &typed, no_api_check)?;
    let meta = bootstrap::collect_metadata(&prog, &typed, file);

    Ok(Compiled {
        file: file.to_string(),
        source: src.to_string(),
        program: prog,
        typed,
        bytes,
        ir_text,
        listing,
        meta,
        proj,
        externs,
    })
}

/// Parse a source file and recursively merge every `import "path.psp"` it
/// references (resolved relative to the importing file's directory).
fn parse_with_imports(file: &str, src: &str) -> Result<Program, String> {
    let tokens = lexer::tokenize(src).map_err(|e| format!("lex error in {file}: {e}"))?;
    let mut prog = parser::parse(&tokens).map_err(|e| format!("parse error in {file}: {e}"))?;
    let base = std::path::Path::new(file)
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_default();
    let mut visited = std::collections::HashSet::new();
    if let Ok(c) = std::fs::canonicalize(file) {
        visited.insert(c.to_string_lossy().into_owned());
    }
    merge_imports(&mut prog, &base, &mut visited)?;
    Ok(prog)
}

/// Recursively load imported modules and append their declarations to `prog`.
/// The imported program is parsed with its own `module`/`import` declarations,
/// so namespaces and nested imports are handled naturally.
fn merge_imports(
    prog: &mut Program,
    base: &std::path::Path,
    visited: &mut std::collections::HashSet<String>,
) -> Result<(), String> {
    let imports = std::mem::take(&mut prog.imports);
    for imp in &imports {
        let p = base.join(&imp.path);
        let canon = match std::fs::canonicalize(&p) {
            Ok(c) => c,
            Err(e) => {
                return Err(format!(
                    "cannot resolve import `{}` (line {}): {e}",
                    imp.path, imp.span.line
                ))
            }
        };
        let canon_str = canon.to_string_lossy().into_owned();
        if !visited.insert(canon_str) {
            continue; // already merged (diamond imports collapse to one copy)
        }
        let isrc = std::fs::read_to_string(&p)
            .map_err(|e| format!("cannot read import `{}`: {e}", imp.path))?;
        let itoks = lexer::tokenize(&isrc)
            .map_err(|e| format!("lex error in import `{}`: {e}", imp.path))?;
        let mut iprog = parser::parse(&itoks)
            .map_err(|e| format!("parse error in import `{}`: {e}", imp.path))?;
        let ibase = p.parent().map(|x| x.to_path_buf()).unwrap_or_default();
        merge_imports(&mut iprog, &ibase, visited)?;
        // merge the imported declarations into the importer
        prog.structs.extend(iprog.structs);
        prog.funcs.extend(iprog.funcs);
        prog.traits.extend(iprog.traits);
        prog.impls.extend(iprog.impls);
        prog.externs.extend(iprog.externs);
        prog.modules.extend(iprog.modules);
    }
    Ok(())
}
