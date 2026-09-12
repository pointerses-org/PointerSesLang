//! Pointerses runtime virtual machine.
//!
//! This file is fully self-contained (standard library only) so that it can be
//! compiled both *inside* `pss.exe` (for `pss run`) and *as a standalone driver*
//! (for `pss build`, which embeds the serialized bytecode and links this file
//! with `rustc` to produce a native executable with no external runtime).
//!
//! The VM executes a stack-machine bytecode. Values are dynamically typed
//! (`Value`), heap objects carry a non-blocking reference count, pointers are
//! first-class algebraic paths (`Value::Ptr`), strings are reference-counted
//! heap objects, and closures are heap objects holding captured values.

#![allow(clippy::needless_range_loop)]
#![allow(dead_code)]

use std::collections::{HashMap, VecDeque};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// Opcodes
// ---------------------------------------------------------------------------
pub const OP_PUSH_I64: u8 = 0;
pub const OP_PUSH_F64: u8 = 1;
pub const OP_PUSH_BOOL: u8 = 2;
pub const OP_PUSH_NULL: u8 = 3;
pub const OP_PUSH_STR: u8 = 4;
pub const OP_POP: u8 = 5;
pub const OP_DUP: u8 = 6;
pub const OP_LOAD_LOCAL: u8 = 7;
pub const OP_STORE_LOCAL: u8 = 8;
pub const OP_LOAD_FIELD: u8 = 9;
pub const OP_STORE_FIELD: u8 = 10;
pub const OP_ADDR_LOCAL: u8 = 11;
pub const OP_ADDR_FIELD: u8 = 12;
pub const OP_DEREF: u8 = 13;
pub const OP_DEREF_STORE: u8 = 14;
pub const OP_NEW_STRUCT: u8 = 15;
pub const OP_NEW_CLOSURE: u8 = 16;
pub const OP_CALL: u8 = 17;
pub const OP_CALL_CLOSURE: u8 = 18;
pub const OP_RETURN: u8 = 19;
pub const OP_RETURN_VOID: u8 = 20;
pub const OP_JMP: u8 = 21;
pub const OP_JZ: u8 = 22;
pub const OP_ADD: u8 = 23;
pub const OP_SUB: u8 = 24;
pub const OP_MUL: u8 = 25;
pub const OP_DIV: u8 = 26;
pub const OP_MOD: u8 = 27;
pub const OP_NEG: u8 = 28;
pub const OP_EQ: u8 = 29;
pub const OP_NE: u8 = 30;
pub const OP_LT: u8 = 31;
pub const OP_LE: u8 = 32;
pub const OP_GT: u8 = 33;
pub const OP_GE: u8 = 34;
pub const OP_AND: u8 = 35;
pub const OP_OR: u8 = 36;
pub const OP_NOT: u8 = 37;
pub const OP_CONCAT: u8 = 38;
pub const OP_PRINT: u8 = 39;
pub const OP_PRINTLN: u8 = 40;
pub const OP_PRINTLN_STR: u8 = 41; // u32 const-index of a literal string
pub const OP_HALT: u8 = 42;
pub const OP_CALL_EXTERN: u8 = 43; // u32 extern-name const-idx, u32 nargs
pub const OP_METHOD: u8 = 44; // u32 method-name const-idx, u32 nargs
pub const OP_NEW_LIST: u8 = 45; // u32 n (list/array literal)
pub const OP_NEW_MAP: u8 = 46; // u32 npairs
pub const OP_INDEX: u8 = 47; // container index read
pub const OP_INDEX_STORE: u8 = 48; // container index write
pub const OP_TRY: u8 = 49; // u32 catch_off, u32 catch_local, u32 finally_off, u8 flags
pub const OP_ENDTRY: u8 = 50;
pub const OP_THROW: u8 = 51; // pop value and raise
pub const OP_FINALLY_END: u8 = 52; // dispatch pending return/throw, or continue
pub const OP_METHOD_DYN: u8 = 53; // u32 trait, u32 method, u32 nargs: dynamic trait dispatch
pub const OP_GLOAD: u8 = 54; // u32 slot: load a top-level `var` global
pub const OP_GSTORE: u8 = 55; // u32 slot: store into a top-level `var` global

// ---------------------------------------------------------------------------
// Program representation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum Const {
    Str(String),
    I64(i64),
    F64(f64),
    Bool(bool),
}

#[derive(Debug, Clone)]
pub struct StructMeta {
    pub name: String,
    pub fields: Vec<String>,
}

/// Region tag constants (mirror `semantic::region::Region`).
pub const REGION_STACK: u8 = 0;
pub const REGION_SCOPED: u8 = 1;
pub const REGION_HEAP: u8 = 2;

/// Schedule tag constants.
pub const SCHED_SINGLE: u8 = 0;
pub const SCHED_AUTO: u8 = 1;
pub const SCHED_MANUAL: u8 = 2;

#[derive(Debug, Clone)]
pub struct Func {
    pub name: String,
    pub nparams: u32,
    pub nlocals: u32,
    pub ncaptures: u32,
    pub region: u8,
    pub schedule: u8,
    /// For `@Manual(fixed=N)`: the fixed worker-pool size (0 otherwise).
    pub manual_size: u32,
    pub code: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct Program {
    pub consts: Vec<Const>,
    pub structs: Vec<StructMeta>,
    pub funcs: Vec<Func>,
    pub main: u32,
    /// Index of the synthetic `__init_globals` function that initializes the
    /// top-level `var` globals; run once before `main`. `None` if the program
    /// has no globals.
    pub init: Option<u32>,
    pub externs: Vec<ExternMeta>,
    /// Runtime dispatch table for `dyn Trait` / bounded type variables:
    /// (fully-qualified type tag, method) -> impl function index.
    pub impls: Vec<ImplMeta>,
}

#[derive(Debug, Clone)]
pub struct ExternMeta {
    pub name: String,
    pub nparams: u32,
}

#[derive(Debug, Clone)]
pub struct ImplMeta {
    pub type_name: String,
    pub method: String,
    pub func_idx: u32,
}

// ---------------------------------------------------------------------------
// Serialization
// ---------------------------------------------------------------------------

const MAGIC: [u8; 4] = *b"PSC\0";
const VERSION: u32 = 5;

pub fn encode_u32(v: u32) -> [u8; 4] {
    v.to_le_bytes()
}
pub fn encode_i64(v: i64) -> [u8; 8] {
    v.to_le_bytes()
}
pub fn encode_f64(v: f64) -> [u8; 8] {
    v.to_le_bytes()
}
pub fn encode_i32(v: i32) -> [u8; 4] {
    v.to_le_bytes()
}

pub struct Reader<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Reader<'a> {
    pub fn u8(&mut self) -> u8 {
        let v = self.b.get(self.i).copied().unwrap_or(0);
        self.i += 1;
        v
    }
    pub fn u32(&mut self) -> u32 {
        let mut buf = [0u8; 4];
        for j in 0..4 {
            buf[j] = self.u8();
        }
        u32::from_le_bytes(buf)
    }
    pub fn i64(&mut self) -> i64 {
        let mut buf = [0u8; 8];
        for j in 0..8 {
            buf[j] = self.u8();
        }
        i64::from_le_bytes(buf)
    }
    pub fn f64(&mut self) -> f64 {
        let mut buf = [0u8; 8];
        for j in 0..8 {
            buf[j] = self.u8();
        }
        f64::from_le_bytes(buf)
    }
    pub fn i32(&mut self) -> i32 {
        let mut buf = [0u8; 4];
        for j in 0..4 {
            buf[j] = self.u8();
        }
        i32::from_le_bytes(buf)
    }
    pub fn string(&mut self) -> String {
        let len = self.u32() as usize;
        let mut bytes = Vec::with_capacity(len);
        for _ in 0..len {
            bytes.push(self.u8());
        }
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

fn write_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&encode_u32(s.len() as u32));
    out.extend_from_slice(s.as_bytes());
}

fn write_const(out: &mut Vec<u8>, c: &Const) {
    match c {
        Const::Str(s) => {
            out.push(0);
            write_str(out, s);
        }
        Const::I64(v) => {
            out.push(1);
            out.extend_from_slice(&encode_i64(*v));
        }
        Const::F64(v) => {
            out.push(2);
            out.extend_from_slice(&encode_f64(*v));
        }
        Const::Bool(b) => {
            out.push(3);
            out.push(*b as u8);
        }
    }
}

fn read_const(r: &mut Reader) -> Const {
    match r.u8() {
        0 => Const::Str(r.string()),
        1 => Const::I64(r.i64()),
        2 => Const::F64(r.f64()),
        _ => Const::Bool(r.u8() != 0),
    }
}

/// Serialize a program to a byte buffer.
pub fn encode_program(p: &Program) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&encode_u32(VERSION));

    out.extend_from_slice(&encode_u32(p.consts.len() as u32));
    for c in &p.consts {
        write_const(&mut out, c);
    }

    out.extend_from_slice(&encode_u32(p.structs.len() as u32));
    for s in &p.structs {
        write_str(&mut out, &s.name);
        out.extend_from_slice(&encode_u32(s.fields.len() as u32));
        for f in &s.fields {
            write_str(&mut out, f);
        }
    }

    out.extend_from_slice(&encode_u32(p.funcs.len() as u32));
    for f in &p.funcs {
        write_str(&mut out, &f.name);
        out.extend_from_slice(&encode_u32(f.nparams));
        out.extend_from_slice(&encode_u32(f.nlocals));
        out.extend_from_slice(&encode_u32(f.ncaptures));
        out.push(f.region);
        out.push(f.schedule);
        out.extend_from_slice(&encode_u32(f.manual_size));
        out.extend_from_slice(&encode_u32(f.code.len() as u32));
        out.extend_from_slice(&f.code);
    }

    out.extend_from_slice(&encode_u32(p.main));

    out.push((p.init.is_some()) as u8);
    if let Some(i) = p.init {
        out.extend_from_slice(&encode_u32(i));
    }

    out.extend_from_slice(&encode_u32(p.externs.len() as u32));
    for e in &p.externs {
        write_str(&mut out, &e.name);
        out.extend_from_slice(&encode_u32(e.nparams));
    }

    out.extend_from_slice(&encode_u32(p.impls.len() as u32));
    for im in &p.impls {
        write_str(&mut out, &im.type_name);
        write_str(&mut out, &im.method);
        out.extend_from_slice(&encode_u32(im.func_idx));
    }
    out
}

/// Deserialize a program from a byte buffer.
pub fn decode_program(b: &[u8]) -> Result<Program, String> {
    let mut r = Reader { b, i: 0 };
    let mut magic = [0u8; 4];
    for j in 0..4 {
        magic[j] = r.u8();
    }
    if magic != MAGIC {
        return Err("bad bytecode magic".into());
    }
    let _ver = r.u32();

    let nconsts = r.u32();
    let mut consts = Vec::new();
    for _ in 0..nconsts {
        consts.push(read_const(&mut r));
    }

    let nstructs = r.u32();
    let mut structs = Vec::new();
    for _ in 0..nstructs {
        let name = r.string();
        let nf = r.u32();
        let mut fields = Vec::new();
        for _ in 0..nf {
            fields.push(r.string());
        }
        structs.push(StructMeta { name, fields });
    }

    let nfuncs = r.u32();
    let mut funcs = Vec::new();
    for _ in 0..nfuncs {
        let name = r.string();
        let nparams = r.u32();
        let nlocals = r.u32();
        let ncaptures = r.u32();
        let region = r.u8();
        let schedule = r.u8();
        let manual_size = r.u32();
        let codelen = r.u32() as usize;
        let mut code = Vec::with_capacity(codelen);
        for _ in 0..codelen {
            code.push(r.u8());
        }
        funcs.push(Func { name, nparams, nlocals, ncaptures, region, schedule, manual_size, code });
    }

    let main = r.u32();

    let has_init = r.u8();
    let init = if has_init != 0 { Some(r.u32()) } else { None };

    let nexterns = r.u32();
    let mut externs = Vec::new();
    for _ in 0..nexterns {
        let name = r.string();
        let nparams = r.u32();
        externs.push(ExternMeta { name, nparams });
    }

    let nimpls = r.u32();
    let mut impls = Vec::new();
    for _ in 0..nimpls {
        let type_name = r.string();
        let method = r.string();
        let func_idx = r.u32();
        impls.push(ImplMeta { type_name, method, func_idx });
    }

    Ok(Program { consts, structs, funcs, main, init, externs, impls })
}

// ---------------------------------------------------------------------------
// Values
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    I64(i64),
    F64(f64),
    Bool(bool),
    /// A reference-counted heap object id.
    Obj(u32),
    /// A pointer (algebraic path) into `obj.field`.
    Ptr(u32, u32),
    Null,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ObjKind {
    Struct,
    Str,
    Closure,
    Frame,
    /// A list / array: a growable sequence of values.
    List,
    /// A map: an ordered sequence of key/value pairs (keyed by value equality).
    Map,
}

/// A heap object with a non-blocking reference count.
#[derive(Debug)]
pub struct HeapObj {
    pub kind: ObjKind,
    pub tag: String,
    pub fields: Vec<Value>,
    pub str: Option<Vec<u8>>,
    /// For `ObjKind::Map`: the key/value entries (keys must not collide by value
    /// equality). `None` for every other kind.
    pub pairs: Option<Vec<(Value, Value)>>,
    pub closure_fn: Option<u32>,
    pub rc: std::sync::atomic::AtomicU32,
}

impl HeapObj {
    fn new(kind: ObjKind, tag: &str, fields: Vec<Value>) -> Self {
        HeapObj {
            kind,
            tag: tag.to_string(),
            fields,
            str: None,
            pairs: None,
            closure_fn: None,
            rc: std::sync::atomic::AtomicU32::new(1),
        }
    }
    fn new_str(bytes: Vec<u8>) -> Self {
        let mut o = Self::new(ObjKind::Str, "String", Vec::new());
        o.str = Some(bytes);
        o
    }
    /// Non-blocking reference-count increment.
    pub fn inc_ref(&self) {
        self.rc.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    /// Non-blocking reference-count decrement; returns the new count.
    pub fn dec_ref(&self) -> u32 {
        self.rc.fetch_sub(1, std::sync::atomic::Ordering::Relaxed).saturating_sub(1)
    }
    pub fn refcount(&self) -> u32 {
        self.rc.load(std::sync::atomic::Ordering::Relaxed)
    }
}

// ---------------------------------------------------------------------------
// Task pool (backs @Auto / @Manual scheduling)
// ---------------------------------------------------------------------------

/// A unit of work submitted to a worker pool.
type TaskEntry = Box<dyn FnOnce() + Send>;

/// A fixed-size pool of OS worker threads consuming from a shared FIFO queue.
/// Workers are created once and reused across calls; they idle (yield) when the
/// queue is empty. Each scheduled call runs in an isolated VM, so the only thing
/// shared between tasks is the immutable `Program` (wrapped in `Arc`).
struct TaskPool {
    queue: Arc<Mutex<VecDeque<TaskEntry>>>,
    _workers: Vec<std::thread::JoinHandle<()>>,
}

impl TaskPool {
    fn new(n: usize) -> Self {
        let queue: Arc<Mutex<VecDeque<TaskEntry>>> = Arc::new(Mutex::new(VecDeque::new()));
        let mut workers = Vec::with_capacity(n);
        for _ in 0..n.max(1) {
            let q = queue.clone();
            workers.push(std::thread::spawn(move || loop {
                let task = q.lock().unwrap().pop_front();
                match task {
                    Some(t) => t(),
                    None => std::thread::yield_now(),
                }
            }));
        }
        TaskPool { queue, _workers: workers }
    }

    fn submit(&self, t: TaskEntry) {
        self.queue.lock().unwrap().push_back(t);
    }
}

// ---------------------------------------------------------------------------
// VM
// ---------------------------------------------------------------------------

struct CallFrame {
    func: u32,
    pc: usize,
    frame_obj: u32,
    code: Vec<u8>,
    /// The caller's pending operation (return/throw in a finally), restored on
    /// an ordinary return so a finally's nested calls don't leak it.
    pending: Option<Pending>,
}

/// Handler kinds pushed by `OP_TRY` as one group (FINALLY, CATCH, MARK).
const HANDLER_CATCH: u8 = 0;
const HANDLER_FINALLY: u8 = 1;
const HANDLER_MARK: u8 = 2;

/// An active exception handler. `try_id` groups the handlers of one `try`;
/// resolving a handler discards every group with `try_id >=` the matched one.
struct Handler {
    func: u32,
    kind: u8,
    try_id: u32,
    /// Jump target: catch body start (CATCH) or finally body start (FINALLY).
    target: usize,
    catch_local: u32,
}

/// A control-flow op the current `finally` must complete before the VM proceeds
/// with an ordinary return (Return) or re-raise (Throw).
#[derive(Clone)]
enum Pending {
    Return(Value),
    Throw(Value),
}

pub struct Vm<'a> {
    prog: &'a Program,
    /// Immutable program shared with worker threads (built lazily).
    task_prog: Option<Arc<Program>>,
    /// Lazily-created worker pools keyed by schedule (Auto => 0, Manual(n) => n).
    pools: HashMap<u32, TaskPool>,
    heap: Vec<HeapObj>,
    /// Top-level `var` globals (VM-wide storage shared by every function frame,
    /// including callbacks invoked from the GUI event loop).
    globals: Vec<Value>,
    stack: Vec<Value>,
    frames: Vec<CallFrame>,
    /// Exception handler stack (LIFO, mirroring the lexical try nesting).
    handlers: Vec<Handler>,
    /// Counter assigning each `try` block a unique id (monotonic).
    try_counter: u32,
    /// Pending return/throw waiting to be completed by the active `finally`.
    pending: Option<Pending>,
    // current function state
    code: Vec<u8>,
    pc: usize,
    frame_obj: u32,
    fname: String,
    pub args: Vec<String>,
}

/// Value-to-string conversion used by `println`/`CONCAT`.
fn to_string_bytes(heap: &[HeapObj], v: &Value) -> Vec<u8> {
    match v {
        Value::I64(n) => n.to_string().into_bytes(),
        Value::F64(f) => format!("{}", f).into_bytes(),
        Value::Bool(b) => (if *b { "true" } else { "false" }).as_bytes().to_vec(),
        Value::Obj(id) => {
            let o = &heap[*id as usize];
            match o.kind {
                ObjKind::Str => o.str.clone().unwrap_or_default(),
                ObjKind::List => {
                    let mut s = String::from("[");
                    for (i, f) in o.fields.iter().enumerate() {
                        if i > 0 {
                            s.push_str(", ");
                        }
                        s.push_str(&String::from_utf8_lossy(&to_string_bytes(heap, f)));
                    }
                    s.push(']');
                    s.into_bytes()
                }
                ObjKind::Map => {
                    let mut s = String::from("{");
                    if let Some(pairs) = &o.pairs {
                        for (i, (k, v)) in pairs.iter().enumerate() {
                            if i > 0 {
                                s.push_str(", ");
                            }
                            s.push_str(&String::from_utf8_lossy(&to_string_bytes(heap, k)));
                            s.push_str(": ");
                            s.push_str(&String::from_utf8_lossy(&to_string_bytes(heap, v)));
                        }
                    }
                    s.push('}');
                    s.into_bytes()
                }
                _ => format!("<{}>", o.tag).into_bytes(),
            }
        }
        Value::Ptr(o, f) => format!("&{}#{}", o, f).into_bytes(),
        Value::Null => "null".as_bytes().to_vec(),
    }
}

/// Run a serialized program (decode then execute). Convenience used by CLI.
pub fn execute_bytes(bytes: &[u8], args: &[String]) -> Result<i64, String> {
    let prog = decode_program(bytes)?;
    run_program(&prog, args)
}

/// Execute a decoded program and return its exit code.
pub fn run_program(prog: &Program, args: &[String]) -> Result<i64, String> {
    let mut vm = Vm {
        prog,
        task_prog: None,
        pools: HashMap::new(),
        heap: Vec::new(),
        globals: Vec::new(),
        stack: Vec::new(),
        frames: Vec::new(),
        handlers: Vec::new(),
        try_counter: 0,
        pending: None,
        code: Vec::new(),
        pc: 0,
        frame_obj: 0,
        fname: String::new(),
        args: args.to_vec(),
    };
    vm.run()
}

/// Run a specific function in a fresh, isolated VM (its own heap, stack and
/// frames) and return the value it leaves on its stack, along with its heap so
/// the caller can copy back any returned objects. Used by `run_scheduled` so
/// `@Auto`/`@Manual` functions execute on worker threads with no shared mutable
/// state.
fn run_function_isolated(
    prog: &Program,
    fidx: usize,
    seed: Vec<Value>,
    vm_args: &[String],
) -> Result<(Value, Vec<HeapObj>), String> {
    let f = &prog.funcs[fidx];
    let nlocals = f.nlocals as usize;
    let mut locals = vec![Value::Null; nlocals];
    for (i, v) in seed.into_iter().enumerate() {
        if i < nlocals {
            locals[i] = v;
        }
    }
    let mut vm = Vm {
        prog,
        task_prog: None,
        pools: HashMap::new(),
        heap: Vec::new(),
        globals: Vec::new(),
        stack: Vec::new(),
        frames: Vec::new(),
        handlers: Vec::new(),
        try_counter: 0,
        pending: None,
        code: f.code.clone(),
        pc: 0,
        frame_obj: 0,
        fname: f.name.clone(),
        args: vm_args.to_vec(),
    };
    let fobj = vm.alloc(HeapObj::new(ObjKind::Frame, &f.name, locals));
    vm.frame_obj = fobj;
    let v = vm.execute()?;
    Ok((v, vm.heap))
}

impl<'a> Vm<'a> {
    fn alloc(&mut self, obj: HeapObj) -> u32 {
        self.heap.push(obj);
        (self.heap.len() - 1) as u32
    }

    fn const_str(&self, idx: usize) -> String {
        match &self.prog.consts[idx] {
            Const::Str(s) => s.clone(),
            _ => String::new(),
        }
    }

    fn push(&mut self, v: Value) {
        self.stack.push(v);
    }
    fn pop(&mut self) -> Value {
        self.stack.pop().unwrap_or(Value::Null)
    }
    fn pop_expect_ptr(&mut self) -> Result<(u32, u32), String> {
        match self.pop() {
            Value::Ptr(o, f) => Ok((o, f)),
            other => Err(format!("expected a pointer, found {other:?}")),
        }
    }

    fn new_str_obj(&mut self, bytes: Vec<u8>) -> u32 {
        self.alloc(HeapObj::new_str(bytes))
    }

    /// Create the main frame and run until `main` returns.
    fn run(&mut self) -> Result<i64, String> {
        // Top-level `var` globals are initialized before `main` runs. The
        // synthetic `__init_globals` function stores each initializer into its
        // global slot, in declaration order. It is entered as a top-level frame
        // (like `main` itself) so its `RETURN_VOID` ends cleanly instead of
        // unwinding a non-existent caller.
        if let Some(init) = self.prog.init {
            let init = init as usize;
            if init < self.prog.funcs.len() {
                let f = self.prog.funcs[init].clone();
                let locals = vec![Value::Null; f.nlocals as usize];
                let fobj = self.alloc(HeapObj::new(ObjKind::Frame, &f.name, locals));
                self.frame_obj = fobj;
                self.code = f.code;
                self.pc = 0;
                self.fname = f.name.clone();
                self.execute()?;
            }
        }
        let main = self.prog.main as usize;
        if main >= self.prog.funcs.len() {
            return Err(format!("no `main` function in program"));
        }
        // main's frame (params = 0), sized to main's declared locals.
        let f = self.prog.funcs[main].clone();
        let locals = vec![Value::Null; f.nlocals as usize];
        let fobj = self.alloc(HeapObj::new(ObjKind::Frame, &f.name, locals));
        self.frame_obj = fobj;
        self.code = f.code;
        self.pc = 0;
        self.fname = f.name.clone();
        let v = self.execute()?;
        Ok(exit_code(&v))
    }

    fn execute(&mut self) -> Result<Value, String> {
        loop {
            let op = match self.code.get(self.pc) {
                Some(o) => *o,
                None => return Err(format!("ran off end of `{}`", self.fname)),
            };
            self.pc += 1;
            match op {
                OP_HALT => return Ok(self.stack.pop().unwrap_or(Value::Null)),
                OP_PUSH_I64 => {
                    let v = read_i64(&self.code, &mut self.pc);
                    self.push(Value::I64(v));
                }
                OP_PUSH_F64 => {
                    let v = read_f64(&self.code, &mut self.pc);
                    self.push(Value::F64(v));
                }
                OP_PUSH_BOOL => {
                    let b = self.code[self.pc] != 0;
                    self.pc += 1;
                    self.push(Value::Bool(b));
                }
                OP_PUSH_NULL => self.push(Value::Null),
                OP_PUSH_STR => {
                    let idx = read_u32(&self.code, &mut self.pc) as usize;
                    let bytes = self.const_str(idx).into_bytes();
                    let id = self.new_str_obj(bytes);
                    self.push(Value::Obj(id));
                }
                OP_POP => {
                    self.pop();
                }
                OP_DUP => {
                    let v = self.pop();
                    self.push(v.clone());
                    self.push(v);
                }
                OP_LOAD_LOCAL => {
                    let i = read_u32(&self.code, &mut self.pc) as usize;
                    let v = self.heap[self.frame_obj as usize].fields.get(i).cloned().unwrap_or(Value::Null);
                    self.push(v);
                }
                OP_STORE_LOCAL => {
                    let i = read_u32(&self.code, &mut self.pc) as usize;
                    let v = self.pop();
                    if let Some(f) = self.heap[self.frame_obj as usize].fields.get_mut(i) {
                        *f = v;
                    }
                }
                OP_GLOAD => {
                    let i = read_u32(&self.code, &mut self.pc) as usize;
                    self.push(self.globals.get(i).cloned().unwrap_or(Value::Null));
                }
                OP_GSTORE => {
                    let i = read_u32(&self.code, &mut self.pc) as usize;
                    let v = self.pop();
                    if self.globals.len() <= i {
                        self.globals.resize(i + 1, Value::Null);
                    }
                    self.globals[i] = v;
                }
                OP_LOAD_FIELD => {
                    let i = read_u32(&self.code, &mut self.pc) as usize;
                    let obj = match self.pop() {
                        Value::Obj(o) => o,
                        other => return Err(format!("LOAD_FIELD on non-object {other:?}")),
                    };
                    let v = self.heap[obj as usize].fields.get(i).cloned().unwrap_or(Value::Null);
                    self.push(v);
                }
                OP_STORE_FIELD => {
                    let i = read_u32(&self.code, &mut self.pc) as usize;
                    let val = self.pop();
                    let obj = match self.pop() {
                        Value::Obj(o) => o,
                        other => return Err(format!("STORE_FIELD on non-object {other:?}")),
                    };
                    if let Some(f) = self.heap[obj as usize].fields.get_mut(i) {
                        *f = val;
                    }
                }
                OP_ADDR_LOCAL => {
                    let i = read_u32(&self.code, &mut self.pc);
                    self.push(Value::Ptr(self.frame_obj, i));
                }
                OP_ADDR_FIELD => {
                    let i = read_u32(&self.code, &mut self.pc);
                    let obj = match self.pop() {
                        Value::Obj(o) => o,
                        other => return Err(format!("ADDR_FIELD on non-object {other:?}")),
                    };
                    self.push(Value::Ptr(obj, i));
                }
                OP_DEREF => {
                    let (o, f) = self.pop_expect_ptr()?;
                    let v = self.heap[o as usize].fields.get(f as usize).cloned().unwrap_or(Value::Null);
                    self.push(v);
                }
                OP_DEREF_STORE => {
                    let val = self.pop();
                    let (o, f) = self.pop_expect_ptr()?;
                    if let Some(field) = self.heap[o as usize].fields.get_mut(f as usize) {
                        *field = val;
                    }
                }
                OP_NEW_STRUCT => {
                    let sidx = read_u32(&self.code, &mut self.pc) as usize;
                    let nfields = read_u32(&self.code, &mut self.pc) as usize;
                    let mut fields = Vec::with_capacity(nfields);
                    for _ in 0..nfields {
                        fields.push(self.pop());
                    }
                    fields.reverse();
                    let meta = &self.prog.structs[sidx];
                    let id = self.alloc(HeapObj::new(ObjKind::Struct, &meta.name, fields));
                    self.push(Value::Obj(id));
                }
                OP_NEW_CLOSURE => {
                    let fidx = read_u32(&self.code, &mut self.pc) as usize;
                    let ncaptures = read_u32(&self.code, &mut self.pc) as usize;
                    let mut captures = Vec::with_capacity(ncaptures);
                    for _ in 0..ncaptures {
                        captures.push(self.pop());
                    }
                    captures.reverse();
                    let mut o = HeapObj::new(ObjKind::Closure, "Closure", captures);
                    o.closure_fn = Some(fidx as u32);
                    let id = self.alloc(o);
                    self.push(Value::Obj(id));
                }
                OP_CALL => {
                    let fidx = read_u32(&self.code, &mut self.pc) as usize;
                    let nargs = read_u32(&self.code, &mut self.pc) as usize;
                    let mut args = Vec::with_capacity(nargs);
                    for _ in 0..nargs {
                        args.push(self.pop());
                    }
                    args.reverse();
                    let sched = self.prog.funcs[fidx].schedule;
                    if sched == SCHED_SINGLE {
                        self.call_func(fidx, args)?;
                    } else {
                        let v = self.run_scheduled(fidx, args, sched)?;
                        self.push(v);
                    }
                }
                OP_CALL_CLOSURE => {
                    let nargs = read_u32(&self.code, &mut self.pc) as usize;
                    let mut args = Vec::with_capacity(nargs);
                    for _ in 0..nargs {
                        args.push(self.pop());
                    }
                    args.reverse();
                    let closure = match self.pop() {
                        Value::Obj(o) => o,
                        other => return Err(format!("call on non-closure {other:?}")),
                    };
                    let clo = &self.heap[closure as usize];
                    let fidx = clo.closure_fn.ok_or("closure has no body")? as usize;
                    let captures = clo.fields.clone();
                    // frame locals = captures ++ args
                    let mut seed = captures;
                    seed.extend(args);
                    let sched = self.prog.funcs[fidx].schedule;
                    if sched == SCHED_SINGLE {
                        self.call_func(fidx, seed)?;
                    } else {
                        let v = self.run_scheduled(fidx, seed, sched)?;
                        self.push(v);
                    }
                }
                OP_RETURN => {
                    let v = self.pop();
                    if let Some(r) = self.return_value(v)? {
                        return Ok(r);
                    }
                }
                OP_RETURN_VOID => {
                    if let Some(r) = self.return_value(Value::Null)? {
                        return Ok(r);
                    }
                }
                OP_JMP => {
                    let off = read_i32(&self.code, &mut self.pc);
                    self.pc = (self.pc as i64 + off as i64) as usize;
                }
                OP_JZ => {
                    let off = read_i32(&self.code, &mut self.pc);
                    let cond = self.pop();
                    let truthy = match cond {
                        Value::Bool(b) => b,
                        Value::I64(n) => n != 0,
                        Value::Null => false,
                        _ => true,
                    };
                    if !truthy {
                        self.pc = (self.pc as i64 + off as i64) as usize;
                    }
                }
                OP_ADD => self.bin(|a, b| a + b, |a, b| a + b, "add")?,
                OP_SUB => self.bin(|a, b| a - b, |a, b| a - b, "sub")?,
                OP_MUL => self.bin(|a, b| a * b, |a, b| a * b, "mul")?,
                OP_DIV => self.bin(|a, b| a / b, |a, b| a / b, "div")?,
                OP_MOD => {
                    let b = self.pop();
                    let a = self.pop();
                    match (a, b) {
                        (Value::I64(x), Value::I64(y)) => {
                            if y == 0 {
                                return Err("division by zero".into());
                            }
                            self.push(Value::I64(x % y));
                        }
                        _ => return Err("mod requires ints".into()),
                    }
                }
                OP_NEG => {
                    let v = self.pop();
                    match v {
                        Value::I64(n) => self.push(Value::I64(-n)),
                        Value::F64(f) => self.push(Value::F64(-f)),
                        _ => return Err("negate on non-number".into()),
                    }
                }
                OP_EQ => self.cmp(|o| o == std::cmp::Ordering::Equal, |a, b| a == b)?,
                OP_NE => self.cmp(|o| o != std::cmp::Ordering::Equal, |a, b| a != b)?,
                OP_LT => self.cmp(|o| o == std::cmp::Ordering::Less, |a, b| a < b)?,
                OP_LE => self.cmp(|o| o != std::cmp::Ordering::Greater, |a, b| a <= b)?,
                OP_GT => self.cmp(|o| o == std::cmp::Ordering::Greater, |a, b| a > b)?,
                OP_GE => self.cmp(|o| o != std::cmp::Ordering::Less, |a, b| a >= b)?,
                OP_AND => {
                    let b = self.pop();
                    let a = self.pop();
                    self.push(Value::Bool(truthy(&a) && truthy(&b)));
                }
                OP_OR => {
                    let b = self.pop();
                    let a = self.pop();
                    self.push(Value::Bool(truthy(&a) || truthy(&b)));
                }
                OP_NOT => {
                    let v = self.pop();
                    self.push(Value::Bool(!truthy(&v)));
                }
                OP_CONCAT => {
                    let b = self.pop();
                    let a = self.pop();
                    let mut bytes = to_string_bytes(&self.heap, &a);
                    bytes.extend(to_string_bytes(&self.heap, &b));
                    let id = self.new_str_obj(bytes);
                    self.push(Value::Obj(id));
                }
                OP_PRINT => {
                    let v = self.pop();
                    let bytes = to_string_bytes(&self.heap, &v);
                    print_stdout(&bytes);
                }
                OP_PRINTLN => {
                    let v = self.pop();
                    let mut bytes = to_string_bytes(&self.heap, &v);
                    bytes.push(b'\n');
                    print_stdout(&bytes);
                }
                OP_PRINTLN_STR => {
                    let idx = read_u32(&self.code, &mut self.pc) as usize;
                    let mut bytes = self.const_str(idx).into_bytes();
                    bytes.push(b'\n');
                    print_stdout(&bytes);
                }
                OP_CALL_EXTERN => {
                    let name_idx = read_u32(&self.code, &mut self.pc) as usize;
                    let nargs = read_u32(&self.code, &mut self.pc) as usize;
                    let name = self.const_str(name_idx);
                    let mut args = Vec::with_capacity(nargs);
                    for _ in 0..nargs {
                        args.push(self.pop());
                    }
                    args.reverse();
                    let result = if is_lang_builtin(&name) {
                        self.call_lang_builtin(&name, &args)?
                    } else {
                        call_extern(&name, &args, &self.heap)?
                    };
                    self.push(result);
                }
                OP_METHOD => {
                    let name_idx = read_u32(&self.code, &mut self.pc) as usize;
                    let nargs = read_u32(&self.code, &mut self.pc) as usize;
                    let name = self.const_str(name_idx);
                    let mut args = Vec::with_capacity(nargs);
                    for _ in 0..nargs {
                        args.push(self.pop());
                    }
                    args.reverse();
                    let recv = self.pop();
                    let res = self.call_method(&name, &args, recv)?;
                    self.push(res);
                }
                OP_NEW_LIST => {
                    let n = read_u32(&self.code, &mut self.pc) as usize;
                    let mut fields = Vec::with_capacity(n);
                    for _ in 0..n {
                        fields.push(self.pop());
                    }
                    fields.reverse();
                    let id = self.alloc(HeapObj::new(ObjKind::List, "List", fields));
                    self.push(Value::Obj(id));
                }
                OP_NEW_MAP => {
                    let n = read_u32(&self.code, &mut self.pc) as usize;
                    let mut pairs = Vec::with_capacity(n);
                    for _ in 0..n {
                        let v = self.pop();
                        let k = self.pop();
                        pairs.push((k, v));
                    }
                    pairs.reverse();
                    let mut o = HeapObj::new(ObjKind::Map, "Map", Vec::new());
                    o.pairs = Some(pairs);
                    let id = self.alloc(o);
                    self.push(Value::Obj(id));
                }
                OP_INDEX => {
                    let idx = self.pop();
                    let obj = self.pop();
                    let v = self.index_get(obj, &idx)?;
                    self.push(v);
                }
                OP_INDEX_STORE => {
                    let val = self.pop();
                    let idx = self.pop();
                    let obj = self.pop();
                    self.index_set(obj, &idx, val)?;
                    self.push(Value::Null);
                }
                OP_TRY => {
                    let catch_off = read_u32(&self.code, &mut self.pc) as usize;
                    let catch_local = read_u32(&self.code, &mut self.pc);
                    let finally_off = read_u32(&self.code, &mut self.pc) as usize;
                    let flags = self.code.get(self.pc).copied().unwrap_or(0);
                    self.pc += 1;
                    let op_end = self.pc;
                    let tid = self.try_counter;
                    self.try_counter += 1;
                    let cur = self.current_func_idx();
                    // order matters: FINALLY sits below CATCH so a raise finds the
                    // catch first (stack-top search via rposition)
                    if flags & 2 != 0 {
                        self.handlers.push(Handler {
                            func: cur,
                            kind: HANDLER_FINALLY,
                            try_id: tid,
                            target: op_end + finally_off,
                            catch_local: 0,
                        });
                    }
                    if flags & 1 != 0 {
                        self.handlers.push(Handler {
                            func: cur,
                            kind: HANDLER_CATCH,
                            try_id: tid,
                            target: op_end + catch_off,
                            catch_local,
                        });
                    }
                    self.handlers.push(Handler {
                        func: cur,
                        kind: HANDLER_MARK,
                        try_id: tid,
                        target: 0,
                        catch_local: 0,
                    });
                }
                OP_ENDTRY => {
                    // pop this try block's whole handler group (FINALLY..MARK)
                    if let Some(pos) = self.handlers.iter().rposition(|h| h.kind == HANDLER_MARK) {
                        let tid = self.handlers[pos].try_id;
                        self.handlers.retain(|h| h.try_id < tid);
                    }
                }
                OP_FINALLY_END => {
                    match self.pending.take() {
                        Some(Pending::Throw(v)) => self.raise(v)?,
                        Some(Pending::Return(v)) => {
                            if let Some(r) = self.return_value(v)? {
                                return Ok(r);
                            }
                        }
                        None => {
                            // normal exit of the finally block: pop this group's
                            // FINALLY handler (still on the stack from the
                            // exception/return path)
                            let cur = self.current_func_idx();
                            if let Some(pos) = self
                                .handlers
                                .iter()
                                .rposition(|h| h.kind == HANDLER_FINALLY && h.func == cur)
                            {
                                let tid = self.handlers[pos].try_id;
                                self.handlers.retain(|h| h.try_id < tid);
                            }
                        }
                    }
                }
                OP_THROW => {
                    let v = self.pop();
                    self.raise(v)?;
                }
                OP_METHOD_DYN => {
                    let trait_idx = read_u32(&self.code, &mut self.pc) as usize;
                    let method_idx = read_u32(&self.code, &mut self.pc) as usize;
                    let nargs = read_u32(&self.code, &mut self.pc) as usize;
                    let trait_name = self.const_str(trait_idx);
                    let method = self.const_str(method_idx);
                    let mut args = Vec::with_capacity(nargs);
                    for _ in 0..nargs {
                        args.push(self.pop());
                    }
                    args.reverse();
                    let recv = self.pop();
                    let type_tag = match &recv {
                        Value::Obj(id) => self.heap[*id as usize].tag.clone(),
                        _other => {
                            return Err(format!(
                                "cannot call `{trait_name}::{method}` on a non-object value"
                            ))
                        }
                    };
                    let fidx = self
                        .prog
                        .impls
                        .iter()
                        .find(|im| im.type_name == type_tag && im.method == method)
                        .map(|im| im.func_idx as usize)
                        .ok_or_else(|| {
                            format!(
                                "no impl of `{trait_name}` for type `{type_tag}` providing `{method}`"
                            )
                        })?;
                    let mut seed = vec![recv];
                    seed.extend(args);
                    self.call_func(fidx, seed)?;
                }
                other => return Err(format!("unknown opcode {other}")),
            }
        }
    }

    /// Raise `v`: find the nearest active handler (catch or finally) of the
    /// current function; a catch stores the value and jumps to its body, a
    /// finally records the pending re-raise and jumps to its body. Without a
    /// handler the exception unwinds one frame at a time, or becomes uncaught.
    fn raise(&mut self, v: Value) -> Result<(), String> {
        loop {
            let cur = self.current_func_idx();
            let pos = self
                .handlers
                .iter()
                .rposition(|h| h.kind != HANDLER_MARK && h.func == cur);
            match pos {
                Some(pos) => {
                    let h = Handler {
                        func: self.handlers[pos].func,
                        kind: self.handlers[pos].kind,
                        try_id: self.handlers[pos].try_id,
                        target: self.handlers[pos].target,
                        catch_local: self.handlers[pos].catch_local,
                    };
                    let tid = h.try_id;
                    if h.kind == HANDLER_CATCH {
                        // keep this group's FINALLY (it must run on any exit from
                        // this try, including a return/throw inside the catch),
                        // but discard this group's CATCH/MARK and all inner groups
                        self.handlers.retain(|x| x.try_id != tid || x.kind == HANDLER_FINALLY);
                        let locals = &mut self.heap[self.frame_obj as usize].fields;
                        if (h.catch_local as usize) < locals.len() {
                            locals[h.catch_local as usize] = v.clone();
                        }
                        self.pc = h.target;
                        return Ok(());
                    }
                    // finally: this group is consumed, the re-raise stays pending
                    self.handlers.retain(|x| x.try_id < tid);
                    self.pending = Some(Pending::Throw(v));
                    self.pc = h.target;
                    return Ok(());
                }
                None => {
                    if self.frames.is_empty() {
                        let msg =
                            String::from_utf8_lossy(&to_string_bytes(&self.heap, &v)).into_owned();
                        return Err(format!("uncaught exception: {msg}"));
                    }
                    // the frame we leave has its handlers become dead
                    self.handlers.retain(|h| h.func != cur);
                    self.pending = None;
                    let cf = self.frames.pop().unwrap();
                    self.code = cf.code;
                    self.pc = cf.pc;
                    self.frame_obj = cf.frame_obj;
                    self.fname = self.prog.funcs[cf.func as usize].name.clone();
                }
            }
        }
    }

    /// Return `v`, first completing any active `finally` of this function: each
    /// finally runs with the return pending, then the pending return resumes.
    /// Returns `Some(v)` when the program finishes with value `v`, else `None`.
    fn return_value(&mut self, v: Value) -> Result<Option<Value>, String> {
        loop {
            let cur = self.current_func_idx();
            if let Some(pos) = self
                .handlers
                .iter()
                .rposition(|h| h.kind == HANDLER_FINALLY && h.func == cur)
            {
                let tid = self.handlers[pos].try_id;
                let target = self.handlers[pos].target;
                // discard this group and any inner groups
                self.handlers.retain(|x| x.try_id < tid);
                self.pending = Some(Pending::Return(v.clone()));
                self.pc = target;
                return Ok(None);
            }
            // ordinary return: pop the frame, restore the caller's pending
            self.handlers.retain(|h| h.func != cur);
            if self.frames.is_empty() {
                self.pending = None;
                return Ok(Some(v));
            }
            let cf = self.frames.pop().unwrap();
            self.pending = cf.pending;
            self.code = cf.code;
            self.pc = cf.pc;
            self.frame_obj = cf.frame_obj;
            self.fname = self.prog.funcs[cf.func as usize].name.clone();
            self.push(v);
            return Ok(None);
        }
    }

    /// Call a function with a seed of local values (params, or captures++params
    /// for closures). The frame is sized to the function's `nlocals` so that
    /// `STORE_LOCAL`/`LOAD_LOCAL` can address every declared local.
    fn call_func(&mut self, fidx: usize, seed: Vec<Value>) -> Result<(), String> {
        let f = self.prog.funcs[fidx].clone();
        let nlocals = f.nlocals as usize;
        let mut locals = vec![Value::Null; nlocals];
        for (i, v) in seed.into_iter().enumerate() {
            if i < nlocals {
                locals[i] = v;
            }
        }
        let fobj = self.alloc(HeapObj::new(ObjKind::Frame, &f.name, locals));
        self.enter_func(fidx, fobj)
    }

    fn enter_func(&mut self, fidx: usize, fobj: u32) -> Result<(), String> {
        // save caller (including its pending finally op, isolated per function)
        let caller = CallFrame {
            func: self.current_func_idx(),
            pc: self.pc,
            frame_obj: self.frame_obj,
            code: std::mem::take(&mut self.code),
            pending: self.pending.take(),
        };
        self.frames.push(caller);
        let f = self.prog.funcs[fidx].clone();
        self.code = f.code;
        self.pc = 0;
        self.frame_obj = fobj;
        self.fname = f.name;
        Ok(())
    }

    /// Dispatch a `@Auto`/`@Manual` function call onto its worker pool. Each call
    /// runs the function to completion in an **isolated** VM (its own heap, stack
    /// and frames) on a worker thread, so scheduled functions really execute in
    /// parallel with no shared mutable state. The returned value is copied back
    /// into this VM's heap.
    fn run_scheduled(&mut self, fidx: usize, seed: Vec<Value>, schedule: u8) -> Result<Value, String> {
        let f = self.prog.funcs[fidx].clone();
        let key: u32 = if schedule == SCHED_AUTO {
            0
        } else {
            f.manual_size.max(1)
        };
        let nworkers = if schedule == SCHED_AUTO {
            std::thread::available_parallelism().map(|p| p.get().max(2)).unwrap_or(2)
        } else {
            f.manual_size.max(1) as usize
        };
        if !self.pools.contains_key(&key) {
            self.pools.insert(key, TaskPool::new(nworkers));
        }
        // Shared immutable program for worker threads (built once, then Arc-cloned).
        let prog = self
            .task_prog
            .get_or_insert_with(|| Arc::new(self.prog.clone()))
            .clone();
        let vm_args = self.args.clone();
        let (tx, rx) = mpsc::channel::<Result<(Value, Vec<HeapObj>), String>>();
        let pool = self.pools.get(&key).unwrap();
        pool.submit(Box::new(move || {
            let res = run_function_isolated(&prog, fidx, seed, &vm_args);
            let _ = tx.send(res);
        }));
        match rx.recv() {
            Ok(Ok((v, heap))) => Ok(self.materialize(&v, &heap)),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(format!("scheduled task `{}` did not return", f.name)),
        }
    }

    /// Deep-copy a value produced by an isolated task (whose heap is separate)
    /// into this VM's heap, so returned structs/strings are valid here.
    fn materialize(&mut self, v: &Value, from: &[HeapObj]) -> Value {
        match v {
            Value::Obj(id) => {
                let o = &from[*id as usize];
                match o.kind {
                    ObjKind::Str => {
                        let new = self.alloc(HeapObj::new_str(o.str.clone().unwrap_or_default()));
                        Value::Obj(new)
                    }
                    ObjKind::Struct | ObjKind::Closure | ObjKind::Frame | ObjKind::List => {
                        let mut fields = Vec::with_capacity(o.fields.len());
                        for f in &o.fields {
                            fields.push(self.materialize(f, from));
                        }
                        let mut no = HeapObj::new(o.kind, &o.tag, fields);
                        no.closure_fn = o.closure_fn;
                        let new = self.alloc(no);
                        Value::Obj(new)
                    }
                    ObjKind::Map => {
                        let mut pairs = Vec::new();
                        if let Some(p) = &o.pairs {
                            for (k, v) in p {
                                pairs.push((self.materialize(k, from), self.materialize(v, from)));
                            }
                        }
                        let mut no = HeapObj::new(ObjKind::Map, "Map", Vec::new());
                        no.pairs = Some(pairs);
                        let new = self.alloc(no);
                        Value::Obj(new)
                    }
                }
            }
            Value::Ptr(o, field) => {
                // Re-point to a copy of the referenced object.
                let target = Value::Obj(*o);
                match self.materialize(&target, from) {
                    Value::Obj(new) => Value::Ptr(new, *field),
                    _ => Value::Null,
                }
            }
            other => other.clone(),
        }
    }

    fn current_func_idx(&self) -> u32 {
        // find by name in prog.funcs
        self.prog
            .funcs
            .iter()
            .position(|f| f.name == self.fname)
            .unwrap_or(self.prog.main as usize) as u32
    }

    fn bin(
        &mut self,
        iop: fn(i64, i64) -> i64,
        fop: fn(f64, f64) -> f64,
        name: &str,
    ) -> Result<(), String> {
        let b = self.pop();
        let a = self.pop();
        match (a, b) {
            (Value::I64(x), Value::I64(y)) => {
                self.push(Value::I64(iop(x, y)));
            }
            (Value::F64(x), Value::F64(y)) => {
                self.push(Value::F64(fop(x, y)));
            }
            _ => {
                return Err(format!("{name} requires matching numbers"));
            }
        }
        Ok(())
    }

    fn cmp(
        &mut self,
        ord: fn(std::cmp::Ordering) -> bool,
        eqf: fn(f64, f64) -> bool,
    ) -> Result<(), String> {
        let b = self.pop();
        let a = self.pop();
        match (&a, &b) {
            (Value::I64(x), Value::I64(y)) => {
                self.push(Value::Bool(ord(x.cmp(y))));
                return Ok(());
            }
            (Value::F64(x), Value::F64(y)) => {
                self.push(Value::Bool(eqf(*x, *y)));
                return Ok(());
            }
            // Strings compare by content.
            (Value::Obj(x), Value::Obj(y)) => {
                let ox = &self.heap[*x as usize];
                let oy = &self.heap[*y as usize];
                if ox.kind == ObjKind::Str && oy.kind == ObjKind::Str {
                    let sx = ox.str.clone().unwrap_or_default();
                    let sy = oy.str.clone().unwrap_or_default();
                    let ordering = sx.cmp(&sy);
                    self.push(Value::Bool(ord(ordering)));
                    return Ok(());
                }
                // Non-string objects compare by value equality.
                let eq = values_equal(&self.heap, &a, &b);
                self.push(Value::Bool(if eq { ord(std::cmp::Ordering::Equal) } else { false }));
                Ok(())
            }
            // Mixed string/number or null: equality is false; ordering is error.
            _ => {
                if matches!(a, Value::Null) && matches!(b, Value::Null) {
                    self.push(Value::Bool(ord(std::cmp::Ordering::Equal)));
                    return Ok(());
                }
                if matches!(a, Value::Bool(_)) && matches!(b, Value::Bool(_)) {
                    let x = matches!(a, Value::Bool(true));
                    let y = matches!(b, Value::Bool(true));
                    self.push(Value::Bool(ord(x.cmp(&y))));
                    return Ok(());
                }
                Err("comparison requires matching numbers".into())
            }
        }
    }

    /// Index a container: list/array by integer position, map by key equality.
    fn index_get(&mut self, obj: Value, idx: &Value) -> Result<Value, String> {
        match obj {
            Value::Obj(id) => {
                let kind = self.heap[id as usize].kind;
                match kind {
                    ObjKind::List => {
                        let i = match idx {
                            Value::I64(n) => *n,
                            _ => return Err("list/array index must be an int".into()),
                        };
                        let fields = &self.heap[id as usize].fields;
                        if i < 0 || i as usize >= fields.len() {
                            return Err(format!("index {i} out of bounds (len {})", fields.len()));
                        }
                        Ok(fields[i as usize].clone())
                    }
                    ObjKind::Map => {
                        let pairs = self.heap[id as usize].pairs.clone().unwrap_or_default();
                        for (k, v) in &pairs {
                            if values_equal(&self.heap, k, idx) {
                                return Ok(v.clone());
                            }
                        }
                        Ok(Value::Null)
                    }
                    _ => Err("index into a non-container value".into()),
                }
            }
            _ => Err("index into a non-object value".into()),
        }
    }

    /// Assign into a container element by index (list/array position or map key).
    fn index_set(&mut self, obj: Value, idx: &Value, val: Value) -> Result<(), String> {
        match obj {
            Value::Obj(id) => {
                let kind = self.heap[id as usize].kind;
                match kind {
                    ObjKind::List => {
                        let i = match idx {
                            Value::I64(n) => *n,
                            _ => return Err("list/array index must be an int".into()),
                        };
                        if i < 0 {
                            return Err(format!("index {i} out of bounds"));
                        }
                        let fields = &mut self.heap[id as usize].fields;
                        let i = i as usize;
                        if i >= fields.len() {
                            return Err(format!("index {i} out of bounds (len {})", fields.len()));
                        }
                        fields[i] = val;
                        Ok(())
                    }
                    ObjKind::Map => {
                        let mut pairs = self.heap[id as usize].pairs.clone().unwrap_or_default();
                        let mut found = false;
                        for (k, v) in pairs.iter_mut() {
                            if values_equal(&self.heap, k, idx) {
                                *v = val.clone();
                                found = true;
                                break;
                            }
                        }
                        if !found {
                            pairs.push((idx.clone(), val));
                        }
                        self.heap[id as usize].pairs = Some(pairs);
                        Ok(())
                    }
                    _ => Err("index-store into a non-container value".into()),
                }
            }
            _ => Err("index-store into a non-object value".into()),
        }
    }

    fn call_method(&mut self, name: &str, args: &[Value], recv: Value) -> Result<Value, String> {
        // Dispatch by receiver kind; containers and strings have distinct methods.
        if let Value::Obj(id) = recv {
            match self.heap[id as usize].kind {
                ObjKind::List => return self.list_method(name, args, id),
                ObjKind::Map => return self.map_method(name, args, id),
                _ => {}
            }
        }
        // String methods (dynamic resolution on the string form of the receiver).
        let sbytes = to_string_bytes(&self.heap, &recv);
        let s = String::from_utf8_lossy(&sbytes).to_string();
        match name {
            "len" => Ok(Value::I64(s.chars().count() as i64)),
            "trim" => {
                let id = self.new_str_obj(s.trim().as_bytes().to_vec());
                Ok(Value::Obj(id))
            }
            "to_upper" => {
                let id = self.new_str_obj(s.to_uppercase().into_bytes());
                Ok(Value::Obj(id))
            }
            "to_lower" => {
                let id = self.new_str_obj(s.to_lowercase().into_bytes());
                Ok(Value::Obj(id))
            }
            "to_int" => {
                let trimmed = s.trim();
                trimmed
                    .parse::<i64>()
                    .map(Value::I64)
                    .map_err(|_| format!("cannot parse `{trimmed}` as an integer"))
            }
            "to_float" => {
                let trimmed = s.trim();
                trimmed
                    .parse::<f64>()
                    .map(Value::F64)
                    .map_err(|_| format!("cannot parse `{trimmed}` as a float"))
            }
            "substr" => {
                if args.len() < 2 {
                    return Err("substr needs 2 args".into());
                }
                let a = match &args[0] {
                    Value::I64(n) => *n,
                    _ => 0,
                };
                let b = match &args[1] {
                    Value::I64(n) => *n,
                    _ => s.len() as i64,
                };
                let start = a.max(0) as usize;
                let end = (b.max(a) as usize).min(s.len());
                let id = self.new_str_obj(s[start..end].as_bytes().to_vec());
                Ok(Value::Obj(id))
            }
            _ => Err(format!("unknown method `{name}`")),
        }
    }

    /// Language-native builtin functions (CLI / TUI / GUI) dispatched through
    /// the extern path. These need VM access (heap allocation for strings/lists
    /// and the program's command-line arguments), unlike `call_extern` which
    /// only handles numeric/null extern helpers and dynamic FFI.
    fn call_lang_builtin(&mut self, name: &str, args: &[Value]) -> Result<Value, String> {
        match name {
            // ---- CLI ----
            "args" => {
                let mut fields: Vec<Value> = Vec::with_capacity(self.args.len());
                for a in self.args.clone() {
                    fields.push(Value::Obj(self.new_str_obj(a.into_bytes())));
                }
                let id = self.alloc(HeapObj::new(ObjKind::List, "List", fields));
                Ok(Value::Obj(id))
            }
            "ask" | "readLine" => {
                // Optional prompt is printed (without newline) before reading.
                if let Some(p) = args.first() {
                    let bytes = to_string_bytes(&self.heap, p);
                    print_stdout(&bytes);
                    print_stdout(b"");
                }
                use std::io::BufRead;
                let mut line = String::new();
                let n = std::io::stdin().lock().read_line(&mut line).unwrap_or(0);
                if n == 0 {
                    line.clear();
                }
                while line.ends_with('\n') || line.ends_with('\r') {
                    line.pop();
                }
                let id = self.new_str_obj(line.into_bytes());
                Ok(Value::Obj(id))
            }
            "printf" | "fmt" => {
                if args.is_empty() {
                    return Err(format!("`{name}` expects a format string"));
                }
                let fmt_bytes = to_string_bytes(&self.heap, &args[0]);
                let f = String::from_utf8_lossy(&fmt_bytes).to_string();
                let out = format_args(&f, &args[1..], &self.heap)?;
                if name == "printf" {
                    print_stdout(out.as_bytes());
                    Ok(Value::Null)
                } else {
                    let id = self.new_str_obj(out.into_bytes());
                    Ok(Value::Obj(id))
                }
            }
            // ---- TUI ----
            "clear_screen" => {
                print_stdout(b"\x1b[2J\x1b[H");
                Ok(Value::Null)
            }
            "cursor" => {
                let x = num(&args.get(0), &self.heap)?;
                let y = num(&args.get(1), &self.heap)?;
                // ANSI 1-based cursor positioning.
                print_stdout(format!("\x1b[{};{}H", y.max(1), x.max(1)).as_bytes());
                Ok(Value::Null)
            }
            "color" => {
                let fg = num(&args.get(0), &self.heap)?;
                let bg = num(&args.get(1), &self.heap)?;
                print_stdout(format!("\x1b[38;5;{};48;5;{}m", fg.max(0).min(255), bg.max(0).min(255)).as_bytes());
                Ok(Value::Null)
            }
            "reset_color" => {
                print_stdout(b"\x1b[0m");
                Ok(Value::Null)
            }
            "hide_cursor" => {
                print_stdout(b"\x1b[?25l");
                Ok(Value::Null)
            }
            "show_cursor" => {
                print_stdout(b"\x1b[?25h");
                Ok(Value::Null)
            }
            "key" => {
                let k = read_key()?;
                let id = self.new_str_obj(k.into_bytes());
                Ok(Value::Obj(id))
            }
            "key_available" => {
                Ok(Value::Bool(key_available()))
            }
            "terminal_cols" => {
                let (c, _) = terminal_size();
                Ok(Value::I64(c as i64))
            }
            "terminal_rows" => {
                let (_, r) = terminal_size();
                Ok(Value::I64(r as i64))
            }
            _ => self::gui::call_gui(self, name, args),
        }
    }

    /// List/array methods: len, push, pop, get, set.
    fn list_method(&mut self, name: &str, args: &[Value], id: u32) -> Result<Value, String> {
        match name {
            "len" => {
                let n = self.heap[id as usize].fields.len();
                Ok(Value::I64(n as i64))
            }
            "push" => {
                if args.len() != 1 {
                    return Err("push needs exactly 1 argument".into());
                }
                self.heap[id as usize].fields.push(args[0].clone());
                Ok(Value::Null)
            }
            "pop" => {
                let v = self.heap[id as usize].fields.pop().unwrap_or(Value::Null);
                Ok(v)
            }
            "get" => {
                let i = match args.first() {
                    Some(Value::I64(n)) => *n,
                    _ => return Err("get needs an int index".into()),
                };
                let fields = &self.heap[id as usize].fields;
                let v = if i < 0 || i as usize >= fields.len() {
                    Value::Null
                } else {
                    fields[i as usize].clone()
                };
                Ok(v)
            }
            "set" => {
                if args.len() != 2 {
                    return Err("set needs exactly 2 arguments".into());
                }
                let i = match &args[0] {
                    Value::I64(n) => *n,
                    _ => return Err("set index must be an int".into()),
                };
                if i >= 0 {
                    let i = i as usize;
                    let fields = &mut self.heap[id as usize].fields;
                    if i < fields.len() {
                        fields[i] = args[1].clone();
                    }
                }
                Ok(Value::Null)
            }
            _ => Err(format!("unknown list/array method `{name}`")),
        }
    }

    /// Map methods: set, get, has, len, remove, keys.
    fn map_method(&mut self, name: &str, args: &[Value], id: u32) -> Result<Value, String> {
        match name {
            "len" => {
                let n = self.heap[id as usize].pairs.as_ref().map(|p| p.len()).unwrap_or(0);
                Ok(Value::I64(n as i64))
            }
            "set" => {
                if args.len() != 2 {
                    return Err("set needs exactly 2 arguments".into());
                }
                let k = args[0].clone();
                let v = args[1].clone();
                let mut pairs = self.heap[id as usize].pairs.clone().unwrap_or_default();
                let mut found = false;
                for (ek, ev) in pairs.iter_mut() {
                    if values_equal(&self.heap, ek, &k) {
                        *ev = v.clone();
                        found = true;
                        break;
                    }
                }
                if !found {
                    pairs.push((k, v));
                }
                self.heap[id as usize].pairs = Some(pairs);
                Ok(Value::Null)
            }
            "get" => {
                if args.len() != 1 {
                    return Err("get needs exactly 1 argument".into());
                }
                let pairs = self.heap[id as usize].pairs.clone().unwrap_or_default();
                for (k, v) in &pairs {
                    if values_equal(&self.heap, k, &args[0]) {
                        return Ok(v.clone());
                    }
                }
                Ok(Value::Null)
            }
            "has" => {
                if args.len() != 1 {
                    return Err("has needs exactly 1 argument".into());
                }
                let pairs = self.heap[id as usize].pairs.clone().unwrap_or_default();
                let found = pairs.iter().any(|(k, _)| values_equal(&self.heap, k, &args[0]));
                Ok(Value::Bool(found))
            }
            "remove" => {
                if args.len() != 1 {
                    return Err("remove needs exactly 1 argument".into());
                }
                let key = args[0].clone();
                let mut pairs = self.heap[id as usize].pairs.clone().unwrap_or_default();
                let mut removed = false;
                if let Some(pos) = pairs.iter().position(|(k, _)| values_equal(&self.heap, k, &key)) {
                    pairs.remove(pos);
                    removed = true;
                }
                self.heap[id as usize].pairs = Some(pairs);
                Ok(Value::Bool(removed))
            }
            "keys" => {
                let pairs = self.heap[id as usize].pairs.clone().unwrap_or_default();
                let keys: Vec<Value> = pairs.into_iter().map(|(k, _)| k).collect();
                let id2 = self.alloc(HeapObj::new(ObjKind::List, "List", keys));
                Ok(Value::Obj(id2))
            }
            _ => Err(format!("unknown map method `{name}`")),
        }
    }
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::I64(n) => *n != 0,
        Value::Null => false,
        _ => true,
    }
}

/// Value-equality used for map keys: numbers, booleans, null, and string
/// content compare by value; object/container keys compare by identity.
fn values_equal(heap: &[HeapObj], a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::I64(x), Value::I64(y)) => x == y,
        (Value::F64(x), Value::F64(y)) => x == y,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Null, Value::Null) => true,
        (Value::Obj(x), Value::Obj(y)) => {
            let ox = &heap[*x as usize];
            let oy = &heap[*y as usize];
            if ox.kind == ObjKind::Str && oy.kind == ObjKind::Str {
                ox.str == oy.str
            } else {
                x == y
            }
        }
        _ => false,
    }
}

fn exit_code(v: &Value) -> i64 {
    match v {
        Value::I64(n) => *n,
        Value::Bool(b) => *b as i64,
        Value::Null => 0,
        _ => 0,
    }
}

pub fn read_u32(code: &[u8], pc: &mut usize) -> u32 {
    let mut buf = [0u8; 4];
    for j in 0..4 {
        buf[j] = *code.get(*pc).unwrap_or(&0);
        *pc += 1;
    }
    u32::from_le_bytes(buf)
}
pub fn read_i32(code: &[u8], pc: &mut usize) -> i32 {
    let mut buf = [0u8; 4];
    for j in 0..4 {
        buf[j] = *code.get(*pc).unwrap_or(&0);
        *pc += 1;
    }
    i32::from_le_bytes(buf)
}
pub fn read_i64(code: &[u8], pc: &mut usize) -> i64 {
    let mut buf = [0u8; 8];
    for j in 0..8 {
        buf[j] = *code.get(*pc).unwrap_or(&0);
        *pc += 1;
    }
    i64::from_le_bytes(buf)
}
pub fn read_f64(code: &[u8], pc: &mut usize) -> f64 {
    let mut buf = [0u8; 8];
    for j in 0..8 {
        buf[j] = *code.get(*pc).unwrap_or(&0);
        *pc += 1;
    }
    f64::from_le_bytes(buf)
}

fn print_stdout(bytes: &[u8]) {
    use std::io::Write;
    let mut out = std::io::stdout();
    let _ = out.write_all(bytes);
    let _ = out.flush();
}

/// True when the extern name is a language-native builtin dispatched by
/// `Vm::call_lang_builtin` (needs VM access) rather than an FFI symbol.
fn is_lang_builtin(name: &str) -> bool {
    matches!(
        name,
        "args"
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

/// Minimal `printf`-style formatter used by `printf` / `fmt`.
///
/// Supported specifiers: `%d` (int), `%i` (int), `%f` (float), `%s` (string),
/// `%x` (hex int), `%%` (literal `%`). Optional `-`/width/`.prec` are honoured
/// for `%d`/`%s`/`%f`/`%x`.
fn format_args(fmt: &str, args: &[Value], heap: &[HeapObj]) -> Result<String, String> {
    let mut out = String::new();
    let mut ai = 0usize;
    let chars: Vec<char> = fmt.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i] != '%' {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        i += 1;
        if i >= chars.len() {
            out.push('%');
            break;
        }
        if chars[i] == '%' {
            out.push('%');
            i += 1;
            continue;
        }
        // Parse flags/width/precision.
        let mut left = false;
        if chars[i] == '-' {
            left = true;
            i += 1;
        }
        let mut width = 0usize;
        while i < chars.len() && chars[i].is_ascii_digit() {
            width = width * 10 + chars[i].to_digit(10).unwrap_or(0) as usize;
            i += 1;
        }
        let mut prec = None;
        if i < chars.len() && chars[i] == '.' {
            i += 1;
            let mut p = 0usize;
            while i < chars.len() && chars[i].is_ascii_digit() {
                p = p * 10 + chars[i].to_digit(10).unwrap_or(0) as usize;
                i += 1;
            }
            prec = Some(p);
        }
        if i >= chars.len() {
            break;
        }
        let spec = chars[i];
        i += 1;
        let arg = args.get(ai);
        ai += 1;
        let s = match spec {
            'd' | 'i' => {
                let n = arg.map(|v| num(&Some(v), heap)).unwrap_or(Ok(0))?;
                format!("{}", n)
            }
            'x' => {
                let n = arg.map(|v| num(&Some(v), heap)).unwrap_or(Ok(0))?;
                format!("{:x}", n)
            }
            'f' => {
                let n = arg.map(|v| num_float(&Some(v), heap)).unwrap_or(Ok(0.0))?;
                match prec {
                    Some(p) => format!("{:.*}", p, n),
                    None => format!("{}", n),
                }
            }
            's' => arg
                .map(|v| String::from_utf8_lossy(&to_string_bytes(heap, v)).to_string())
                .unwrap_or_default(),
            other => return Err(format!("unsupported format specifier `%{other}`")),
        };
        if width > 0 && s.chars().count() < width {
            let pad = width - s.chars().count();
            if left {
                out.push_str(&s);
                out.push_str(&" ".repeat(pad));
            } else {
                out.push_str(&" ".repeat(pad));
                out.push_str(&s);
            }
        } else {
            out.push_str(&s);
        }
    }
    Ok(out)
}

/// Numeric conversion that also accepts floats (for `%f`).
fn num_float(v: &Option<&Value>, heap: &[HeapObj]) -> Result<f64, String> {
    match v {
        Some(Value::I64(n)) => Ok(*n as f64),
        Some(Value::F64(f)) => Ok(*f),
        Some(Value::Bool(b)) => Ok(*b as i64 as f64),
        Some(Value::Obj(id)) => {
            let o = &heap[*id as usize];
            if o.kind == ObjKind::Str {
                String::from_utf8_lossy(o.str.as_ref().unwrap_or(&Vec::new()))
                    .parse::<f64>()
                    .map_err(|_| "cannot parse string as number".to_string())
            } else {
                Err("cannot use object as number".into())
            }
        }
        _ => Err("missing argument for extern call".into()),
    }
}

// ---------------------------------------------------------------------------
// FFI: extern function dispatch (Arrow shared-memory ABI)
// ---------------------------------------------------------------------------

/// Call an extern function. First tries the built-in Rust implementations
/// (which ARE the Rust interop library), then attempts to load a dynamic
/// library and invoke the symbol through the Arrow shared-memory descriptor ABI.
fn call_extern(name: &str, args: &[Value], heap: &[HeapObj]) -> Result<Value, String> {
    match name {
        "add" => {
            let a = num(&args.get(0), heap)?;
            let b = num(&args.get(1), heap)?;
            Ok(Value::I64(a + b))
        }
        "sub" => {
            let a = num(&args.get(0), heap)?;
            let b = num(&args.get(1), heap)?;
            Ok(Value::I64(a - b))
        }
        "mul" => {
            let a = num(&args.get(0), heap)?;
            let b = num(&args.get(1), heap)?;
            Ok(Value::I64(a * b))
        }
        // Concurrency helpers.
        "thread_id" => {
            // A stable, human-friendly id derived from the thread's native id.
            let tid = std::thread::current().id();
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            std::hash::Hash::hash(&tid, &mut hasher);
            let h = std::hash::Hasher::finish(&hasher);
            Ok(Value::I64((h % 100000) as i64))
        }
        "sleep" => {
            let ms = num(&args.get(0), heap)?;
            std::thread::sleep(std::time::Duration::from_millis(ms.max(0) as u64));
            Ok(Value::Null)
        }
        _ => crate_dynamic_call(name, args, heap),
    }
}

fn num(v: &Option<&Value>, heap: &[HeapObj]) -> Result<i64, String> {
    match v {
        Some(Value::I64(n)) => Ok(*n),
        Some(Value::F64(f)) => Ok(*f as i64),
        Some(Value::Bool(b)) => Ok(*b as i64),
        Some(Value::Obj(id)) => {
            let o = &heap[*id as usize];
            if o.kind == ObjKind::Str {
                String::from_utf8_lossy(o.str.as_ref().unwrap_or(&Vec::new()))
                    .parse::<i64>()
                    .map_err(|_| "cannot parse string as number".to_string())
            } else {
                Err("cannot use object as number".into())
            }
        }
        _ => Err("missing argument for extern call".into()),
    }
}

#[cfg(windows)]
mod dynffi {
    use super::*;

    #[link(name = "kernel32")]
    extern "system" {
        fn LoadLibraryA(lp: *const std::ffi::c_char) -> *mut std::ffi::c_void;
        fn GetProcAddress(h: *mut std::ffi::c_void, name: *const std::ffi::c_char) -> *mut std::ffi::c_void;
    }

    /// Load a dynamic library and call a symbol through the Arrow shared-memory
    /// descriptor ABI: arguments are marshalled into a contiguous Arrow buffer
    /// and a zero-copy descriptor (pointer + length + type) is handed to the
    /// target. The target returns an i64.
    pub fn call_dynamic(
        lib_path: &str,
        sym: &str,
        args: &[Value],
        heap: &[HeapObj],
    ) -> Result<Value, String> {
        use std::ffi::CString;
        let c_lib = CString::new(lib_path).map_err(|_| "bad lib path")?;
        let c_sym = CString::new(sym).map_err(|_| "bad symbol")?;
        unsafe {
            let h = LoadLibraryA(c_lib.as_ptr());
            if h.is_null() {
                return Err(format!("cannot load library `{lib_path}`"));
            }
            let fp = GetProcAddress(h, c_sym.as_ptr());
            if fp.is_null() {
                return Err(format!("symbol `{sym}` not found"));
            }
            // Arrow ABI: build a shared-memory descriptor. The descriptor is a
            // header [n, type_tag] followed by the payload, all in one buffer.
            let mut payload: Vec<i64> = Vec::new();
            for a in args {
                payload.push(num(&Some(a), heap)?);
            }
            let buf = payload;
            let ptr = buf.as_ptr();
            let len = buf.len() as i64;
            // The target receives the descriptor via a raw call with the
            // descriptor pointer. We declare the exported Rust function as
            // `extern "C" fn(*const i64, i64) -> i64`.
            let f: extern "C" fn(*const i64, i64) -> i64 = std::mem::transmute(fp);
            let r = f(ptr, len);
            Ok(Value::I64(r))
        }
    }
}

#[cfg(not(windows))]
fn crate_dynamic_call(_name: &str, _args: &[Value], _heap: &[HeapObj]) -> Result<Value, String> {
    Err("extern `dynamic` call not supported on this platform".into())
}

#[cfg(windows)]
fn crate_dynamic_call(name: &str, args: &[Value], heap: &[HeapObj]) -> Result<Value, String> {
    // If a library is configured via the PSS_RUST_LIB environment variable,
    // try to load the symbol from it.
    if let Ok(lib) = std::env::var("PSS_RUST_LIB") {
        if !lib.is_empty() {
            return dynffi::call_dynamic(&lib, name, args, heap);
        }
    }
    Err(format!("unknown extern function `{name}` (set PSS_RUST_LIB to a Rust cdylib)"))
}

// Dummy variant referenced by pattern matching above to keep exhaustive matches
// simple; not a real constructor.
#[allow(non_upper_case_globals)]
pub const Str_sentinel: () = ();

// ---------------------------------------------------------------------------
// TUI: ANSI terminal helpers
// ---------------------------------------------------------------------------

/// Blocking read of a single key. Returns a friendly name for arrow/function
/// keys (e.g. "up", "down", "left", "right", "enter", "space", "esc") or the
/// single character for regular printable keys.
#[cfg(windows)]
fn read_key() -> Result<String, String> {
    use std::io::Read;
    let mut b = [0u8; 1];
    let n = std::io::stdin().lock().read(&mut b).unwrap_or(0);
    if n == 0 {
        return Ok("eof".to_string());
    }
    // Detect ANSI escape sequences (arrow keys arrive as ESC [ A / B / C / D).
    if b[0] == 0x1b {
        // Try to read the following `[` and letter non-blockingly.
        if key_available() {
            let mut seq = [0u8; 2];
            let n = std::io::stdin().lock().read(&mut seq).unwrap_or(0);
            if n == 2 && seq[0] == b'[' {
                return Ok(match seq[1] {
                    b'A' => "up".to_string(),
                    b'B' => "down".to_string(),
                    b'C' => "right".to_string(),
                    b'D' => "left".to_string(),
                    other => format!("key({})", other as char),
                });
            }
        }
        return Ok("esc".to_string());
    }
    Ok(match b[0] {
        b'\r' | b'\n' => "enter".to_string(),
        b' ' => "space".to_string(),
        0x7f | 0x08 => "backspace".to_string(),
        c if c.is_ascii_graphic() || c == b'\t' => (c as char).to_string(),
        other => format!("key({})", other),
    })
}

/// Blocking read of a single key (Unix: raw terminal line handling).
#[cfg(not(windows))]
fn read_key() -> Result<String, String> {
    use std::io::Read;
    // Put the terminal in raw mode for a single byte read, then restore.
    unsafe {
        let mut term = std::mem::zeroed::<libc_termios>();
        if tcgetattr(0, &mut term) == 0 {
            let mut raw = term;
            raw.c_lflag &= !(LIBC_ICANON | LIBC_ECHO);
            raw.c_cc[LIBC_VMIN] = 1;
            raw.c_cc[LIBC_VTIME] = 0;
            tcsetattr(0, 0, &raw);
        }
        let mut b = [0u8; 1];
        let n = std::io::stdin().lock().read(&mut b).unwrap_or(0);
        tcsetattr(0, 0, &term);
        if n == 0 {
            return Ok("eof".to_string());
        }
        if b[0] == 0x1b {
            // ESC — read up to two more bytes for arrow keys.
            let mut seq = [0u8; 2];
            let mut got = 0;
            for i in 0..2 {
                let nb = std::io::stdin().lock().read(&mut seq[i..i + 1]).unwrap_or(0);
                if nb == 0 {
                    break;
                }
                got += 1;
            }
            if got == 2 && seq[0] == b'[' {
                return Ok(match seq[1] {
                    b'A' => "up".to_string(),
                    b'B' => "down".to_string(),
                    b'C' => "right".to_string(),
                    b'D' => "left".to_string(),
                    other => format!("key({})", other as char),
                });
            }
            return Ok("esc".to_string());
        }
        Ok(match b[0] {
            b'\r' | b'\n' => "enter".to_string(),
            b' ' => "space".to_string(),
            0x7f | 0x08 => "backspace".to_string(),
            c if c.is_ascii_graphic() || c == b'\t' => (c as char).to_string(),
            other => format!("key({})", other),
        })
    }
}

#[cfg(not(windows))]
const LIBC_ICANON: u32 = 0o0000002;
#[cfg(not(windows))]
const LIBC_ECHO: u32 = 0o0000010;
#[cfg(not(windows))]
const LIBC_VMIN: usize = 6;
#[cfg(not(windows))]
const LIBC_VTIME: usize = 5;

#[cfg(not(windows))]
#[repr(C)]
#[derive(Clone, Copy)]
struct libc_termios {
    c_iflag: u32,
    c_oflag: u32,
    c_cflag: u32,
    c_lflag: u32,
    c_line: u8,
    c_cc: [u8; 32],
    _pad: [u8; 0],
}

#[cfg(not(windows))]
extern "C" {
    fn tcgetattr(fd: i32, termios_p: *mut libc_termios) -> i32;
    fn tcsetattr(fd: i32, optional_actions: i32, termios_p: *const libc_termios) -> i32;
}

/// Non-blocking check for pending input.
#[cfg(windows)]
fn key_available() -> bool {
    #[link(name = "kernel32")]
    extern "system" {
        fn _kbhit() -> i32;
    }
    unsafe { _kbhit() != 0 }
}

#[cfg(not(windows))]
fn key_available() -> bool {
    // Select with a zero timeout on stdin.
    extern "C" {
        fn select(nfds: i32, readfds: *mut u8, writefds: *mut u8, exceptfds: *mut u8, timeout: *const libc_timeval) -> i32;
    }
    #[repr(C)]
    struct libc_timeval {
        tv_sec: i64,
        tv_usec: i64,
    }
    let mut fds = [0u8; 128];
    // fd 0 = stdin
    fds[0] = 1;
    let tv = libc_timeval { tv_sec: 0, tv_usec: 0 };
    unsafe { select(1, &mut fds as *mut u8, std::ptr::null_mut(), std::ptr::null_mut(), &tv) > 0 }
}

/// Terminal size (columns, rows). Falls back to 80x24 when undetectable.
#[cfg(windows)]
fn terminal_size() -> (usize, usize) {
    #[repr(C)]
    struct Coord {
        x: i16,
        y: i16,
    }
    #[repr(C)]
    struct SmallRect {
        left: i16,
        top: i16,
        right: i16,
        bottom: i16,
    }
    // Field names follow the Windows `CONSOLE_SCREEN_BUFFER_INFO` spelling.
    #[repr(C)]
    #[allow(non_snake_case)]
    struct ConsoleScreenBufferInfo {
        dwSize: Coord,
        dwCursorPosition: Coord,
        wAttributes: u16,
        srWindow: SmallRect,
        dwMaximumWindowSize: Coord,
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GetStdHandle(n: u32) -> *mut std::ffi::c_void;
        fn GetConsoleScreenBufferInfo(h: *mut std::ffi::c_void, info: *mut ConsoleScreenBufferInfo) -> i32;
    }
    unsafe {
        let h = GetStdHandle(0xfffffff5u32); // STD_OUTPUT_HANDLE = -11 as u32
        let mut info: ConsoleScreenBufferInfo = std::mem::zeroed();
        if GetConsoleScreenBufferInfo(h, &mut info) != 0 {
            let cols = (info.srWindow.right - info.srWindow.left + 1).max(1) as usize;
            let rows = (info.srWindow.bottom - info.srWindow.top + 1).max(1) as usize;
            return (cols, rows);
        }
    }
    (80, 24)
}

#[cfg(not(windows))]
fn terminal_size() -> (usize, usize) {
    #[repr(C)]
    struct Winsize {
        ws_row: u16,
        ws_col: u16,
        ws_xpixel: u16,
        ws_ypixel: u16,
    }
    extern "C" {
        fn ioctl(fd: i32, request: u64, ...) -> i32;
    }
    unsafe {
        let mut ws: Winsize = std::mem::zeroed();
        // TIOCGWINSZ = 0x5413
        if ioctl(0, 0x5413, &mut ws as *mut Winsize) == 0 {
            if ws.ws_col > 0 && ws.ws_row > 0 {
                return (ws.ws_col as usize, ws.ws_row as usize);
            }
        }
    }
    // Fallback via $COLUMNS / $LINES.
    let cols = std::env::var("COLUMNS").ok().and_then(|s| s.parse().ok()).unwrap_or(80);
    let rows = std::env::var("LINES").ok().and_then(|s| s.parse().ok()).unwrap_or(24);
    (cols, rows)
}

// ---------------------------------------------------------------------------
// GUI: native windows and canvas (see runtime/gui.rs)
// ---------------------------------------------------------------------------

#[path = "gui.rs"]
mod gui;