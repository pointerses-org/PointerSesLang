//! Lexical analysis for Pointerses.
//!
//! Produces a token stream. Supports identifiers, integer/float literals, string
//! literals (with escapes), `//` line and `/* ... */` block comments, all
//! keywords, the closure arrow `->`, pointer operators `&` / `&mut` / `*`, and
//! Groovy-style semicolon omission (newlines become significant `Newline`
//! tokens that the parser folds into statement terminators).

pub mod token;
pub use token::{InterpPart, Tok, Token};

/// A lexical error with position.
#[derive(Debug)]
pub struct LexError {
    pub msg: String,
    pub line: usize,
    pub col: usize,
}

impl std::fmt::Display for LexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}: {}", self.line, self.col, self.msg)
    }
}

fn err(line: usize, col: usize, msg: impl Into<String>) -> LexError {
    LexError { msg: msg.into(), line, col }
}

/// Tokenize a Pointerses source string.
pub fn tokenize(src: &str) -> Result<Vec<Token>, LexError> {
    let bytes: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut line = 1usize;
    let mut col = 1usize;
    let n = bytes.len();

    let mut last_was_newline = false;

    while i < n {
        let c = bytes[i];
        let start_line = line;
        let start_col = col;

        // Whitespace (but not newline)
        if c == ' ' || c == '\t' || c == '\r' {
            i += 1;
            col += 1;
            continue;
        }
        // Newline handling (Groovy-style semicolon omission)
        if c == '\n' {
            if !last_was_newline {
                out.push(Token::new(Tok::Newline, line, col));
            }
            last_was_newline = true;
            i += 1;
            line += 1;
            col = 1;
            continue;
        }
        last_was_newline = false;

        // Line comment
        if c == '/' && i + 1 < n && bytes[i + 1] == '/' {
            while i < n && bytes[i] != '\n' {
                i += 1;
                col += 1;
            }
            continue;
        }
        // Block comment
        if c == '/' && i + 1 < n && bytes[i + 1] == '*' {
            i += 2;
            col += 2;
            let mut depth = 1;
            while i < n && depth > 0 {
                if bytes[i] == '\n' {
                    line += 1;
                    col = 1;
                }
                if bytes[i] == '/' && i + 1 < n && bytes[i + 1] == '*' {
                    depth += 1;
                    i += 2;
                    col += 2;
                    continue;
                }
                if bytes[i] == '*' && i + 1 < n && bytes[i + 1] == '/' {
                    depth -= 1;
                    i += 2;
                    col += 2;
                    continue;
                }
                i += 1;
                col += 1;
            }
            continue;
        }

        // Identifier / keyword
        if c.is_alphabetic() || c == '_' {
            let mut s = String::new();
            while i < n && (bytes[i].is_alphanumeric() || bytes[i] == '_') {
                s.push(bytes[i]);
                i += 1;
                col += 1;
            }
            let kind = keyword(&s);
            out.push(Token::new(kind, start_line, start_col));
            continue;
        }

        // Number
        if c.is_ascii_digit() {
            let mut s = String::new();
            while i < n && bytes[i].is_ascii_digit() {
                s.push(bytes[i]);
                i += 1;
                col += 1;
            }
            let mut is_float = false;
            if i < n && bytes[i] == '.' && i + 1 < n && bytes[i + 1].is_ascii_digit() {
                is_float = true;
                s.push('.');
                i += 1;
                col += 1;
                while i < n && bytes[i].is_ascii_digit() {
                    s.push(bytes[i]);
                    i += 1;
                    col += 1;
                }
            }
            if is_float {
                let v: f64 = s.parse().map_err(|_| {
                    LexError { msg: format!("invalid float `{s}`"), line: start_line, col: start_col }
                })?;
                out.push(Token::new(Tok::Float(v), start_line, start_col));
            } else {
                let v: i64 = s.parse().map_err(|_| {
                    LexError { msg: format!("integer literal `{s}` out of range"), line: start_line, col: start_col }
                })?;
                out.push(Token::new(Tok::Int(v), start_line, start_col));
            }
            continue;
        }

        // String literal (optionally interpolated)
        if c == '"' {
            let mut text = String::new();
            let mut parts: Vec<InterpPart> = Vec::new();
            let mut has_interp = false;
            i += 1;
            col += 1;
            let mut closed = false;
            while i < n {
                let ch = bytes[i];
                if ch == '"' {
                    closed = true;
                    i += 1;
                    col += 1;
                    break;
                }
                if ch == '\\' && i + 1 < n {
                    let esc = bytes[i + 1];
                    match esc {
                        'n' => text.push('\n'),
                        't' => text.push('\t'),
                        'r' => text.push('\r'),
                        '\\' => text.push('\\'),
                        '"' => text.push('"'),
                        '$' => text.push('$'),
                        '0' => text.push('\0'),
                        'u' => {
                            // \u{XXXX}
                            if i + 2 < n && bytes[i + 2] == '{' {
                                let mut j = i + 3;
                                let mut hex = String::new();
                                while j < n && bytes[j] != '}' {
                                    hex.push(bytes[j]);
                                    j += 1;
                                }
                                if let Ok(cp) = u32::from_str_radix(&hex, 16) {
                                    if let Some(ch) = char::from_u32(cp) {
                                        text.push(ch);
                                    }
                                }
                                i = j + 1;
                                continue;
                            }
                            text.push('u');
                        }
                        other => text.push(other),
                    }
                    i += 2;
                    col += 2;
                    continue;
                }
                // `$ident` or `${ expr }` interpolation
                if ch == '$' && i + 1 < n {
                    let (expr, consumed): (Option<String>, usize) = if bytes[i + 1] == '{' {
                        // balanced scan until the matching `}`; nested strings and
                        // brackets are accounted for.
                        let mut j = i + 2;
                        let mut depth: i64 = 1;
                        let mut buf = String::new();
                        while j < n && depth > 0 {
                            let cc = bytes[j];
                            if cc == '"' {
                                buf.push(cc);
                                j += 1;
                                while j < n && bytes[j] != '"' {
                                    if bytes[j] == '\\' && j + 1 < n {
                                        buf.push(bytes[j]);
                                        buf.push(bytes[j + 1]);
                                        j += 2;
                                        continue;
                                    }
                                    buf.push(bytes[j]);
                                    j += 1;
                                }
                                if j < n {
                                    buf.push(bytes[j]);
                                    j += 1;
                                }
                                continue;
                            }
                            match cc {
                                '{' | '(' | '[' => depth += 1,
                                '}' | ')' | ']' => {
                                    depth -= 1;
                                    if depth == 0 {
                                        j += 1;
                                        break;
                                    }
                                }
                                _ => {}
                            }
                            buf.push(cc);
                            j += 1;
                        }
                        if depth != 0 {
                            return Err(err(start_line, start_col, "unterminated `${...}` interpolation"));
                        }
                        (Some(buf), j - i)
                    } else if bytes[i + 1].is_alphabetic() || bytes[i + 1] == '_' {
                        let mut j = i + 1;
                        let mut buf = String::new();
                        while j < n && (bytes[j].is_alphanumeric() || bytes[j] == '_') {
                            buf.push(bytes[j]);
                            j += 1;
                        }
                        (Some(buf), j - i)
                    } else {
                        (None, 0)
                    };
                    match expr {
                        Some(src) => {
                            if !text.is_empty() {
                                parts.push(InterpPart::Text(std::mem::take(&mut text)));
                            }
                            parts.push(InterpPart::Expr(src));
                            has_interp = true;
                            i += consumed;
                            col += consumed;
                            continue;
                        }
                        None => {
                            // a bare `$` (not an interpolation) is treated as text
                            text.push('$');
                            i += 1;
                            col += 1;
                            continue;
                        }
                    }
                }
                if ch == '\n' {
                    line += 1;
                    col = 1;
                }
                text.push(ch);
                i += 1;
                col += 1;
            }
            if !closed {
                return Err(err(start_line, start_col, "unterminated string literal"));
            }
            if has_interp {
                if !text.is_empty() {
                    parts.push(InterpPart::Text(text));
                }
                out.push(Token::new(Tok::InterpStr(parts), start_line, start_col));
            } else {
                out.push(Token::new(Tok::Str(text), start_line, start_col));
            }
            continue;
        }

        // Operators / punctuation
        let (kind, width) = match c {
            '{' => (Tok::LBrace, 1),
            '}' => (Tok::RBrace, 1),
            '(' => (Tok::LParen, 1),
            ')' => (Tok::RParen, 1),
            '[' => (Tok::LBracket, 1),
            ']' => (Tok::RBracket, 1),
            ',' => (Tok::Comma, 1),
            '.' => (Tok::Dot, 1),
            ':' => {
                if i + 1 < n && bytes[i + 1] == ':' {
                    (Tok::ColonColon, 2)
                } else {
                    (Tok::Colon, 1)
                }
            }
            ';' => (Tok::Semi, 1),
            '=' => {
                if i + 1 < n && bytes[i + 1] == '=' {
                    (Tok::EqEq, 2)
                } else if i + 1 < n && bytes[i + 1] == '>' {
                    (Tok::FatArrow, 2)
                } else {
                    (Tok::Eq, 1)
                }
            }
            '+' => {
                if i + 1 < n && bytes[i + 1] == '=' {
                    (Tok::PlusEq, 2)
                } else if i + 1 < n && bytes[i + 1] == '+' {
                    (Tok::PlusPlus, 2)
                } else {
                    (Tok::Plus, 1)
                }
            }
            '-' => {
                if i + 1 < n && bytes[i + 1] == '>' {
                    (Tok::Arrow, 2)
                } else if i + 1 < n && bytes[i + 1] == '=' {
                    (Tok::MinusEq, 2)
                } else if i + 1 < n && bytes[i + 1] == '-' {
                    (Tok::MinusMinus, 2)
                } else {
                    (Tok::Minus, 1)
                }
            }
            '*' => {
                if i + 1 < n && bytes[i + 1] == '=' {
                    (Tok::StarEq, 2)
                } else {
                    (Tok::Star, 1)
                }
            }
            '/' => {
                if i + 1 < n && bytes[i + 1] == '=' {
                    (Tok::SlashEq, 2)
                } else {
                    (Tok::Slash, 1)
                }
            }
            '%' => {
                if i + 1 < n && bytes[i + 1] == '=' {
                    (Tok::PercentEq, 2)
                } else {
                    (Tok::Percent, 1)
                }
            }
            '?' => (Tok::Question, 1),
            '&' => {
                if i + 1 < n && bytes[i + 1] == '&' {
                    (Tok::AndAnd, 2)
                } else {
                    (Tok::Amp, 1)
                }
            }
            '|' => {
                if i + 1 < n && bytes[i + 1] == '|' {
                    (Tok::OrOr, 2)
                } else {
                    (Tok::Pipe, 1)
                }
            }
            '!' => {
                if i + 1 < n && bytes[i + 1] == '=' {
                    (Tok::NotEq, 2)
                } else {
                    (Tok::Bang, 1)
                }
            }
            '<' => {
                if i + 1 < n && bytes[i + 1] == '=' {
                    (Tok::Le, 2)
                } else {
                    (Tok::Lt, 1)
                }
            }
            '>' => {
                if i + 1 < n && bytes[i + 1] == '=' {
                    (Tok::Ge, 2)
                } else {
                    (Tok::Gt, 1)
                }
            }
            '@' => (Tok::At, 1),
            other => {
                return Err(err(start_line, start_col, format!("unexpected character `{other}`")));
            }
        };
        out.push(Token::new(kind, start_line, start_col));
        for _ in 0..width {
            i += 1;
            col += 1;
        }
    }

    out.push(Token::new(Tok::Eof, line, col));
    Ok(out)
}

fn keyword(s: &str) -> Tok {
    match s {
        "func" => Tok::Fn,
        "var" => Tok::Let,
        "class" => Tok::Struct,
        "return" => Tok::Return,
        "if" => Tok::If,
        "else" => Tok::Else,
        "while" => Tok::While,
        "for" => Tok::For,
        "break" => Tok::Break,
        "continue" => Tok::Continue,
        "true" => Tok::True,
        "false" => Tok::False,
        "null" => Tok::Null,
        "extern" => Tok::Extern,
        "mut" => Tok::Mut,
        "region" => Tok::Region,
        "infs" => Tok::Trait,
        "with" => Tok::With,
        "imp" => Tok::Imp,
        "try" => Tok::Try,
        "catch" => Tok::Catch,
        "throw" => Tok::Throw,
        "finally" => Tok::Finally,
        "module" => Tok::Module,
        "import" => Tok::Import,
        "this" => Tok::This,
        _ => Tok::Ident(s.to_string()),
    }
}

/// Convenience: strip `Newline` tokens from a stream (used where newlines are
/// pure whitespace, e.g. inside parentheses).
pub fn strip_newlines(tokens: &[Token]) -> Vec<Token> {
    tokens
        .iter()
        .filter(|t| t.kind != Tok::Newline)
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_tokens() {
        let toks = tokenize("var x = 42").unwrap();
        assert!(matches!(toks[0].kind, Tok::Let));
        assert!(matches!(toks[1].kind, Tok::Ident(_)));
        assert!(matches!(toks[2].kind, Tok::Eq));
        assert!(matches!(toks[3].kind, Tok::Int(42)));
    }

    #[test]
    fn closure_arrow_and_comments() {
        let toks = tokenize("|x| -> x // c").unwrap();
        assert!(toks.iter().any(|t| t.kind == Tok::Arrow));
    }

    #[test]
    fn new_keywords_and_path_ops() {
        let toks = tokenize("infs with imp try catch throw finally module import this a::b").unwrap();
        assert!(toks.iter().any(|t| t.kind == Tok::Trait));
        assert!(toks.iter().any(|t| t.kind == Tok::With));
        assert!(toks.iter().any(|t| t.kind == Tok::Imp));
        assert!(toks.iter().any(|t| t.kind == Tok::Try));
        assert!(toks.iter().any(|t| t.kind == Tok::Catch));
        assert!(toks.iter().any(|t| t.kind == Tok::Throw));
        assert!(toks.iter().any(|t| t.kind == Tok::Finally));
        assert!(toks.iter().any(|t| t.kind == Tok::Module));
        assert!(toks.iter().any(|t| t.kind == Tok::Import));
        assert!(toks.iter().any(|t| t.kind == Tok::This));
        assert!(toks.iter().any(|t| t.kind == Tok::ColonColon));
    }

    #[test]
    fn string_interpolation_tokens() {
        let toks = tokenize(r#""hi ${1 + 2} $name!""#).unwrap();
        let interps: Vec<_> = toks.iter().filter(|t| matches!(t.kind, Tok::InterpStr(_))).collect();
        assert_eq!(interps.len(), 1);
        if let Tok::InterpStr(parts) = &interps[0].kind {
            assert_eq!(parts.len(), 5);
            assert_eq!(parts[0], InterpPart::Text("hi ".into()));
            assert_eq!(parts[1], InterpPart::Expr("1 + 2".into()));
            assert_eq!(parts[2], InterpPart::Text(" ".into()));
            assert_eq!(parts[3], InterpPart::Expr("name".into()));
            assert_eq!(parts[4], InterpPart::Text("!".into()));
        } else {
            panic!("expected InterpStr");
        }
        // plain strings still lex as Str, and \$ is an escaped dollar
        let plain = tokenize(r#""cost: \$5""#).unwrap();
        if let Tok::Str(s) = &plain[0].kind {
            assert_eq!(s, "cost: $5");
        } else {
            panic!("expected plain Str");
        }
    }

    #[test]
    fn nested_interp_braces() {
        let toks = tokenize(r#""f(${fn_call(1, { 2 })} tail)""#).unwrap();
        if let Tok::InterpStr(parts) = &toks[0].kind {
            assert_eq!(parts.len(), 3);
            assert_eq!(parts[0], InterpPart::Text("f(".into()));
            assert_eq!(parts[1], InterpPart::Expr("fn_call(1, { 2 })".into()));
            assert_eq!(parts[2], InterpPart::Text(" tail)".into()));
        } else {
            panic!("expected InterpStr");
        }
    }
}
