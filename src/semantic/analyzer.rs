//! Semantic analysis: strict type inference, scope validation, region
//! lifetime validation, pointer-legality proofing, and closure capture
//! collection.
//!
//! The analyzer walks the whole program, resolves every identifier against a
//! lexical scope stack, infers and checks the type of every expression, proves
//! every pointer legal (algebraic-path offset within bounds + region
//! outlives), and fills each closure's `captures` with the free variables it
//! closes over. The result is a `TypedProgram` consumed by the code generator.

use std::collections::{HashMap, HashSet};

use crate::parser::ast::*;
use crate::concurrency::Schedule;

use super::pointer::{PointerChecker, PointerProof};
use super::region::Region;
use super::types::{build_layout, FieldLayout, StructLayout};

/// A semantic error with a position.
#[derive(Debug)]
pub struct SemanticError {
    pub msg: String,
    pub line: usize,
    pub col: usize,
}

impl std::fmt::Display for SemanticError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}: {}", self.line, self.col, self.msg)
    }
}

impl From<SemanticError> for String {
    fn from(e: SemanticError) -> String {
        e.to_string()
    }
}

fn serr(span: Span, msg: impl Into<String>) -> SemanticError {
    SemanticError { msg: msg.into(), line: span.line, col: span.col }
}

/// Resolved function signature.
#[derive(Debug, Clone)]
pub struct FuncSig {
    pub params: Vec<Type>,
    pub ret: Type,
}

/// The typed, analyzed program handed to code generation.
#[derive(Debug, Clone, Default)]
pub struct TypedProgram {
    pub structs: HashMap<String, StructLayout>,
    pub funcs: HashMap<String, FuncSig>,
    pub regions: HashMap<String, Region>,
    pub schedules: HashMap<String, Schedule>,
    pub proofs: Vec<PointerProof>,
    pub externs: HashSet<String>,
    /// Top-level `var` globals and their inferred types (code generation uses
    /// these for static-type decisions such as string concatenation).
    pub globals: HashMap<String, Type>,
}

/// Perform semantic analysis on a program, mutating closure `captures` in place.
pub fn analyze(prog: &mut Program) -> Result<TypedProgram, SemanticError> {
    analyze_nac(prog, false)
}

/// Perform semantic analysis, honouring the `no-api-check` flag: when enabled,
/// calls to functions that are neither defined in the program nor declared
/// `extern` are *not* rejected — they are treated as host-provided API calls
/// that a Rust program resolves at runtime.
pub fn analyze_nac(prog: &mut Program, no_api_check: bool) -> Result<TypedProgram, SemanticError> {
    let mut a = Analyzer::new(prog, no_api_check);
    a.run()
}

struct ScopeFrame {
    vars: HashMap<String, Type>,
}

pub struct Analyzer<'a> {
    prog: &'a mut Program,
    structs: HashMap<String, StructLayout>,
    funcs: HashMap<String, FuncSig>,
    regions: HashMap<String, Region>,
    schedules: HashMap<String, Schedule>,
    externs: HashSet<String>,
    /// Top-level `var` globals. Kept separate from `frames` because function
    /// analysis clears the frame stack, while globals must stay visible in every
    /// function body (and survive across callbacks).
    global_types: HashMap<String, Type>,
    frames: Vec<ScopeFrame>,
    /// Number of frames that belong to the currently analyzed closure.
    closure_base: usize,
    captures: Vec<String>,
    /// Current loop nesting depth (for validating break/continue).
    loop_depth: usize,
    checker: PointerChecker,
    /// Stack of active generic type-parameter sets (function/impl level).
    type_params: Vec<HashSet<String>>,
    /// Registered function name -> its generic type parameters.
    func_type_params: HashMap<String, Vec<String>>,
    /// (struct template, method) -> mangled impl function name (static dispatch).
    impl_resolver: HashMap<(String, String), String>,
    /// Mangled impl function name -> the impl's type parameters.
    impl_type_params: HashMap<String, Vec<String>>,
    /// `(Trait::type::method)` mangled name -> (receiver type, receiver type args).
    impl_receivers: HashMap<String, (String, Vec<String>)>,
    /// Struct template name -> its type parameters (for Generic substitution).
    struct_tparams: HashMap<String, Vec<String>>,
    /// `(trait, method)` -> the trait's declared method (for dynamic dispatch
    /// through `dyn Trait` or a bounded type variable).
    trait_methods: HashMap<(String, String), TraitMethod>,
    /// Module of the function currently being analyzed (for resolving unqualified
    /// names to that module's members before falling back to global short names).
    current_module: Option<String>,
    /// Active generic type-parameter -> trait bound (`T: Describe`).
    cur_bounds: HashMap<String, String>,
    /// `no-api-check`: skip the "called function must exist" check, treating
    /// unknown names as host-provided API calls (resolved at runtime).
    no_api_check: bool,
}

impl<'a> Analyzer<'a> {
    fn new(prog: &'a mut Program, no_api_check: bool) -> Self {
        Analyzer {
            prog,
            structs: HashMap::new(),
            funcs: HashMap::new(),
            regions: HashMap::new(),
            schedules: HashMap::new(),
            externs: HashSet::new(),
            global_types: HashMap::new(),
            frames: Vec::new(),
            closure_base: 0,
            captures: Vec::new(),
            loop_depth: 0,
            checker: PointerChecker::default(),
            type_params: Vec::new(),
            func_type_params: HashMap::new(),
            impl_resolver: HashMap::new(),
            impl_type_params: HashMap::new(),
            impl_receivers: HashMap::new(),
            struct_tparams: HashMap::new(),
            current_module: None,
            trait_methods: HashMap::new(),
            cur_bounds: HashMap::new(),
            no_api_check,
        }
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

    /// Resolve an unqualified function name to its fully-registered name: prefer
    /// a member of the current module, else the global short name. Explicit
    /// `a::b` paths are left untouched (they never start with a module prefix).
    fn resolve_func_name(&self, name: &str) -> String {
        if !name.contains("::") {
            if let Some(m) = &self.current_module {
                let full = format!("{m}::{name}");
                if self.funcs.contains_key(&full) {
                    return full;
                }
            }
        }
        name.to_string()
    }

    /// Resolve an unqualified struct name to its layout: prefer the current
    /// module's member, else the global short name.
    fn resolve_struct_layout(&self, name: &str) -> Option<&StructLayout> {
        if !name.contains("::") {
            if let Some(m) = &self.current_module {
                let full = format!("{m}::{name}");
                if let Some(l) = self.structs.get(&full) {
                    return Some(l);
                }
            }
        }
        self.structs.get(name)
    }

    /// Whether `name` is currently in scope as a generic type parameter.
    fn in_type_params(&self, name: &str) -> bool {
        self.type_params.iter().any(|s| s.contains(name))
    }

    /// Whether any type variable is in scope.
    fn has_type_params(&self) -> bool {
        !self.type_params.is_empty()
    }

    /// Register a struct layout under its namespace-aware names.
    fn insert_struct(&mut self, names: Vec<String>, layout: StructLayout) {
        for n in names {
            self.structs.insert(n, layout.clone());
        }
    }

    /// Register a function signature under its namespace-aware names.
    fn insert_func(&mut self, names: Vec<String>, sig: FuncSig) {
        for n in names {
            self.funcs.insert(n, sig.clone());
        }
    }

    // -- layout & signature collection --------------------------------------

    fn build_struct_layouts(&mut self) -> Result<(), SemanticError> {
        // Generic structs (`struct Box[T]`) erase to a dynamic layout: the VM
        // stores fields as values, so every field occupies one word. Their
        // registered name stays the template name so all instantiations share it.
        let generic: Vec<StructDef> = self
            .prog
            .structs
            .iter()
            .filter(|s| !s.type_params.is_empty())
            .cloned()
            .collect();
        for s in &generic {
            let params: HashSet<String> = s.type_params.iter().cloned().collect();
            self.struct_tparams.insert(s.name.clone(), s.type_params.clone());
            let mut fields = Vec::new();
            let mut offset = 0u64;
            for f in &s.fields {
                let ty = self.normalize_ty(&f.ty, &params);
                fields.push(FieldLayout { name: f.name.clone(), ty, offset });
                offset += 8;
            }
            let layout = StructLayout {
                name: s.name.clone(),
                fields,
                size: s.fields.len() as u64 * 8,
                align: 8,
            };
            let names = Self::decl_names(&s.module, &s.name);
            self.insert_struct(names, layout);
        }

        // Iterative fixpoint so forward references between non-generic structs
        // resolve.
        let mut pending: Vec<(String, Option<String>, Vec<(String, Type)>, Span)> = self
            .prog
            .structs
            .iter()
            .filter(|s| s.type_params.is_empty())
            .map(|s| {
                (
                    s.name.clone(),
                    s.module.clone(),
                    s.fields.iter().map(|f| (f.name.clone(), f.ty.clone())).collect(),
                    s.span,
                )
            })
            .collect();
        let mut progress = true;
        while !pending.is_empty() && progress {
            progress = false;
            let mut next = Vec::new();
            for (name, module, fields, span) in pending {
                match build_layout(&name, &fields, &self.structs) {
                    Ok(layout) => {
                        let names = Self::decl_names(&module, &name);
                        self.insert_struct(names, layout);
                        progress = true;
                    }
                    Err(_) => {
                        // try later if it depends on a not-yet-built struct
                        next.push((name, module, fields, span));
                    }
                }
            }
            pending = next;
        }
        // Anything left is a true dependency error.
        for (name, _m, _f, span) in pending {
            return Err(serr(span, format!("cannot build layout for struct `{name}` (unknown field type)")));
        }
        Ok(())
    }

    fn collect_sigs(&mut self) -> Result<(), SemanticError> {
        for f in &self.prog.funcs {
            let params_set: HashSet<String> = f.type_params.iter().cloned().collect();
            let mut params = Vec::new();
            for p in &f.params {
                let ty = p
                    .ty
                    .clone()
                    .ok_or_else(|| serr(p.span, format!("function parameter `{}` needs a type", p.name)))?;
                params.push(self.normalize_ty(&ty, &params_set));
            }
            let ret = self.normalize_ty(&f.ret.clone().unwrap_or(Type::Void), &params_set);
            let sig = FuncSig { params, ret };
            let names = Self::decl_names(&f.module, &f.name);
            for n in &names {
                self.funcs.insert(n.clone(), sig.clone());
                if !f.type_params.is_empty() {
                    self.func_type_params.insert(n.clone(), f.type_params.clone());
                }
            }
            // region + schedule from annotations (registered under every name)
            let region = region_of(&f.annotations).unwrap_or(Region::Scoped);
            let sched = schedule_of(&f.annotations).unwrap_or(Schedule::Single);
            for n in &names {
                self.regions.insert(n.clone(), region);
                self.schedules.insert(n.clone(), sched.clone());
            }
        }
        for e in &self.prog.externs {
            let params = e.params.iter().map(|p| p.ty.clone().unwrap_or(Type::Int)).collect();
            let ret = e.ret.clone().unwrap_or(Type::Void);
            let sig = FuncSig { params, ret };
            let names = Self::decl_names(&e.module, &e.name);
            for n in &names {
                self.externs.insert(n.clone());
                self.funcs.insert(n.clone(), sig.clone());
            }
        }
        Ok(())
    }

    /// Replace `Named(t)` with `Var(t)` when `t` is an active type parameter.
    fn normalize_ty(&self, ty: &Type, params: &HashSet<String>) -> Type {
        match ty {
            Type::Named(n) if params.contains(n) => Type::Var(n.clone()),
            Type::Generic(n, args) => {
                Type::Generic(n.clone(), args.iter().map(|a| self.normalize_ty(a, params)).collect())
            }
            Type::Ptr { mutable, target } => {
                Type::Ptr { mutable: *mutable, target: Box::new(self.normalize_ty(target, params)) }
            }
            Type::Fn { params: ps, ret } => Type::Fn {
                params: ps.iter().map(|p| self.normalize_ty(p, params)).collect(),
                ret: Box::new(self.normalize_ty(ret, params)),
            },
            Type::List(t) => Type::List(Box::new(self.normalize_ty(t, params))),
            Type::Map(k, v) => Type::Map(
                Box::new(self.normalize_ty(k, params)),
                Box::new(self.normalize_ty(v, params)),
            ),
            Type::Array(t) => Type::Array(Box::new(self.normalize_ty(t, params))),
            other => other.clone(),
        }
    }

    /// Substitute concrete types for type variables.
    fn subst_ty(ty: &Type, map: &HashMap<String, Type>) -> Type {
        match ty {
            Type::Var(n) => map.get(n).cloned().unwrap_or_else(|| Type::Var(n.clone())),
            Type::Generic(n, args) => {
                Type::Generic(n.clone(), args.iter().map(|a| Self::subst_ty(a, map)).collect())
            }
            Type::Ptr { mutable, target } => {
                Type::Ptr { mutable: *mutable, target: Box::new(Self::subst_ty(target, map)) }
            }
            Type::Fn { params, ret } => Type::Fn {
                params: params.iter().map(|p| Self::subst_ty(p, map)).collect(),
                ret: Box::new(Self::subst_ty(ret, map)),
            },
            Type::List(t) => Type::List(Box::new(Self::subst_ty(t, map))),
            Type::Map(k, v) => Type::Map(
                Box::new(Self::subst_ty(k, map)),
                Box::new(Self::subst_ty(v, map)),
            ),
            Type::Array(t) => Type::Array(Box::new(Self::subst_ty(t, map))),
            other => other.clone(),
        }
    }

    /// Lift `impl` methods into the function table with an implicit `this`
    /// receiver parameter, and record the static-dispatch resolution map.
    fn lift_impl_methods(&mut self) -> Result<(), SemanticError> {
        let impls = std::mem::take(&mut self.prog.impls);
        for blk in &impls {
            let type_name = blk.type_name.clone();
            // Fully-qualified receiver name (`M::Type`), used for the implicit
            // `this` parameter so field access and dispatch resolve correctly.
            let recv_full = match &blk.module {
                Some(m) if !m.is_empty() => format!("{m}::{type_name}"),
                _ => type_name.clone(),
            };
            for m in &blk.methods {
                let fname = impl_func_name(&blk.trait_name, &type_name, &m.name);
                let mut params = Vec::new();
                params.push(Param {
                    name: "this".into(),
                    ty: Some(Type::Named(recv_full.clone())),
                    span: m.span,
                });
                params.extend(m.params.clone());
                let fd = FuncDef {
                    name: fname.clone(),
                    type_params: blk.type_params.clone(),
                    bounds: Vec::new(),
                    params,
                    ret: m.ret.clone(),
                    body: m.body.clone(),
                    annotations: m.annotations.clone(),
                    module: None,
                    span: m.span,
                };
                self.prog.funcs.push(fd);
                // Register under both the fully-qualified and the short name so
                // both a `M::Type` literal and a module-internal `Type` resolve.
                for n in Self::decl_names(&blk.module, &type_name) {
                    self.impl_resolver.insert((n, m.name.clone()), fname.clone());
                }
                if !blk.type_params.is_empty() {
                    self.impl_type_params.insert(fname.clone(), blk.type_params.clone());
                }
                self.impl_receivers
                    .insert(fname.clone(), (recv_full.clone(), blk.type_params.clone()));
            }
        }
        self.prog.impls = impls;
        Ok(())
    }

    // -- scope helpers ------------------------------------------------------

    fn push_frame(&mut self) {
        self.frames.push(ScopeFrame { vars: HashMap::new() });
    }
    fn pop_frame(&mut self) {
        self.frames.pop();
    }
    fn declare(&mut self, name: &str, ty: Type) {
        if let Some(f) = self.frames.last_mut() {
            f.vars.insert(name.to_string(), ty);
        }
    }

    /// Resolve a variable. Returns (type, frame_index, is_capture).
    fn resolve(&mut self, name: &str) -> Result<(Type, Option<usize>, bool), SemanticError> {
        for i in (0..self.frames.len()).rev() {
            if let Some(ty) = self.frames[i].vars.get(name) {
                let ty = ty.clone();
                let outside = i < self.closure_base;
                let is_capture = outside && self.closure_base > 0;
                return Ok((ty, Some(i), is_capture));
            }
        }
        if let Some(ty) = self.global_types.get(name) {
            return Ok((ty.clone(), None, false));
        }
        if self.funcs.contains_key(name) {
            return Ok((Type::Fn { params: self.funcs[name].params.clone(), ret: Box::new(self.funcs[name].ret.clone()) }, None, false));
        }
        Err(SemanticError {
            msg: format!("undefined variable or function `{name}`"),
            line: 0,
            col: 0,
        })
    }

    /// Whether `name` is a local variable (incl. a closure variable) in the
    /// current frame stack — as opposed to an unknown global function name.
    fn is_local(&self, name: &str) -> bool {
        self.frames.iter().any(|f| f.vars.contains_key(name))
    }

    // -- program entry ------------------------------------------------------

    fn run(&mut self) -> Result<TypedProgram, SemanticError> {
        self.lift_impl_methods()?;
        self.build_struct_layouts()?;
        self.collect_sigs()?;
        // Top-level `var` globals must be visible inside every function body.
        self.analyze_globals()?;
        // Index trait method signatures for dynamic dispatch (`dyn Trait` and
        // bounded type variables `T: Trait`).
        for t in &self.prog.traits {
            for m in &t.methods {
                self.trait_methods.insert((t.name.clone(), m.name.clone()), m.clone());
            }
        }

        // Analyze every function body (copy function list to avoid borrow issues).
        let funcs = std::mem::take(&mut self.prog.funcs);
        let mut analyzed = Vec::new();
        for f in funcs {
            let ret = self.analyze_func(&f)?;
            analyzed.push(ret);
        }
        self.prog.funcs = analyzed;

        Ok(TypedProgram {
            structs: self.structs.clone(),
            funcs: self.funcs.clone(),
            regions: self.regions.clone(),
            schedules: self.schedules.clone(),
            proofs: self.checker.proofs.clone(),
            externs: self.externs.clone(),
            globals: self.global_types.clone(),
        })
    }

    /// Analyze the top-level `var` globals before any function body, so that every
    /// function (including GUI callbacks) can read and write them. Two passes are
    /// used: first every global is declared with a wildcard type so initializers
    /// that reference other globals resolve, then the initializers are analyzed in
    /// declaration order to infer the real type (which matches the VM's
    /// left-to-right initialization).
    fn analyze_globals(&mut self) -> Result<(), SemanticError> {
        let globals = self.prog.globals.clone();
        for g in &globals {
            self.global_types
                .entry(g.name.clone())
                .or_insert_with(|| Type::Var("Any".into()));
        }
        for g in &globals {
            let init_ty = self.analyze_expr(&g.init)?;
            let ty = match &g.ty {
                Some(d) => {
                    check_compat(d, &init_ty, g.span)?;
                    d.clone()
                }
                None => init_ty,
            };
            self.global_types.insert(g.name.clone(), ty);
        }
        Ok(())
    }

    fn analyze_func(&mut self, f: &FuncDef) -> Result<FuncDef, SemanticError> {
        let ret_ty = f.ret.clone().unwrap_or(Type::Void);
        self.current_module = f.module.clone();
        self.cur_bounds = f.bounds.iter().cloned().collect();
        self.frames.clear();
        self.closure_base = 0;
        self.captures.clear();
        self.push_frame();
        // Impl methods declare the receiver as `this` in local slot 0.
        if let Some((recv, tparams)) = self.impl_receivers.get(&f.name).cloned() {
            if !tparams.is_empty() {
                let params_set: HashSet<String> = tparams.iter().cloned().collect();
                let recv_ty = self.normalize_ty(&Type::Named(recv.clone()), &params_set);
                self.declare("this", recv_ty);
            } else {
                self.declare("this", Type::Named(recv));
            }
        }
        // Push the function's generic type parameters so `Named(t)` resolves to
        // `Var(t)` inside the body (e.g. `let x: T = ...`).
        let params_set: HashSet<String> = f.type_params.iter().cloned().collect();
        let pushed_params = !params_set.is_empty();
        if pushed_params {
            self.type_params.push(params_set);
        }
        let r = self.analyze_func_inner(f, &ret_ty);
        if pushed_params {
            self.type_params.pop();
        }
        r
    }

    fn analyze_func_inner(&mut self, f: &FuncDef, ret_ty: &Type) -> Result<FuncDef, SemanticError> {
        let params_set: HashSet<String> = f.type_params.iter().cloned().collect();
        for p in &f.params {
            let raw = p.ty.clone().unwrap_or(Type::Int);
            // Normalize `Named(T)` to `Var(T)` so a generic parameter declared as
            // `x: T` behaves as a type variable inside the body.
            let ty = self.normalize_ty(&raw, &params_set);
            self.declare(&p.name, ty);
        }
        let mut body = Vec::new();
        for s in &f.body {
            self.analyze_stmt(s, ret_ty, &mut body)?;
        }
        let mut out = f.clone();
        out.body = body;
        Ok(out)
    }

    // -- statements ---------------------------------------------------------

    fn analyze_stmt(
        &mut self,
        s: &Stmt,
        ret_ty: &Type,
        out: &mut Vec<Stmt>,
    ) -> Result<(), SemanticError> {
        match s {
            Stmt::Let(p, init, sp) => {
                let init_ty = self.analyze_expr(init)?;
                let declared = p.ty.clone();
                let ty = match (&declared, init_ty.clone()) {
                    (Some(d), _) => {
                        check_compat(d, &init_ty, *sp)?;
                        d.clone()
                    }
                    (None, Type::Void) => {
                        return Err(serr(*sp, "cannot infer type of `let {}` (no initializer and no annotation)"));
                    }
                    (None, t) => t,
                };
                self.declare(&p.name, ty.clone());
                // Record the inferred type on the AST so the code generator can
                // reconstruct static types (pointer-ness, field access, etc.).
                out.push(Stmt::Let(
                    Param { name: p.name.clone(), ty: Some(ty.clone()), span: p.span },
                    init.clone(),
                    *sp,
                ));
            }
            Stmt::Expr(e, sp) => {
                self.analyze_expr(e)?;
                out.push(Stmt::Expr(e.clone(), *sp));
            }
            Stmt::Return(e, sp) => {
                let t = match e {
                    Some(e) => {
                        let t = self.analyze_expr(e)?;
                        check_compat(ret_ty, &t, *sp)?;
                        out.push(Stmt::Return(Some(e.clone()), *sp));
                        t
                    }
                    None => {
                        out.push(Stmt::Return(None, *sp));
                        Type::Void
                    }
                };
                if *ret_ty == Type::Void && t != Type::Void {
                    return Err(serr(*sp, "function returns void but `return` produced a value"));
                }
            }
            Stmt::If(cond, then_b, else_b, sp) => {
                let ct = self.analyze_expr(cond)?;
                if ct != Type::Bool {
                    return Err(serr(*sp, format!("`if` condition must be bool, got {}", ct.display())));
                }
                let mut tb = Vec::new();
                self.push_frame();
                for s in then_b {
                    self.analyze_stmt(s, ret_ty, &mut tb)?;
                }
                self.pop_frame();
                let mut eb = None;
                if let Some(ebs) = else_b {
                    let mut ebs2 = Vec::new();
                    self.push_frame();
                    for s in ebs {
                        self.analyze_stmt(s, ret_ty, &mut ebs2)?;
                    }
                    self.pop_frame();
                    eb = Some(ebs2);
                }
                out.push(Stmt::If(cond.clone(), tb, eb, *sp));
            }
            Stmt::While(cond, body, sp) => {
                let ct = self.analyze_expr(cond)?;
                if ct != Type::Bool {
                    return Err(serr(*sp, "`while` condition must be bool"));
                }
                let mut nb = Vec::new();
                self.push_frame();
                self.loop_depth += 1;
                for s in body {
                    self.analyze_stmt(s, ret_ty, &mut nb)?;
                }
                self.loop_depth -= 1;
                self.pop_frame();
                out.push(Stmt::While(cond.clone(), nb, *sp));
            }
            Stmt::For(init, cond, step, body, sp) => {
                let mut ni = Vec::new();
                self.push_frame();
                self.loop_depth += 1;
                for s in init {
                    self.analyze_stmt(s, ret_ty, &mut ni)?;
                }
                if let Some(c) = cond {
                    let ct = self.analyze_expr(c)?;
                    if ct != Type::Bool {
                        return Err(serr(*sp, "`for` condition must be bool"));
                    }
                }
                if let Some(st) = step {
                    self.analyze_expr(st)?;
                }
                let mut nb = Vec::new();
                for s in body {
                    self.analyze_stmt(s, ret_ty, &mut nb)?;
                }
                self.loop_depth -= 1;
                self.pop_frame();
                out.push(Stmt::For(ni, cond.clone(), step.clone(), nb, *sp));
            }
            Stmt::Break(sp) => {
                if self.loop_depth == 0 {
                    return Err(serr(*sp, "`break` used outside a loop"));
                }
                out.push(Stmt::Break(*sp));
            }
            Stmt::Continue(sp) => {
                if self.loop_depth == 0 {
                    return Err(serr(*sp, "`continue` used outside a loop"));
                }
                out.push(Stmt::Continue(*sp));
            }
            Stmt::Directive(a, sp) => {
                out.push(Stmt::Directive(a.clone(), *sp));
            }
            Stmt::Try(body, catch, fin, sp) => {
                let mut nb = Vec::new();
                self.push_frame();
                for s in body {
                    self.analyze_stmt(s, ret_ty, &mut nb)?;
                }
                self.pop_frame();
                let mut nc = None;
                if let Some((name, cbody, cspan)) = catch {
                    let mut ncb = Vec::new();
                    self.push_frame();
                    // catch variable may hold any thrown value
                    self.declare(name, Type::Var("Any".into()));
                    for s in cbody {
                        self.analyze_stmt(s, ret_ty, &mut ncb)?;
                    }
                    self.pop_frame();
                    nc = Some((name.clone(), ncb, *cspan));
                }
                let mut nf = None;
                if let Some(fb) = fin {
                    let mut nfb = Vec::new();
                    self.push_frame();
                    for s in fb {
                        self.analyze_stmt(s, ret_ty, &mut nfb)?;
                    }
                    self.pop_frame();
                    nf = Some(nfb);
                }
                out.push(Stmt::Try(nb, nc, nf, *sp));
            }
            Stmt::Throw(e, sp) => {
                // any value may be thrown; checked dynamically by the VM
                self.analyze_expr(e)?;
                out.push(Stmt::Throw(e.clone(), *sp));
            }
        }
        Ok(())
    }

    // -- expressions --------------------------------------------------------

    fn analyze_expr(&mut self, e: &Expr) -> Result<Type, SemanticError> {
        match e {
            Expr::IntLit(_, _) => Ok(Type::Int),
            Expr::FloatLit(_, _) => Ok(Type::Float),
            Expr::BoolLit(_, _) => Ok(Type::Bool),
            Expr::StrLit(_, _) => Ok(Type::Str),
            Expr::NullLit(_) => Ok(Type::Void),
            Expr::Ident(name, sp) => {
                let (ty, _, is_capture) = self.resolve(name).map_err(|err| {
                    SemanticError { msg: err.msg, line: sp.line, col: sp.col }
                })?;
                if is_capture {
                    self.add_capture(name);
                }
                Ok(ty)
            }
            Expr::Binary(op, l, r, sp) => self.analyze_binary(*op, l, r, *sp),
            Expr::Unary(op, inner, sp) => self.analyze_unary(*op, inner, *sp),
            Expr::Call(callee, args, sp) => self.analyze_call(callee, args, *sp),
            Expr::Field(obj, name, sp) => {
                let ot = self.analyze_expr(obj)?;
                let target = deref_type(&ot);
                match target {
                    Type::Named(n) => {
                        let layout = self.resolve_struct_layout(&n).ok_or_else(|| serr(*sp, format!("unknown struct `{n}`")))?;
                        let f = layout.fields.iter().find(|f| &f.name == name).ok_or_else(|| {
                            serr(*sp, format!("`{n}` has no field `{name}`"))
                        })?;
                        Ok(f.ty.clone())
                    }
                    Type::Generic(n, inst) => {
                        let layout = self.resolve_struct_layout(&n).ok_or_else(|| serr(*sp, format!("unknown struct `{n}`")))?;
                        let f = layout.fields.iter().find(|f| &f.name == name).ok_or_else(|| {
                            serr(*sp, format!("`{n}` has no field `{name}`"))
                        })?;
                        // substitute the instantiation's type args for the template's vars
                        let tps = self.struct_tparams.get(&n).cloned().unwrap_or_default();
                        let map: HashMap<String, Type> = tps.iter().cloned().zip(inst.iter().cloned()).collect();
                        Ok(Self::subst_ty(&f.ty, &map))
                    }
                    other => Err(serr(*sp, format!("cannot access field `{name}` on `{}`", other.display()))),
                }
            }
            Expr::MethodCall(obj, name, args, sp) => self.analyze_method(obj, name, args, *sp),
            Expr::Assign(target, value, sp) => {
                let tt = self.analyze_expr(target)?;
                let vt = self.analyze_expr(value)?;
                // allow implicit deref on pointer assignment target
                let effective = match &tt {
                    Type::Ptr { target, .. } => (**target).clone(),
                    t => t.clone(),
                };
                check_compat(&effective, &vt, *sp)?;
                Ok(effective)
            }
            Expr::StructLit(name, fields, sp) => {
                let layout = self
                    .resolve_struct_layout(name)
                    .ok_or_else(|| serr(*sp, format!("unknown struct `{name}`")))?
                    .clone();
                let mut provided = HashSet::new();
                for (fname, fexpr) in fields {
                    let ft = self.analyze_expr(fexpr)?;
                    let fl = layout
                        .fields
                        .iter()
                        .find(|f| &f.name == fname)
                        .ok_or_else(|| serr(*sp, format!("`{name}` has no field `{fname}`")))?;
                    check_compat(&fl.ty, &ft, *sp)?;
                    provided.insert(fname.clone());
                }
                for fl in &layout.fields {
                    if !provided.contains(&fl.name) {
                        return Err(serr(*sp, format!("struct literal `{name}` missing field `{}`", fl.name)));
                    }
                }
                Ok(Type::Named(name.clone()))
            }
            Expr::ArrayLit(elems, sp) => {
                let mut elem: Option<Type> = None;
                for el in elems {
                    let t = self.analyze_expr(el)?;
                    match &elem {
                        None => elem = Some(t),
                        Some(prev) => check_compat(prev, &t, *sp)?,
                    }
                }
                Ok(Type::List(Box::new(elem.unwrap_or(Type::Void))))
            }
            Expr::MapLit(entries, sp) => {
                let mut kt: Option<Type> = None;
                let mut vt: Option<Type> = None;
                for (k, v) in entries {
                    let k2 = self.analyze_expr(k)?;
                    let v2 = self.analyze_expr(v)?;
                    match &kt {
                        None => kt = Some(k2),
                        Some(prev) => check_compat(prev, &k2, *sp)?,
                    }
                    match &vt {
                        None => vt = Some(v2),
                        Some(prev) => check_compat(prev, &v2, *sp)?,
                    }
                }
                Ok(Type::Map(Box::new(kt.unwrap_or(Type::Void)), Box::new(vt.unwrap_or(Type::Void))))
            }
            Expr::Index(obj, idx, sp) => {
                let ot = self.analyze_expr(obj)?;
                let target = deref_type(&ot);
                let it = self.analyze_expr(idx)?;
                match target {
                    Type::List(e) | Type::Array(e) => {
                        check_compat(&Type::Int, &it, *sp)?;
                        Ok(*e)
                    }
                    Type::Map(k, v) => {
                        check_compat(&k, &it, *sp)?;
                        Ok(*v)
                    }
                    other => Err(serr(*sp, format!("cannot index into `{}`", other.display()))),
                }
            }
            Expr::Closure(c) => self.analyze_closure(c),
            Expr::Block(stmts, sp) => {
                self.push_frame();
                let mut last = Type::Void;
                for s in stmts {
                    // replicate statement analysis, but blocks-as-expr use last expr type
                    let mut tmp = Vec::new();
                    self.analyze_stmt(s, &Type::Void, &mut tmp)?;
                    if let Stmt::Expr(e, _) = s {
                        last = self.analyze_expr(e)?;
                    }
                }
                self.pop_frame();
                let _ = sp;
                Ok(last)
            }
            Expr::IfExpr(cond, then_e, else_e, sp) => {
                let ct = self.analyze_expr(cond)?;
                if ct != Type::Bool {
                    return Err(serr(*sp, "if-expression condition must be bool"));
                }
                let tt = self.analyze_expr(then_e)?;
                let et = match else_e {
                    Some(x) => self.analyze_expr(x)?,
                    None => Type::Void,
                };
                if tt != et && et != Type::Void {
                    return Err(serr(*sp, "if-expression branches must have the same type"));
                }
                Ok(tt)
            }
            Expr::Ternary(cond, then_e, else_e, sp) => {
                let ct = self.analyze_expr(cond)?;
                if ct != Type::Bool {
                    return Err(serr(*sp, "ternary condition must be bool"));
                }
                let tt = self.analyze_expr(then_e)?;
                let et = self.analyze_expr(else_e)?;
                if tt != et {
                    return Err(serr(*sp, "ternary branches must have the same type"));
                }
                Ok(tt)
            }
            Expr::InterpStr(parts, _sp) => {
                for p in parts {
                    if let InterpPart::Expr(e) = p {
                        self.analyze_expr(e)?;
                    }
                }
                Ok(Type::Str)
            }
            Expr::Path(segs, sp) => {
                let full = segs.join("::");
                if let Some(sig) = self.funcs.get(&full).cloned() {
                    Ok(Type::Fn { params: sig.params, ret: Box::new(sig.ret) })
                } else {
                    Err(serr(*sp, format!("unknown module path `{full}`")))
                }
            }
            Expr::GenericCall(name, targs, args, sp) => self.analyze_generic_call(name, targs, args, *sp),
            Expr::GenericStructLit(name, targs, fields, sp) => {
                self.analyze_generic_struct_lit(name, targs, fields, *sp)
            }
        }
    }

    fn analyze_generic_call(
        &mut self,
        name: &str,
        targs: &[Type],
        args: &[Expr],
        sp: Span,
    ) -> Result<Type, SemanticError> {
        // `no-api-check`: unknown generic functions are host-provided API calls
        // (resolved at runtime); skip the existence check and treat the result
        // as an untyped value.
        if self.no_api_check && !self.funcs.contains_key(name) {
            for a in args {
                self.analyze_expr(a)?;
            }
            return Ok(Type::Var("Any".into()));
        }
        let sig = self.funcs.get(name).cloned().ok_or_else(|| serr(sp, format!("unknown function `{name}`")))?;
        let tps = self.func_type_params.get(name).cloned().unwrap_or_default();
        if targs.len() != tps.len() {
            return Err(serr(sp, format!("`{name}` expects {} type arguments, got {}", tps.len(), targs.len())));
        }
        let map: HashMap<String, Type> = tps.iter().cloned().zip(targs.iter().cloned()).collect();
        let checked = FuncSig {
            params: sig.params.iter().map(|p| Self::subst_ty(p, &map)).collect(),
            ret: Self::subst_ty(&sig.ret, &map),
        };
        let mut arg_types = Vec::new();
        for a in args {
            arg_types.push(self.analyze_expr(a)?);
        }
        if checked.params.len() != arg_types.len() {
            return Err(serr(sp, format!("`{name}` expects {} args, got {}", checked.params.len(), arg_types.len())));
        }
        for (i, (pt, at)) in checked.params.iter().zip(arg_types.iter()).enumerate() {
            check_compat(pt, at, sp)
                .map_err(|e| SemanticError { msg: format!("arg {} of `{name}`: {}", i, e.msg), line: sp.line, col: sp.col })?;
        }
        Ok(checked.ret)
    }

    fn analyze_generic_struct_lit(
        &mut self,
        name: &str,
        targs: &[Type],
        fields: &[(String, Expr)],
        sp: Span,
    ) -> Result<Type, SemanticError> {
        let layout = self
            .resolve_struct_layout(name)
            .cloned()
            .ok_or_else(|| serr(sp, format!("unknown struct `{name}`")))?;
        let tps = self.struct_tparams.get(name).cloned().unwrap_or_default();
        if targs.len() != tps.len() {
            return Err(serr(sp, format!("`{name}` expects {} type arguments, got {}", tps.len(), targs.len())));
        }
        let map: HashMap<String, Type> = tps.iter().cloned().zip(targs.iter().cloned()).collect();
        let mut provided = HashSet::new();
        for (fname, fexpr) in fields {
            let ft = self.analyze_expr(fexpr)?;
            let fl = layout
                .fields
                .iter()
                .find(|f| &f.name == fname)
                .ok_or_else(|| serr(sp, format!("`{name}` has no field `{fname}`")))?;
            let expected = Self::subst_ty(&fl.ty, &map);
            check_compat(&expected, &ft, sp)?;
            provided.insert(fname.clone());
        }
        for fl in &layout.fields {
            if !provided.contains(&fl.name) {
                return Err(serr(sp, format!("struct literal `{name}` missing field `{}`", fl.name)));
            }
        }
        Ok(Type::Generic(name.to_string(), targs.to_vec()))
    }

    fn add_capture(&mut self, name: &str) {
        if !self.captures.iter().any(|c| c == name) {
            self.captures.push(name.to_string());
        }
    }

    fn analyze_binary(&mut self, op: BinOp, l: &Expr, r: &Expr, sp: Span) -> Result<Type, SemanticError> {
        let lt = self.analyze_expr(l)?;
        let rt = self.analyze_expr(r)?;
        match op {
            BinOp::Add => {
                // String concatenation: a string operand on either side yields a
                // string (the other operand is coerced to its string form).
                if lt == Type::Str || rt == Type::Str {
                    return Ok(Type::Str);
                }
                if lt == Type::Int && rt == Type::Int {
                    return Ok(Type::Int);
                }
                if lt == Type::Float && rt == Type::Float {
                    return Ok(Type::Float);
                }
                if matches!(lt, Type::Var(_)) || matches!(rt, Type::Var(_)) {
                    // inside a generic body: keep the concrete side if any
                    return Ok(if matches!(lt, Type::Var(_)) { rt } else { lt });
                }
                Err(serr(sp, format!("cannot add `{}` and `{}`", lt.display(), rt.display())))
            }
            BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod => {
                if lt == rt && (lt == Type::Int || lt == Type::Float) {
                    return Ok(lt);
                }
                if matches!(lt, Type::Var(_)) || matches!(rt, Type::Var(_)) {
                    // assume the type variable is numeric
                    return Ok(if matches!(lt, Type::Var(_)) { rt } else { lt });
                }
                Err(serr(sp, format!("cannot apply `{}` to `{}` and `{}`", op.symbol(), lt.display(), rt.display())))
            }
            BinOp::Eq | BinOp::Ne => {
                check_compat(&lt, &rt, sp)?;
                Ok(Type::Bool)
            }
            BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                if lt == rt && (lt == Type::Int || lt == Type::Float) {
                    return Ok(Type::Bool);
                }
                if matches!(lt, Type::Var(_)) || matches!(rt, Type::Var(_)) {
                    return Ok(Type::Bool);
                }
                Err(serr(sp, format!("cannot compare `{}` and `{}`", lt.display(), rt.display())))
            }
            BinOp::And | BinOp::Or => {
                if lt == Type::Bool && rt == Type::Bool {
                    return Ok(Type::Bool);
                }
                if matches!(lt, Type::Var(_)) || matches!(rt, Type::Var(_)) {
                    return Ok(Type::Bool);
                }
                Err(serr(sp, "logical operators require bool operands"))
            }
        }
    }

    fn analyze_unary(&mut self, op: UnOp, inner: &Expr, sp: Span) -> Result<Type, SemanticError> {
        let it = self.analyze_expr(inner)?;
        match op {
            UnOp::Neg => {
                if it == Type::Int || it == Type::Float || matches!(it, Type::Var(_)) {
                    Ok(it)
                } else {
                    Err(serr(sp, format!("cannot negate `{}`", it.display())))
                }
            }
            UnOp::Not => {
                if it == Type::Bool || matches!(it, Type::Var(_)) {
                    Ok(Type::Bool)
                } else {
                    Err(serr(sp, "`!` requires a bool"))
                }
            }
            UnOp::Addr => {
                self.prove_pointer(inner, false, sp)?;
                Ok(Type::Ptr { mutable: false, target: Box::new(it) })
            }
            UnOp::AddrMut => {
                self.prove_pointer(inner, true, sp)?;
                Ok(Type::Ptr { mutable: true, target: Box::new(it) })
            }
            UnOp::Deref => {
                match it {
                    Type::Ptr { target, .. } => Ok((*target).clone()),
                    _ => Err(serr(sp, format!("cannot dereference non-pointer `{}`", it.display()))),
                }
            }
        }
    }

    /// Prove a pointer created from `&x` / `&mut x` where `x` is an algebraic path.
    fn prove_pointer(&mut self, inner: &Expr, mutable: bool, sp: Span) -> Result<(), SemanticError> {
        let root_ty = match inner {
            Expr::Ident(_, _) => {
                // pointer to a local; compute its type via resolve
                match inner {
                    Expr::Ident(name, isp) => {
                        let (ty, _, _) = self.resolve(name).map_err(|err| SemanticError {
                            msg: err.msg,
                            line: isp.line,
                            col: isp.col,
                        })?;
                        ty
                    }
                    _ => unreachable!(),
                }
            }
            _ => self.analyze_expr(inner)?,
        };
        if let Some((_, steps)) = super::pointer::root_path(inner) {
            if !steps.is_empty() {
                // pointer into a field: prove the field exists and offset is in-bounds
                let ptr_region = self.current_region();
                let pointee_region = pointee_region_of(&root_ty);
                self.checker
                    .prove(&root_ty, &steps, &self.structs, mutable, ptr_region, pointee_region)
                    .map_err(|m| serr(sp, m))?;
            }
        }
        Ok(())
    }

    fn current_region(&self) -> Region {
        // Pointers created from an address-of expression default to the
        // enclosing scope region (Scoped). Per-function heap/stack regions are
        // tracked separately in `self.regions`; the scope region always outlives
        // stack values, which keeps `&local` legal within its frame.
        Region::Scoped
    }

    fn analyze_call(&mut self, callee: &Expr, args: &[Expr], sp: Span) -> Result<Type, SemanticError> {
        // Built-in output / input / terminal functions (language-native, no
        // `extern` declaration needed).
        if let Expr::Ident(name, _) = callee {
            match name.as_str() {
                "say" | "write" => {
                    if args.len() != 1 {
                        return Err(serr(sp, format!("`{name}` expects exactly 1 argument")));
                    }
                    let t = self.analyze_expr(&args[0])?;
                    let _ = t;
                    return Ok(Type::Void);
                }
                // CLI: command-line arguments.
                "args" => {
                    if args.len() != 0 {
                        return Err(serr(sp, "`args` takes no arguments"));
                    }
                    return Ok(Type::List(Box::new(Type::Str)));
                }
                // CLI: interactive input.
                "ask" | "readLine" => {
                    if args.len() > 1 {
                        return Err(serr(sp, format!("`{name}` expects at most 1 argument (optional prompt)")));
                    }
                    if let Some(p) = args.first() {
                        let t = self.analyze_expr(p)?;
                        check_compat(&Type::Str, &t, sp)?;
                    }
                    return Ok(Type::Str);
                }
                // CLI: formatted output / formatted string.
                "printf" => {
                    if args.is_empty() {
                        return Err(serr(sp, "`printf` expects a format string"));
                    }
                    let ft = self.analyze_expr(&args[0])?;
                    check_compat(&Type::Str, &ft, sp)?;
                    for a in &args[1..] {
                        self.analyze_expr(a)?;
                    }
                    return Ok(Type::Void);
                }
                "fmt" => {
                    if args.is_empty() {
                        return Err(serr(sp, "`fmt` expects a format string"));
                    }
                    let ft = self.analyze_expr(&args[0])?;
                    check_compat(&Type::Str, &ft, sp)?;
                    for a in &args[1..] {
                        self.analyze_expr(a)?;
                    }
                    return Ok(Type::Str);
                }
                // TUI: ANSI terminal control.
                "clear_screen" | "reset_color" | "hide_cursor" | "show_cursor" => {
                    if args.len() != 0 {
                        return Err(serr(sp, format!("`{name}` takes no arguments")));
                    }
                    return Ok(Type::Void);
                }
                "cursor" | "color" => {
                    if args.len() != 2 {
                        return Err(serr(sp, format!("`{name}` expects exactly 2 arguments")));
                    }
                    for a in args {
                        let t = self.analyze_expr(a)?;
                        check_compat(&Type::Int, &t, sp)?;
                    }
                    return Ok(Type::Void);
                }
                "key" => {
                    if args.len() != 0 {
                        return Err(serr(sp, "`key` takes no arguments"));
                    }
                    return Ok(Type::Str);
                }
                "key_available" => {
                    if args.len() != 0 {
                        return Err(serr(sp, "`key_available` takes no arguments"));
                    }
                    return Ok(Type::Bool);
                }
                "terminal_cols" | "terminal_rows" => {
                    if args.len() != 0 {
                        return Err(serr(sp, format!("`{name}` takes no arguments")));
                    }
                    return Ok(Type::Int);
                }
                // GUI: window / drawing / events.
                "window" => {
                    if args.len() != 3 {
                        return Err(serr(sp, "`window` expects exactly 3 arguments (title, width, height)"));
                    }
                    let t = self.analyze_expr(&args[0])?;
                    check_compat(&Type::Str, &t, sp)?;
                    for a in &args[1..] {
                        let t = self.analyze_expr(a)?;
                        check_compat(&Type::Int, &t, sp)?;
                    }
                    return Ok(Type::Int);
                }
                "window_close" | "window_title" => {
                    if args.is_empty() {
                        return Err(serr(sp, format!("`{name}` expects arguments")));
                    }
                    self.analyze_expr(&args[0])?;
                    if name == "window_title" {
                        if args.len() != 2 {
                            return Err(serr(sp, "`window_title` expects exactly 2 arguments (id, title)"));
                        }
                        let t = self.analyze_expr(&args[1])?;
                        check_compat(&Type::Str, &t, sp)?;
                    } else if args.len() != 1 {
                        return Err(serr(sp, "`window_close` expects exactly 1 argument (id)"));
                    }
                    return Ok(Type::Void);
                }
                "draw_text" => {
                    if args.len() != 4 {
                        return Err(serr(sp, "`draw_text` expects 4 arguments (id, x, y, text)"));
                    }
                    for a in &args[0..3] {
                        let t = self.analyze_expr(a)?;
                        check_compat(&Type::Int, &t, sp)?;
                    }
                    let t = self.analyze_expr(&args[3])?;
                    check_compat(&Type::Str, &t, sp)?;
                    return Ok(Type::Void);
                }
                "draw_rect" | "fill_rect" => {
                    if args.len() != 6 {
                        return Err(serr(sp, format!("`{name}` expects 6 arguments (id, x, y, w, h, color)")));
                    }
                    for a in args {
                        let t = self.analyze_expr(a)?;
                        check_compat(&Type::Int, &t, sp)?;
                    }
                    return Ok(Type::Void);
                }
                "clear_canvas" => {
                    if args.len() != 1 {
                        return Err(serr(sp, "`clear_canvas` expects 1 argument (id)"));
                    }
                    let t = self.analyze_expr(&args[0])?;
                    check_compat(&Type::Int, &t, sp)?;
                    return Ok(Type::Void);
                }
                "on_key" | "on_click" => {
                    if args.len() != 2 {
                        return Err(serr(sp, format!("`{name}` expects exactly 2 arguments (id, handler)")));
                    }
                    self.analyze_expr(&args[0])?;
                    self.analyze_expr(&args[1])?;
                    return Ok(Type::Void);
                }
                "event_loop" => {
                    if args.len() != 0 {
                        return Err(serr(sp, "`event_loop` takes no arguments"));
                    }
                    return Ok(Type::Void);
                }
                "rgb" => {
                    if args.len() != 3 {
                        return Err(serr(sp, "`rgb` expects exactly 3 arguments (r, g, b)"));
                    }
                    for a in args {
                        let t = self.analyze_expr(a)?;
                        check_compat(&Type::Int, &t, sp)?;
                    }
                    return Ok(Type::Int);
                }
                _ => {}
            }
            // `thread_id()` -> int and `sleep(ms)` -> void: runtime concurrency
            // helpers, resolved as builtins so they need no `extern` declaration.
            if name == "thread_id" {
                if args.len() != 0 {
                    return Err(serr(sp, "`thread_id` takes no arguments"));
                }
                return Ok(Type::Int);
            }
            if name == "sleep" {
                if args.len() != 1 {
                    return Err(serr(sp, "`sleep` expects exactly 1 argument"));
                }
                let t = self.analyze_expr(&args[0])?;
                check_compat(&Type::Int, &t, sp)?;
                return Ok(Type::Void);
            }
        }
        // Resolve the callee to a function name if it is a plain identifier or a
        // module path (`a::b`).
        let callee_name = match callee {
            Expr::Ident(n, _) => Some(n.clone()),
            Expr::Path(segs, _) => Some(segs.join("::")),
            _ => None,
        };
        let mut arg_types = Vec::new();
        for a in args {
            arg_types.push(self.analyze_expr(a)?);
        }
        if let Some(name) = &callee_name {
            let resolved = self.resolve_func_name(name);
            if let Some(sig) = self.funcs.get(&resolved).cloned() {
                // Generic function without explicit type arguments: infer the
                // type variables from the argument types, then type-check.
                let checked = if let Some(_tps) = self.func_type_params.get(&resolved).cloned() {
                    let mut map = HashMap::new();
                    for (i, pt) in sig.params.iter().enumerate() {
                        if let Some(at) = arg_types.get(i) {
                            if let Type::Var(v) = pt {
                                map.entry(v.clone()).or_insert_with(|| at.clone());
                            }
                        }
                    }
                    FuncSig {
                        params: sig.params.iter().map(|p| Self::subst_ty(p, &map)).collect(),
                        ret: Self::subst_ty(&sig.ret, &map),
                    }
                } else {
                    sig
                };
                if checked.params.len() != arg_types.len() {
                    return Err(serr(sp, format!("`{name}` expects {} args, got {}", checked.params.len(), arg_types.len())));
                }
                for (i, (pt, at)) in checked.params.iter().zip(arg_types.iter()).enumerate() {
                    check_compat(pt, at, sp).map_err(|e| {
                        SemanticError {
                            msg: format!("arg {} of `{name}`: {}", i, e.msg),
                            line: sp.line,
                            col: sp.col,
                        }
                    })?;
                }
                return Ok(checked.ret);
            }
            // `no-api-check`: the name is neither defined here nor declared
            // `extern`; treat it as a host-provided API call (resolved at
            // runtime via `PSS_RUST_LIB`). Skip the existence check. This is
            // only allowed when the callee is a plain name (not a local
            // closure variable, which is handled by the closure path below).
            if self.no_api_check && !self.is_local(name) {
                return Ok(Type::Var("Any".into()));
            }
        }
        // Closure / function-typed callee
        let ct = self.analyze_expr(callee)?;
        match ct {
            Type::Fn { params, ret } => {
                if params.len() != arg_types.len() {
                    return Err(serr(sp, format!("closure expects {} args, got {}", params.len(), arg_types.len())));
                }
                for (pt, at) in params.iter().zip(arg_types.iter()) {
                    check_compat(pt, at, sp)?;
                }
                Ok(*ret)
            }
            other => Err(serr(sp, format!("cannot call value of type `{}`", other.display()))),
        }
    }

    fn analyze_method(&mut self, obj: &Expr, name: &str, args: &[Expr], sp: Span) -> Result<Type, SemanticError> {
        let ot = self.analyze_expr(obj)?;
        let target = deref_type(&ot);
        // Container methods.
        match &target {
            Type::List(elem) => match name {
                "push" => {
                    if args.len() != 1 {
                        return Err(serr(sp, "`push` expects exactly 1 argument"));
                    }
                    let a = self.analyze_expr(&args[0])?;
                    check_compat(elem, &a, sp)?;
                    return Ok(Type::Void);
                }
                "pop" => {
                    for a in args {
                        self.analyze_expr(a)?;
                    }
                    return Ok((**elem).clone());
                }
                "len" => {
                    for a in args {
                        self.analyze_expr(a)?;
                    }
                    return Ok(Type::Int);
                }
                "get" => {
                    if args.len() != 1 {
                        return Err(serr(sp, "`get` expects exactly 1 argument"));
                    }
                    let a = self.analyze_expr(&args[0])?;
                    check_compat(&Type::Int, &a, sp)?;
                    return Ok((**elem).clone());
                }
                "set" => {
                    if args.len() != 2 {
                        return Err(serr(sp, "`set` expects exactly 2 arguments"));
                    }
                    let a = self.analyze_expr(&args[0])?;
                    check_compat(&Type::Int, &a, sp)?;
                    let b = self.analyze_expr(&args[1])?;
                    check_compat(elem, &b, sp)?;
                    return Ok(Type::Void);
                }
                _ => {}
            },
            Type::Array(elem) => match name {
                "len" => {
                    for a in args {
                        self.analyze_expr(a)?;
                    }
                    return Ok(Type::Int);
                }
                "get" => {
                    if args.len() != 1 {
                        return Err(serr(sp, "`get` expects exactly 1 argument"));
                    }
                    let a = self.analyze_expr(&args[0])?;
                    check_compat(&Type::Int, &a, sp)?;
                    return Ok((**elem).clone());
                }
                "set" => {
                    if args.len() != 2 {
                        return Err(serr(sp, "`set` expects exactly 2 arguments"));
                    }
                    let a = self.analyze_expr(&args[0])?;
                    check_compat(&Type::Int, &a, sp)?;
                    let b = self.analyze_expr(&args[1])?;
                    check_compat(elem, &b, sp)?;
                    return Ok(Type::Void);
                }
                _ => {}
            },
            Type::Map(k, v) => match name {
                "set" => {
                    if args.len() != 2 {
                        return Err(serr(sp, "`set` expects exactly 2 arguments"));
                    }
                    let a = self.analyze_expr(&args[0])?;
                    check_compat(k, &a, sp)?;
                    let b = self.analyze_expr(&args[1])?;
                    check_compat(v, &b, sp)?;
                    return Ok(Type::Void);
                }
                "get" => {
                    if args.len() != 1 {
                        return Err(serr(sp, "`get` expects exactly 1 argument"));
                    }
                    let a = self.analyze_expr(&args[0])?;
                    check_compat(k, &a, sp)?;
                    return Ok((**v).clone());
                }
                "has" => {
                    if args.len() != 1 {
                        return Err(serr(sp, "`has` expects exactly 1 argument"));
                    }
                    let a = self.analyze_expr(&args[0])?;
                    check_compat(k, &a, sp)?;
                    return Ok(Type::Bool);
                }
                "len" => {
                    for a in args {
                        self.analyze_expr(a)?;
                    }
                    return Ok(Type::Int);
                }
                "remove" => {
                    if args.len() != 1 {
                        return Err(serr(sp, "`remove` expects exactly 1 argument"));
                    }
                    let a = self.analyze_expr(&args[0])?;
                    check_compat(k, &a, sp)?;
                    return Ok(Type::Bool);
                }
                "keys" => {
                    for a in args {
                        self.analyze_expr(a)?;
                    }
                    return Ok(Type::List(Box::new((**k).clone())));
                }
                _ => {}
            },
            _ => {}
        }
        // User-defined methods (inherent `impl T` or `impl Trait for T`): static
        // dispatch resolved at compile time to the impl's function.
        match &target {
            Type::Named(_) | Type::Generic(..) => {
                let (tn, inst): (String, Vec<Type>) = match &target {
                    Type::Named(n) => (n.clone(), Vec::new()),
                    Type::Generic(n, args) => (n.clone(), args.clone()),
                    _ => unreachable!(),
                };
                if let Some(fname) = self.impl_resolver.get(&(tn, name.to_string())).cloned() {
                    if let Some(sig) = self.funcs.get(&fname).cloned() {
                        let checked = if let Some(tps) = self.impl_type_params.get(&fname).cloned() {
                            let map: HashMap<String, Type> =
                                tps.iter().cloned().zip(inst.iter().cloned()).collect();
                            FuncSig {
                                params: sig.params.iter().map(|p| Self::subst_ty(p, &map)).collect(),
                                ret: Self::subst_ty(&sig.ret, &map),
                            }
                        } else {
                            sig
                        };
                        if checked.params.len().saturating_sub(1) != args.len() {
                            return Err(serr(sp, format!(
                                "`{name}` expects {} args, got {}",
                                checked.params.len() - 1,
                                args.len()
                            )));
                        }
                        let mut arg_types = Vec::new();
                        for a in args {
                            arg_types.push(self.analyze_expr(a)?);
                        }
                        for (i, (pt, at)) in checked.params.iter().skip(1).zip(arg_types.iter()).enumerate() {
                            check_compat(pt, at, sp).map_err(|e| SemanticError {
                                msg: format!("arg {} of `{name}`: {}", i, e.msg),
                                line: sp.line,
                                col: sp.col,
                            })?;
                        }
                        return Ok(checked.ret);
                    }
                }
            }
            _ => {}
        }
        // Dynamic dispatch through a trait object (`dyn Trait`) or a bounded
        // type variable (`T: Trait`): the concrete receiver type is unknown at
        // compile time, so the call is resolved against the trait's declared
        // signature and dispatched at runtime by the receiver's type tag.
        let dyn_trait = match &target {
            Type::Trait(t) => Some(t.clone()),
            Type::Var(v) => self.cur_bounds.get(v).cloned(),
            _ => None,
        };
        if let Some(tr) = dyn_trait {
            if let Some(tm) = self.trait_methods.get(&(tr.clone(), name.to_string())).cloned() {
                if tm.params.len() != args.len() {
                    return Err(serr(sp, format!(
                        "`{name}` expects {} args, got {}",
                        tm.params.len(),
                        args.len()
                    )));
                }
                let mut arg_types = Vec::new();
                for a in args {
                    arg_types.push(self.analyze_expr(a)?);
                }
                for (i, (pt, at)) in tm.params.iter().zip(arg_types.iter()).enumerate() {
                    let pt = pt.ty.clone().unwrap_or(Type::Int);
                    check_compat(&pt, at, sp).map_err(|e| SemanticError {
                        msg: format!("arg {} of `{name}`: {}", i, e.msg),
                        line: sp.line,
                        col: sp.col,
                    })?;
                }
                return Ok(tm.ret.clone().unwrap_or(Type::Void));
            }
            return Err(serr(sp, format!("trait `{tr}` has no method `{name}`")));
        }
        // Built-in String methods (dynamically resolved in the VM).
        match (&target, name) {
            (Type::Str, "len") | (Type::Str, "trim") | (Type::Str, "to_upper") | (Type::Str, "to_lower")
            | (Type::Str, "to_int") | (Type::Str, "to_float") => {
                for a in args {
                    self.analyze_expr(a)?;
                }
                match name {
                    "len" => Ok(Type::Int),
                    "to_int" => Ok(Type::Int),
                    "to_float" => Ok(Type::Float),
                    _ => Ok(Type::Str),
                }
            }
            (Type::Str, "substr") => {
                if args.len() != 2 {
                    return Err(serr(sp, "`substr` expects 2 args"));
                }
                for a in args {
                    self.analyze_expr(a)?;
                }
                Ok(Type::Str)
            }
            _ => Err(serr(sp, format!("no method `{name}` on `{}`", target.display()))),
        }
    }

    fn analyze_closure(&mut self, c: &ClosureExpr) -> Result<Type, SemanticError> {
        // New closure: capture boundary = number of frames currently on stack.
        let saved_base = self.closure_base;
        let saved_captures = std::mem::take(&mut self.captures);
        self.closure_base = self.frames.len();
        self.captures = Vec::new();

        self.push_frame();
        let mut param_types = Vec::new();
        for p in &c.params {
            let ty = p
                .ty
                .clone()
                .ok_or_else(|| serr(p.span, format!("closure parameter `{}` needs a type", p.name)))?;
            self.declare(&p.name, ty.clone());
            param_types.push(ty);
        }
        let ret = match &c.ret {
            Some(t) => t.clone(),
            None => Type::Void,
        };
        // Analyze the closure body against the closure's return type so that
        // `return` statements inside a closure body type-check correctly.
        match &*c.body {
            Expr::Block(stmts, _) => {
                let mut tmp = Vec::new();
                for s in stmts {
                    self.analyze_stmt(s, &ret, &mut tmp)?;
                }
            }
            other => {
                let bt = self.analyze_expr(other)?;
                if ret != Type::Void {
                    check_compat(&ret, &bt, c.span)?;
                }
            }
        }
        self.pop_frame();

        self.captures = saved_captures;
        self.closure_base = saved_base;

        Ok(Type::Fn { params: param_types, ret: Box::new(ret) })
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn deref_type(t: &Type) -> Type {
    match t {
        Type::Ptr { target, .. } => (**target).clone(),
        other => other.clone(),
    }
}

fn check_compat(expected: &Type, actual: &Type, sp: Span) -> Result<(), SemanticError> {
    if expected == actual {
        return Ok(());
    }
    // Generic type variables are wildcards: inside a generic function the
    // compiler cannot know the concrete type yet, so any type unifies with it.
    if matches!(expected, Type::Var(_)) || matches!(actual, Type::Var(_)) {
        return Ok(());
    }
    // implicit deref: a pointer of expected type may be used where its pointee is expected
    if let Type::Ptr { target, .. } = expected {
        if &**target == actual {
            return Ok(());
        }
    }
    // containers are structurally compatible on their outer kind (the runtime is
    // dynamically typed), so `[]` infers to `List[Void]` yet unifies with any
    // `List[T]`, and a `List[int]` may hold whatever is pushed at runtime.
    if matches!(expected, Type::List(_)) && matches!(actual, Type::List(_)) {
        return Ok(());
    }
    if matches!(expected, Type::Array(_)) && matches!(actual, Type::Array(_)) {
        return Ok(());
    }
    if matches!(expected, Type::Map(..)) && matches!(actual, Type::Map(..)) {
        return Ok(());
    }
    // The catch variable of a `try/catch` is typed `Var("Any")` and accepts any
    // thrown value; also allow an unannotated `catch` to hold any value.
    if matches!(expected, Type::Var(_)) {
        return Ok(());
    }
    // A concrete struct may be passed where a trait object is expected; the
    // runtime verifies the impl actually exists when dispatching.
    if matches!(expected, Type::Trait(_))
        && (matches!(actual, Type::Named(_)) || matches!(actual, Type::Generic(..)))
    {
        return Ok(());
    }
    Err(SemanticError {
        msg: format!("type mismatch: expected `{}`, found `{}`", expected.display(), actual.display()),
        line: sp.line,
        col: sp.col,
    })
}

fn region_of(anns: &[Annotation]) -> Option<Region> {
    for a in anns {
        if a.name == "region" {
            if let Some(v) = a.args.first() {
                let s = v.value.as_str().unwrap_or("Scoped");
                return match s {
                    "Stack" => Some(Region::Stack),
                    "Heap" => Some(Region::Heap),
                    "Scoped" => Some(Region::Scoped),
                    _ => None,
                };
            }
        }
    }
    None
}

fn schedule_of(anns: &[Annotation]) -> Option<Schedule> {
    for a in anns {
        match a.name.as_str() {
            "Auto" => return Some(Schedule::Auto),
            "Manual" => {
                let n = a.int_arg("fixed").unwrap_or(4);
                return Some(Schedule::Manual(n as usize));
            }
            _ => {}
        }
    }
    None
}

fn pointee_region_of(t: &Type) -> Region {
    match t {
        Type::Named(_) | Type::Generic(..) => Region::Heap,
        Type::Str => Region::Heap,
        _ => Region::Stack,
    }
}

/// Mangle an impl method into a globally unique function name.
fn impl_func_name(trait_name: &Option<String>, type_name: &str, method: &str) -> String {
    match trait_name {
        Some(t) => format!("impl::{t}::{type_name}::{method}"),
        None => format!("impl::{type_name}::{method}"),
    }
}
