//! Recursive-descent parser for Pointerses.
//!
//! Builds a complete AST supporting function definitions, variable declarations,
//! closures, conditionals, loops, struct definitions, method calls, property
//! access, pointer address-of / dereference, annotations (region + concurrency)
//! and Groovy-style semicolon omission (statements are terminated by newlines,
//! semicolons, or the end of a block).

pub mod ast;
pub use ast::*;

use crate::lexer::{strip_newlines, Tok, Token};

/// A parse error with position.
#[derive(Debug)]
pub struct ParseError {
    pub msg: String,
    pub line: usize,
    pub col: usize,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}: {}", self.line, self.col, self.msg)
    }
}

struct Parser {
    toks: Vec<Token>,
    pos: usize,
    /// Active `module` namespace prefix for declarations in this file.
    current_module: Option<String>,
}

/// Parse a token stream into a program.
pub fn parse(tokens: &[Token]) -> Result<Program, ParseError> {
    let mut p = Parser { toks: tokens.to_vec(), pos: 0, current_module: None };
    p.parse_program()
}

impl Parser {
    // -- token helpers ------------------------------------------------------

    fn peek(&self) -> &Token {
        &self.toks[self.pos.min(self.toks.len() - 1)]
    }
    fn peek2(&self) -> &Token {
        &self.toks[(self.pos + 1).min(self.toks.len() - 1)]
    }
    fn at(&self, k: &Tok) -> bool {
        self.peek().kind == *k
    }
    fn at2(&self, k: &Tok) -> bool {
        self.peek2().kind == *k
    }
    fn next(&mut self) -> Token {
        let t = self.toks[self.pos.min(self.toks.len() - 1)].clone();
        if self.pos < self.toks.len() - 1 {
            self.pos += 1;
        }
        t
    }
    fn span(&self) -> Span {
        let t = self.peek();
        Span::new(t.line, t.col)
    }
    fn skip_newlines(&mut self) {
        while self.at(&Tok::Newline) {
            self.next();
        }
    }
    fn skip_terminators(&mut self) {
        while self.at(&Tok::Newline) || self.at(&Tok::Semi) {
            self.next();
        }
    }
    fn eat(&mut self, k: &Tok) -> bool {
        if self.at(k) {
            self.next();
            true
        } else {
            false
        }
    }
    fn expect(&mut self, k: &Tok, what: &str) -> Result<Token, ParseError> {
        if self.at(k) {
            Ok(self.next())
        } else {
            Err(self.err_here(format!("expected {what}, found {}", self.peek().kind.describe())))
        }
    }
    fn err_here(&self, msg: String) -> ParseError {
        let t = self.peek();
        ParseError { msg, line: t.line, col: t.col }
    }
    fn expect_ident(&mut self, what: &str) -> Result<String, ParseError> {
        match self.peek().kind.clone() {
            Tok::Ident(s) => {
                self.next();
                Ok(s)
            }
            _ => Err(self.err_here(format!("expected {what}, found {}", self.peek().kind.describe()))),
        }
    }

    // -- program ------------------------------------------------------------

    fn parse_program(&mut self) -> Result<Program, ParseError> {
        let mut prog = Program::default();
        loop {
            self.skip_terminators();
            if self.at(&Tok::Eof) {
                break;
            }
            // Leading annotations attach to the following declaration.
            let annotations = self.parse_annotations()?;
            self.skip_newlines();
            if self.at(&Tok::Struct) {
                prog.structs.push(self.parse_struct(annotations)?);
            } else if self.at(&Tok::Fn) {
                prog.funcs.push(self.parse_func(annotations)?);
            } else if self.at(&Tok::Extern) {
                let e = self.parse_extern()?;
                prog.externs.push(e);
            } else if self.at(&Tok::Module) {
                let sp = self.span();
                self.next(); // module
                let name = self.expect_ident("module name")?;
                self.current_module = Some(name.clone());
                prog.modules.push(ModuleDecl { name, span: sp });
            } else if self.at(&Tok::Import) {
                let sp = self.span();
                self.next(); // import
                let path = match self.peek().kind.clone() {
                    Tok::Str(s) => {
                        self.next();
                        s
                    }
                    _ => {
                        return Err(self.err_here(format!(
                            "expected a quoted import path, found {}",
                            self.peek().kind.describe()
                        )))
                    }
                };
                prog.imports.push(ImportDecl { path, span: sp });
            } else if self.at(&Tok::Trait) {
                prog.traits.push(self.parse_trait()?);
            } else if self.at(&Tok::With) || self.at(&Tok::Imp) {
                prog.impls.push(self.parse_impl()?);
            } else if self.at(&Tok::Let) {
                // Top-level `var name[: Type] = expr;` — a global variable.
                let sp = self.span();
                let s = self.parse_stmt()?;
                match s {
                    Stmt::Let(p, init, _sp) => prog.globals.push(GlobalDecl {
                        name: p.name,
                        ty: p.ty,
                        init,
                        span: sp,
                    }),
                    _ => unreachable!("`var` always parses as a Let statement"),
                }
            } else if !annotations.is_empty() {
                return Err(self.err_here("annotation must precede `func` or `class`".into()));
            } else {
                return Err(self.err_here(format!(
                    "expected declaration, found {}",
                    self.peek().kind.describe()
                )));
            }
        }
        Ok(prog)
    }

    fn parse_annotations(&mut self) -> Result<Vec<Annotation>, ParseError> {
        let mut out = Vec::new();
        while self.at(&Tok::At) {
            let span = self.span();
            self.next(); // @
            let name = self.expect_ident("annotation name")?;
            let mut args: Vec<NamedArg> = Vec::new();
            if self.eat(&Tok::LParen) {
                self.skip_newlines();
                if !self.at(&Tok::RParen) {
                    loop {
                        let arg = self.parse_ann_arg()?;
                        args.push(arg);
                        self.skip_newlines();
                        if self.eat(&Tok::Comma) {
                            self.skip_newlines();
                            continue;
                        }
                        break;
                    }
                }
                self.expect(&Tok::RParen, "`)`")?;
            }
            out.push(Annotation { name, args, span });
        }
        Ok(out)
    }

    fn parse_ann_arg(&mut self) -> Result<NamedArg, ParseError> {
        // positional value
        let (name, value) = if matches!(self.peek().kind, Tok::Ident(_)) {
            // could be `key=value` or a bare identifier value
            if self.at2(&Tok::Eq) {
                let key = self.expect_ident("argument name")?;
                self.next(); // =
                let v = self.parse_ann_value()?;
                (Some(key), v)
            } else {
                let id = self.expect_ident("argument value")?;
                (None, AnnArg::Ident(id))
            }
        } else {
            (None, self.parse_ann_value()?)
        };
        Ok(NamedArg { name, value })
    }

    fn parse_ann_value(&mut self) -> Result<AnnArg, ParseError> {
        let sp = self.span();
        match self.peek().kind.clone() {
            Tok::Int(v) => {
                self.next();
                Ok(AnnArg::Int(v))
            }
            Tok::Float(v) => {
                self.next();
                Ok(AnnArg::Float(v))
            }
            Tok::Str(s) => {
                self.next();
                Ok(AnnArg::Str(s))
            }
            Tok::Minus => {
                // negative numbers
                self.next();
                match self.peek().kind.clone() {
                    Tok::Int(v) => {
                        self.next();
                        Ok(AnnArg::Int(-v))
                    }
                    Tok::Float(v) => {
                        self.next();
                        Ok(AnnArg::Float(-v))
                    }
                    _ => Err(ParseError { msg: "expected number after `-`".into(), line: sp.line, col: sp.col }),
                }
            }
            Tok::Ident(s) => {
                self.next();
                Ok(AnnArg::Ident(s))
            }
            _ => Err(self.err_here(format!("invalid annotation argument {}", self.peek().kind.describe()))),
        }
    }

    // -- declarations -------------------------------------------------------

    fn parse_struct(&mut self, annotations: Vec<Annotation>) -> Result<StructDef, ParseError> {
        let span = self.span();
        self.next(); // struct
        let name = self.expect_ident("struct name")?;
        let type_params: Vec<String> = self
            .parse_optional_type_params()?
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        self.skip_newlines();
        self.expect(&Tok::LBrace, "`{`")?;
        let mut fields = Vec::new();
        loop {
            self.skip_terminators();
            if self.at(&Tok::RBrace) {
                self.next();
                break;
            }
            if self.at(&Tok::Eof) {
                return Err(self.err_here("unexpected end of file inside struct".into()));
            }
            let fspan = self.span();
            let fname = self.expect_ident("field name")?;
            self.expect(&Tok::Colon, "`:`")?;
            let ty = self.parse_type()?;
            fields.push(Field { name: fname, ty, span: fspan });
            self.skip_terminators();
            if !self.eat(&Tok::Comma) {
                // newline / semicolon separation already consumed; continue
            }
        }
        Ok(StructDef { name, type_params, fields, annotations, module: self.current_module.clone(), span })
    }

    /// Parse an optional `[T1, T2]` generic parameter list after a name.
    /// Parse `[T, U: Trait, ...]` returning `(param, optional trait bound)`.
    fn parse_optional_type_params(&mut self) -> Result<Vec<(String, Option<String>)>, ParseError> {
        let mut out = Vec::new();
        if self.eat(&Tok::LBracket) {
            self.skip_newlines();
            loop {
                let name = self.expect_ident("type parameter name")?;
                let mut bound = None;
                self.skip_newlines();
                if self.at(&Tok::Colon) {
                    self.next(); // :
                    self.skip_newlines();
                    let t = self.expect_ident("trait bound name")?;
                    bound = Some(self.consume_path_segments(t)?);
                }
                out.push((name, bound));
                self.skip_newlines();
                if self.eat(&Tok::Comma) {
                    self.skip_newlines();
                    continue;
                }
                break;
            }
            self.skip_newlines();
            self.expect(&Tok::RBracket, "`]`")?;
        }
        Ok(out)
    }

    fn parse_func(&mut self, annotations: Vec<Annotation>) -> Result<FuncDef, ParseError> {
        let span = self.span();
        self.next(); // func
        let name = self.expect_ident("function name")?;
        let params_meta = self.parse_optional_type_params()?;
        let type_params: Vec<String> = params_meta.iter().map(|(n, _)| n.clone()).collect();
        let bounds: Vec<(String, String)> = params_meta
            .into_iter()
            .filter_map(|(n, b)| b.map(|b| (n, b)))
            .collect();
        let params = self.parse_params()?;
        let mut ret = None;
        self.skip_newlines();
        if self.eat(&Tok::Arrow) {
            self.skip_newlines();
            ret = Some(self.parse_type()?);
        }
        self.skip_newlines();
        let body = self.parse_block()?;
        Ok(FuncDef { name, type_params, bounds, params, ret, body, annotations, module: self.current_module.clone(), span })
    }

    fn parse_extern(&mut self) -> Result<ExternDecl, ParseError> {
        let span = self.span();
        self.next(); // extern
        // optional ABI qualifier: `extern "arrow"` or `extern "c"`
        let mut abi = "arrow".to_string();
        if let Tok::Str(s) = self.peek().kind.clone() {
            abi = s;
            self.next();
        } else if matches!(self.peek().kind, Tok::Ident(_)) && matches!(self.peek2().kind, Tok::LParen) {
            // no abi qualifier
        }
        // optional `func` keyword: `extern func add(...)`
        self.eat(&Tok::Fn);
        let name = self.expect_ident("extern function name")?;
        let params = self.parse_params()?;
        let mut ret = None;
        self.skip_newlines();
        if self.eat(&Tok::Arrow) {
            ret = Some(self.parse_type()?);
        }
        self.skip_terminators();
        Ok(ExternDecl { name, params, ret, abi, module: self.current_module.clone(), span })
    }

    // -- trait / impl -------------------------------------------------------

    fn parse_trait(&mut self) -> Result<TraitDef, ParseError> {
        let span = self.span();
        self.next(); // trait
        let name = self.expect_ident("trait name")?;
        self.skip_newlines();
        self.expect(&Tok::LBrace, "`{`")?;
        let mut methods = Vec::new();
        loop {
            self.skip_terminators();
            if self.at(&Tok::RBrace) {
                self.next();
                break;
            }
            if self.at(&Tok::Eof) {
                return Err(self.err_here("unexpected end of file inside trait".into()));
            }
            let mspan = self.span();
            self.expect(&Tok::Fn, "`func`")?;
            let mname = self.expect_ident("trait method name")?;
            let params = self.parse_params()?;
            let mut ret = None;
            self.skip_newlines();
            if self.eat(&Tok::Arrow) {
                self.skip_newlines();
                ret = Some(self.parse_type()?);
            }
            methods.push(TraitMethod { name: mname, params, ret, span: mspan });
            self.skip_terminators();
        }
        Ok(TraitDef { name, methods, span })
    }

    fn parse_impl(&mut self) -> Result<ImplBlock, ParseError> {
        let span = self.span();
        // `with Type[T]` (inherent) or `imp Trait for Type[T]` (interface impl).
        let is_interface = self.at(&Tok::Imp);
        self.next(); // with | imp
        self.skip_newlines();
        let mut trait_name = None;
        let first = self.expect_ident("type or trait name")?;
        // consume `::` path segments for a namespaced type/trait
        let first = self.consume_path_segments(first)?;
        self.skip_newlines();
        if self.eat(&Tok::For) {
            trait_name = Some(first);
            self.skip_newlines();
            let tname = self.expect_ident("impl target type name")?;
            let tname = self.consume_path_segments(tname)?;
            let type_params: Vec<String> = self
                .parse_optional_type_params()?
                .into_iter()
                .map(|(n, _)| n)
                .collect();
            self.skip_newlines();
            self.expect(&Tok::LBrace, "`{`")?;
            let methods = self.parse_impl_methods()?;
            return Ok(ImplBlock { trait_name, type_name: tname, type_params, methods, module: self.current_module.clone(), span });
        }
        if is_interface {
            return Err(self.err_here("expected `for` after interface name in `imp` block".into()));
        }
        let type_params: Vec<String> = self
            .parse_optional_type_params()?
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        self.skip_newlines();
        self.expect(&Tok::LBrace, "`{`")?;
        let methods = self.parse_impl_methods()?;
        Ok(ImplBlock { trait_name, type_name: first, type_params, methods, module: self.current_module.clone(), span })
    }

    /// Consume `::seg` path continuations, joining them with `::`.
    fn consume_path_segments(&mut self, first: String) -> Result<String, ParseError> {
        let mut name = first;
        while self.at(&Tok::ColonColon) {
            self.next();
            name = format!("{name}::{}", self.expect_ident("path segment")?);
        }
        Ok(name)
    }

    /// Best-effort parse of a generic type-argument list `[T1, T2, ...]`
    /// (including the closing `]`). Returns `Err(())` if the tokens are not a
    /// well-formed type list, letting the caller treat the construct as an
    /// index expression instead.
    fn try_parse_type_list(&mut self) -> Result<Vec<Type>, ()> {
        let mut targs = Vec::new();
        self.skip_newlines();
        targs.push(self.parse_type().map_err(|_| ())?);
        self.skip_newlines();
        while self.eat(&Tok::Comma) {
            self.skip_newlines();
            targs.push(self.parse_type().map_err(|_| ())?);
        }
        self.skip_newlines();
        self.expect(&Tok::RBracket, "`]`").map_err(|_| ())?;
        Ok(targs)
    }

    fn parse_impl_methods(&mut self) -> Result<Vec<FuncDef>, ParseError> {
        let mut methods = Vec::new();
        loop {
            self.skip_terminators();
            if self.at(&Tok::RBrace) {
                self.next();
                break;
            }
            if self.at(&Tok::Eof) {
                return Err(self.err_here("unexpected end of file inside impl".into()));
            }
            let annotations = self.parse_annotations()?;
            self.skip_newlines();
            if !self.at(&Tok::Fn) {
                return Err(self.err_here(format!(
                    "expected `func` in impl, found {}",
                    self.peek().kind.describe()
                )));
            }
            methods.push(self.parse_func(annotations)?);
        }
        Ok(methods)
    }

    fn parse_params(&mut self) -> Result<Vec<Param>, ParseError> {
        let mut out = Vec::new();
        self.expect(&Tok::LParen, "`(`")?;
        self.skip_newlines();
        if !self.at(&Tok::RParen) {
            loop {
                let pspan = self.span();
                let name = self.expect_ident("parameter name")?;
                let mut ty = None;
                self.skip_newlines();
                if self.eat(&Tok::Colon) {
                    self.skip_newlines();
                    ty = Some(self.parse_type()?);
                }
                out.push(Param { name, ty, span: pspan });
                self.skip_newlines();
                if self.eat(&Tok::Comma) {
                    self.skip_newlines();
                    continue;
                }
                break;
            }
        }
        self.expect(&Tok::RParen, "`)`")?;
        Ok(out)
    }

    // -- types --------------------------------------------------------------

    fn parse_type(&mut self) -> Result<Type, ParseError> {
        self.skip_newlines();
        let t = self.peek().kind.clone();
        match t {
            Tok::Int(_) => {
                self.next();
                Ok(Type::Int)
            }
            Tok::Float(_) => {
                self.next();
                Ok(Type::Float)
            }
            Tok::Ident(s) if s == "bool" => {
                self.next();
                Ok(Type::Bool)
            }
            Tok::Ident(s) if s == "int" => {
                self.next();
                Ok(Type::Int)
            }
            Tok::Ident(s) if s == "float" => {
                self.next();
                Ok(Type::Float)
            }
            Tok::Str(_) => {
                self.next();
                Ok(Type::Str)
            }
            Tok::Ident(s) if s == "String" || s == "string" || s == "str" => {
                self.next();
                Ok(Type::Str)
            }
            Tok::Ident(s) if s == "void" => {
                self.next();
                Ok(Type::Void)
            }
            Tok::Ident(s) if s == "dyn" => {
                // trait object type: `dyn Trait`
                self.next();
                self.skip_newlines();
                let t = self.expect_ident("trait name after `dyn`")?;
                let t = self.consume_path_segments(t)?;
                Ok(Type::Trait(t))
            }
            Tok::Ident(s) if s == "List" || s == "Map" => {
                // generic container type: `List[T]` / `Map[K, V]`
                let ctor = s;
                self.next(); // ident
                self.expect(&Tok::LBracket, "`[`")?;
                self.skip_newlines();
                let a = self.parse_type()?;
                if ctor == "Map" {
                    self.skip_newlines();
                    self.expect(&Tok::Comma, "`,`")?;
                    self.skip_newlines();
                    let b = self.parse_type()?;
                    self.skip_newlines();
                    self.expect(&Tok::RBracket, "`]`")?;
                    Ok(Type::Map(Box::new(a), Box::new(b)))
                } else {
                    self.skip_newlines();
                    self.expect(&Tok::RBracket, "`]`")?;
                    Ok(Type::List(Box::new(a)))
                }
            }
            Tok::Ident(s) => {
                self.next();
                // user-defined generic type: `Foo[a, b]`
                if self.at(&Tok::LBracket) {
                    self.next();
                    self.skip_newlines();
                    let mut args = vec![self.parse_type()?];
                    self.skip_newlines();
                    while self.eat(&Tok::Comma) {
                        self.skip_newlines();
                        args.push(self.parse_type()?);
                    }
                    self.skip_newlines();
                    self.expect(&Tok::RBracket, "`]`")?;
                    return Ok(Type::Generic(s, args));
                }
                // namespaced type path: `math::Point`
                if self.at(&Tok::ColonColon) {
                    let mut name = s;
                    while self.at(&Tok::ColonColon) {
                        self.next();
                        name = format!("{name}::{}", self.expect_ident("path segment")?);
                    }
                    return Ok(Type::Named(name));
                }
                Ok(Type::Named(s))
            }
            Tok::LBracket => {
                // array type `[T]`
                self.next();
                self.skip_newlines();
                let elem = self.parse_type()?;
                self.skip_newlines();
                self.expect(&Tok::RBracket, "`]`")?;
                Ok(Type::Array(Box::new(elem)))
            }
            Tok::Amp => {
                self.next();
                let mutable = self.eat(&Tok::Mut);
                let target = self.parse_type()?;
                Ok(Type::Ptr { mutable, target: Box::new(target) })
            }
            Tok::LParen => {
                // function type `(A, B) -> R`
                self.next();
                let mut params = Vec::new();
                self.skip_newlines();
                if !self.at(&Tok::RParen) {
                    loop {
                        params.push(self.parse_type()?);
                        self.skip_newlines();
                        if self.eat(&Tok::Comma) {
                            self.skip_newlines();
                            continue;
                        }
                        break;
                    }
                }
                self.expect(&Tok::RParen, "`)`")?;
                self.expect(&Tok::Arrow, "`->`")?;
                let ret = self.parse_type()?;
                Ok(Type::Fn { params, ret: Box::new(ret) })
            }
            other => Err(self.err_here(format!("expected type, found {}", other.describe()))),
        }
    }

    // -- statements ---------------------------------------------------------

    fn parse_block(&mut self) -> Result<Vec<Stmt>, ParseError> {
        self.expect(&Tok::LBrace, "`{`")?;
        let mut stmts = Vec::new();
        loop {
            self.skip_terminators();
            if self.at(&Tok::RBrace) {
                self.next();
                break;
            }
            if self.at(&Tok::Eof) {
                return Err(self.err_here("unexpected end of file inside block".into()));
            }
            let stmt = self.parse_stmt()?;
            stmts.push(stmt);
            // consume statement terminator(s)
            if !self.at(&Tok::RBrace) && !self.at(&Tok::Eof) {
                self.skip_terminators();
            }
        }
        Ok(stmts)
    }

    fn parse_stmt(&mut self) -> Result<Stmt, ParseError> {
        let sp = self.span();
        match self.peek().kind.clone() {
            Tok::Let => {
                self.next();
                let nspan = self.span();
                let name = self.expect_ident("variable name")?;
                let mut ty = None;
                self.skip_newlines();
                if self.eat(&Tok::Colon) {
                    self.skip_newlines();
                    ty = Some(self.parse_type()?);
                }
                self.skip_newlines();
                let init = if self.eat(&Tok::Eq) {
                    self.skip_newlines();
                    Some(self.parse_expr()?)
                } else {
                    None
                };
                Ok(Stmt::Let(Param { name, ty, span: nspan }, init.unwrap_or(Expr::NullLit(sp)), sp))
            }
            Tok::Return => {
                self.next();
                let e = if self.stmt_end() {
                    None
                } else {
                    Some(self.parse_expr()?)
                };
                Ok(Stmt::Return(e, sp))
            }
            Tok::If => {
                self.next();
                self.skip_newlines();
                let paren = self.eat(&Tok::LParen);
                let cond = self.parse_expr()?;
                if paren {
                    self.expect(&Tok::RParen, "`)`")?;
                }
                self.skip_newlines();
                let then_b = self.parse_block()?;
                let mut else_b = None;
                self.skip_terminators();
                if self.at(&Tok::Else) {
                    self.next();
                    self.skip_newlines();
                    if self.at(&Tok::If) {
                        // else-if: parse as nested if statement list
                        let nested = self.parse_stmt()?;
                        else_b = Some(vec![nested]);
                    } else {
                        else_b = Some(self.parse_block()?);
                    }
                }
                Ok(Stmt::If(cond, then_b, else_b, sp))
            }
            Tok::While => {
                self.next();
                self.skip_newlines();
                let paren = self.eat(&Tok::LParen);
                let cond = self.parse_expr()?;
                if paren {
                    self.expect(&Tok::RParen, "`)`")?;
                }
                self.skip_newlines();
                let body = self.parse_block()?;
                Ok(Stmt::While(cond, body, sp))
            }
            Tok::For => {
                self.next();
                self.skip_newlines();
                let paren = self.eat(&Tok::LParen);
                // init: either a `let` declaration or an expression statement
                let init = if self.at(&Tok::Semi) {
                    Vec::new()
                } else if self.at(&Tok::Let) {
                    vec![self.parse_stmt()?]
                } else {
                    vec![Stmt::Expr(self.parse_expr()?, sp)]
                };
                self.expect(&Tok::Semi, "`;`")?;
                // cond
                let cond = if self.at(&Tok::Semi) {
                    None
                } else {
                    Some(self.parse_expr()?)
                };
                self.expect(&Tok::Semi, "`;`")?;
                // step
                let step = if self.at(&Tok::RParen) {
                    None
                } else {
                    Some(self.parse_expr()?)
                };
                if paren {
                    self.expect(&Tok::RParen, "`)`")?;
                }
                self.skip_newlines();
                let body = self.parse_block()?;
                Ok(Stmt::For(init, cond, step, body, sp))
            }
            Tok::Break => {
                self.next();
                Ok(Stmt::Break(sp))
            }
            Tok::Continue => {
                self.next();
                Ok(Stmt::Continue(sp))
            }
            Tok::At => {
                let anns = self.parse_annotations()?;
                let ann = anns.into_iter().next().unwrap();
                Ok(Stmt::Directive(ann, sp))
            }
            Tok::Try => self.parse_try(),
            Tok::Throw => {
                self.next();
                self.skip_newlines();
                let e = self.parse_expr()?;
                Ok(Stmt::Throw(Box::new(e), sp))
            }
            _ => {
                let e = self.parse_expr()?;
                Ok(Stmt::Expr(e, sp))
            }
        }
    }

    fn parse_try(&mut self) -> Result<Stmt, ParseError> {
        let sp = self.span();
        self.next(); // try
        self.skip_newlines();
        let body = self.parse_block()?;
        self.skip_terminators();
        let mut catch = None;
        if self.at(&Tok::Catch) {
            self.next();
            self.skip_newlines();
            self.expect(&Tok::LParen, "`(`")?;
            let cspan = self.span();
            let name = self.expect_ident("catch variable")?;
            self.skip_newlines();
            self.expect(&Tok::RParen, "`)`")?;
            self.skip_newlines();
            let cbody = self.parse_block()?;
            catch = Some((name, cbody, cspan));
        }
        self.skip_terminators();
        let mut fin = None;
        if self.at(&Tok::Finally) {
            self.next();
            self.skip_newlines();
            fin = Some(self.parse_block()?);
        }
        if catch.is_none() && fin.is_none() {
            return Err(ParseError {
                msg: "`try` requires at least a `catch` or a `finally` block".into(),
                line: sp.line,
                col: sp.col,
            });
        }
        Ok(Stmt::Try(body, catch, fin, sp))
    }

    /// Whether the current position terminates a statement (before a newline,
    /// semicolon, `}` or EOF).
    fn stmt_end(&self) -> bool {
        matches!(
            self.peek().kind,
            Tok::Newline | Tok::Semi | Tok::RBrace | Tok::Eof
        )
    }

    // -- expressions --------------------------------------------------------

    fn parse_expr(&mut self) -> Result<Expr, ParseError> {
        self.parse_assign()
    }

    fn parse_assign(&mut self) -> Result<Expr, ParseError> {
        self.skip_newlines();
        let lhs = self.parse_ternary()?;
        // Compound assignments: `+=` `-=` `*=` `/=` `%=` desugar to
        // `lhs = lhs <op> rhs`.
        let comp = match self.peek().kind {
            Tok::PlusEq => Some(BinOp::Add),
            Tok::MinusEq => Some(BinOp::Sub),
            Tok::StarEq => Some(BinOp::Mul),
            Tok::SlashEq => Some(BinOp::Div),
            Tok::PercentEq => Some(BinOp::Mod),
            _ => None,
        };
        if let Some(op) = comp {
            let sp = self.span();
            self.next();
            self.skip_newlines();
            let rhs = self.parse_assign()?;
            let bin = Expr::Binary(op, Box::new(lhs.clone()), Box::new(rhs), sp);
            return Ok(Expr::Assign(Box::new(lhs), Box::new(bin), sp));
        }
        if self.at(&Tok::Eq) {
            let sp = self.span();
            self.next();
            self.skip_newlines();
            let rhs = self.parse_assign()?;
            return Ok(Expr::Assign(Box::new(lhs), Box::new(rhs), sp));
        }
        Ok(lhs)
    }

    fn parse_ternary(&mut self) -> Result<Expr, ParseError> {
        let cond = self.parse_or()?;
        if self.at(&Tok::Question) {
            let sp = self.span();
            self.next();
            self.skip_newlines();
            let then_e = self.parse_or()?;
            self.expect(&Tok::Colon, "`:`")?;
            self.skip_newlines();
            let else_e = self.parse_ternary()?;
            return Ok(Expr::Ternary(Box::new(cond), Box::new(then_e), Box::new(else_e), sp));
        }
        Ok(cond)
    }

    fn parse_or(&mut self) -> Result<Expr, ParseError> {
        let mut l = self.parse_and()?;
        while self.at(&Tok::OrOr) {
            let sp = self.span();
            self.next();
            let r = self.parse_and()?;
            l = Expr::Binary(BinOp::Or, Box::new(l), Box::new(r), sp);
        }
        Ok(l)
    }

    fn parse_and(&mut self) -> Result<Expr, ParseError> {
        let mut l = self.parse_equality()?;
        while self.at(&Tok::AndAnd) {
            let sp = self.span();
            self.next();
            let r = self.parse_equality()?;
            l = Expr::Binary(BinOp::And, Box::new(l), Box::new(r), sp);
        }
        Ok(l)
    }

    fn parse_equality(&mut self) -> Result<Expr, ParseError> {
        let mut l = self.parse_comparison()?;
        loop {
            let op = match self.peek().kind {
                Tok::EqEq => BinOp::Eq,
                Tok::NotEq => BinOp::Ne,
                _ => break,
            };
            let sp = self.span();
            self.next();
            let r = self.parse_comparison()?;
            l = Expr::Binary(op, Box::new(l), Box::new(r), sp);
        }
        Ok(l)
    }

    fn parse_comparison(&mut self) -> Result<Expr, ParseError> {
        let mut l = self.parse_additive()?;
        loop {
            let op = match self.peek().kind {
                Tok::Lt => BinOp::Lt,
                Tok::Le => BinOp::Le,
                Tok::Gt => BinOp::Gt,
                Tok::Ge => BinOp::Ge,
                _ => break,
            };
            let sp = self.span();
            self.next();
            let r = self.parse_additive()?;
            l = Expr::Binary(op, Box::new(l), Box::new(r), sp);
        }
        Ok(l)
    }

    fn parse_additive(&mut self) -> Result<Expr, ParseError> {
        let mut l = self.parse_multiplicative()?;
        loop {
            let op = match self.peek().kind {
                Tok::Plus => BinOp::Add,
                Tok::Minus => BinOp::Sub,
                _ => break,
            };
            let sp = self.span();
            self.next();
            let r = self.parse_multiplicative()?;
            l = Expr::Binary(op, Box::new(l), Box::new(r), sp);
        }
        Ok(l)
    }

    fn parse_multiplicative(&mut self) -> Result<Expr, ParseError> {
        let mut l = self.parse_unary()?;
        loop {
            let op = match self.peek().kind {
                Tok::Star => BinOp::Mul,
                Tok::Slash => BinOp::Div,
                Tok::Percent => BinOp::Mod,
                _ => break,
            };
            let sp = self.span();
            self.next();
            let r = self.parse_unary()?;
            l = Expr::Binary(op, Box::new(l), Box::new(r), sp);
        }
        Ok(l)
    }

    fn parse_unary(&mut self) -> Result<Expr, ParseError> {
        let sp = self.span();
        match self.peek().kind {
            Tok::Minus => {
                self.next();
                let e = self.parse_unary()?;
                Ok(Expr::Unary(UnOp::Neg, Box::new(e), sp))
            }
            Tok::Bang => {
                self.next();
                let e = self.parse_unary()?;
                Ok(Expr::Unary(UnOp::Not, Box::new(e), sp))
            }
            Tok::Amp => {
                self.next();
                // `&` (immutable) or `&mut` (mutable)
                let mutable = self.eat(&Tok::Mut);
                let e = self.parse_unary()?;
                let op = if mutable { UnOp::AddrMut } else { UnOp::Addr };
                Ok(Expr::Unary(op, Box::new(e), sp))
            }
            Tok::Star => {
                self.next();
                let e = self.parse_unary()?;
                Ok(Expr::Unary(UnOp::Deref, Box::new(e), sp))
            }
            // Prefix `++` / `--`: `++e` -> `e = e + 1`.
            Tok::PlusPlus | Tok::MinusMinus => {
                let inc = self.peek().kind == Tok::PlusPlus;
                self.next();
                let e = self.parse_unary()?;
                let one = Expr::IntLit(1, sp);
                let op = if inc { BinOp::Add } else { BinOp::Sub };
                let bin = Expr::Binary(op, Box::new(e.clone()), Box::new(one), sp);
                Ok(Expr::Assign(Box::new(e), Box::new(bin), sp))
            }
            _ => self.parse_postfix(),
        }
    }

    fn parse_postfix(&mut self) -> Result<Expr, ParseError> {
        let mut e = self.parse_primary()?;
        loop {
            match self.peek().kind {
                Tok::LParen => {
                    // function call
                    let sp = self.span();
                    let args = self.parse_call_args()?;
                    e = Expr::Call(Box::new(e), args, sp);
                }
                Tok::Dot => {
                    let sp = self.span();
                    self.next();
                    let name = self.expect_ident("member name")?;
                    if self.at(&Tok::LParen) {
                        let args = self.parse_call_args()?;
                        e = Expr::MethodCall(Box::new(e), name, args, sp);
                    } else {
                        e = Expr::Field(Box::new(e), name, sp);
                    }
                }
                Tok::LBracket => {
                    // index: `container[index]`
                    let sp = self.span();
                    self.next();
                    self.skip_newlines();
                    let idx = self.parse_expr()?;
                    self.skip_newlines();
                    self.expect(&Tok::RBracket, "`]`")?;
                    e = Expr::Index(Box::new(e), Box::new(idx), sp);
                }
                // Postfix `++` / `--`: `e++` -> `e = e + 1` (value is old e).
                Tok::PlusPlus | Tok::MinusMinus => {
                    let sp = self.span();
                    let inc = self.peek().kind == Tok::PlusPlus;
                    self.next();
                    let one = Expr::IntLit(1, sp);
                    let op = if inc { BinOp::Add } else { BinOp::Sub };
                    let bin = Expr::Binary(op, Box::new(e.clone()), Box::new(one), sp);
                    e = Expr::Assign(Box::new(e), Box::new(bin), sp);
                }
                _ => break,
            }
        }
        Ok(e)
    }

    fn parse_call_args(&mut self) -> Result<Vec<Expr>, ParseError> {
        let mut args = Vec::new();
        self.expect(&Tok::LParen, "`(`")?;
        self.skip_newlines();
        if !self.at(&Tok::RParen) {
            loop {
                args.push(self.parse_expr()?);
                self.skip_newlines();
                if self.eat(&Tok::Comma) {
                    self.skip_newlines();
                    continue;
                }
                break;
            }
        }
        self.expect(&Tok::RParen, "`)`")?;
        Ok(args)
    }

    fn parse_primary(&mut self) -> Result<Expr, ParseError> {
        let sp = self.span();
        match self.peek().kind.clone() {
            Tok::Int(v) => {
                self.next();
                Ok(Expr::IntLit(v, sp))
            }
            Tok::Float(v) => {
                self.next();
                Ok(Expr::FloatLit(v, sp))
            }
            Tok::True => {
                self.next();
                Ok(Expr::BoolLit(true, sp))
            }
            Tok::False => {
                self.next();
                Ok(Expr::BoolLit(false, sp))
            }
            Tok::Null => {
                self.next();
                Ok(Expr::NullLit(sp))
            }
            Tok::Str(s) => {
                self.next();
                Ok(Expr::StrLit(s, sp))
            }
            Tok::InterpStr(parts) => {
                self.next();
                // re-parse each interpolation fragment as an expression
                let mut out = Vec::new();
                for part in parts {
                    match part {
                        crate::lexer::InterpPart::Text(t) => out.push(ast::InterpPart::Text(t)),
                        crate::lexer::InterpPart::Expr(src) => {
                            let toks = crate::lexer::tokenize(&src)
                                .map_err(|e| ParseError { msg: format!("in interpolation: {e}"), line: sp.line, col: sp.col })?;
                            let mut sub = Parser { toks: strip_newlines(&toks), pos: 0, current_module: self.current_module.clone() };
                            let e = sub.parse_expr()
                                .map_err(|e| ParseError { msg: format!("in interpolation: {}", e.msg), line: sp.line, col: sp.col })?;
                            out.push(ast::InterpPart::Expr(Box::new(e)));
                        }
                    }
                }
                Ok(Expr::InterpStr(out, sp))
            }
            Tok::This => {
                self.next();
                Ok(Expr::Ident("this".into(), sp))
            }
            Tok::Ident(name) => {
                // map literal: `Map[K, V] { k: v, ... }`
                if (name == "Map" || name == "List") && self.at2(&Tok::LBracket) {
                    self.next(); // ident
                    self.expect(&Tok::LBracket, "`[`")?;
                    self.skip_newlines();
                    let _a = self.parse_type()?;
                    if name == "Map" {
                        self.skip_newlines();
                        self.expect(&Tok::Comma, "`,`")?;
                        self.skip_newlines();
                        let _b = self.parse_type()?;
                    }
                    self.skip_newlines();
                    self.expect(&Tok::RBracket, "`]`")?;
                    self.skip_newlines();
                    self.expect(&Tok::LBrace, "`{`")?;
                    let mut entries = Vec::new();
                    loop {
                        self.skip_terminators();
                        if self.at(&Tok::RBrace) {
                            self.next();
                            break;
                        }
                        if name == "Map" {
                            let k = self.parse_expr()?;
                            self.skip_newlines();
                            self.expect(&Tok::Colon, "`:`")?;
                            self.skip_newlines();
                            let v = self.parse_expr()?;
                            entries.push((k, v));
                        } else {
                            // List[T] { a, b, c }
                            let e = self.parse_expr()?;
                            let sp2 = e.span();
                            entries.push((Expr::NullLit(sp2), e));
                        }
                        self.skip_terminators();
                        self.eat(&Tok::Comma);
                    }
                    if name == "Map" {
                        Ok(Expr::MapLit(entries, sp))
                    } else {
                        let vals: Vec<Expr> = entries.into_iter().map(|(_, v)| v).collect();
                        Ok(Expr::ArrayLit(vals, sp))
                    }
                }
                // namespaced path: `math::square` / `math::Point { ... }`
                else if self.at2(&Tok::ColonColon) {
                    self.next(); // ident
                    let mut segs = vec![name];
                    while self.at(&Tok::ColonColon) {
                        self.next();
                        segs.push(self.expect_ident("path segment")?);
                    }
                    self.skip_newlines();
                    if self.at(&Tok::LBrace) {
                        // namespaced struct literal: `math::Point { ... }`
                        return self.parse_struct_lit(segs.join("::"), sp);
                    }
                    Ok(Expr::Path(segs, sp))
                }
                // generic instantiation: `Name[T1, T2]` followed by `(` or `{`.
                // A leading identifier/type-list that does not parse (e.g.
                // `arr[0]`, `arr[i+1]`) is an index access and falls back to it.
                else if self.at2(&Tok::LBracket) {
                    let save = self.pos;
                    self.next(); // ident
                    self.expect(&Tok::LBracket, "`[`")?;
                    if let Ok(targs) = self.try_parse_type_list() {
                        self.skip_newlines();
                        if self.at(&Tok::LParen) {
                            let args = self.parse_call_args()?;
                            Ok(Expr::GenericCall(name, targs, args, sp))
                        } else if self.at(&Tok::LBrace) {
                            // generic struct literal: `Box[int] { value: 1 }`
                            self.next(); // `{`
                            let fields = self.parse_struct_lit_fields()?;
                            Ok(Expr::GenericStructLit(name, targs, fields, sp))
                        } else {
                            // not a generic call/literal -> index access
                            self.pos = save;
                            self.next();
                            Ok(Expr::Ident(name, sp))
                        }
                    } else {
                        // not a type list -> index access (e.g. `arr[i+1]`)
                        self.pos = save;
                        self.next();
                        Ok(Expr::Ident(name, sp))
                    }
                }
                // struct literal if followed by `{`
                else if self.at2(&Tok::LBrace) {
                    self.next(); // ident
                    return self.parse_struct_lit(name, sp);
                } else {
                    self.next();
                    Ok(Expr::Ident(name, sp))
                }
            }
            Tok::LBracket => {
                // array literal: `[1, 2, 3]` or `[]`
                self.next();
                self.skip_newlines();
                let mut elems = Vec::new();
                if !self.at(&Tok::RBracket) {
                    loop {
                        elems.push(self.parse_expr()?);
                        self.skip_newlines();
                        if self.eat(&Tok::Comma) {
                            self.skip_newlines();
                            continue;
                        }
                        break;
                    }
                }
                self.expect(&Tok::RBracket, "`]`")?;
                Ok(Expr::ArrayLit(elems, sp))
            }
            Tok::LParen => {
                self.next();
                self.skip_newlines();
                let e = self.parse_expr()?;
                self.skip_newlines();
                self.expect(&Tok::RParen, "`)`")?;
                Ok(e)
            }
            Tok::Pipe => self.parse_closure(),
            Tok::LBrace => {
                let block = self.parse_block()?;
                Ok(Expr::Block(block, sp))
            }
            Tok::If => {
                // if-expression
                self.next();
                self.skip_newlines();
                let paren = self.eat(&Tok::LParen);
                let cond = self.parse_expr()?;
                if paren {
                    self.expect(&Tok::RParen, "`)`")?;
                }
                self.skip_newlines();
                let then_e = Box::new(self.parse_primary()?);
                let mut else_e = None;
                self.skip_newlines();
                if self.at(&Tok::Else) {
                    self.next();
                    self.skip_newlines();
                    else_e = Some(Box::new(self.parse_primary()?));
                }
                Ok(Expr::IfExpr(Box::new(cond), then_e, else_e, sp))
            }
            _ => Err(self.err_here(format!("unexpected token {}", self.peek().kind.describe()))),
        }
    }

    fn parse_struct_lit(&mut self, name: String, sp: Span) -> Result<Expr, ParseError> {
        self.expect(&Tok::LBrace, "`{`")?;
        let fields = self.parse_struct_lit_fields()?;
        Ok(Expr::StructLit(name, fields, sp))
    }

    /// Parse `{ name: expr, ... }` after the opening `{` has been consumed.
    fn parse_struct_lit_fields(&mut self) -> Result<Vec<(String, Expr)>, ParseError> {
        let mut fields = Vec::new();
        loop {
            self.skip_terminators();
            if self.at(&Tok::RBrace) {
                self.next();
                break;
            }
            let fname = self.expect_ident("field name")?;
            self.expect(&Tok::Colon, "`:`")?;
            let value = self.parse_expr()?;
            fields.push((fname, value));
            self.skip_terminators();
            self.eat(&Tok::Comma);
        }
        Ok(fields)
    }

    fn parse_closure(&mut self) -> Result<Expr, ParseError> {
        let sp = self.span();
        self.expect(&Tok::Pipe, "`|`")?;
        self.skip_newlines();
        let mut params = Vec::new();
        if !self.at(&Tok::Pipe) {
            loop {
                let pspan = self.span();
                let name = self.expect_ident("closure parameter")?;
                let mut ty = None;
                self.skip_newlines();
                if self.eat(&Tok::Colon) {
                    self.skip_newlines();
                    ty = Some(self.parse_type()?);
                }
                params.push(Param { name, ty, span: pspan });
                self.skip_newlines();
                if self.eat(&Tok::Comma) {
                    self.skip_newlines();
                    continue;
                }
                break;
            }
        }
        self.expect(&Tok::Pipe, "`|`")?;
        self.skip_newlines();
        let mut ret = None;
        if self.at(&Tok::Arrow) {
            self.next();
            self.skip_newlines();
            ret = Some(self.parse_type()?);
        }
        self.skip_newlines();
        let body = if self.at(&Tok::LBrace) {
            let block = self.parse_block()?;
            Expr::Block(block, sp)
        } else {
            self.parse_expr()?
        };
        Ok(Expr::Closure(ClosureExpr { params, ret, body: Box::new(body), span: sp, captures: Vec::new() }))
    }
}
