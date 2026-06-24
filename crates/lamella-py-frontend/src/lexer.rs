//! The lexer for the first-light Python subset.

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

/// A lexical token kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tok {
    /// A decimal integer literal.
    Int(i64),
    /// An identifier that is not a keyword.
    Name(String),
    /// A reserved keyword that exists in Python but is outside the first-light
    /// subset (e.g. `for`, `class`, `and`, `import`). The lexer recognizes the
    /// full reserved set (Language Reference 2.3.1) so these can never be used as
    /// identifiers; the parser rejects them with a clear message.
    Reserved(String),

    /// `def`
    KwDef,
    /// `return`
    KwReturn,
    /// `if`
    KwIf,
    /// `elif`
    KwElif,
    /// `else`
    KwElse,
    /// `while`
    KwWhile,
    /// `True`
    KwTrue,
    /// `False`
    KwFalse,
    /// `None`
    KwNone,
    /// `and`
    KwAnd,
    /// `or`
    KwOr,
    /// `not`
    KwNot,
    /// `for`
    KwFor,
    /// `in`
    KwIn,

    /// `+`
    Plus,
    /// `-`
    Minus,
    /// `*`
    Star,
    /// `/`
    Slash,
    /// `//`
    DoubleSlash,
    /// `%`
    Percent,
    /// `&`
    Amper,
    /// `|`
    Pipe,
    /// `^`
    Caret,
    /// `~`
    Tilde,
    /// `<<`
    LtLt,
    /// `>>`
    GtGt,
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
    /// `==`
    EqEq,
    /// `!=`
    NotEq,
    /// `=`
    Assign,
    /// `+=`
    PlusEq,
    /// `-=`
    MinusEq,
    /// `*=`
    StarEq,
    /// `//=`
    SlashSlashEq,
    /// `%=`
    PercentEq,
    /// `&=`
    AmperEq,
    /// `|=`
    PipeEq,
    /// `^=`
    CaretEq,
    /// `<<=`
    LtLtEq,
    /// `>>=`
    GtGtEq,
    /// `:`
    Colon,
    /// `,`
    Comma,
    /// `.`
    Dot,
    /// `->`
    Arrow,
    /// `(`
    LParen,
    /// `)`
    RParen,

    /// The end of a logical line.
    Newline,
    /// An increase in indentation.
    Indent,
    /// A decrease in indentation (one per level closed).
    Dedent,
    /// The end of the token stream.
    Eof,
}

/// A token and the 1-based source line it began on (for diagnostics).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    /// The token kind.
    pub kind: Tok,
    /// The 1-based line number where the token starts.
    pub line: u32,
}

/// A lexing failure: an offending line and a human-readable reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LexError {
    /// The 1-based line number where lexing failed.
    pub line: u32,
    /// What went wrong.
    pub message: String,
}

impl core::fmt::Display for LexError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

/// Tokenize `source` into the token stream the parser consumes, ending in
/// [`Tok::Eof`].
pub fn tokenize(source: &str) -> Result<Vec<Token>, LexError> {
    let mut lexer = Lexer {
        chars: source.chars().collect(),
        pos: 0,
        line: 1,
        indents: vec![0],
        bracket_depth: 0,
        tokens: Vec::new(),
    };
    lexer.run()?;
    Ok(lexer.tokens)
}

struct Lexer {
    chars: Vec<char>,
    pos: usize,
    line: u32,
    indents: Vec<u32>,
    bracket_depth: u32,
    tokens: Vec<Token>,
}

impl Lexer {
    fn run(&mut self) -> Result<(), LexError> {
        loop {
            if !self.begin_logical_line()? {
                break;
            }
            self.scan_logical_line()?;
        }
        self.finish();
        Ok(())
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn peek2(&self) -> Option<char> {
        self.chars.get(self.pos + 1).copied()
    }

    fn peek3(&self) -> Option<char> {
        self.chars.get(self.pos + 2).copied()
    }

    fn push(&mut self, kind: Tok) {
        self.tokens.push(Token {
            kind,
            line: self.line,
        });
    }

    fn err(&self, message: impl Into<String>) -> LexError {
        LexError {
            line: self.line,
            message: message.into(),
        }
    }

    fn current_indent(&self) -> u32 {
        self.indents.last().copied().unwrap_or(0)
    }

    /// Consume a run of spaces and tabs at the start of a line and return its
    /// indentation column, expanding a tab to the next multiple of eight (per the
    /// Language Reference).
    fn measure_indent(&mut self) -> u32 {
        let mut col = 0u32;
        loop {
            match self.peek() {
                Some(' ') => {
                    col += 1;
                    self.pos += 1;
                }
                Some('\t') => {
                    col = (col / 8 + 1) * 8;
                    self.pos += 1;
                }
                Some('\u{0C}') => {
                    self.pos += 1;
                }
                _ => break,
            }
        }
        col
    }

    /// Consume one line ending (`\n`, `\r`, or `\r\n`) if present, advancing the
    /// line counter.
    fn consume_newline(&mut self) {
        match self.peek() {
            Some('\r') => {
                self.pos += 1;
                if self.peek() == Some('\n') {
                    self.pos += 1;
                }
                self.line += 1;
            }
            Some('\n') => {
                self.pos += 1;
                self.line += 1;
            }
            _ => {}
        }
    }

    /// Consume from a `#` to (but not including) the end of the line.
    fn skip_comment(&mut self) {
        while let Some(c) = self.peek() {
            if c == '\n' || c == '\r' {
                break;
            }
            self.pos += 1;
        }
    }

    /// Position at the next real logical line, emitting its indentation tokens.
    /// Skips blank and comment-only lines. Returns `false` at end of input.
    fn begin_logical_line(&mut self) -> Result<bool, LexError> {
        loop {
            let col = self.measure_indent();
            match self.peek() {
                None => return Ok(false),
                Some('#') => {
                    self.skip_comment();
                    self.consume_newline();
                }
                Some('\n') | Some('\r') => self.consume_newline(),
                _ => {
                    self.apply_indentation(col)?;
                    return Ok(true);
                }
            }
        }
    }

    /// Emit [`Tok::Indent`]/[`Tok::Dedent`] for the new line's indentation `col`
    /// relative to the indentation stack.
    fn apply_indentation(&mut self, col: u32) -> Result<(), LexError> {
        let top = self.current_indent();
        if col > top {
            self.indents.push(col);
            self.push(Tok::Indent);
        } else if col < top {
            while self.current_indent() > col {
                self.indents.pop();
                self.push(Tok::Dedent);
            }
            if self.current_indent() != col {
                return Err(self.err("unindent does not match any outer indentation level"));
            }
        }
        Ok(())
    }

    /// Scan tokens until the logical line ends. Returns after consuming a
    /// significant newline (and emitting [`Tok::Newline`]) or at end of input.
    fn scan_logical_line(&mut self) -> Result<(), LexError> {
        loop {
            while matches!(self.peek(), Some(' ' | '\t' | '\u{0C}')) {
                self.pos += 1;
            }
            match self.peek() {
                None => return Ok(()),
                Some('#') => self.skip_comment(),
                Some('\\') => {
                    if matches!(self.peek2(), Some('\n' | '\r')) {
                        self.pos += 1;
                        self.consume_newline();
                    } else {
                        return Err(self.err("'\\' may only join lines at the end of a line"));
                    }
                }
                Some('\n') | Some('\r') => {
                    if self.bracket_depth > 0 {
                        self.consume_newline();
                    } else {
                        self.push(Tok::Newline);
                        self.consume_newline();
                        return Ok(());
                    }
                }
                Some(_) => self.lex_token()?,
            }
        }
    }

    fn lex_token(&mut self) -> Result<(), LexError> {
        let c = self.peek().expect("lex_token called at end of input");
        if c.is_ascii_digit() {
            self.lex_number()
        } else if c == '_' || c.is_ascii_alphabetic() {
            self.lex_name();
            Ok(())
        } else {
            self.lex_operator()
        }
    }

    /// Lex an integer literal per the Language Reference (2.4.4): a decimal digit run
    /// (no leading zeros on a non-zero value), or a base-prefixed `0x`/`0o`/`0b`
    /// literal. In every base, `_` separators are permitted only between digits (or
    /// right after the prefix). Float and imaginary literals remain out of scope.
    fn lex_number(&mut self) -> Result<(), LexError> {
        let first = self.peek().expect("lex_number called at a digit");
        if first == '0' {
            let radix = match self.peek2() {
                Some('x' | 'X') => Some(16),
                Some('o' | 'O') => Some(8),
                Some('b' | 'B') => Some(2),
                _ => None,
            };
            if let Some(radix) = radix {
                return self.lex_radix_int(radix);
            }
        }
        let mut digits = String::new();
        digits.push(first);
        self.pos += 1;
        loop {
            match self.peek() {
                Some(c) if c.is_ascii_digit() => {
                    digits.push(c);
                    self.pos += 1;
                }
                Some('_') => {
                    if matches!(self.peek2(), Some(c) if c.is_ascii_digit()) {
                        self.pos += 1;
                    } else {
                        return Err(
                            self.err("underscores in a numeric literal must be between digits")
                        );
                    }
                }
                _ => break,
            }
        }
        if matches!(self.peek(), Some(c) if c == '_' || c.is_ascii_alphabetic()) {
            return Err(self.err("invalid integer literal"));
        }
        if digits.starts_with('0') && digits.bytes().any(|b| b != b'0') {
            return Err(self.err("leading zeros in a decimal integer literal are not permitted"));
        }
        match digits.parse::<i64>() {
            Ok(value) => {
                self.push(Tok::Int(value));
                Ok(())
            }
            Err(_) => Err(self.err("integer literal too large for first light (exceeds 64 bits)")),
        }
    }

    /// Lex a base-prefixed integer (`0x`/`0o`/`0b`; the prefix not yet consumed) in
    /// `radix`, per the Language Reference (2.4.4): at least one digit, with `_`
    /// separators only between digits (one may follow the prefix). Folded to `i64`.
    fn lex_radix_int(&mut self, radix: u32) -> Result<(), LexError> {
        self.pos += 2;
        let mut digits = String::new();
        loop {
            match self.peek() {
                Some(c) if c.is_digit(radix) => {
                    digits.push(c);
                    self.pos += 1;
                }
                Some('_') => {
                    if matches!(self.peek2(), Some(c) if c.is_digit(radix)) {
                        self.pos += 1;
                    } else {
                        return Err(
                            self.err("underscores in a numeric literal must be between digits"),
                        );
                    }
                }
                _ => break,
            }
        }
        if digits.is_empty() {
            return Err(self.err("a base-prefixed integer literal needs at least one digit"));
        }
        if matches!(self.peek(), Some(c) if c == '_' || c.is_ascii_alphanumeric()) {
            return Err(self.err("invalid digit in a base-prefixed integer literal"));
        }
        match i64::from_str_radix(&digits, radix) {
            Ok(value) => {
                self.push(Tok::Int(value));
                Ok(())
            }
            Err(_) => Err(self.err("integer literal too large for first light (exceeds 64 bits)")),
        }
    }

    fn lex_name(&mut self) {
        let mut name = String::new();
        while let Some(c) = self.peek() {
            if c == '_' || c.is_ascii_alphanumeric() {
                name.push(c);
                self.pos += 1;
            } else {
                break;
            }
        }
        let kind = match name.as_str() {
            "def" => Tok::KwDef,
            "return" => Tok::KwReturn,
            "if" => Tok::KwIf,
            "elif" => Tok::KwElif,
            "else" => Tok::KwElse,
            "while" => Tok::KwWhile,
            "and" => Tok::KwAnd,
            "or" => Tok::KwOr,
            "not" => Tok::KwNot,
            "for" => Tok::KwFor,
            "in" => Tok::KwIn,
            "True" => Tok::KwTrue,
            "False" => Tok::KwFalse,
            "None" => Tok::KwNone,
            "as" | "assert" | "async" | "await" | "break" | "class" | "continue" | "del"
            | "except" | "finally" | "from" | "global" | "import" | "is" | "lambda"
            | "nonlocal" | "pass" | "raise" | "try" | "with" | "yield" => Tok::Reserved(name),
            _ => Tok::Name(name),
        };
        self.push(kind);
    }

    fn lex_operator(&mut self) -> Result<(), LexError> {
        let c = self.peek().expect("lex_operator called at end of input");
        let next = self.peek2();
        let third = self.peek3();
        let (kind, width) = match c {
            '+' if next == Some('=') => (Tok::PlusEq, 2),
            '+' => (Tok::Plus, 1),
            '-' if next == Some('>') => (Tok::Arrow, 2),
            '-' if next == Some('=') => (Tok::MinusEq, 2),
            '-' => (Tok::Minus, 1),
            '*' if next == Some('*') => {
                return Err(self.err("exponentiation '**' is out of the first-light subset"));
            }
            '*' if next == Some('=') => (Tok::StarEq, 2),
            '*' => (Tok::Star, 1),
            '/' if next == Some('/') && third == Some('=') => (Tok::SlashSlashEq, 3),
            '/' if next == Some('/') => (Tok::DoubleSlash, 2),
            '/' => (Tok::Slash, 1),
            '%' if next == Some('=') => (Tok::PercentEq, 2),
            '%' => (Tok::Percent, 1),
            '&' if next == Some('=') => (Tok::AmperEq, 2),
            '&' => (Tok::Amper, 1),
            '|' if next == Some('=') => (Tok::PipeEq, 2),
            '|' => (Tok::Pipe, 1),
            '^' if next == Some('=') => (Tok::CaretEq, 2),
            '^' => (Tok::Caret, 1),
            '~' => (Tok::Tilde, 1),
            '<' if next == Some('<') && third == Some('=') => (Tok::LtLtEq, 3),
            '<' if next == Some('<') => (Tok::LtLt, 2),
            '<' if next == Some('=') => (Tok::Le, 2),
            '<' => (Tok::Lt, 1),
            '>' if next == Some('>') && third == Some('=') => (Tok::GtGtEq, 3),
            '>' if next == Some('>') => (Tok::GtGt, 2),
            '>' if next == Some('=') => (Tok::Ge, 2),
            '>' => (Tok::Gt, 1),
            '=' if next == Some('=') => (Tok::EqEq, 2),
            '=' => (Tok::Assign, 1),
            '!' if next == Some('=') => (Tok::NotEq, 2),
            '!' => return Err(self.err("'!' is only valid as '!='")),
            ':' => (Tok::Colon, 1),
            ',' => (Tok::Comma, 1),
            '.' => (Tok::Dot, 1),
            '(' => (Tok::LParen, 1),
            ')' => (Tok::RParen, 1),
            other => return Err(self.err(format!("unexpected character {other:?}"))),
        };
        self.pos += width;
        if kind == Tok::LParen {
            self.bracket_depth += 1;
        } else if kind == Tok::RParen {
            self.bracket_depth = self.bracket_depth.saturating_sub(1);
        }
        self.push(kind);
        Ok(())
    }

    /// Close out the stream: a trailing [`Tok::Newline`] if the last line lacked
    /// one, then a [`Tok::Dedent`] per open indentation level, then [`Tok::Eof`].
    fn finish(&mut self) {
        if matches!(self.tokens.last(), Some(t) if t.kind != Tok::Newline) {
            self.push(Tok::Newline);
        }
        while self.current_indent() > 0 {
            self.indents.pop();
            self.push(Tok::Dedent);
        }
        self.push(Tok::Eof);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(source: &str) -> Vec<Tok> {
        tokenize(source)
            .expect("tokenizes")
            .into_iter()
            .map(|t| t.kind)
            .collect()
    }

    #[test]
    fn operators_and_literals() {
        assert_eq!(
            kinds("a = 1 + 2 * 3\n"),
            vec![
                Tok::Name("a".into()),
                Tok::Assign,
                Tok::Int(1),
                Tok::Plus,
                Tok::Int(2),
                Tok::Star,
                Tok::Int(3),
                Tok::Newline,
                Tok::Eof,
            ]
        );
    }

    #[test]
    fn two_char_operators() {
        assert_eq!(
            kinds("x <= y != z >= w == v // u\n"),
            vec![
                Tok::Name("x".into()),
                Tok::Le,
                Tok::Name("y".into()),
                Tok::NotEq,
                Tok::Name("z".into()),
                Tok::Ge,
                Tok::Name("w".into()),
                Tok::EqEq,
                Tok::Name("v".into()),
                Tok::DoubleSlash,
                Tok::Name("u".into()),
                Tok::Newline,
                Tok::Eof,
            ]
        );
    }

    #[test]
    fn underscores_in_integers() {
        assert_eq!(
            kinds("1_000\n"),
            vec![Tok::Int(1000), Tok::Newline, Tok::Eof]
        );
    }

    #[test]
    fn indentation_emits_indent_and_dedent() {
        let src = "def f():\n    return 1\n";
        assert_eq!(
            kinds(src),
            vec![
                Tok::KwDef,
                Tok::Name("f".into()),
                Tok::LParen,
                Tok::RParen,
                Tok::Colon,
                Tok::Newline,
                Tok::Indent,
                Tok::KwReturn,
                Tok::Int(1),
                Tok::Newline,
                Tok::Dedent,
                Tok::Eof,
            ]
        );
    }

    #[test]
    fn nested_indentation_dedents_to_zero_at_eof() {
        let src = "def f():\n    while x:\n        return 1";
        let toks = kinds(src);
        let tail = &toks[toks.len() - 3..];
        assert_eq!(tail, &[Tok::Dedent, Tok::Dedent, Tok::Eof]);
    }

    #[test]
    fn blank_and_comment_lines_are_invisible() {
        let src = "a = 1\n\n# a comment\n   # indented comment\nb = 2\n";
        assert_eq!(
            kinds(src),
            vec![
                Tok::Name("a".into()),
                Tok::Assign,
                Tok::Int(1),
                Tok::Newline,
                Tok::Name("b".into()),
                Tok::Assign,
                Tok::Int(2),
                Tok::Newline,
                Tok::Eof,
            ]
        );
    }

    #[test]
    fn newlines_inside_parentheses_are_joined() {
        let src = "f(\n    1,\n    2,\n)\n";
        assert_eq!(
            kinds(src),
            vec![
                Tok::Name("f".into()),
                Tok::LParen,
                Tok::Int(1),
                Tok::Comma,
                Tok::Int(2),
                Tok::Comma,
                Tok::RParen,
                Tok::Newline,
                Tok::Eof,
            ]
        );
    }

    #[test]
    fn attribute_dot_and_arrow() {
        assert_eq!(
            kinds("obj.x\n"),
            vec![
                Tok::Name("obj".into()),
                Tok::Dot,
                Tok::Name("x".into()),
                Tok::Newline,
                Tok::Eof,
            ]
        );
        assert!(kinds("def f() -> int: return 1\n").contains(&Tok::Arrow));
    }

    #[test]
    fn inconsistent_dedent_is_an_error() {
        let src = "if x:\n    a = 1\n  b = 2\n";
        let err = tokenize(src).expect_err("should fail");
        assert!(err.message.contains("unindent"));
    }

    #[test]
    fn empty_source_is_just_eof() {
        assert_eq!(kinds(""), vec![Tok::Eof]);
        assert_eq!(kinds("\n\n  \n"), vec![Tok::Eof]);
    }

    #[test]
    fn out_of_subset_keywords_are_reserved_not_names() {
        assert_eq!(
            kinds("lambda x class\n"),
            vec![
                Tok::Reserved("lambda".into()),
                Tok::Name("x".into()),
                Tok::Reserved("class".into()),
                Tok::Newline,
                Tok::Eof,
            ]
        );
    }

    #[test]
    fn soft_keywords_remain_identifiers() {
        for word in ["match", "case", "type", "_"] {
            assert_eq!(kinds(&alloc::format!("{word}\n"))[0], Tok::Name(word.into()));
        }
    }

    #[test]
    fn backslash_joins_physical_lines() {
        assert_eq!(
            kinds("a = 1 + \\\n    2\n"),
            vec![
                Tok::Name("a".into()),
                Tok::Assign,
                Tok::Int(1),
                Tok::Plus,
                Tok::Int(2),
                Tok::Newline,
                Tok::Eof,
            ]
        );
    }

    #[test]
    fn integer_literal_rules_follow_the_reference() {
        assert_eq!(kinds("0\n")[0], Tok::Int(0));
        assert_eq!(kinds("00\n")[0], Tok::Int(0));
        assert_eq!(kinds("12_345\n")[0], Tok::Int(12_345));
        assert!(tokenize("0123\n").is_err());
        assert!(tokenize("1__2\n").is_err());
        assert!(tokenize("1_\n").is_err());
        assert!(tokenize("2 ** 3\n").is_err());
    }

    #[test]
    fn non_decimal_integer_literals_follow_the_reference() {
        assert_eq!(kinds("0xFF\n")[0], Tok::Int(255));
        assert_eq!(kinds("0Xff\n")[0], Tok::Int(255));
        assert_eq!(kinds("0o17\n")[0], Tok::Int(15));
        assert_eq!(kinds("0O17\n")[0], Tok::Int(15));
        assert_eq!(kinds("0b1010\n")[0], Tok::Int(10));
        assert_eq!(kinds("0B1010\n")[0], Tok::Int(10));
        assert_eq!(kinds("0xDE_AD\n")[0], Tok::Int(0xDEAD));
        assert_eq!(kinds("0x_FF\n")[0], Tok::Int(255));
        assert!(tokenize("0x\n").is_err());
        assert!(tokenize("0xFF_\n").is_err());
        assert!(tokenize("0xF__F\n").is_err());
        assert!(tokenize("0o8\n").is_err());
        assert!(tokenize("0b2\n").is_err());
        assert!(tokenize("0xG\n").is_err());
    }
}
