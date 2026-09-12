//! Token definitions for the Pointerses lexer.

/// A fragment of a string-literal with interpolation.
///
/// `"a ${x + 1} b"` lexes into `Text("a ")`, `Expr("x + 1")`, `Text(" b")`.
/// The expression fragments are source text re-parsed by the parser.
#[derive(Debug, Clone, PartialEq)]
pub enum InterpPart {
    /// Plain literal text.
    Text(String),
    /// Interpolated expression source (without the enclosing `${` / `}`).
    Expr(String),
}

/// A lexical token kind.
#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    // Literals / identifiers
    Ident(String),
    Int(i64),
    Float(f64),
    Str(String),
    /// A string literal containing `$ident` / `${expr}` interpolations.
    InterpStr(Vec<InterpPart>),

    // Keywords
    Fn,
    Let,
    Struct,
    Return,
    If,
    Else,
    While,
    For,
    Break,
    Continue,
    True,
    False,
    Null,
    Extern,
    Mut,   // &mut
    Region, // region keyword (also usable as annotation target)
    // Language-extension keywords
    Trait,
    With,
    Imp,
    Try,
    Catch,
    Throw,
    Finally,
    Module,
    Import,
    /// `this` — the receiver inside an `impl` method body.
    This,

    // Annotations
    At, // '@'

    // Punctuation / operators
    LBrace,
    RBrace,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Comma,
    Dot,
    Colon,
    ColonColon, // ::
    Semi,
    Eq,     // =
    Plus,   // +
    Minus,  // -
    Star,   // *
    Slash,  // /
    Percent,// %
    Amp,    // &
    Pipe,   // |
    Bang,   // !
    Lt,     // <
    Gt,     // >
    EqEq,   // ==
    NotEq,  // !=
    Le,     // <=
    Ge,     // >=
    AndAnd, // &&
    OrOr,   // ||
    Arrow,  // ->
    FatArrow, // => (reserved)
    // Compound assignment
    PlusEq,  // +=
    MinusEq, // -=
    StarEq,  // *=
    SlashEq, // /=
    PercentEq,// %=
    // Increment / decrement
    PlusPlus,   // ++
    MinusMinus, // --
    // Ternary
    Question, // ?

    // Structural
    Newline, // significant for Groovy-style semicolon omission
    Eof,
}

impl Tok {
    pub fn describe(&self) -> String {
        match self {
            Tok::Ident(s) => format!("identifier `{s}`"),
            Tok::Int(n) => format!("integer `{n}`"),
            Tok::Float(f) => format!("float `{f}`"),
            Tok::Str(_) => "string literal".into(),
            Tok::InterpStr(_) => "interpolated string".into(),
            Tok::Eof => "end of file".into(),
            Tok::Newline => "newline".into(),
            other => format!("`{}`", other.symbol()),
        }
    }

    /// A compact symbolic rendering used in error messages.
    pub fn symbol(&self) -> &'static str {
        match self {
            Tok::Ident(_) => "ident",
            Tok::Int(_) => "int",
            Tok::Float(_) => "float",
            Tok::Str(_) => "str",
            Tok::InterpStr(_) => "interpolated str",
            Tok::Fn => "func",
            Tok::Let => "var",
            Tok::Struct => "class",
            Tok::Return => "return",
            Tok::If => "if",
            Tok::Else => "else",
            Tok::While => "while",
            Tok::For => "for",
            Tok::Break => "break",
            Tok::Continue => "continue",
            Tok::True => "true",
            Tok::False => "false",
            Tok::Null => "null",
            Tok::Extern => "extern",
            Tok::Mut => "mut",
            Tok::Region => "region",
            Tok::Trait => "infs",
            Tok::With => "with",
            Tok::Imp => "imp",
            Tok::Try => "try",
            Tok::Catch => "catch",
            Tok::Throw => "throw",
            Tok::Finally => "finally",
            Tok::Module => "module",
            Tok::Import => "import",
            Tok::This => "this",
            Tok::At => "@",
            Tok::LBrace => "{",
            Tok::RBrace => "}",
            Tok::LParen => "(",
            Tok::RParen => ")",
            Tok::LBracket => "[",
            Tok::RBracket => "]",
            Tok::Comma => ",",
            Tok::Dot => ".",
            Tok::Colon => ":",
            Tok::ColonColon => "::",
            Tok::Semi => ";",
            Tok::Eq => "=",
            Tok::Plus => "+",
            Tok::Minus => "-",
            Tok::Star => "*",
            Tok::Slash => "/",
            Tok::Percent => "%",
            Tok::Amp => "&",
            Tok::Pipe => "|",
            Tok::Bang => "!",
            Tok::Lt => "<",
            Tok::Gt => ">",
            Tok::EqEq => "==",
            Tok::NotEq => "!=",
            Tok::Le => "<=",
            Tok::Ge => ">=",
            Tok::AndAnd => "&&",
            Tok::OrOr => "||",
            Tok::Arrow => "->",
            Tok::FatArrow => "=>",
            Tok::PlusEq => "+=",
            Tok::MinusEq => "-=",
            Tok::StarEq => "*=",
            Tok::SlashEq => "/=",
            Tok::PercentEq => "%=",
            Tok::PlusPlus => "++",
            Tok::MinusMinus => "--",
            Tok::Question => "?",
            Tok::Newline => "\\n",
            Tok::Eof => "eof",
        }
    }

    pub fn is_keyword(&self) -> bool {
        matches!(
            self,
            Tok::Fn | Tok::Let | Tok::Struct | Tok::Return | Tok::If | Tok::Else | Tok::While
                | Tok::For | Tok::Break | Tok::Continue
                | Tok::True | Tok::False | Tok::Null | Tok::Extern | Tok::Mut | Tok::Region
                | Tok::Trait | Tok::With | Tok::Imp | Tok::Try | Tok::Catch | Tok::Throw | Tok::Finally
                | Tok::Module | Tok::Import | Tok::This
        )
    }
}

/// A positioned token.
#[derive(Debug, Clone)]
pub struct Token {
    pub kind: Tok,
    pub line: usize,
    pub col: usize,
}

impl Token {
    pub fn new(kind: Tok, line: usize, col: usize) -> Self {
        Token { kind, line, col }
    }
}
