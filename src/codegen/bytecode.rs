//! Bytecode compiler (native backend).
//!
//! Lowers the validated AST into the VM's stack-machine bytecode. Closures are
//! compiled to synthetic functions that receive their captured values as a
//! prefix of their locals; pointers compile to first-class `Ptr` values; region
//! and scheduling annotations are recorded as function metadata.

use std::collections::{HashMap, HashSet};

use crate::parser::ast::{Program, *};
use crate::semantic::analyzer::FuncSig;
use crate::semantic::TypedProgram;
use crate::vm::runtime::*;

use super::codegen_error::CodegenError;

/// A compile error.
pub type Error = CodegenError;

fn cerr(msg: impl Into<String>) -> Error {
    Error::new(msg.into())
}

// ---------------------------------------------------------------------------
// Code builder
// ---------------------------------------------------------------------------

struct CodeBuilder {
    code: Vec<u8>,
}

impl CodeBuilder {
    fn new() -> Self {
        CodeBuilder { code: Vec::new() }
    }
    fn op(&mut self, op: u8) {
        self.code.push(op);
    }
    fn u32(&mut self, v: u32) {
        self.code.extend_from_slice(&encode_u32(v));
    }
    fn i64(&mut self, v: i64) {
        self.code.extend_from_slice(&encode_i64(v));
    }
    fn f64(&mut self, v: f64) {
        self.code.extend_from_slice(&encode_f64(v));
    }
    fn mark(&self) -> usize {
        self.code.len()
    }
    /// Emit a JMP placeholder and return its position for patching.
    fn jmp_placeholder(&mut self) -> usize {
        let pos = self.code.len();
        self.op(OP_JMP);
        self.code.extend_from_slice(&encode_i32(0));
        pos
    }
    /// Emit a JZ placeholder and return its position for patching.
    fn jz_placeholder(&mut self) -> usize {
        let pos = self.code.len();
        self.op(OP_JZ);
        self.code.extend_from_slice(&encode_i32(0));
        pos
    }
    /// Patch a jump operand (the i32 at `pos`) to jump to `target`.
    ///
    /// The VM decodes `JMP`/`JZ` as: the opcode byte at `pos` is consumed, then
    /// the 4-byte relative offset is read, leaving PC at `pos + 5`; the offset
    /// must therefore be `target - (pos + 5)` (forwards or backwards). The
    /// operand itself lives at `pos + 1 .. pos + 5` (never overwrite the opcode).
    fn patch(&mut self, pos: usize, target: usize) {
        let off = target as i64 - (pos as i64 + 5);
        let bytes = encode_i32(off as i32);
        for (j, b) in bytes.iter().enumerate() {
            self.code[pos + 1 + j] = *b;
        }
    }
    fn finish(self) -> Vec<u8> {
        self.code
    }
}

// ---------------------------------------------------------------------------
// Scopes
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct LocalInfo {
    idx: u32,
    ty: Option<Type>,
}

#[derive(Clone)]
struct Scope {
    vars: HashMap<String, LocalInfo>,
    next: u32,
}

/// Loop context: collects JMP placeholder positions for `break` / `continue`
/// inside a loop body, patched to their final targets when the loop ends.
struct LoopCtx {
    continue_patches: Vec<usize>,
    break_patches: Vec<usize>,
}

impl Scope {
    fn new() -> Self {
        Scope { vars: HashMap::new(), next: 0 }
    }
    fn declare(&mut self, name: &str, ty: Option<Type>) -> u32 {
        let idx = self.next;
        self.next += 1;
        self.vars.insert(name.to_string(), LocalInfo { idx, ty });
        idx
    }
    /// Rebind an existing name to a specific local (used to keep a catch
    /// variable resolving to its own slot even when a nested block re-declares
    /// the same name).
    fn force(&mut self, name: &str, idx: u32) {
        self.vars.insert(name.to_string(), LocalInfo { idx, ty: Some(Type::Var("Any".into())) });
    }
    fn lookup(&self, name: &str) -> Option<u32> {
        self.vars.get(name).map(|i| i.idx)
    }
    fn ty(&self, name: &str) -> Option<Type> {
        self.vars.get(name).and_then(|i| i.ty.clone())
    }
}

// ---------------------------------------------------------------------------
// Compiler
// ---------------------------------------------------------------------------

pub struct Compiler<'a> {
    prog: &'a Program,
    typed: &'a TypedProgram,
    // structs
    struct_index: HashMap<String, u32>,
    field_index: HashMap<(String, String), u32>,
    // funcs
    func_index: HashMap<String, u32>,
    /// Top-level `var` globals -> VM global slot. A global is never a local, so
    /// the per-function `Scope` never shadows this table into a local slot.
    global_index: HashMap<String, u32>,
    externs: HashSet<String>,
    // constant pool
    consts: Vec<Const>,
    const_map: HashMap<String, u32>,
    // emitted functions
    funcs: Vec<Func>,
    structs_meta: Vec<StructMeta>,
    externs_meta: Vec<ExternMeta>,
    /// (struct template, method) -> function index, for static method dispatch.
    impl_method_index: HashMap<(String, String), u32>,
    // current function build state
    cur: CodeBuilder,
    cur_nlocals: u32,
    /// Module of the function currently being compiled, for resolving unqualified
    /// names to that module's members first (mirrors the semantic analyzer).
    cur_module: Option<String>,
    /// Active generic type-parameter -> trait bound (`T: Describe`).
    cur_bounds: HashMap<String, String>,
    /// `(trait, method)` -> the trait's declared signature (for dynamic dispatch
    /// of `dyn Trait` / bounded type variables).
    trait_methods: HashMap<(String, String), FuncSig>,
    /// Loop context stack: (continue_target, break_target).
    /// Continue jumps to the loop's step/condition start; break to loop end.
    loop_stack: Vec<LoopCtx>,
    /// `no-api-check`: compile calls to undefined names as `OP_CALL_EXTERN`
    /// (host-provided at runtime via `PSS_RUST_LIB`) instead of erroring.
    no_api_check: bool,
}

impl<'a> Compiler<'a> {
    fn new(prog: &'a Program, typed: &'a TypedProgram, no_api_check: bool) -> Self {
        let mut c = Compiler {
            prog,
            typed,
            struct_index: HashMap::new(),
            field_index: HashMap::new(),
            func_index: HashMap::new(),
            global_index: HashMap::new(),
            externs: HashSet::new(),
            consts: Vec::new(),
            const_map: HashMap::new(),
            funcs: Vec::new(),
            structs_meta: Vec::new(),
            externs_meta: Vec::new(),
            impl_method_index: HashMap::new(),
            cur: CodeBuilder::new(),
            cur_nlocals: 0,
            cur_module: None,
            cur_bounds: HashMap::new(),
            trait_methods: HashMap::new(),
            loop_stack: Vec::new(),
            no_api_check,
        };
        // Index trait method signatures for dynamic dispatch.
        for t in &prog.traits {
            for m in &t.methods {
                c.trait_methods.insert(
                    (t.name.clone(), m.name.clone()),
                    FuncSig {
                        params: m.params.iter().map(|p| p.ty.clone().unwrap_or(Type::Int)).collect(),
                        ret: m.ret.clone().unwrap_or(Type::Void),
                    },
                );
            }
        }
        c.build_struct_tables();
        c
    }

    /// If `ty` is a `dyn Trait` object or a type variable bounded by a trait,
    /// return the trait name for runtime dispatch.
    fn dyn_trait_of(&self, ty: Option<Type>) -> Option<String> {
        match ty {
            Some(Type::Trait(t)) => Some(t),
            Some(Type::Var(v)) => self.cur_bounds.get(&v).cloned(),
            _ => None,
        }
    }

    /// Resolve an unqualified function name: prefer a member of the current
    /// module, else the global short name.
    fn resolve_func_name(&self, name: &str) -> String {
        if !name.contains("::") {
            if let Some(m) = &self.cur_module {
                let full = format!("{m}::{name}");
                if self.func_index.contains_key(&full) {
                    return full;
                }
            }
        }
        name.to_string()
    }

    /// Resolve an unqualified struct name: prefer a member of the current module.
    fn resolve_struct_name(&self, name: &str) -> String {
        if !name.contains("::") {
            if let Some(m) = &self.cur_module {
                let full = format!("{m}::{name}");
                if self.struct_index.contains_key(&full) {
                    return full;
                }
            }
        }
        name.to_string()
    }

    fn build_struct_tables(&mut self) {
        // Dedup by (module, name): two modules may each define a `Point`, and
        // both layouts must be emitted into the VM's struct metadata (the short
        // name only aliases whichever merged last). Generic templates share one
        // metadata entry because instantiations resolve to the template name.
        let mut seen = HashSet::new();
        for (i, s) in self.prog.structs.iter().enumerate() {
            let idx = i as u32;
            let names = decl_names(&s.module, &s.name);
            for n in &names {
                self.struct_index.insert(n.clone(), idx);
            }
            let fields: Vec<String> = s.fields.iter().map(|f| f.name.clone()).collect();
            for n in &names {
                for (fi, f) in s.fields.iter().enumerate() {
                    self.field_index.insert((n.clone(), f.name.clone()), fi as u32);
                }
            }
            let key = format!("{}::{}", s.module.clone().unwrap_or_default(), s.name);
            if seen.insert(key) {
                // Runtime type tag: fully-qualified name so dynamic dispatch
                // (`dyn Trait`) can tell two same-named structs apart.
                let full = match &s.module {
                    Some(m) => format!("{m}::{}", s.name),
                    None => s.name.clone(),
                };
                self.structs_meta.push(StructMeta { name: full, fields });
            }
        }
    }

    fn intern_str(&mut self, s: &str) -> u32 {
        if let Some(i) = self.const_map.get(s) {
            return *i;
        }
        let idx = self.consts.len() as u32;
        self.consts.push(Const::Str(s.to_string()));
        self.const_map.insert(s.to_string(), idx);
        idx
    }

    fn struct_field_idx(&self, s: &str, f: &str) -> Option<u32> {
        self.field_index.get(&(s.to_string(), f.to_string())).copied()
    }

    /// Static dispatch: resolve `obj.name()` to the impl function index.
    fn lookup_impl_method(&self, ty: Option<Type>, name: &str) -> Option<u32> {
        let n = match ty {
            Some(Type::Named(n)) => n,
            Some(Type::Generic(n, _)) => n,
            _ => return None,
        };
        self.impl_method_index.get(&(n, name.to_string())).copied()
    }

    // -- main entry ---------------------------------------------------------

    pub fn compile(prog: &'a Program, typed: &'a TypedProgram, no_api_check: bool) -> Result<Vec<u8>, Error> {
        let mut c = Compiler::new(prog, typed, no_api_check);
        c.run()
    }

    fn run(&mut self) -> Result<Vec<u8>, Error> {
        // Register named functions first (so calls resolve to indices). The
        // namespaced (`M::f`) name and the short name alias to the same index.
        for f in &self.prog.funcs {
            let idx = self.funcs.len() as u32;
            let names = decl_names(&f.module, &f.name);
            for n in &names {
                self.func_index.insert(n.clone(), idx);
            }
            // placeholder func; will fill code later
            self.funcs.push(Func {
                name: f.name.clone(),
                nparams: f.params.len() as u32,
                nlocals: 0,
                ncaptures: 0,
                region: region_tag(&self.typed, &f.name),
                schedule: schedule_tag(&self.typed, &f.name),
                manual_size: manual_size(&self.typed, &f.name),
                code: Vec::new(),
            });
        }
        // Extern metadata
        for e in &self.prog.externs {
            for n in decl_names(&e.module, &e.name) {
                self.externs.insert(n.clone());
            }
            self.externs_meta.push(ExternMeta { name: e.name.clone(), nparams: e.params.len() as u32 });
        }

        // Static dispatch table for impl methods (under both the fully-qualified
        // and short receiver name).
        for blk in &self.prog.impls {
            for m in &blk.methods {
                let fname = impl_func_name(&blk.trait_name, &blk.type_name, &m.name);
                if let Some(&fidx) = self.func_index.get(&fname) {
                    for n in decl_names(&blk.module, &blk.type_name) {
                        self.impl_method_index.insert((n, m.name.clone()), fidx);
                    }
                }
            }
        }

        // Index top-level `var` globals (slot = declaration order).
        for (i, g) in self.prog.globals.iter().enumerate() {
            self.global_index.insert(g.name.clone(), i as u32);
        }

        // Compile each named function body. The function's own slot index is its
        // position in prog.funcs (which matches how the funcs table was built);
        // a short-name lookup here would be wrong for cross-module name clashes.
        for (i, f) in self.prog.funcs.iter().enumerate() {
            self.compile_func_body(i as u32, f)?;
        }

        // Compile the synthetic `__init_globals` function, which stores each
        // top-level `var` initializer into its VM global slot in declaration
        // order. The VM runs it once, before `main` (see `Vm::run`).
        let mut init: Option<u32> = None;
        if !self.prog.globals.is_empty() {
            let idx = self.funcs.len() as u32;
            self.funcs.push(Func {
                name: "__init_globals".into(),
                nparams: 0,
                nlocals: 0,
                ncaptures: 0,
                region: 0,
                schedule: 0,
                manual_size: 0,
                code: Vec::new(),
            });
            let mut scope = Scope::new();
            self.cur = CodeBuilder::new();
            self.cur_nlocals = 0;
            for g in &self.prog.globals {
                self.compile_expr(&mut scope, &g.init)?;
                self.cur.op(OP_GSTORE);
                self.cur.u32(self.global_index[&g.name]);
            }
            self.cur.op(OP_RETURN_VOID);
            let code = std::mem::replace(&mut self.cur, CodeBuilder::new()).finish();
            self.funcs[idx as usize].code = code;
            init = Some(idx);
        }

        // main index
        let main = self
            .func_index
            .get("main")
            .copied()
            .ok_or_else(|| cerr("no `main` function defined"))?;

        // Runtime dispatch table for `dyn Trait` / bounded type variables: maps
        // (fully-qualified type tag, method) -> impl function index.
        let mut impls = Vec::new();
        for blk in &self.prog.impls {
            for m in &blk.methods {
                let fname = impl_func_name(&blk.trait_name, &blk.type_name, &m.name);
                if let Some(&fidx) = self.func_index.get(&fname) {
                    let type_name = match &blk.module {
                        Some(m) => format!("{m}::{}", blk.type_name),
                        None => blk.type_name.clone(),
                    };
                    impls.push(ImplMeta { type_name, method: m.name.clone(), func_idx: fidx });
                }
            }
        }

        let out = crate::vm::runtime::Program {
            consts: self.consts.clone(),
            structs: self.structs_meta.clone(),
            funcs: self.funcs.clone(),
            main,
            init,
            externs: self.externs_meta.clone(),
            impls,
        };
        Ok(encode_program(&out))
    }

    fn compile_func_body(&mut self, fidx: u32, f: &FuncDef) -> Result<(), Error> {
        let mut scope = Scope::new();
        self.cur_module = f.module.clone();
        self.cur_bounds = f.bounds.iter().cloned().collect();
        for p in &f.params {
            let raw = p.ty.clone().unwrap_or(Type::Int);
            // Normalize `Named(T)` to `Var(T)` for generic parameters, matching
            // the semantic analyzer (so bounded dispatch sees a type variable).
            let ty = match &raw {
                Type::Named(n) if f.type_params.iter().any(|t| t == n) => Type::Var(n.clone()),
                _ => raw,
            };
            scope.declare(&p.name, Some(ty));
        }
        self.cur = CodeBuilder::new();
        self.cur_nlocals = f.params.len() as u32;
        for s in &f.body {
            self.compile_stmt(&mut scope, s)?;
        }
        // implicit return void at end
        self.cur.op(OP_RETURN_VOID);
        let code = std::mem::replace(&mut self.cur, CodeBuilder::new()).finish();
        self.funcs[fidx as usize].code = code;
        self.funcs[fidx as usize].nlocals = self.cur_nlocals;
        Ok(())
    }

    // -- statements ---------------------------------------------------------

    fn compile_stmt(&mut self, scope: &mut Scope, s: &Stmt) -> Result<(), Error> {
        match s {
            Stmt::Let(p, init, _) => {
                let idx = scope.declare(&p.name, p.ty.clone());
                self.compile_expr(scope, init)?;
                self.cur.op(OP_STORE_LOCAL);
                self.cur.u32(idx);
                if idx as u32 >= self.cur_nlocals {
                    self.cur_nlocals = idx + 1;
                }
            }
            Stmt::Expr(e, _) => {
                self.compile_expr(scope, e)?;
                // `say(...)` and `a = b` leave nothing on the operand stack, so
                // there is no result to discard: popping would eat the caller's
                // value instead.
                if leaves_value(e) {
                    self.cur.op(OP_POP);
                }
            }
            Stmt::Return(e, _) => {
                match e {
                    Some(e) => {
                        self.compile_expr(scope, e)?;
                        self.cur.op(OP_RETURN);
                    }
                    None => {
                        self.cur.op(OP_RETURN_VOID);
                    }
                }
            }
            Stmt::If(cond, then_b, else_b, _) => {
                self.compile_expr(scope, cond)?;
                let jz = self.cur.jz_placeholder();
                for s in then_b {
                    self.compile_stmt(scope, s)?;
                }
                if let Some(eb) = else_b {
                    let jmp = self.cur.jmp_placeholder();
                    let target = self.cur.mark();
                    self.cur.patch(jz, target);
                    for s in eb {
                        self.compile_stmt(scope, s)?;
                    }
                    self.cur.patch(jmp, self.cur.mark());
                } else {
                    let target = self.cur.mark();
                    self.cur.patch(jz, target);
                }
            }
            Stmt::While(cond, body, _) => {
                let start = self.cur.mark();
                self.compile_expr(scope, cond)?;
                let jz = self.cur.jz_placeholder();
                let ctx = LoopCtx { continue_patches: Vec::new(), break_patches: Vec::new() };
                self.loop_stack.push(ctx);
                for s in body {
                    self.compile_stmt(scope, s)?;
                }
                // continue -> jump back to condition start
                for p in &self.loop_stack.last().unwrap().continue_patches.clone() {
                    self.cur.patch(*p, start);
                }
                self.cur.op(OP_JMP);
                self.cur.code.extend_from_slice(&encode_i32(start as i32 - (self.cur.mark() as i32 + 4)));
                let end = self.cur.mark();
                self.cur.patch(jz, end);
                let ctx = self.loop_stack.pop().unwrap();
                for p in &ctx.break_patches {
                    self.cur.patch(*p, end);
                }
            }
            Stmt::For(init, cond, step, body, _) => {
                for i in init {
                    self.compile_stmt(scope, i)?;
                }
                let cond_start = self.cur.mark();
                let mut jz = None;
                if let Some(c) = cond {
                    self.compile_expr(scope, c)?;
                    jz = Some(self.cur.jz_placeholder());
                }
                let ctx = LoopCtx { continue_patches: Vec::new(), break_patches: Vec::new() };
                self.loop_stack.push(ctx);
                for s in body {
                    self.compile_stmt(scope, s)?;
                }
                // continue target: the step expression start (or cond start if none)
                let step_start = self.cur.mark();
                if let Some(st) = step {
                    self.compile_expr(scope, st)?;
                    if leaves_value(st) {
                        self.cur.op(OP_POP);
                    }
                }
                let continue_target = step_start;
                let ctx = self.loop_stack.last().unwrap();
                for p in &ctx.continue_patches {
                    self.cur.patch(*p, continue_target);
                }
                self.cur.op(OP_JMP);
                self.cur.code.extend_from_slice(&encode_i32(cond_start as i32 - (self.cur.mark() as i32 + 4)));
                let end = self.cur.mark();
                if let Some(jz) = jz {
                    self.cur.patch(jz, end);
                }
                let ctx = self.loop_stack.pop().unwrap();
                for p in &ctx.break_patches {
                    self.cur.patch(*p, end);
                }
            }
            Stmt::Break(_) => {
                let ctx = self
                    .loop_stack
                    .last_mut()
                    .ok_or_else(|| cerr("`break` outside a loop"))?;
                let pos = self.cur.jmp_placeholder();
                ctx.break_patches.push(pos);
            }
            Stmt::Continue(_) => {
                let ctx = self
                    .loop_stack
                    .last_mut()
                    .ok_or_else(|| cerr("`continue` outside a loop"))?;
                let pos = self.cur.jmp_placeholder();
                ctx.continue_patches.push(pos);
            }
            Stmt::Directive(_, _) => {
                // no runtime effect at statement level
            }
            Stmt::Throw(e, _) => {
                self.compile_expr(scope, e)?;
                self.cur.op(OP_THROW);
            }
            Stmt::Try(body, catch, fin, _) => {
                //   OP_TRY <catch_off> <catch_local> <finally_off> <flags>
                //   <try body>
                //   OP_ENDTRY
                //   JMP <finally|after>
                //   [<catch>: <catch body> JMP <finally|after>]
                //   [<finally>: <finally body> OP_FINALLY_END]
                //   <after>
                let has_catch = catch.is_some();
                let has_finally = fin.is_some();
                let mut catch_local: u32 = 0;
                if let Some((name, _, _)) = &catch {
                    let idx = scope.declare(name, Some(Type::Var("Any".into())));
                    if idx >= self.cur_nlocals {
                        self.cur_nlocals = idx + 1;
                    }
                    catch_local = idx;
                }
                self.cur.op(OP_TRY);
                let off_slot = self.cur.mark();
                self.cur.u32(0); // catch_off placeholder
                self.cur.u32(catch_local);
                self.cur.u32(0); // finally_off placeholder
                self.cur
                    .code
                    .push((if has_catch { 1 } else { 0 }) | (if has_finally { 2 } else { 0 }));
                let op_end = off_slot + 13;

                for s in body {
                    self.compile_stmt(scope, s)?;
                }
                self.cur.op(OP_ENDTRY);
                let end_jmp = self.cur.jmp_placeholder(); // normal exit -> finally/after

                let mut catch_jmp = None;
                if let Some((name, cbody, _)) = &catch {
                    let catch_target = self.cur.mark();
                    // patch catch_off = catch_target - op_end
                    let off = catch_target as i64 - op_end as i64;
                    let bytes = encode_u32(off as u32);
                    for (j, b) in bytes.iter().enumerate() {
                        self.cur.code[off_slot + j] = *b;
                    }
                    // The catch variable must resolve to its own slot even when a
                    // nested block re-declares the same name.
                    scope.force(name, catch_local);
                    for s in cbody {
                        self.compile_stmt(scope, s)?;
                    }
                    catch_jmp = Some(self.cur.jmp_placeholder());
                }

                if has_finally {
                    let fin_start = self.cur.mark();
                    // patch finally_off = fin_start - op_end
                    let off = fin_start as i64 - op_end as i64;
                    let bytes = encode_u32(off as u32);
                    for (j, b) in bytes.iter().enumerate() {
                        self.cur.code[off_slot + 8 + j] = *b;
                    }
                    self.cur.patch(end_jmp, fin_start);
                    if let Some(cj) = catch_jmp {
                        self.cur.patch(cj, fin_start);
                    }
                    let fb = fin.as_ref().unwrap();
                    for s in fb {
                        self.compile_stmt(scope, s)?;
                    }
                    self.cur.op(OP_FINALLY_END);
                } else {
                    let after = self.cur.mark();
                    self.cur.patch(end_jmp, after);
                    if let Some(cj) = catch_jmp {
                        self.cur.patch(cj, after);
                    }
                }
            }
        }
        Ok(())
    }

    // -- expressions --------------------------------------------------------

    fn compile_expr(&mut self, scope: &mut Scope, e: &Expr) -> Result<(), Error> {
        match e {
            Expr::IntLit(v, _) => {
                self.cur.op(OP_PUSH_I64);
                self.cur.i64(*v);
            }
            Expr::FloatLit(v, _) => {
                self.cur.op(OP_PUSH_F64);
                self.cur.f64(*v);
            }
            Expr::BoolLit(v, _) => {
                self.cur.op(OP_PUSH_BOOL);
                self.cur.code.push(*v as u8);
            }
            Expr::StrLit(s, _) => {
                let idx = self.intern_str(s);
                self.cur.op(OP_PUSH_STR);
                self.cur.u32(idx);
            }
            Expr::NullLit(_) => {
                self.cur.op(OP_PUSH_NULL);
            }
            Expr::Ident(name, sp) => {
                if let Some(idx) = scope.lookup(name) {
                    self.cur.op(OP_LOAD_LOCAL);
                    self.cur.u32(idx);
                } else if let Some(gidx) = self.global_index.get(name) {
                    self.cur.op(OP_GLOAD);
                    self.cur.u32(*gidx);
                } else if let Some(&fidx) = self.func_index.get(&self.resolve_func_name(name)) {
                    // A bare function name used as a value — e.g. a GUI callback
                    // handler in `on_click(win, on_clicked)` — compiles to a
                    // zero-capture closure that wraps the named function.
                    self.cur.op(OP_NEW_CLOSURE);
                    self.cur.u32(fidx);
                    self.cur.u32(0);
                } else if self.func_index.contains_key(name) {
                    return Err(cerr(format!("{}: cannot use function `{name}` as a value", pos(sp))));
                } else if self.externs.contains(name) {
                    return Err(cerr(format!("{}: cannot use extern `{name}` as a value", pos(sp))));
                } else {
                    return Err(cerr(format!("{}: undefined variable `{name}`", pos(sp))));
                }
            }
            Expr::Binary(op, l, r, _) => {
                self.compile_expr(scope, l)?;
                self.compile_expr(scope, r)?;
                // String concatenation uses a dedicated opcode.
                if *op == BinOp::Add {
                    let lt = self.static_type(scope, l);
                    let rt = self.static_type(scope, r);
                    if lt == Some(Type::Str) || rt == Some(Type::Str) {
                        self.cur.op(OP_CONCAT);
                        return Ok(());
                    }
                }
                let o = match op {
                    BinOp::Add => OP_ADD,
                    BinOp::Sub => OP_SUB,
                    BinOp::Mul => OP_MUL,
                    BinOp::Div => OP_DIV,
                    BinOp::Mod => OP_MOD,
                    BinOp::Eq => OP_EQ,
                    BinOp::Ne => OP_NE,
                    BinOp::Lt => OP_LT,
                    BinOp::Le => OP_LE,
                    BinOp::Gt => OP_GT,
                    BinOp::Ge => OP_GE,
                    BinOp::And => OP_AND,
                    BinOp::Or => OP_OR,
                };
                self.cur.op(o);
            }
            Expr::Unary(op, inner, _) => {
                self.compile_unary(scope, *op, inner)?;
            }
            Expr::Call(callee, args, _) => {
                self.compile_call(scope, callee, args)?;
            }
            Expr::Field(obj, name, sp) => {
                // object may be a pointer -> deref first
                if is_ptr(&self.static_type(scope, obj)) {
                    self.compile_expr(scope, obj)?;
                    self.cur.op(OP_DEREF);
                } else {
                    self.compile_expr(scope, obj)?;
                }
                let obj_ty = deref_of(&self.static_type(scope, obj)).ok_or_else(|| {
                    cerr(format!("{}: cannot access field `{name}`: unknown receiver type", pos(sp)))
                })?;
                let fidx = match &obj_ty {
                    Type::Named(n) | Type::Generic(n, _) => self
                        .struct_field_idx(&self.resolve_struct_name(n), name)
                        .ok_or_else(|| cerr(format!("{}: no field `{name}` on `{n}`", pos(sp))))?,
                    _ => return Err(cerr(format!("{}: cannot access field `{name}`", pos(sp)))),
                };
                self.cur.op(OP_LOAD_FIELD);
                self.cur.u32(fidx);
            }
            Expr::MethodCall(obj, name, args, _) => {
                if is_ptr(&self.static_type(scope, obj)) {
                    self.compile_expr(scope, obj)?;
                    self.cur.op(OP_DEREF);
                } else {
                    self.compile_expr(scope, obj)?;
                }
                // Static dispatch to a user-defined impl method if one exists.
                let obj_ty = deref_of(&self.static_type(scope, obj));
                if let Some(fidx) = self.lookup_impl_method(obj_ty.clone(), name) {
                    for a in args {
                        self.compile_expr(scope, a)?;
                    }
                    self.cur.op(OP_CALL);
                    self.cur.u32(fidx);
                    self.cur.u32(args.len() as u32 + 1); // +1 for `this`
                    return Ok(());
                }
                // Dynamic dispatch through `dyn Trait` or a bounded type variable.
                if let Some(tr) = self.dyn_trait_of(obj_ty) {
                    for a in args {
                        self.compile_expr(scope, a)?;
                    }
                    let tn = self.intern_str(&tr);
                    let mn = self.intern_str(name);
                    self.cur.op(OP_METHOD_DYN);
                    self.cur.u32(tn);
                    self.cur.u32(mn);
                    self.cur.u32(args.len() as u32);
                    return Ok(());
                }
                for a in args {
                    self.compile_expr(scope, a)?;
                }
                let nidx = self.intern_str(name);
                self.cur.op(OP_METHOD);
                self.cur.u32(nidx);
                self.cur.u32(args.len() as u32);
            }
            Expr::Assign(target, value, _) => {
                self.compile_assign(scope, target, value)?;
            }
            Expr::StructLit(name, fields, sp) => {
                let sidx = self
                    .struct_index
                    .get(&self.resolve_struct_name(name))
                    .copied()
                    .ok_or_else(|| cerr(format!("{}: unknown struct `{name}`", pos(sp))))?;
                let declared = &self.prog.structs[sidx as usize].fields;
                // emit fields in declared order for a stable layout
                for df in declared {
                    let found = fields.iter().find(|(n, _)| n == &df.name);
                    match found {
                        Some((_, fexpr)) => self.compile_expr(scope, fexpr)?,
                        None => {
                            self.cur.op(OP_PUSH_NULL);
                        }
                    }
                }
                self.cur.op(OP_NEW_STRUCT);
                self.cur.u32(sidx);
                self.cur.u32(declared.len() as u32);
            }
            Expr::GenericStructLit(name, _targs, fields, sp) => {
                let sidx = self
                    .struct_index
                    .get(&self.resolve_struct_name(name))
                    .copied()
                    .ok_or_else(|| cerr(format!("{}: unknown struct `{name}`", pos(sp))))?;
                let declared = &self.prog.structs[sidx as usize].fields;
                // emit fields in declared order for a stable layout
                for df in declared {
                    let found = fields.iter().find(|(n, _)| n == &df.name);
                    match found {
                        Some((_, fexpr)) => self.compile_expr(scope, fexpr)?,
                        None => {
                            self.cur.op(OP_PUSH_NULL);
                        }
                    }
                }
                self.cur.op(OP_NEW_STRUCT);
                self.cur.u32(sidx);
                self.cur.u32(declared.len() as u32);
            }
            Expr::GenericCall(name, _targs, args, _) => {
                let resolved = self.resolve_func_name(name);
                let fidx = match self.func_index.get(&resolved).copied() {
                    Some(f) => Some(f),
                    None if self.no_api_check => None,
                    None => return Err(cerr(format!("unknown function `{name}`"))),
                };
                match fidx {
                    Some(fidx) => {
                        for a in args {
                            self.compile_expr(scope, a)?;
                        }
                        self.cur.op(OP_CALL);
                        self.cur.u32(fidx);
                        self.cur.u32(args.len() as u32);
                    }
                    None => {
                        // `no-api-check`: host-provided generic API — resolve by
                        // name at runtime via the extern dispatcher.
                        for a in args {
                            self.compile_expr(scope, a)?;
                        }
                        let nidx = self.intern_str(name);
                        self.cur.op(OP_CALL_EXTERN);
                        self.cur.u32(nidx);
                        self.cur.u32(args.len() as u32);
                    }
                }
            }
            Expr::InterpStr(parts, _) => {
                let mut count = 0u32;
                for p in parts {
                    match p {
                        crate::parser::ast::InterpPart::Text(t) => {
                            if !t.is_empty() {
                                let idx = self.intern_str(t);
                                self.cur.op(OP_PUSH_STR);
                                self.cur.u32(idx);
                                count += 1;
                            }
                        }
                        crate::parser::ast::InterpPart::Expr(e) => {
                            self.compile_expr(scope, e)?;
                            count += 1;
                        }
                    }
                }
                if count == 0 {
                    let idx = self.intern_str("");
                    self.cur.op(OP_PUSH_STR);
                    self.cur.u32(idx);
                } else {
                    for _ in 1..count {
                        self.cur.op(OP_CONCAT);
                    }
                }
            }
            Expr::Path(segs, _) => {
                let full = segs.join("::");
                return Err(cerr(format!("cannot use module function `{full}` as a value (only call it)")));
            }
            Expr::ArrayLit(elems, _) => {
                for el in elems {
                    self.compile_expr(scope, el)?;
                }
                self.cur.op(OP_NEW_LIST);
                self.cur.u32(elems.len() as u32);
            }
            Expr::MapLit(entries, _) => {
                for (k, v) in entries {
                    self.compile_expr(scope, k)?;
                    self.compile_expr(scope, v)?;
                }
                self.cur.op(OP_NEW_MAP);
                self.cur.u32(entries.len() as u32);
            }
            Expr::Index(obj, idx, _) => {
                if is_ptr(&self.static_type(scope, obj)) {
                    self.compile_expr(scope, obj)?;
                    self.cur.op(OP_DEREF);
                } else {
                    self.compile_expr(scope, obj)?;
                }
                self.compile_expr(scope, idx)?;
                self.cur.op(OP_INDEX);
            }
            Expr::Closure(c) => {
                self.compile_closure(scope, c)?;
            }
            Expr::Block(stmts, _) => {
                for s in stmts {
                    self.compile_stmt(scope, s)?;
                }
                self.cur.op(OP_PUSH_NULL);
            }
            Expr::IfExpr(cond, then_e, else_e, _) => {
                self.compile_expr(scope, cond)?;
                let jz = self.cur.jz_placeholder();
                self.compile_expr(scope, then_e)?;
                if let Some(ee) = else_e {
                    let jmp = self.cur.jmp_placeholder();
                    let target = self.cur.mark();
                    self.cur.patch(jz, target);
                    self.compile_expr(scope, ee)?;
                    self.cur.patch(jmp, self.cur.mark());
                } else {
                    let target = self.cur.mark();
                    self.cur.patch(jz, target);
                    self.cur.op(OP_PUSH_NULL);
                }
            }
            Expr::Ternary(cond, then_e, else_e, _) => {
                self.compile_expr(scope, cond)?;
                let jz = self.cur.jz_placeholder();
                self.compile_expr(scope, then_e)?;
                let jmp = self.cur.jmp_placeholder();
                let target = self.cur.mark();
                self.cur.patch(jz, target);
                self.compile_expr(scope, else_e)?;
                self.cur.patch(jmp, self.cur.mark());
            }
        }
        Ok(())
    }

    fn compile_unary(&mut self, scope: &mut Scope, op: UnOp, inner: &Expr) -> Result<(), Error> {
        match op {
            UnOp::Neg => {
                self.compile_expr(scope, inner)?;
                self.cur.op(OP_NEG);
            }
            UnOp::Not => {
                self.compile_expr(scope, inner)?;
                self.cur.op(OP_NOT);
            }
            UnOp::Addr | UnOp::AddrMut => {
                // address-of: produce a pointer into a local or a field
                match inner {
                    Expr::Ident(name, sp) => {
                        let idx = scope.lookup(name).ok_or_else(|| {
                            cerr(format!("{}: cannot take address of unknown `{name}`", pos(sp)))
                        })?;
                        self.cur.op(OP_ADDR_LOCAL);
                        self.cur.u32(idx);
                    }
                    Expr::Field(obj, name, sp) => {
                        if is_ptr(&self.static_type(scope, obj)) {
                            self.compile_expr(scope, obj)?;
                            self.cur.op(OP_DEREF);
                        } else {
                            self.compile_expr(scope, obj)?;
                        }
                        let obj_ty = deref_of(&self.static_type(scope, obj)).ok_or_else(|| {
                            cerr(format!("{}: cannot address field `{name}`: unknown receiver", pos(sp)))
                        })?;
                        let fidx = match &obj_ty {
                            Type::Named(n) => self
                                .struct_field_idx(&self.resolve_struct_name(n), name)
                                .ok_or_else(|| {
                                    cerr(format!("{}: no field `{name}` on `{n}`", pos(sp)))
                                })?,
                            _ => return Err(cerr(format!("{}: cannot address field", pos(sp)))),
                        };
                        self.cur.op(OP_ADDR_FIELD);
                        self.cur.u32(fidx);
                    }
                    Expr::Unary(UnOp::Deref, p, _) => {
                        // &*p == p
                        self.compile_expr(scope, p)?;
                    }
                    _ => {
                        return Err(cerr(format!(
                            "cannot take address of a non-lvalue (position {})",
                            pos(&inner.span())
                        )))
                    }
                }
            }
            UnOp::Deref => {
                self.compile_expr(scope, inner)?;
                self.cur.op(OP_DEREF);
            }
        }
        Ok(())
    }

    fn compile_assign(&mut self, scope: &mut Scope, target: &Expr, value: &Expr) -> Result<(), Error> {
        match target {
            Expr::Ident(name, sp) => {
                if let Some(idx) = scope.lookup(name) {
                    self.compile_expr(scope, value)?;
                    self.cur.op(OP_STORE_LOCAL);
                    self.cur.u32(idx);
                } else if self.global_index.contains_key(name) {
                    let gidx = self.global_index[name];
                    self.compile_expr(scope, value)?;
                    self.cur.op(OP_GSTORE);
                    self.cur.u32(gidx);
                } else {
                    return Err(cerr(format!("{}: cannot assign to unknown `{name}`", pos(sp))));
                }
            }
            Expr::Field(obj, name, sp) => {
                if is_ptr(&self.static_type(scope, obj)) {
                    self.compile_expr(scope, obj)?;
                    self.cur.op(OP_DEREF);
                } else {
                    self.compile_expr(scope, obj)?;
                }
                let obj_ty = deref_of(&self.static_type(scope, obj)).ok_or_else(|| {
                    cerr(format!("{}: cannot assign field `{name}`: unknown receiver", pos(sp)))
                })?;
                let fidx = match &obj_ty {
                    Type::Named(n) => self
                        .struct_field_idx(&self.resolve_struct_name(n), name)
                        .ok_or_else(|| {
                            cerr(format!("{}: no field `{name}` on `{n}`", pos(sp)))
                        })?,
                    _ => return Err(cerr(format!("{}: cannot assign field", pos(sp)))),
                };
                self.compile_expr(scope, value)?;
                self.cur.op(OP_STORE_FIELD);
                self.cur.u32(fidx);
            }
            Expr::Unary(UnOp::Deref, p, _) => {
                self.compile_expr(scope, p)?;
                self.compile_expr(scope, value)?;
                self.cur.op(OP_DEREF_STORE);
            }
            Expr::Index(obj, idx, _) => {
                if is_ptr(&self.static_type(scope, obj)) {
                    self.compile_expr(scope, obj)?;
                    self.cur.op(OP_DEREF);
                } else {
                    self.compile_expr(scope, obj)?;
                }
                self.compile_expr(scope, idx)?;
                self.compile_expr(scope, value)?;
                self.cur.op(OP_INDEX_STORE);
            }
            _ => {
                return Err(cerr(format!("invalid assignment target at {}", pos(&target.span()))))
            }
        }
        Ok(())
    }

    fn compile_call(&mut self, scope: &mut Scope, callee: &Expr, args: &[Expr]) -> Result<(), Error> {
        // Built-in output functions.
        if let Expr::Ident(name, _) = callee {
            if name == "say" || name == "write" {
                if args.len() != 1 {
                    return Err(cerr(format!("`{name}` expects exactly 1 argument")));
                }
                self.compile_expr(scope, &args[0])?;
                self.cur.op(if name == "say" { OP_PRINTLN } else { OP_PRINT });
                return Ok(());
            }
            // Language-native runtime helpers: `thread_id` / `sleep` plus the
            // CLI / TUI / GUI builtins all go through the extern dispatcher.
            if is_language_builtin(name) {
                for a in args {
                    self.compile_expr(scope, a)?;
                }
                let nidx = self.intern_str(name);
                self.cur.op(OP_CALL_EXTERN);
                self.cur.u32(nidx);
                self.cur.u32(args.len() as u32);
                return Ok(());
            }
        }
        // 1) extern call
        if let Expr::Ident(name, _) = callee {
            if self.externs.contains(name) {
                for a in args {
                    self.compile_expr(scope, a)?;
                }
                let nidx = self.intern_str(name);
                self.cur.op(OP_CALL_EXTERN);
                self.cur.u32(nidx);
                self.cur.u32(args.len() as u32);
                return Ok(());
            }
        }
        // 2) plain named function call
        let callee_name = match callee {
            Expr::Ident(n, _) => Some(n.clone()),
            Expr::Path(segs, _) => Some(segs.join("::")),
            _ => None,
        };
        if let Some(name) = &callee_name {
            let resolved = self.resolve_func_name(name);
            if let Some(&fidx) = self.func_index.get(&resolved) {
                for a in args {
                    self.compile_expr(scope, a)?;
                }
                self.cur.op(OP_CALL);
                self.cur.u32(fidx);
                self.cur.u32(args.len() as u32);
                return Ok(());
            }
            // `no-api-check`: the name is neither defined nor declared `extern`;
            // a host Rust program provides it at runtime (via `PSS_RUST_LIB`).
            // Only plain names (not locals, which are handled by the closure
            // path below) are routed through the extern dispatcher.
            if self.no_api_check && scope.lookup(name).is_none() {
                for a in args {
                    self.compile_expr(scope, a)?;
                }
                let nidx = self.intern_str(name);
                self.cur.op(OP_CALL_EXTERN);
                self.cur.u32(nidx);
                self.cur.u32(args.len() as u32);
                return Ok(());
            }
        }
        // 3) closure / function-typed value call
        self.compile_expr(scope, callee)?;
        for a in args {
            self.compile_expr(scope, a)?;
        }
        self.cur.op(OP_CALL_CLOSURE);
        self.cur.u32(args.len() as u32);
        Ok(())
    }

    // -- closures -----------------------------------------------------------

    fn compile_closure(&mut self, scope: &Scope, c: &ClosureExpr) -> Result<(), Error> {
        // 1. compute captured free variables (in order of first use)
        let own: HashSet<String> = c
            .params
            .iter()
            .map(|p| p.name.clone())
            .collect();
        let mut declared = own.clone();
        let mut captures: Vec<String> = Vec::new();
        collect_free_vars(&c.body, &mut declared, &mut captures, &self.func_index, &self.externs);

        // 2. keep only captures that resolve in the enclosing scope; under
        //    `no-api-check`, names that do not resolve are host API calls and
        //    are dropped (they compile to OP_CALL_EXTERN inside the body).
        let mut resolved: Vec<String> = Vec::new();
        for cap in &captures {
            match scope.lookup(cap) {
                Some(_) => resolved.push(cap.clone()),
                // A top-level `var` global lives in VM-wide storage, so a
                // closure never needs to capture it.
                None if self.global_index.contains_key(cap) => {}
                None if self.no_api_check => {}
                None => {
                    return Err(cerr(format!("closure capture `{cap}` not found in enclosing scope")));
                }
            }
        }
        let captures = resolved;
        // push captured values from the enclosing scope
        for cap in &captures {
            let idx = scope.lookup(cap).expect("checked above");
            self.cur.op(OP_LOAD_LOCAL);
            self.cur.u32(idx);
        }

        // 3. allocate a synthetic function for the closure body
        let fidx = self.funcs.len() as u32;
        self.funcs.push(Func {
            name: format!("__closure_{}", fidx),
            nparams: c.params.len() as u32,
            nlocals: 0,
            ncaptures: captures.len() as u32,
            region: REGION_SCOPED,
            schedule: SCHED_SINGLE,
            manual_size: 0,
            code: Vec::new(),
        });

        // 4. compile the closure body with captures + params seeding the scope
        let saved = std::mem::replace(&mut self.cur, CodeBuilder::new());
        let saved_nlocals = self.cur_nlocals;
        self.cur_nlocals = captures.len() as u32 + c.params.len() as u32;

        let mut cscope = Scope::new();
        for cap in &captures {
            cscope.declare(cap, scope.ty(cap));
        }
        for p in &c.params {
            cscope.declare(&p.name, p.ty.clone());
        }
        match &*c.body {
            Expr::Block(stmts, _) => {
                for s in stmts {
                    self.compile_stmt(&mut cscope, s)?;
                }
                self.cur.op(OP_RETURN_VOID);
            }
            other => {
                self.compile_expr(&mut cscope, other)?;
                self.cur.op(OP_RETURN);
            }
        }
        let code = std::mem::replace(&mut self.cur, saved).finish();
        let nlocals = self.cur_nlocals;
        self.cur_nlocals = saved_nlocals;
        self.funcs[fidx as usize].code = code;
        self.funcs[fidx as usize].nlocals = nlocals;

        // 5. emit NEW_CLOSURE
        self.cur.op(OP_NEW_CLOSURE);
        self.cur.u32(fidx);
        self.cur.u32(captures.len() as u32);
        Ok(())
    }

    // -- static type reconstruction -----------------------------------------

    fn static_type(&self, scope: &Scope, e: &Expr) -> Option<Type> {
        match e {
            Expr::IntLit(_, _) => Some(Type::Int),
            Expr::FloatLit(_, _) => Some(Type::Float),
            Expr::BoolLit(_, _) => Some(Type::Bool),
            Expr::StrLit(_, _) => Some(Type::Str),
            Expr::NullLit(_) => Some(Type::Void),
            Expr::Ident(n, _) => {
                if let Some(t) = scope.ty(n) {
                    Some(t)
                } else if let Some(t) = self.typed.globals.get(n) {
                    Some(t.clone())
                } else if let Some(sig) = self.typed.funcs.get(n) {
                    Some(Type::Fn { params: sig.params.clone(), ret: Box::new(sig.ret.clone()) })
                } else {
                    None
                }
            }
            Expr::Binary(op, l, r, _) => {
                if matches!(op, BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge | BinOp::And | BinOp::Or) {
                    Some(Type::Bool)
                } else if matches!(op, BinOp::Add) {
                    // Propagate string-ness so multi-term concat chains stay OP_CONCAT.
                    let lt = self.static_type(scope, l);
                    let rt = self.static_type(scope, r);
                    if lt == Some(Type::Str) || rt == Some(Type::Str) {
                        Some(Type::Str)
                    } else {
                        lt.or(rt)
                    }
                } else {
                    None
                }
            }
            Expr::Unary(op, inner, _) => match op {
                UnOp::Neg => self.static_type(scope, inner),
                UnOp::Not => Some(Type::Bool),
                UnOp::Addr => {
                    let t = self.static_type(scope, inner);
                    t.map(|t| Type::Ptr { mutable: false, target: Box::new(t) })
                }
                UnOp::AddrMut => {
                    let t = self.static_type(scope, inner);
                    t.map(|t| Type::Ptr { mutable: true, target: Box::new(t) })
                }
                UnOp::Deref => {
                    let t = self.static_type(scope, inner)?;
                    match t {
                        Type::Ptr { target, .. } => Some(*target),
                        _ => None,
                    }
                }
            },
            Expr::Call(callee, _, _) => {
                if let Expr::Ident(n, _) = &**callee {
                    if let Some(sig) = self.typed.funcs.get(n) {
                        return Some(sig.ret.clone());
                    }
                } else if let Expr::Path(segs, _) = &**callee {
                    if let Some(sig) = self.typed.funcs.get(&segs.join("::")) {
                        return Some(sig.ret.clone());
                    }
                }
                None
            }
            Expr::Field(obj, name, _) => {
                let t = deref_of(&self.static_type(scope, obj));
                match t {
                    Some(Type::Named(n)) => self
                        .typed
                        .structs
                        .get(&n)
                        .and_then(|l| l.fields.iter().find(|f| &f.name == name))
                        .map(|f| f.ty.clone()),
                    Some(Type::Generic(n, _)) => self
                        .typed
                        .structs
                        .get(&n)
                        .and_then(|l| l.fields.iter().find(|f| &f.name == name))
                        .map(|f| f.ty.clone()),
                    _ => None,
                }
            }
            Expr::MethodCall(obj, name, _, _) => {
                // an impl method returns its declared return type
                let obj_ty = deref_of(&self.static_type(scope, obj));
                if let Some(fidx) = self.lookup_impl_method(obj_ty.clone(), name) {
                    if let Some(f) = self.funcs.get(fidx as usize) {
                        if let Some(sig) = self.typed.funcs.get(&f.name) {
                            return Some(sig.ret.clone());
                        }
                    }
                }
                // dynamic dispatch: use the trait's declared return type
                if let Some(tr) = self.dyn_trait_of(obj_ty) {
                    if let Some(sig) = self.trait_methods.get(&(tr, name.to_string())) {
                        return Some(sig.ret.clone());
                    }
                }
                if name == "len" {
                    Some(Type::Int)
                } else {
                    Some(Type::Str)
                }
            }
            Expr::Assign(_, v, _) => self.static_type(scope, v),
            Expr::StructLit(n, _, _) => Some(Type::Named(n.clone())),
            Expr::ArrayLit(elems, _) => {
                let elem = elems
                    .first()
                    .and_then(|e| self.static_type(scope, e))
                    .unwrap_or(Type::Void);
                Some(Type::List(Box::new(elem)))
            }
            Expr::MapLit(entries, _) => {
                let k = entries
                    .first()
                    .and_then(|(k, _)| self.static_type(scope, k))
                    .unwrap_or(Type::Void);
                let v = entries
                    .first()
                    .and_then(|(_, v)| self.static_type(scope, v))
                    .unwrap_or(Type::Void);
                Some(Type::Map(Box::new(k), Box::new(v)))
            }
            Expr::Index(obj, _, _) => {
                let t = deref_of(&self.static_type(scope, obj));
                match t {
                    Some(Type::List(e)) | Some(Type::Array(e)) => Some(*e),
                    Some(Type::Map(_, v)) => Some(*v),
                    _ => None,
                }
            }
            Expr::Closure(c) => Some(Type::Fn {
                params: c.params.iter().map(|p| p.ty.clone().unwrap_or(Type::Int)).collect(),
                ret: Box::new(c.ret.clone().unwrap_or(Type::Void)),
            }),
            Expr::InterpStr(..) => Some(Type::Str),
            Expr::GenericCall(n, _, _, _) => self.typed.funcs.get(n).map(|s| s.ret.clone()),
            Expr::GenericStructLit(n, targs, _, _) => Some(Type::Generic(n.clone(), targs.clone())),
            Expr::Path(..) => None,
            Expr::Block(_, _) | Expr::IfExpr(_, _, _, _) | Expr::Ternary(_, _, _, _) => None,
        }
    }
}

fn deref_of(t: &Option<Type>) -> Option<Type> {
    match t {
        Some(Type::Ptr { target, .. }) => Some((**target).clone()),
        other => other.clone(),
    }
}

fn is_ptr(t: &Option<Type>) -> bool {
    matches!(t, Some(Type::Ptr { .. }))
}

/// Registered declaration names: the namespaced (`M::x`) and the short (`x`).
fn decl_names(module: &Option<String>, name: &str) -> Vec<String> {
    let mut names = Vec::new();
    if let Some(m) = module {
        names.push(format!("{m}::{name}"));
    }
    names.push(name.to_string());
    names
}

/// Mangle an impl method into a globally unique function name.
fn impl_func_name(trait_name: &Option<String>, type_name: &str, method: &str) -> String {
    match trait_name {
        Some(t) => format!("impl::{t}::{type_name}::{method}"),
        None => format!("impl::{type_name}::{method}"),
    }
}

fn pos(sp: &Span) -> String {
    format!("{}:{}", sp.line, sp.col)
}

/// Produce a human-readable disassembly listing of a serialized program.
pub fn disassemble(bytes: &[u8]) -> String {
    match decode_program(bytes) {
        Ok(prog) => {
            let mut out = String::new();
            out.push_str(&format!(
                "Pointerses bytecode: {} constants, {} structs, {} functions\n",
                prog.consts.len(),
                prog.structs.len(),
                prog.funcs.len()
            ));
            for (i, s) in prog.structs.iter().enumerate() {
                out.push_str(&format!("  struct {}: {}\n", i, s.fields.join(", ")));
            }
            for (i, f) in prog.funcs.iter().enumerate() {
                out.push_str(&format!(
                    "  fn {}(params={}, locals={}, captures={}, region={}, schedule={}) @{}\n",
                    f.name, f.nparams, f.nlocals, f.ncaptures, f.region, f.schedule, i
                ));
                disassemble_func(&f, &prog.consts, &mut out);
            }
            out.push_str(&format!("  main = {}\n", prog.main));
            out
        }
        Err(e) => format!("(cannot disassemble: {e})"),
    }
}

/// Hex rendering of a byte slice (for the instruction preview).
fn bytes_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect::<Vec<_>>().join(" ")
}

/// Disassemble one function's instruction stream into `out`.
fn disassemble_func(f: &crate::vm::runtime::Func, consts: &[crate::vm::runtime::Const], out: &mut String) {
    use crate::vm::runtime::*;
    let code = &f.code;
    let mut pc = 0usize;
    let mut addr = 0usize;
    while pc < code.len() {
        let op = code[pc];
        let start = pc;
        pc += 1;
        // hex preview of this instruction's raw bytes (opcode + operands)
        let op_hex = match op {
            OP_PUSH_I64 => format!("{:02x} {}", op, bytes_hex(&code[start + 1..start + 9])),
            OP_PUSH_F64 => format!("{:02x} {}", op, bytes_hex(&code[start + 1..start + 9])),
            OP_PUSH_BOOL => format!("{:02x} {:02x}", op, code.get(start + 1).copied().unwrap_or(0)),
            OP_PUSH_STR | OP_LOAD_LOCAL | OP_STORE_LOCAL | OP_LOAD_FIELD | OP_STORE_FIELD
            | OP_ADDR_LOCAL | OP_ADDR_FIELD | OP_GLOAD | OP_GSTORE
            | OP_PRINTLN_STR | OP_NEW_LIST | OP_NEW_MAP => {
                format!("{:02x} {}", op, bytes_hex(&code[start + 1..start + 5]))
            }
            OP_NEW_STRUCT | OP_NEW_CLOSURE | OP_CALL | OP_CALL_EXTERN | OP_METHOD | OP_CALL_CLOSURE => {
                format!("{:02x} {}", op, bytes_hex(&code[start + 1..start + 9]))
            }
            OP_JMP | OP_JZ => format!("{:02x} {}", op, bytes_hex(&code[start + 1..start + 5])),
            OP_TRY => format!("{:02x} {}", op, bytes_hex(&code[start + 1..start + 14])),
            OP_METHOD_DYN => format!("{:02x} {}", op, bytes_hex(&code[start + 1..start + 13])),
            _ => format!("{op:02x}"),
        };
        out.push_str(&format!("    {addr:04}: [{op_hex}] "));
        match op {
            OP_PUSH_I64 => {
                let v = read_i64(code, &mut pc);
                out.push_str(&format!("PUSH_I64 {v}\n"));
            }
            OP_PUSH_F64 => {
                let v = read_f64(code, &mut pc);
                out.push_str(&format!("PUSH_F64 {v}\n"));
            }
            OP_PUSH_BOOL => {
                let b = code[pc];
                pc += 1;
                out.push_str(&format!("PUSH_BOOL {b}\n"));
            }
            OP_PUSH_NULL => out.push_str("PUSH_NULL\n"),
            OP_PUSH_STR => {
                let idx = read_u32(code, &mut pc) as usize;
                let s = match consts.get(idx) {
                    Some(Const::Str(s)) => format!("\"{s}\""),
                    other => format!("{other:?}"),
                };
                out.push_str(&format!("PUSH_STR #{idx} {s}\n"));
            }
            OP_POP => out.push_str("POP\n"),
            OP_DUP => out.push_str("DUP\n"),
            OP_LOAD_LOCAL => {
                let i = read_u32(code, &mut pc);
                out.push_str(&format!("LOAD_LOCAL {i}\n"));
            }
            OP_STORE_LOCAL => {
                let i = read_u32(code, &mut pc);
                out.push_str(&format!("STORE_LOCAL {i}\n"));
            }
            OP_GLOAD => {
                let i = read_u32(code, &mut pc);
                out.push_str(&format!("GLOAD {i}\n"));
            }
            OP_GSTORE => {
                let i = read_u32(code, &mut pc);
                out.push_str(&format!("GSTORE {i}\n"));
            }
            OP_LOAD_FIELD => {
                let i = read_u32(code, &mut pc);
                out.push_str(&format!("LOAD_FIELD {i}\n"));
            }
            OP_STORE_FIELD => {
                let i = read_u32(code, &mut pc);
                out.push_str(&format!("STORE_FIELD {i}\n"));
            }
            OP_ADDR_LOCAL => {
                let i = read_u32(code, &mut pc);
                out.push_str(&format!("ADDR_LOCAL {i}\n"));
            }
            OP_ADDR_FIELD => {
                let i = read_u32(code, &mut pc);
                out.push_str(&format!("ADDR_FIELD {i}\n"));
            }
            OP_DEREF => out.push_str("DEREF\n"),
            OP_DEREF_STORE => out.push_str("DEREF_STORE\n"),
            OP_NEW_STRUCT => {
                let a = read_u32(code, &mut pc);
                let b = read_u32(code, &mut pc);
                out.push_str(&format!("NEW_STRUCT {a} x{b}\n"));
            }
            OP_NEW_CLOSURE => {
                let a = read_u32(code, &mut pc);
                let b = read_u32(code, &mut pc);
                out.push_str(&format!("NEW_CLOSURE {a} x{b}\n"));
            }
            OP_CALL => {
                let a = read_u32(code, &mut pc);
                let b = read_u32(code, &mut pc);
                out.push_str(&format!("CALL {a} x{b}\n"));
            }
            OP_CALL_CLOSURE => {
                let a = read_u32(code, &mut pc);
                out.push_str(&format!("CALL_CLOSURE x{a}\n"));
            }
            OP_RETURN => out.push_str("RETURN\n"),
            OP_RETURN_VOID => out.push_str("RETURN_VOID\n"),
            OP_JMP => {
                let off = read_i32(code, &mut pc);
                out.push_str(&format!("JMP {} -> {}\n", off, pc as i64 + off as i64));
            }
            OP_JZ => {
                let off = read_i32(code, &mut pc);
                out.push_str(&format!("JZ {} -> {}\n", off, pc as i64 + off as i64));
            }
            OP_TRY => {
                let c_off = read_i32(code, &mut pc);
                let local = read_u32(code, &mut pc);
                let f_off = read_i32(code, &mut pc);
                let flags = code.get(pc).copied().unwrap_or(0);
                pc += 1;
                let op_end = pc as i64;
                let mut s = format!(
                    "TRY catch {}->{} local={} finally {}->{} flags={}",
                    c_off,
                    op_end + c_off as i64,
                    local,
                    f_off,
                    op_end + f_off as i64,
                    flags
                );
                if flags & 1 != 0 {
                    s.push_str(" [catch]");
                }
                if flags & 2 != 0 {
                    s.push_str(" [finally]");
                }
                out.push_str(&s);
                out.push('\n');
            }
            OP_ENDTRY => out.push_str("ENDTRY\n"),
            OP_THROW => out.push_str("THROW\n"),
            OP_FINALLY_END => out.push_str("FINALLY_END\n"),
            OP_ADD => out.push_str("ADD\n"),
            OP_SUB => out.push_str("SUB\n"),
            OP_MUL => out.push_str("MUL\n"),
            OP_DIV => out.push_str("DIV\n"),
            OP_MOD => out.push_str("MOD\n"),
            OP_NEG => out.push_str("NEG\n"),
            OP_EQ => out.push_str("EQ\n"),
            OP_NE => out.push_str("NE\n"),
            OP_LT => out.push_str("LT\n"),
            OP_LE => out.push_str("LE\n"),
            OP_GT => out.push_str("GT\n"),
            OP_GE => out.push_str("GE\n"),
            OP_AND => out.push_str("AND\n"),
            OP_OR => out.push_str("OR\n"),
            OP_NOT => out.push_str("NOT\n"),
            OP_CONCAT => out.push_str("CONCAT\n"),
            OP_PRINT => out.push_str("PRINT\n"),
            OP_PRINTLN => out.push_str("PRINTLN\n"),
            OP_PRINTLN_STR => {
                let idx = read_u32(code, &mut pc) as usize;
                let s = match consts.get(idx) {
                    Some(Const::Str(s)) => format!("\"{s}\""),
                    other => format!("{other:?}"),
                };
                out.push_str(&format!("PRINTLN_STR #{idx} {s}\n"));
            }
            OP_HALT => out.push_str("HALT\n"),
            OP_CALL_EXTERN => {
                let a = read_u32(code, &mut pc);
                let b = read_u32(code, &mut pc);
                let n = match consts.get(a as usize) {
                    Some(Const::Str(s)) => format!("\"{s}\""),
                    other => format!("{other:?}"),
                };
                out.push_str(&format!("CALL_EXTERN {n} x{b}\n"));
            }
            OP_METHOD => {
                let a = read_u32(code, &mut pc);
                let b = read_u32(code, &mut pc);
                let n = match consts.get(a as usize) {
                    Some(Const::Str(s)) => format!("\"{s}\""),
                    other => format!("{other:?}"),
                };
                out.push_str(&format!("METHOD {n} x{b}\n"));
            }
            OP_METHOD_DYN => {
                let t = read_u32(code, &mut pc);
                let m = read_u32(code, &mut pc);
                let n = read_u32(code, &mut pc);
                let ts = match consts.get(t as usize) {
                    Some(Const::Str(s)) => s.clone(),
                    other => format!("{other:?}"),
                };
                let ms = match consts.get(m as usize) {
                    Some(Const::Str(s)) => s.clone(),
                    other => format!("{other:?}"),
                };
                out.push_str(&format!("METHOD_DYN {ts}::{ms} x{n}\n"));
            }
            OP_NEW_LIST => {
                let a = read_u32(code, &mut pc);
                out.push_str(&format!("NEW_LIST x{a}\n"));
            }
            OP_NEW_MAP => {
                let a = read_u32(code, &mut pc);
                out.push_str(&format!("NEW_MAP x{a}\n"));
            }
            OP_INDEX => out.push_str("INDEX\n"),
            OP_INDEX_STORE => out.push_str("INDEX_STORE\n"),
            _ => {
                out.push_str(&format!("(unknown opcode {op})\n"));
            }
        }
        addr += pc - start;
    }
}


fn region_tag(typed: &TypedProgram, name: &str) -> u8 {
    use crate::semantic::Region;
    match typed.regions.get(name) {
        Some(Region::Stack) => REGION_STACK,
        Some(Region::Heap) => REGION_HEAP,
        _ => REGION_SCOPED,
    }
}

fn schedule_tag(typed: &TypedProgram, name: &str) -> u8 {
    use crate::concurrency::Schedule;
    match typed.schedules.get(name) {
        Some(Schedule::Auto) => SCHED_AUTO,
        Some(Schedule::Manual(_)) => SCHED_MANUAL,
        _ => SCHED_SINGLE,
    }
}

/// Size of a `@Manual(fixed=N)` pool (0 when the schedule is not `Manual`).
fn manual_size(typed: &TypedProgram, name: &str) -> u32 {
    use crate::concurrency::Schedule;
    match typed.schedules.get(name) {
        Some(Schedule::Manual(n)) => (*n).max(1) as u32,
        _ => 0,
    }
}

fn is_builtin(name: &str) -> bool {
    matches!(name, "say" | "write")
}

/// Whether compiling expression `e` leaves a value on the operand stack.
///
/// `say(...)` / `write(...)` lower to `OP_PRINTLN` / `OP_PRINT`, which pop their
/// argument and push nothing, and `a = b` lowers to a store op
/// (`STORE_LOCAL` / `GSTORE` / `STORE_FIELD` / `DEREF_STORE` / `INDEX_STORE`)
/// that consumes the value. Statement compilation normally appends an `OP_POP` to
/// discard an expression statement's result; emitting it for either of these
/// would pop an *unrelated* value, so it must be skipped.
fn leaves_value(e: &Expr) -> bool {
    match e {
        Expr::Assign(..) => false,
        Expr::Call(callee, _, _) => match callee.as_ref() {
            Expr::Ident(n, _) => !is_builtin(n),
            _ => true,
        },
        _ => true,
    }
}

/// Language-native runtime helpers (output, CLI, TUI, GUI) that are resolved
/// through the extern dispatcher in the VM. They need no `extern` declaration.
fn is_language_builtin(name: &str) -> bool {
    matches!(
        name,
        "thread_id"
            | "sleep"
            | "args"
            | "ask"
            | "readLine"
            | "printf"
            | "fmt"
            | "clear_screen"
            | "cursor"
            | "color"
            | "rgb"
            | "reset_color"
            | "hide_cursor"
            | "show_cursor"
            | "key"
            | "key_available"
            | "terminal_cols"
            | "terminal_rows"
            | "window"
            | "window_close"
            | "window_title"
            | "draw_text"
            | "draw_rect"
            | "fill_rect"
            | "clear_canvas"
            | "on_key"
            | "on_click"
            | "event_loop"
    )
}

/// Collect free variables of an expression (identifiers not declared within the
/// closure and not global functions/externs), in order of first appearance.
fn collect_free_vars(
    e: &Expr,
    declared: &mut HashSet<String>,
    captures: &mut Vec<String>,
    funcs: &HashMap<String, u32>,
    externs: &HashSet<String>,
) {
    match e {
        Expr::Ident(name, _) => {
            if !declared.contains(name)
                && !funcs.contains_key(name)
                && !externs.contains(name)
                && !is_builtin(name)
                && !captures.iter().any(|c| c == name)
            {
                captures.push(name.clone());
            }
        }
        Expr::Binary(_, l, r, _) => {
            collect_free_vars(l, declared, captures, funcs, externs);
            collect_free_vars(r, declared, captures, funcs, externs);
        }
        Expr::Unary(_, inner, _) => collect_free_vars(inner, declared, captures, funcs, externs),
        Expr::Call(callee, args, _) => {
            collect_free_vars(callee, declared, captures, funcs, externs);
            for a in args {
                collect_free_vars(a, declared, captures, funcs, externs);
            }
        }
        Expr::Field(obj, _, _) => collect_free_vars(obj, declared, captures, funcs, externs),
        Expr::MethodCall(obj, _, args, _) => {
            collect_free_vars(obj, declared, captures, funcs, externs);
            for a in args {
                collect_free_vars(a, declared, captures, funcs, externs);
            }
        }
        Expr::Assign(t, v, _) => {
            collect_free_vars(t, declared, captures, funcs, externs);
            collect_free_vars(v, declared, captures, funcs, externs);
        }
        Expr::StructLit(_, fields, _) => {
            for (_, fe) in fields {
                collect_free_vars(fe, declared, captures, funcs, externs);
            }
        }
        Expr::ArrayLit(elems, _) => {
            for e in elems {
                collect_free_vars(e, declared, captures, funcs, externs);
            }
        }
        Expr::MapLit(entries, _) => {
            for (k, v) in entries {
                collect_free_vars(k, declared, captures, funcs, externs);
                collect_free_vars(v, declared, captures, funcs, externs);
            }
        }
        Expr::Index(obj, idx, _) => {
            collect_free_vars(obj, declared, captures, funcs, externs);
            collect_free_vars(idx, declared, captures, funcs, externs);
        }
        Expr::Closure(c) => {
            let mut inner_decl = declared.clone();
            for p in &c.params {
                inner_decl.insert(p.name.clone());
            }
            collect_free_vars(&c.body, &mut inner_decl, captures, funcs, externs);
        }
        Expr::Block(stmts, _) => {
            let mut inner_decl = declared.clone();
            for s in stmts {
                if let Stmt::Let(p, _, _) = s {
                    inner_decl.insert(p.name.clone());
                }
            }
            for s in stmts {
                collect_stmt_free_vars(s, &mut inner_decl, captures, funcs, externs);
            }
        }
        Expr::IfExpr(cond, t, e2, _) => {
            collect_free_vars(cond, declared, captures, funcs, externs);
            collect_free_vars(t, declared, captures, funcs, externs);
            if let Some(x) = e2 {
                collect_free_vars(x, declared, captures, funcs, externs);
            }
        }
        Expr::Ternary(cond, t, e2, _) => {
            collect_free_vars(cond, declared, captures, funcs, externs);
            collect_free_vars(t, declared, captures, funcs, externs);
            collect_free_vars(e2, declared, captures, funcs, externs);
        }
        Expr::InterpStr(parts, _) => {
            for p in parts {
                if let crate::parser::ast::InterpPart::Expr(e) = p {
                    collect_free_vars(e, declared, captures, funcs, externs);
                }
            }
        }
        Expr::GenericCall(_, _, args, _) => {
            for a in args {
                collect_free_vars(a, declared, captures, funcs, externs);
            }
        }
        Expr::GenericStructLit(_, _, fields, _) => {
            for (_, fe) in fields {
                collect_free_vars(fe, declared, captures, funcs, externs);
            }
        }
        Expr::Path(..) => {}
        _ => {}
    }
}

fn collect_stmt_free_vars(
    s: &Stmt,
    declared: &mut HashSet<String>,
    captures: &mut Vec<String>,
    funcs: &HashMap<String, u32>,
    externs: &HashSet<String>,
) {
    match s {
        Stmt::Let(_, init, _) => collect_free_vars(init, declared, captures, funcs, externs),
        Stmt::Expr(e, _) => collect_free_vars(e, declared, captures, funcs, externs),
        Stmt::Return(e, _) => {
            if let Some(e) = e {
                collect_free_vars(e, declared, captures, funcs, externs);
            }
        }
        Stmt::If(c, t, eb, _) => {
            collect_free_vars(c, declared, captures, funcs, externs);
            for s in t {
                collect_stmt_free_vars(s, declared, captures, funcs, externs);
            }
            if let Some(eb) = eb {
                for s in eb {
                    collect_stmt_free_vars(s, declared, captures, funcs, externs);
                }
            }
        }
        Stmt::While(c, body, _) => {
            collect_free_vars(c, declared, captures, funcs, externs);
            for s in body {
                collect_stmt_free_vars(s, declared, captures, funcs, externs);
            }
        }
        Stmt::For(init, cond, step, body, _) => {
            for s in init {
                collect_stmt_free_vars(s, declared, captures, funcs, externs);
            }
            if let Some(c) = cond {
                collect_free_vars(c, declared, captures, funcs, externs);
            }
            if let Some(st) = step {
                collect_free_vars(st, declared, captures, funcs, externs);
            }
            for s in body {
                collect_stmt_free_vars(s, declared, captures, funcs, externs);
            }
        }
        Stmt::Break(_) | Stmt::Continue(_) => {}
        Stmt::Directive(_, _) => {}
        Stmt::Throw(e, _) => collect_free_vars(e, declared, captures, funcs, externs),
        Stmt::Try(body, catch, fin, _) => {
            for s in body {
                collect_stmt_free_vars(s, declared, captures, funcs, externs);
            }
            if let Some((_name, cbody, _)) = catch {
                // the catch variable is bound inside the handler
                declared.insert("this".into()); // no-op; keep symmetry
                for s in cbody {
                    collect_stmt_free_vars(s, declared, captures, funcs, externs);
                }
            }
            if let Some(fb) = fin {
                for s in fb {
                    collect_stmt_free_vars(s, declared, captures, funcs, externs);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests: top-level `var` globals
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use crate::compiler;
    use crate::vm::runtime::{decode_program, OP_GLOAD, OP_GSTORE};

    fn compile(src: &str) -> Vec<u8> {
        let c = compiler::compile_source("<test>", src, "native", false)
            .unwrap_or_else(|e| panic!("compile failed: {e}"));
        c.bytes
    }

    #[test]
    fn top_level_vars_emit_init_globals() {
        let src = "var a = 1\nvar b = a + 1\nfunc main() -> int { return a + b }\n";
        let prog = decode_program(&compile(src)).unwrap();
        let i = prog.init.expect("globals should emit an init function");
        let f = &prog.funcs[i as usize];
        assert_eq!(f.name, "__init_globals");
        assert!(f.code.contains(&OP_GLOAD), "init must read globals");
        assert!(f.code.contains(&OP_GSTORE), "init must store globals");
    }

    #[test]
    fn globals_run_before_main_and_are_mutable() {
        let src = "var c = 10\nfunc bump() -> int { c = c + 5\nreturn c }\nfunc main() -> int { return bump() + bump() }\n";
        let code = crate::vm::execute_bytes(&compile(src), &[]).unwrap();
        assert_eq!(code, 35); // 10 -> 15, then 15 -> 20; main returns 15 + 20
    }

    #[test]
    fn program_without_globals_has_no_init() {
        let src = "func main() -> int { return 7 }\n";
        let prog = decode_program(&compile(src)).unwrap();
        assert!(prog.init.is_none());
    }
}
