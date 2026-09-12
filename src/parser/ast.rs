//! Abstract Syntax Tree for Pointerses.
//!
//! The AST is a first-class product of the parser. It contains first-class
//! nodes for pointer types (`Ptr`), algebraic-path pointer expressions
//! (`Addr` / `Deref`), closures, struct definitions, annotations (including the
//! concurrency annotations `@Auto` / `@Manual(fixed=N)` and region hints) and
//! region descriptors used by the region calculus.

/// A source position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Span {
    pub line: usize,
    pub col: usize,
}

impl Span {
    pub fn new(line: usize, col: usize) -> Self {
        Span { line, col }
    }
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Value types. `VType` default-allocation is stack; `Heap`-bound objects live
/// in the heap region. `Ptr` is a first-class pointer node.
#[derive(Debug, Clone, PartialEq)]
pub enum Type {
    Int,
    Float,
    Bool,
    Str,
    Void,
    /// Named struct / class type.
    Named(String),
    /// User-defined generic instantiation `Foo[int, String]`.
    Generic(String, Vec<Type>),
    /// Type-variable reference (a generic parameter), e.g. `T`.
    Var(String),
    /// Trait object type `dyn Trait` (dispatched at runtime).
    Trait(String),
    /// Algebraic-path pointer type with mutability and a target type.
    Ptr { mutable: bool, target: Box<Type> },
    /// Function / closure type.
    Fn { params: Vec<Type>, ret: Box<Type> },
    /// Fixed-size array type `[T]`.
    Array(Box<Type>),
    /// Growable list type `List[T]`.
    List(Box<Type>),
    /// Map type `Map[K, V]`.
    Map(Box<Type>, Box<Type>),
}

impl Type {
    pub fn display(&self) -> String {
        match self {
            Type::Int => "int".into(),
            Type::Float => "float".into(),
            Type::Bool => "bool".into(),
            Type::Str => "String".into(),
            Type::Void => "void".into(),
            Type::Named(n) => n.clone(),
            Type::Generic(n, args) => {
                let as_: Vec<String> = args.iter().map(|a| a.display()).collect();
                format!("{n}[{}]", as_.join(", "))
            }
            Type::Var(v) => v.clone(),
            Type::Trait(t) => format!("dyn {t}"),
            Type::Ptr { mutable, target } => {
                let m = if *mutable { "mut " } else { "" };
                format!("&{m}{}", target.display())
            }
            Type::Fn { params, ret } => {
                let ps: Vec<String> = params.iter().map(|p| p.display()).collect();
                format!("({}) -> {}", ps.join(", "), ret.display())
            }
            Type::Array(t) => format!("[{}]", t.display()),
            Type::List(t) => format!("List[{}]", t.display()),
            Type::Map(k, v) => format!("Map[{}, {}]", k.display(), v.display()),
        }
    }
}

// ---------------------------------------------------------------------------
// Expressions
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
}

impl BinOp {
    pub fn symbol(&self) -> &'static str {
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::Mod => "%",
            BinOp::Eq => "==",
            BinOp::Ne => "!=",
            BinOp::Lt => "<",
            BinOp::Le => "<=",
            BinOp::Gt => ">",
            BinOp::Ge => ">=",
            BinOp::And => "&&",
            BinOp::Or => "||",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Not,
    /// Pointer address-of (`&v`); the mutability is carried on `Addr`.
    Addr,
    /// Mutable pointer address-of (`&mut v`).
    AddrMut,
    /// Pointer dereference (`*p`).
    Deref,
}

/// A closure expression. `captures` is filled by the semantic analyzer with the
/// names of free variables captured from enclosing scopes.
#[derive(Debug, Clone)]
pub struct ClosureExpr {
    pub params: Vec<Param>,
    pub ret: Option<Type>,
    pub body: Box<Expr>,
    pub span: Span,
    pub captures: Vec<String>,
}

/// A fragment of an interpolated string in the AST.
#[derive(Debug, Clone)]
pub enum InterpPart {
    /// Plain literal text.
    Text(String),
    /// An interpolated sub-expression (already parsed).
    Expr(Box<Expr>),
}

#[derive(Debug, Clone)]
pub enum Expr {
    IntLit(i64, Span),
    FloatLit(f64, Span),
    BoolLit(bool, Span),
    StrLit(String, Span),
    NullLit(Span),
    /// Variable / constant reference.
    Ident(String, Span),
    /// Binary expression.
    Binary(BinOp, Box<Expr>, Box<Expr>, Span),
    /// Unary expression (`-x`, `!x`, `&x`, `&mut x`, `*x`).
    Unary(UnOp, Box<Expr>, Span),
    /// Function call: callee expression + arguments.
    Call(Box<Expr>, Vec<Expr>, Span),
    /// Field / property access: `obj.field`.
    Field(Box<Expr>, String, Span),
    /// Method call: `obj.method(args)`.
    MethodCall(Box<Expr>, String, Vec<Expr>, Span),
    /// Assignment: `lhs = rhs`.
    Assign(Box<Expr>, Box<Expr>, Span),
    /// Struct literal: `Point { x: 1, y: 2 }`.
    StructLit(String, Vec<(String, Expr)>, Span),
    /// Generic struct literal: `Box[int] { value: 1 }`.
    GenericStructLit(String, Vec<Type>, Vec<(String, Expr)>, Span),
    /// Array / list literal: `[1, 2, 3]` or `[]`.
    ArrayLit(Vec<Expr>, Span),
    /// Map literal: `Map[K, V] { k1: v1, k2: v2 }`.
    MapLit(Vec<(Expr, Expr)>, Span),
    /// Indexing: `container[index]` (list/array element or map lookup).
    Index(Box<Expr>, Box<Expr>, Span),
    /// Closure literal.
    Closure(ClosureExpr),
    /// Block expression (evaluates to last expression).
    Block(Vec<Stmt>, Span),
    /// If-as-expression.
    IfExpr(Box<Expr>, Box<Expr>, Option<Box<Expr>>, Span),
    /// Ternary: `cond ? then : else`.
    Ternary(Box<Expr>, Box<Expr>, Box<Expr>, Span),
    /// String interpolation: `"a ${x} b"`.
    InterpStr(Vec<InterpPart>, Span),
    /// Module path reference: `math::square` (function name for a call).
    Path(Vec<String>, Span),
    /// Generic function call with explicit type arguments: `identity[int](5)`.
    GenericCall(String, Vec<Type>, Vec<Expr>, Span),
}

impl Expr {
    pub fn span(&self) -> Span {
        match self {
            Expr::IntLit(_, s)
            | Expr::FloatLit(_, s)
            | Expr::BoolLit(_, s)
            | Expr::StrLit(_, s)
            | Expr::NullLit(s)
            | Expr::Ident(_, s)
            | Expr::Binary(_, _, _, s)
            | Expr::Unary(_, _, s)
            | Expr::Call(_, _, s)
            | Expr::Field(_, _, s)
            | Expr::MethodCall(_, _, _, s)
            | Expr::Assign(_, _, s)
            | Expr::StructLit(_, _, s)
            | Expr::GenericStructLit(_, _, _, s)
            | Expr::ArrayLit(_, s)
            | Expr::MapLit(_, s)
            | Expr::Index(_, _, s)
            | Expr::Block(_, s)
            | Expr::IfExpr(_, _, _, s)
            | Expr::Ternary(_, _, _, s)
            | Expr::InterpStr(_, s)
            | Expr::Path(_, s)
            | Expr::GenericCall(_, _, _, s) => *s,
            Expr::Closure(c) => c.span,
        }
    }
}

// ---------------------------------------------------------------------------
// Statements
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum Stmt {
    /// `let name: Type = expr;`
    Let(Param, Expr, Span),
    /// Expression statement.
    Expr(Expr, Span),
    /// `return expr;`
    Return(Option<Expr>, Span),
    /// `if cond { ... } else { ... }`
    If(Expr, Vec<Stmt>, Option<Vec<Stmt>>, Span),
    /// `while cond { ... }`
    While(Expr, Vec<Stmt>, Span),
    /// `for (init; cond; step) { ... }` (each clause optional)
    For(Vec<Stmt>, Option<Expr>, Option<Expr>, Vec<Stmt>, Span),
    /// `break` — exit the innermost loop.
    Break(Span),
    /// `continue` — jump to the innermost loop's next iteration.
    Continue(Span),
    /// `@annotation` statement-level directive.
    Directive(Annotation, Span),
    /// `try { ... } catch (e) { ... } finally { ... }`
    /// Catch is `(var name, body, span)`; finally body optional.
    Try(Vec<Stmt>, Option<(String, Vec<Stmt>, Span)>, Option<Vec<Stmt>>, Span),
    /// `throw expr` — raise a runtime exception value.
    Throw(Box<Expr>, Span),
}

// ---------------------------------------------------------------------------
// Declarations
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Param {
    pub name: String,
    pub ty: Option<Type>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Field {
    pub name: String,
    pub ty: Type,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct StructDef {
    pub name: String,
    /// Generic type parameters, e.g. `struct Box[T] { ... }` -> `["T"]`.
    pub type_params: Vec<String>,
    pub fields: Vec<Field>,
    pub annotations: Vec<Annotation>,
    /// Namespace prefix (from `module M;`) applied to the registered name.
    pub module: Option<String>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct FuncDef {
    pub name: String,
    /// Generic type parameters, e.g. `fn id[T](x: T) -> T`.
    pub type_params: Vec<String>,
    /// Trait bounds for type parameters: `(param, trait)`, e.g. `T: Describe`.
    pub bounds: Vec<(String, String)>,
    pub params: Vec<Param>,
    pub ret: Option<Type>,
    pub body: Vec<Stmt>,
    pub annotations: Vec<Annotation>,
    /// Namespace prefix (from `module M;`) applied to the registered name.
    pub module: Option<String>,
    pub span: Span,
}

/// One method signature declared inside a `trait`.
#[derive(Debug, Clone)]
pub struct TraitMethod {
    pub name: String,
    pub params: Vec<Param>,
    pub ret: Option<Type>,
    pub span: Span,
}

/// `trait Name { fn method(...) -> Ret; ... }`.
#[derive(Debug, Clone)]
pub struct TraitDef {
    pub name: String,
    pub methods: Vec<TraitMethod>,
    pub span: Span,
}

/// `impl [Trait for] TypeName[T1, T2] { fn ... { ... } }`.
#[derive(Debug, Clone)]
pub struct ImplBlock {
    /// `Some(trait)` for `impl Trait for T`, `None` for an inherent impl.
    pub trait_name: Option<String>,
    /// The receiver struct name (template name when generic, e.g. `Box`).
    pub type_name: String,
    /// Generic parameters of the target type (`Box[T]` -> `["T"]`).
    pub type_params: Vec<String>,
    pub methods: Vec<FuncDef>,
    /// Namespace prefix (from `module M;`) of the file containing this impl.
    pub module: Option<String>,
    pub span: Span,
}

/// `module name;` — puts the file's declarations into a namespace.
#[derive(Debug, Clone)]
pub struct ModuleDecl {
    pub name: String,
    pub span: Span,
}

/// `import "path.psp";` — recursively merges another source file.
#[derive(Debug, Clone)]
pub struct ImportDecl {
    pub path: String,
    pub span: Span,
}

/// A top-level global variable: `var name = expr;` (or `var name: Type = expr;`).
///
/// Globals live in VM-wide storage (see `OP_GLOAD` / `OP_GSTORE`) rather than in a
/// function's frame, so they survive across callbacks (GUI `on_key` / `on_click`).
/// They are initialized by the synthetic `__init_globals` function before `main`.
#[derive(Debug, Clone)]
pub struct GlobalDecl {
    pub name: String,
    pub ty: Option<Type>,
    pub init: Expr,
    pub span: Span,
}

/// An extern FFI declaration (Arrow shared-memory ABI by default).
#[derive(Debug, Clone)]
pub struct ExternDecl {
    pub name: String,
    pub params: Vec<Param>,
    pub ret: Option<Type>,
    pub abi: String, // "arrow" | "c"
    pub module: Option<String>,
    pub span: Span,
}

#[derive(Debug, Clone, Default)]
pub struct Program {
    pub structs: Vec<StructDef>,
    pub funcs: Vec<FuncDef>,
    pub externs: Vec<ExternDecl>,
    pub traits: Vec<TraitDef>,
    pub impls: Vec<ImplBlock>,
    pub modules: Vec<ModuleDecl>,
    pub imports: Vec<ImportDecl>,
    /// Top-level `var` declarations; initialized in order before `main` runs.
    pub globals: Vec<GlobalDecl>,
}

// ---------------------------------------------------------------------------
// Annotations
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum AnnArg {
    Str(String),
    Int(i64),
    Float(f64),
    Ident(String),
}

impl AnnArg {
    pub fn as_int(&self) -> Option<i64> {
        match self {
            AnnArg::Int(v) => Some(*v),
            _ => None,
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            AnnArg::Str(v) => Some(v),
            AnnArg::Ident(v) => Some(v),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Annotation {
    pub name: String,
    pub args: Vec<NamedArg>,
    pub span: Span,
}

/// A single annotation argument; may be named (`fixed=4`) or positional.
#[derive(Debug, Clone)]
pub struct NamedArg {
    pub name: Option<String>,
    pub value: AnnArg,
}

impl Annotation {
    /// Extract a named integer argument, e.g. `@Manual(fixed=4)`.
    pub fn int_arg(&self, key: &str) -> Option<i64> {
        for a in &self.args {
            if a.name.as_deref() == Some(key) {
                return a.value.as_int();
            }
        }
        None
    }

    /// Extract a named string argument.
    pub fn str_arg(&self, key: &str) -> Option<&str> {
        for a in &self.args {
            if a.name.as_deref() == Some(key) {
                return a.value.as_str();
            }
        }
        None
    }
}
