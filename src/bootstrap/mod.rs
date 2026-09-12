//! Bootstrap (self-hosting) metadata.
//!
//! The Pointerses front-end (lexer, parser, semantic analysis) and the
//! optimization layer are slated to be rewritten in Pointerses itself, while the
//! back-end continues to rely on LLVM. To make that possible, the Rust compiler
//! emits a self-describing metadata bundle (token stream summary, AST shape,
//! type/struct tables, region and pointer-proof tables) that a Pointerses-based
//! front-end can consume. The interfaces here are stable and are what the
//! self-hosted compiler will target.

use crate::parser::Program;
use crate::semantic::TypedProgram;

/// Self-hosting metadata collected from a compilation.
#[derive(Debug, Clone, Default)]
pub struct MetaBundle {
    pub source: String,
    pub token_count: usize,
    pub function_count: usize,
    pub struct_count: usize,
    pub extern_count: usize,
    pub proof_count: usize,
    pub structs: Vec<(String, Vec<String>)>,
    pub funcs: Vec<(String, String)>,
    pub proofs: Vec<(String, u64)>,
    pub regions: Vec<(String, String)>,
    pub schedules: Vec<(String, String)>,
}

/// Collect bootstrap metadata from a parsed + typed program.
pub fn collect_metadata(prog: &Program, typed: &TypedProgram, source: &str) -> MetaBundle {
    let structs: Vec<(String, Vec<String>)> = prog
        .structs
        .iter()
        .map(|s| (s.name.clone(), s.fields.iter().map(|f| f.name.clone()).collect()))
        .collect();
    let funcs: Vec<(String, String)> = prog
        .funcs
        .iter()
        .map(|f| {
            let sig = typed.funcs.get(&f.name);
            let ret = sig.map(|s| s.ret.display()).unwrap_or_else(|| "void".into());
            (f.name.clone(), ret)
        })
        .collect();
    let proofs: Vec<(String, u64)> = typed.proofs.iter().map(|p| (p.path.clone(), p.offset)).collect();
    let regions: Vec<(String, String)> = typed
        .regions
        .iter()
        .map(|(k, v)| (k.clone(), v.name().to_string()))
        .collect();
    let schedules: Vec<(String, String)> = typed
        .schedules
        .iter()
        .map(|(k, v)| (k.clone(), v.describe()))
        .collect();

    MetaBundle {
        source: source.to_string(),
        token_count: count_tokens(prog),
        function_count: prog.funcs.len(),
        struct_count: prog.structs.len(),
        extern_count: prog.externs.len(),
        proof_count: typed.proofs.len(),
        structs,
        funcs,
        proofs,
        regions,
        schedules,
    }
}

fn count_tokens(_prog: &Program) -> usize {
    // The CLI counts actual tokens earlier; here we report an approximation of
    // syntactic elements so the metadata is self-describing without re-lexing.
    let mut n = 0usize;
    for f in &_prog.funcs {
        n += f.params.len() + 4;
    }
    for s in &_prog.structs {
        n += s.fields.len() + 3;
    }
    n
}

/// Render the metadata bundle as JSON (std-only writer, no external crate).
pub fn to_json(m: &MetaBundle) -> String {
    let mut s = String::new();
    s.push_str("{\n");
    s.push_str(&format!("  \"source\": {},\n", json_str(&m.source)));
    s.push_str(&format!("  \"token_count\": {},\n", m.token_count));
    s.push_str(&format!("  \"function_count\": {},\n", m.function_count));
    s.push_str(&format!("  \"struct_count\": {},\n", m.struct_count));
    s.push_str(&format!("  \"extern_count\": {},\n", m.extern_count));
    s.push_str(&format!("  \"proof_count\": {},\n", m.proof_count));
    s.push_str("  \"structs\": [\n");
    for (i, (n, fields)) in m.structs.iter().enumerate() {
        s.push_str(&format!(
            "    {{ \"name\": {}, \"fields\": [{}] }}{}\n",
            json_str(n),
            fields.iter().map(|f| json_str(f)).collect::<Vec<_>>().join(", "),
            if i + 1 < m.structs.len() { "," } else { "" }
        ));
    }
    s.push_str("  ],\n");
    s.push_str("  \"funcs\": [\n");
    for (i, (n, r)) in m.funcs.iter().enumerate() {
        s.push_str(&format!(
            "    {{ \"name\": {}, \"ret\": {} }}{}\n",
            json_str(n),
            json_str(r),
            if i + 1 < m.funcs.len() { "," } else { "" }
        ));
    }
    s.push_str("  ],\n");
    s.push_str("  \"regions\": [\n");
    for (i, (k, v)) in m.regions.iter().enumerate() {
        s.push_str(&format!(
            "    {{ \"fn\": {}, \"region\": {} }}{}\n",
            json_str(k),
            json_str(v),
            if i + 1 < m.regions.len() { "," } else { "" }
        ));
    }
    s.push_str("  ],\n");
    s.push_str("  \"schedules\": [\n");
    for (i, (k, v)) in m.schedules.iter().enumerate() {
        s.push_str(&format!(
            "    {{ \"fn\": {}, \"schedule\": {} }}{}\n",
            json_str(k),
            json_str(v),
            if i + 1 < m.schedules.len() { "," } else { "" }
        ));
    }
    s.push_str("  ]\n");
    s.push_str("}\n");
    s
}

fn json_str(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Emit the self-hosting metadata file (`<file>.self.json`) for a compiled
/// source. This is the interface the Pointerses-based front-end consumes.
pub fn emit_selfhost(meta: &MetaBundle, file: &str) -> Result<u8, String> {
    let base = std::path::Path::new(file)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| file.to_string());
    let out = format!("{base}.self.json");
    let json = to_json(meta);
    std::fs::write(&out, &json).map_err(|e| format!("cannot write metadata: {e}"))?;
    println!("wrote self-hosting metadata to `{out}`");
    Ok(0)
}

/// Serialize a [`MetaBundle`] to its JSON form (used when packaging `.project`
/// / `.psdl` files so tools can inspect metadata without a live compile).
pub fn meta_to_json(meta: &MetaBundle) -> String {
    to_json(meta)
}
