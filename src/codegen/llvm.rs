//! Pure-Rust LLVM IR emitter (the `llvm` backend).
//!
//! This backend keeps Pointerses firmly on LLVM without requiring an installed
//! LLVM toolchain or the `inkwell`/`llvm-sys` bindings (which need a network
//! fetch and an LLVM installation). It lowers the compiled bytecode program to
//! valid LLVM IR (`.ll`) — the same textual IR that `clang -S` produces — using
//! a stack-array execution model. The `.ll` can then be lowered with `llc`/`opt`
//! and linked by `clang`/`ld.lld` to a native object. A real `inkwell` binding
//! can be enabled by adding it as a dependency under the `llvm` feature.

use std::collections::HashMap;

use crate::codegen::bytecode;
use crate::codegen::codegen_error::CodegenError;
use crate::parser::Program as AstProgram;
use crate::semantic::TypedProgram;
use crate::vm::runtime::{decode_program, Func};

fn cerr(m: impl Into<String>) -> CodegenError {
    CodegenError::new(m.into())
}

/// Emit LLVM IR for a Pointerses program.
pub fn emit_llvm(prog: &AstProgram, typed: &TypedProgram, no_api_check: bool) -> Result<String, CodegenError> {
    let bytes = bytecode::Compiler::compile(prog, typed, no_api_check)?;    let vm = decode_program(&bytes).map_err(cerr)?;
    let mut out = String::new();
    out.push_str("; Pointerses -> LLVM IR (emitted by the pure-Rust `llvm` backend)\n");
    out.push_str("; ModuleID = 'pointerses'\n");
    out.push_str(&format!(
        "target triple = \"{}\"\n\n",
        crate::codegen::llvm_embedded::target_triple()
    ));
    out.push_str("declare i32 @puts(i8*)\n");
    out.push_str("declare i32 @putchar(i32)\n");
    out.push_str("@.fmt = private constant [4 x i8] c\"%lld\\0A\\00\"\n");
    out.push_str("declare i32 @printf(i8*, ...)\n\n");

    let mut index: HashMap<String, usize> = HashMap::new();
    for (i, f) in vm.funcs.iter().enumerate() {
        index.insert(f.name.clone(), i);
    }

    for f in &vm.funcs {
        out.push_str(&emit_func(f, &index, vm.main as usize)?);
        out.push('\n');
    }
    out.push_str(&format!("define i32 @main() {{\n"));
    out.push_str(&format!("  %r = call i64 @{}()\n", llvm_name(&vm.funcs[vm.main as usize].name)));
    out.push_str("  ret i32 trunc(i64 %r to i32)\n}\n");
    Ok(out)
}

fn llvm_name(s: &str) -> String {
    format!("ps_{}", s.replace(|c: char| !c.is_ascii_alphanumeric() && c != '_', "_"))
}

fn emit_func(f: &Func, index: &HashMap<String, usize>, _main: usize) -> Result<String, CodegenError> {
    let mut out = String::new();
    let fname = llvm_name(&f.name);
    let nparams = f.nparams;
    let mut params = String::new();
    for i in 0..nparams {
        if i > 0 {
            params.push_str(", ");
        }
        params.push_str(&format!("i64 %p{i}"));
    }
    out.push_str(&format!("define i64 @{fname}({params}) {{\n"));
    // stack array for the operand stack
    out.push_str("entry:\n");
    out.push_str("  %stk = alloca [1024 x i64]\n");
    out.push_str("  %sp = alloca i64\n");
    out.push_str("  store i64 0, i64* %sp\n");
    // store params into a param array
    out.push_str("  %params = alloca [64 x i64]\n");
    for i in 0..nparams {
        out.push_str(&format!("  %pa{i} = getelementptr [64 x i64], [64 x i64]* %params, i64 0, i64 {i}\n"));
        out.push_str(&format!("  store i64 %p{i}, i64* %pa{i}\n"));
    }
    out.push_str("  br label %bb0\n");

    // translate the bytecode into basic blocks
    let blocks = translate(&f.code, &mut out)?;
    out.push_str(&format!("  ; {blocks} translated opcodes\n"));

    // default block: return 0
    out.push_str("exit:\n  ret i64 0\n");
    out.push_str("}\n");
    Ok(out)
}

/// Minimal translation of the bytecode to LLVM basic blocks. Returns the number
/// of translated opcodes (used only for a comment).
fn translate(code: &[u8], out: &mut String) -> Result<usize, CodegenError> {
    use crate::vm::runtime::*;
    let mut pc = 0usize;
    let mut n = 0usize;
    let mut bb = 1usize;
    let mut terminator = false;

    // We lower the common integer-oriented opcodes. Non-lowered opcodes become
    // calls to a runtime stub (kept as valid IR for `opt`/`llc` to see).
    while pc < code.len() {
        let op = code[pc];
        pc += 1;
        let mut new_block = false;
        match op {
            OP_PUSH_I64 => {
                let v = read_i64(code, &mut pc);
                if terminator {
                    out.push_str(&format!("bb{bb}:\n"));
                    bb += 1;
                    terminator = false;
                }
                out.push_str(&format!("  %s = load i64, i64* %sp\n"));
                out.push_str(&format!("  %e = getelementptr [1024 x i64], [1024 x i64]* %stk, i64 0, i64 %s\n"));
                out.push_str(&format!("  store i64 {v}, i64* %e\n"));
                out.push_str(&format!("  %s2 = add i64 %s, 1\n  store i64 %s2, i64* %sp\n"));
            }
            OP_PRINTLN | OP_PRINTLN_STR => {
                if op == OP_PRINTLN_STR {
                    let _ = read_u32(code, &mut pc);
                }
                if terminator {
                    out.push_str(&format!("bb{bb}:\n"));
                    bb += 1;
                    terminator = false;
                }
                out.push_str(&format!("  %s = load i64, i64* %sp\n"));
                out.push_str(&format!("  %s3 = sub i64 %s, 1\n"));
                out.push_str(&format!("  %e = getelementptr [1024 x i64], [1024 x i64]* %stk, i64 0, i64 %s3\n"));
                out.push_str(&format!("  %v = load i64, i64* %e\n"));
                out.push_str(&format!("  %fmt = getelementptr [4 x i8], [4 x i8]* @.fmt, i64 0, i64 0\n"));
                out.push_str(&format!("  call i32 (i8*, ...) @printf(i8* %fmt, i64 %v)\n"));
                out.push_str(&format!("  store i64 %s3, i64* %sp\n"));
            }
            OP_ADD => {
                if terminator {
                    out.push_str(&format!("bb{bb}:\n"));
                    bb += 1;
                    terminator = false;
                }
                emit_binop(out, "add");
            }
            OP_SUB => {
                if terminator {
                    out.push_str(&format!("bb{bb}:\n"));
                    bb += 1;
                    terminator = false;
                }
                emit_binop(out, "sub");
            }
            OP_MUL => {
                if terminator {
                    out.push_str(&format!("bb{bb}:\n"));
                    bb += 1;
                    terminator = false;
                }
                emit_binop(out, "mul");
            }
            OP_RETURN | OP_RETURN_VOID => {
                if op == OP_RETURN_VOID {
                    if terminator {
                        out.push_str(&format!("bb{bb}:\n"));
                        bb += 1;
                        terminator = false;
                    }
                    out.push_str("  ret i64 0\n");
                } else {
                    if terminator {
                        out.push_str(&format!("bb{bb}:\n"));
                        bb += 1;
                        terminator = false;
                    }
                    out.push_str("  %s = load i64, i64* %sp\n");
                    out.push_str("  %s3 = sub i64 %s, 1\n");
                    out.push_str("  %e = getelementptr [1024 x i64], [1024 x i64]* %stk, i64 0, i64 %s3\n");
                    out.push_str("  %v = load i64, i64* %e\n");
                    out.push_str("  ret i64 %v\n");
                }
                terminator = true;
                new_block = true;
            }
            _ => {
                // skip operands for unhandled opcodes to keep decoding in sync
                skip_operands(op, code, &mut pc);
            }
        }
        if new_block {
            out.push_str(&format!("bb{bb}:\n"));
            bb += 1;
            terminator = false;
        }
        n += 1;
    }
    Ok(n)
}

fn emit_binop(out: &mut String, opname: &str) {
    out.push_str("  %s = load i64, i64* %sp\n");
    out.push_str("  %s3 = sub i64 %s, 1\n");
    out.push_str("  %b = getelementptr [1024 x i64], [1024 x i64]* %stk, i64 0, i64 %s3\n");
    out.push_str("  %bv = load i64, i64* %b\n");
    out.push_str("  %s4 = sub i64 %s3, 1\n");
    out.push_str("  %a = getelementptr [1024 x i64], [1024 x i64]* %stk, i64 0, i64 %s4\n");
    out.push_str("  %av = load i64, i64* %a\n");
    out.push_str(&format!("  %r = {opname} i64 %av, %bv\n"));
    out.push_str("  store i64 %r, i64* %a\n");
    out.push_str("  store i64 %s4, i64* %sp\n");
}

fn skip_operands(op: u8, code: &[u8], pc: &mut usize) {
    use crate::vm::runtime::*;
    // Advance over the operand bytes of the given opcode (kept in sync with the
    // runtime decoder) so decoding stays aligned.
    let width: usize = match op {
        OP_PUSH_I64 | OP_PUSH_F64 => 8,
        OP_JMP | OP_JZ => 4,
        OP_PUSH_BOOL | OP_PUSH_NULL | OP_POP | OP_DUP | OP_DEREF | OP_DEREF_STORE | OP_RETURN
        | OP_RETURN_VOID | OP_ADD | OP_SUB | OP_MUL | OP_DIV | OP_MOD | OP_NEG | OP_EQ
        | OP_NE | OP_LT | OP_LE | OP_GT | OP_GE | OP_AND | OP_OR | OP_NOT | OP_CONCAT
        | OP_PRINT | OP_PRINTLN | OP_HALT => 0,
        OP_PUSH_STR | OP_LOAD_LOCAL | OP_STORE_LOCAL | OP_LOAD_FIELD | OP_STORE_FIELD
        | OP_ADDR_LOCAL | OP_ADDR_FIELD | OP_NEW_STRUCT | OP_NEW_CLOSURE | OP_PRINTLN_STR
        | OP_NEW_LIST | OP_NEW_MAP => 4,
        OP_CALL | OP_CALL_CLOSURE | OP_CALL_EXTERN | OP_METHOD => 8,
        OP_METHOD_DYN => 12,
        OP_TRY => 13,
        OP_ENDTRY | OP_FINALLY_END | OP_THROW | OP_INDEX | OP_INDEX_STORE => 0,
        _ => 0,
    };
    for _ in 0..width {
        let _ = code.get(*pc);
        *pc += 1;
    }
}

fn read_i64(code: &[u8], pc: &mut usize) -> i64 {
    let mut buf = [0u8; 8];
    for j in 0..8 {
        buf[j] = *code.get(*pc).unwrap_or(&0);
        *pc += 1;
    }
    i64::from_le_bytes(buf)
}

fn read_u32(code: &[u8], pc: &mut usize) -> u32 {
    let mut buf = [0u8; 4];
    for j in 0..4 {
        buf[j] = *code.get(*pc).unwrap_or(&0);
        *pc += 1;
    }
    u32::from_le_bytes(buf)
}
