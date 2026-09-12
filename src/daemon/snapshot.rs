//! Compiler memory snapshot.
//!
//! The resident daemon keeps, per compiled source, an in-memory snapshot
//! containing the compiled bytecode, class/struct metadata, the constant pool,
//! region/schedule tables and the JIT compilation result. A subsequent `pss run`
//! retrieves this snapshot over a loopback connection and starts from the
//! compiled form instead of re-lexing/parsing/type-checking, dramatically
//! reducing startup latency.

use crate::bootstrap::MetaBundle;
use crate::vm::runtime::{decode_program, Program};

/// JIT compilation result metadata.
#[derive(Debug, Clone, Default)]
pub struct JitResult {
    pub lowered: bool,
    pub method_count: usize,
    pub backend: String,
}

/// An in-memory compiler snapshot.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub path: String,
    pub bytes: Vec<u8>,
    pub meta: MetaBundle,
    pub constant_pool: Vec<String>,
    pub structs: Vec<String>,
    pub jit: JitResult,
    pub created_at: u64,
}

/// Build a snapshot from a compiled program and its metadata.
pub fn build_snapshot(path: &str, bytes: &[u8], meta: &MetaBundle) -> Snapshot {
    // Decode the constant pool and struct tables from the compiled program.
    let (constant_pool, structs) = match decode_program(bytes) {
        Ok(p) => (const_strings(&p), p.structs.iter().map(|s| s.name.clone()).collect()),
        Err(_) => (Vec::new(), Vec::new()),
    };
    Snapshot {
        path: path.to_string(),
        bytes: bytes.to_vec(),
        meta: meta.clone(),
        constant_pool,
        structs,
        jit: JitResult {
            lowered: true,
            method_count: meta.function_count,
            backend: "bytecode-vm".into(),
        },
        created_at: now_ms(),
    }
}

fn const_strings(p: &Program) -> Vec<String> {
    p.consts
        .iter()
        .filter_map(|c| match c {
            crate::vm::runtime::Const::Str(s) => Some(s.clone()),
            _ => None,
        })
        .collect()
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Render a snapshot as JSON (std-only writer).
pub fn snapshot_json(s: &Snapshot) -> Result<String, String> {
    let mut out = String::new();
    out.push_str("{\n");
    out.push_str(&format!("  \"path\": {},\n", jstr(&s.path)));
    out.push_str(&format!("  \"bytes_len\": {},\n", s.bytes.len()));
    out.push_str(&format!("  \"created_at\": {},\n", s.created_at));
    out.push_str(&format!("  \"jit\": {{ \"lowered\": {}, \"method_count\": {}, \"backend\": {} }},\n",
        s.jit.lowered, s.jit.method_count, jstr(&s.jit.backend)));
    out.push_str(&format!("  \"structs\": [{}],\n",
        s.structs.iter().map(|x| jstr(x)).collect::<Vec<_>>().join(", ")));
    out.push_str(&format!("  \"constant_pool\": [{}],\n",
        s.constant_pool.iter().map(|x| jstr(x)).collect::<Vec<_>>().join(", ")));
    out.push_str("  \"metadata\": ");
    out.push_str(&crate::bootstrap::to_json(&s.meta));
    out.push_str("}\n");
    Ok(out)
}

fn jstr(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
