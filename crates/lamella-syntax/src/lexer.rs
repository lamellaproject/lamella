//! The scanner: turning C# source text into a token stream.

use crate::diagnostic::{Diagnostic, DiagnosticKind};
use crate::span::Span;
use crate::token::{IntegerSuffix, Keyword, Punctuator, RealSuffix, Token, TokenKind};
use alloc::vec::Vec;

/// The result of scanning a source file: the full token stream (ending in one
/// [`TokenKind::EndOfFile`]) and any diagnostics gathered along the way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tokenized {
    /// Every token in source order, trivia included, ending in `EndOfFile`.
    pub tokens: Vec<Token>,
    /// Lexical diagnostics, in source order.
    pub diagnostics: Vec<Diagnostic>,
}

/// Scans `source` into a complete [`Tokenized`] stream.
///
/// The token stream is a gap-free cover of the source: concatenating the text of
/// every non-`EndOfFile` token reproduces the input, after the single trailing
/// Control-Z removal of 9.3.1.
#[must_use]
pub fn tokenize(source: &str) -> Tokenized {
    let mut lexer = Lexer::new(source);
    let mut tokens = Vec::new();
    loop {
        let token = lexer.next_token();
        let reached_end = token.kind == TokenKind::EndOfFile;
        tokens.push(token);
        if reached_end {
            break;
        }
    }
    Tokenized {
        tokens,
        diagnostics: lexer.into_diagnostics(),
    }
}

/// A pull-based scanner over a single source file.
///
/// Call [`Lexer::next_token`] repeatedly; it yields tokens until the end of the
/// source, after which it returns `EndOfFile` indefinitely. Diagnostics are
/// collected as scanning proceeds.
#[derive(Debug)]
pub struct Lexer<'a> {
    source: &'a str,
    position: usize,
    diagnostics: Vec<Diagnostic>,
}

impl<'a> Lexer<'a> {
    /// Creates a scanner over `source`.
    ///
    /// A single trailing Control-Z (U+001A) is removed up front, as 9.3.1
    /// requires. Because the result is a prefix of the original text, all byte
    /// offsets still line up with the caller's source.
    #[must_use]
    pub fn new(source: &'a str) -> Lexer<'a> {
        let source = source.strip_suffix('\u{001A}').unwrap_or(source);
        Lexer {
            source,
            position: 0,
            diagnostics: Vec::new(),
        }
    }

    /// The diagnostics collected so far.
    #[must_use]
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    /// Consumes the scanner and returns the collected diagnostics.
    #[must_use]
    pub fn into_diagnostics(self) -> Vec<Diagnostic> {
        self.diagnostics
    }

    /// Scans and returns the next token. At the end of the source this returns
    /// an `EndOfFile` token, and continues to do so on further calls.
    pub fn next_token(&mut self) -> Token {
        let start = self.position;
        let Some(c) = self.peek() else {
            return Token::new(TokenKind::EndOfFile, Span::empty_at(start as u32));
        };

        let kind = if is_new_line(c) {
            self.scan_new_line()
        } else if is_whitespace(c) {
            self.scan_whitespace()
        } else if c == '/' && matches!(self.peek_second(), Some('/' | '*')) {
            self.scan_comment(start)
        } else if is_identifier_start(c) {
            self.scan_identifier_or_keyword(start)
        } else if c == '@' {
            self.scan_verbatim(start)
        } else if c.is_ascii_digit()
            || (c == '.' && self.peek_second().is_some_and(|next| next.is_ascii_digit()))
        {
            self.scan_numeric_literal(start)
        } else if c == '\'' {
            self.scan_character_literal(start)
        } else if c == '"' {
            self.scan_string_literal(start)
        } else if let Some(punctuator) = self.try_scan_operator() {
            TokenKind::Punctuator(punctuator)
        } else if is_deferred_start(c) {
            self.bump();
            TokenKind::Unknown
        } else {
            self.bump();
            self.report(DiagnosticKind::UnexpectedCharacter { character: c }, start);
            TokenKind::Unknown
        };

        Token::new(kind, Span::new(start as u32, self.position as u32))
    }

    fn scan_new_line(&mut self) -> TokenKind {
        let c = self.bump().expect("a current character is present");
        if c == '\r' && self.peek() == Some('\n') {
            self.bump();
        }
        TokenKind::NewLine
    }

    fn scan_whitespace(&mut self) -> TokenKind {
        while let Some(c) = self.peek() {
            if is_whitespace(c) {
                self.bump();
            } else {
                break;
            }
        }
        TokenKind::Whitespace
    }

    fn scan_comment(&mut self, start: usize) -> TokenKind {
        self.bump();
        match self.bump() {
            Some('/') => {
                while let Some(c) = self.peek() {
                    if is_new_line(c) {
                        break;
                    }
                    self.bump();
                }
                TokenKind::SingleLineComment
            }
            Some('*') => {
                loop {
                    match self.bump() {
                        None => {
                            self.report(DiagnosticKind::UnterminatedDelimitedComment, start);
                            break;
                        }
                        Some('*') if self.peek() == Some('/') => {
                            self.bump();
                            break;
                        }
                        Some(_) => {}
                    }
                }
                TokenKind::DelimitedComment
            }
            _ => unreachable!("scan_comment runs only when '//' or '/*' is present"),
        }
    }

    fn scan_identifier_or_keyword(&mut self, start: usize) -> TokenKind {
        self.bump();
        self.consume_identifier_part();
        let text = &self.source[start..self.position];
        match Keyword::from_text(text) {
            Some(keyword) => TokenKind::Keyword(keyword),
            None => TokenKind::Identifier(text.into()),
        }
    }

    fn scan_verbatim(&mut self, start: usize) -> TokenKind {
        match self.peek_second() {
            Some(c) if is_identifier_start(c) => {
                self.bump();
                let name_start = self.position;
                self.bump();
                self.consume_identifier_part();
                let text = &self.source[name_start..self.position];
                TokenKind::Identifier(text.into())
            }
            Some('"') => self.scan_verbatim_string(start),
            _ => {
                self.bump();
                self.report(
                    DiagnosticKind::UnexpectedCharacter { character: '@' },
                    start,
                );
                TokenKind::Unknown
            }
        }
    }

    fn consume_identifier_part(&mut self) {
        while let Some(c) = self.peek() {
            if is_identifier_part(c) {
                self.bump();
            } else {
                break;
            }
        }
    }

    /// Scans a character literal (9.4.4.4): one `character` between single
    /// quotes. The value is a single UTF-16 code unit; an empty literal is
    /// `CS1011` and one holding more than one unit is `CS1012`. A literal cut
    /// short by a line terminator or end of file is `CS1010` and ends there.
    fn scan_character_literal(&mut self, start: usize) -> TokenKind {
        self.bump();
        let mut units = Vec::new();
        let terminated = self.scan_quoted_body('\'', &mut units, start);
        if terminated {
            match units.len() {
                0 => self.report(DiagnosticKind::EmptyCharacterLiteral, start),
                1 => {}
                _ => self.report(DiagnosticKind::TooManyCharactersInCharacterLiteral, start),
            }
        }
        TokenKind::CharacterLiteral(units.first().copied().unwrap_or(0))
    }

    /// Scans a regular string literal (9.4.4.5): zero or more `character`s
    /// between double quotes, with escapes decoded to UTF-16 code units. A
    /// literal cut short by a line terminator or end of file is `CS1010`.
    fn scan_string_literal(&mut self, start: usize) -> TokenKind {
        self.bump();
        let mut units = Vec::new();
        self.scan_quoted_body('"', &mut units, start);
        TokenKind::StringLiteral(units.into())
    }

    /// Scans the body of a regular (non-verbatim) character or string literal up
    /// to and including the closing `quote`, decoding escapes into `units`.
    /// Returns whether the closing quote was reached. A line terminator or end
    /// of file first is `CS1010` (a newline does not belong to the constant, so
    /// it is left for the next token) and the body ends without a closing quote.
    fn scan_quoted_body(&mut self, quote: char, units: &mut Vec<u16>, start: usize) -> bool {
        loop {
            match self.peek() {
                None => {
                    self.report(DiagnosticKind::NewlineInConstant, start);
                    return false;
                }
                Some(c) if is_new_line(c) => {
                    self.report(DiagnosticKind::NewlineInConstant, start);
                    return false;
                }
                Some(c) if c == quote => {
                    self.bump();
                    return true;
                }
                Some('\\') => self.scan_escape_sequence(units),
                Some(c) => {
                    self.bump();
                    push_utf16(units, c);
                }
            }
        }
    }

    /// Scans a verbatim string literal (9.4.4.5): `@"`, then characters taken
    /// verbatim (newlines included, no backslash escapes), where a doubled quote
    /// `""` stands for one quote, up to the closing quote. End of file first is
    /// `CS1039`.
    fn scan_verbatim_string(&mut self, start: usize) -> TokenKind {
        self.bump();
        self.bump();
        let mut units = Vec::new();
        loop {
            match self.bump() {
                None => {
                    self.report(DiagnosticKind::UnterminatedStringLiteral, start);
                    break;
                }
                Some('"') if self.peek() == Some('"') => {
                    self.bump();
                    units.push(u16::from(b'"'));
                }
                Some('"') => break,
                Some(c) => push_utf16(&mut units, c),
            }
        }
        TokenKind::StringLiteral(units.into())
    }

    /// Decodes one backslash escape (9.4.4.4, 9.4.1) into `units`, with the
    /// scanner positioned at the backslash. An unrecognised escape, a `\x` with
    /// no hex digits, a `\u`/`\U` with too few, or a `\U` above U+10FFFF is
    /// `CS1009`; recovery still consumes the offending characters so scanning of
    /// the rest of the literal continues.
    fn scan_escape_sequence(&mut self, units: &mut Vec<u16>) {
        let escape_start = self.position;
        self.bump();
        let unit = match self.peek() {
            Some('\'') => 0x0027,
            Some('"') => 0x0022,
            Some('\\') => 0x005C,
            Some('0') => 0x0000,
            Some('a') => 0x0007,
            Some('b') => 0x0008,
            Some('f') => 0x000C,
            Some('n') => 0x000A,
            Some('r') => 0x000D,
            Some('t') => 0x0009,
            Some('v') => 0x000B,
            Some('x') => return self.scan_hexadecimal_escape(units, escape_start),
            Some('u') => return self.scan_unicode_escape(units, escape_start, 4),
            Some('U') => return self.scan_unicode_escape(units, escape_start, 8),
            Some(c) => {
                self.report(DiagnosticKind::UnrecognizedEscapeSequence, escape_start);
                self.bump();
                push_utf16(units, c);
                return;
            }
            None => {
                self.report(DiagnosticKind::UnrecognizedEscapeSequence, escape_start);
                return;
            }
        };
        self.bump();
        units.push(unit);
    }

    /// Decodes a hexadecimal escape `\x hex-digit{1,4}` (9.4.4.4) into `units`,
    /// with the scanner positioned at the `x`. The four-digit cap means the
    /// value always fits one UTF-16 code unit. No digits at all is `CS1009`.
    fn scan_hexadecimal_escape(&mut self, units: &mut Vec<u16>, escape_start: usize) {
        self.bump();
        let mut value: u16 = 0;
        let mut digits = 0;
        while digits < 4 {
            let Some(digit) = self.peek().and_then(|c| c.to_digit(16)) else {
                break;
            };
            value = value * 16 + digit as u16;
            self.bump();
            digits += 1;
        }
        if digits == 0 {
            self.report(DiagnosticKind::UnrecognizedEscapeSequence, escape_start);
            units.push(REPLACEMENT_UNIT);
            return;
        }
        units.push(value);
    }

    /// Decodes a Unicode escape (9.4.1) into `units`, with the scanner
    /// positioned at the `u` or `U`. `width` is 4 for `\u` and 8 for `\U`;
    /// exactly that many hex digits are required. A four-digit `\u` yields the
    /// 16-bit value directly, so a lone surrogate is representable; an
    /// eight-digit `\U` is a scalar value encoded as UTF-16, one or two units.
    /// Too few digits, or a `\U` value that is not a Unicode scalar, is `CS1009`.
    fn scan_unicode_escape(&mut self, units: &mut Vec<u16>, escape_start: usize, width: u32) {
        self.bump();
        let mut value: u32 = 0;
        for _ in 0..width {
            let Some(digit) = self.peek().and_then(|c| c.to_digit(16)) else {
                self.report(DiagnosticKind::UnrecognizedEscapeSequence, escape_start);
                units.push(REPLACEMENT_UNIT);
                return;
            };
            value = value * 16 + digit;
            self.bump();
        }
        if width == 4 {
            units.push(value as u16);
        } else if let Some(scalar) = char::from_u32(value) {
            push_utf16(units, scalar);
        } else {
            self.report(DiagnosticKind::UnrecognizedEscapeSequence, escape_start);
            units.push(REPLACEMENT_UNIT);
        }
    }

    fn scan_numeric_literal(&mut self, start: usize) -> TokenKind {
        if self.peek() == Some('.') {
            self.bump();
            self.consume_decimal_digits();
            self.consume_exponent();
            let suffix = self.try_consume_real_suffix().unwrap_or(RealSuffix::None);
            return TokenKind::RealLiteral { suffix };
        }

        if self.peek() == Some('0') && matches!(self.peek_second(), Some('x' | 'X')) {
            self.bump();
            self.bump();
            let digits_start = self.position;
            self.consume_hex_digits();
            if self.position == digits_start {
                self.report(DiagnosticKind::MalformedNumericLiteral, start);
            }
            let digits = &self.source[digits_start..self.position];
            let suffix = self.consume_integer_suffix();
            let value = self.parse_integer_value(digits, 16, start);
            return TokenKind::IntegerLiteral { value, suffix };
        }

        let digits_start = self.position;
        self.consume_decimal_digits();
        let integer_digits_end = self.position;

        let mut is_real = false;
        if self.peek() == Some('.') && self.peek_second().is_some_and(|c| c.is_ascii_digit()) {
            is_real = true;
            self.bump();
            self.consume_decimal_digits();
        }
        if self.consume_exponent() {
            is_real = true;
        }

        if is_real {
            let suffix = self.try_consume_real_suffix().unwrap_or(RealSuffix::None);
            TokenKind::RealLiteral { suffix }
        } else if let Some(suffix) = self.try_consume_real_suffix() {
            TokenKind::RealLiteral { suffix }
        } else {
            let digits = &self.source[digits_start..integer_digits_end];
            let suffix = self.consume_integer_suffix();
            let value = self.parse_integer_value(digits, 10, start);
            TokenKind::IntegerLiteral { value, suffix }
        }
    }

    fn consume_decimal_digits(&mut self) {
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.bump();
        }
    }

    fn consume_hex_digits(&mut self) {
        while self.peek().is_some_and(|c| c.is_ascii_hexdigit()) {
            self.bump();
        }
    }

    fn consume_exponent(&mut self) -> bool {
        if !matches!(self.peek(), Some('e' | 'E')) {
            return false;
        }
        let exponent_start = self.position;
        self.bump();
        if matches!(self.peek(), Some('+' | '-')) {
            self.bump();
        }
        let digits_start = self.position;
        self.consume_decimal_digits();
        if self.position == digits_start {
            self.report(DiagnosticKind::MalformedNumericLiteral, exponent_start);
        }
        true
    }

    fn try_consume_real_suffix(&mut self) -> Option<RealSuffix> {
        let suffix = match self.peek() {
            Some('f' | 'F') => RealSuffix::Float,
            Some('d' | 'D') => RealSuffix::Double,
            Some('m' | 'M') => RealSuffix::Decimal,
            _ => return None,
        };
        self.bump();
        Some(suffix)
    }

    fn consume_integer_suffix(&mut self) -> IntegerSuffix {
        let mut unsigned = false;
        let mut long = false;
        loop {
            match self.peek() {
                Some('u' | 'U') if !unsigned => unsigned = true,
                Some('l' | 'L') if !long => long = true,
                _ => break,
            }
            self.bump();
        }
        match (unsigned, long) {
            (false, false) => IntegerSuffix::None,
            (true, false) => IntegerSuffix::Unsigned,
            (false, true) => IntegerSuffix::Long,
            (true, true) => IntegerSuffix::UnsignedLong,
        }
    }

    fn parse_integer_value(&mut self, digits: &str, radix: u32, start: usize) -> u64 {
        let mut value: u64 = 0;
        for c in digits.chars() {
            let digit = c
                .to_digit(radix)
                .expect("the scanner validated these digits");
            let next = value
                .checked_mul(u64::from(radix))
                .and_then(|scaled| scaled.checked_add(u64::from(digit)));
            match next {
                Some(updated) => value = updated,
                None => {
                    self.report(DiagnosticKind::IntegerLiteralTooLarge, start);
                    return 0;
                }
            }
        }
        value
    }

    /// Tries to scan an operator or punctuator at the current position by maximal
    /// munch, taking the longest spelling that matches (9.4.5). Returns `None`
    /// without advancing if none matches.
    fn try_scan_operator(&mut self) -> Option<Punctuator> {
        let rest = self.remaining();
        for length in (1..=3).rev() {
            if rest.len() >= length && rest.is_char_boundary(length) {
                if let Some(punctuator) = Punctuator::from_text(&rest[..length]) {
                    self.position += length;
                    return Some(punctuator);
                }
            }
        }
        None
    }

    fn report(&mut self, kind: DiagnosticKind, start: usize) {
        let span = Span::new(start as u32, self.position as u32);
        self.diagnostics.push(Diagnostic::new(kind, span));
    }

    fn peek(&self) -> Option<char> {
        self.remaining().chars().next()
    }

    fn peek_second(&self) -> Option<char> {
        let mut chars = self.remaining().chars();
        chars.next();
        chars.next()
    }

    fn remaining(&self) -> &'a str {
        &self.source[self.position..]
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.position += c.len_utf8();
        Some(c)
    }
}

/// A line terminator (9.3.1). A CR/LF pair is combined into one by the scanner.
fn is_new_line(c: char) -> bool {
    matches!(c, '\r' | '\n' | '\u{2028}' | '\u{2029}')
}

/// White space (9.3.3). ASCII-only for now: the space is the only ASCII member
/// of Unicode class Zs, and the remaining Zs characters need UCD tables.
fn is_whitespace(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\u{000B}' | '\u{000C}')
}

/// An identifier-start character (9.4.2). ASCII-only for now.
fn is_identifier_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

/// An identifier-part character (9.4.2). ASCII-only for now.
fn is_identifier_part(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// The Unicode replacement character (U+FFFD), stood in for one ill-formed
/// escape so the rest of the literal still scans and a character literal with a
/// single bad escape counts as one character rather than as empty.
const REPLACEMENT_UNIT: u16 = 0xFFFD;

/// Appends the UTF-16 encoding of `scalar` to `units`: one code unit for a
/// character in the Basic Multilingual Plane, a surrogate pair for one above it.
fn push_utf16(units: &mut Vec<u16>, scalar: char) {
    let mut buffer = [0u16; 2];
    units.extend_from_slice(scalar.encode_utf16(&mut buffer));
}

/// A character that begins a construct scanned in a later chunk: a pre-processing
/// directive (9.5).
fn is_deferred_start(c: char) -> bool {
    c == '#'
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;

    fn kinds(source: &str) -> Vec<TokenKind> {
        tokenize(source)
            .tokens
            .into_iter()
            .map(|token| token.kind)
            .collect()
    }

    fn ident(text: &str) -> TokenKind {
        TokenKind::Identifier(text.into())
    }

    fn int(value: u64, suffix: IntegerSuffix) -> TokenKind {
        TokenKind::IntegerLiteral { value, suffix }
    }

    fn real(suffix: RealSuffix) -> TokenKind {
        TokenKind::RealLiteral { suffix }
    }

    #[test]
    fn decimal_integer_literals() {
        assert_eq!(
            kinds("0"),
            vec![int(0, IntegerSuffix::None), TokenKind::EndOfFile]
        );
        assert_eq!(
            kinds("42"),
            vec![int(42, IntegerSuffix::None), TokenKind::EndOfFile]
        );
        assert_eq!(
            kinds("007"),
            vec![int(7, IntegerSuffix::None), TokenKind::EndOfFile]
        );
    }

    #[test]
    fn hexadecimal_integer_literals() {
        assert_eq!(
            kinds("0xFF"),
            vec![int(255, IntegerSuffix::None), TokenKind::EndOfFile]
        );
        assert_eq!(
            kinds("0X10"),
            vec![int(16, IntegerSuffix::None), TokenKind::EndOfFile]
        );
        assert_eq!(
            kinds("0xcafe"),
            vec![int(0xcafe, IntegerSuffix::None), TokenKind::EndOfFile]
        );
    }

    #[test]
    fn integer_suffixes_in_any_order() {
        assert_eq!(
            kinds("1u"),
            vec![int(1, IntegerSuffix::Unsigned), TokenKind::EndOfFile]
        );
        assert_eq!(
            kinds("1L"),
            vec![int(1, IntegerSuffix::Long), TokenKind::EndOfFile]
        );
        assert_eq!(
            kinds("1ul"),
            vec![int(1, IntegerSuffix::UnsignedLong), TokenKind::EndOfFile]
        );
        assert_eq!(
            kinds("1LU"),
            vec![int(1, IntegerSuffix::UnsignedLong), TokenKind::EndOfFile]
        );
    }

    #[test]
    fn ulong_max_is_fine_but_one_more_overflows() {
        let max = tokenize("18446744073709551615");
        assert_eq!(max.tokens[0].kind, int(u64::MAX, IntegerSuffix::None));
        assert!(max.diagnostics.is_empty());

        let over = tokenize("18446744073709551616");
        assert_eq!(over.diagnostics.len(), 1);
        assert_eq!(over.diagnostics[0].code(), 1021);
    }

    #[test]
    fn real_literals_in_their_several_forms() {
        assert_eq!(
            kinds("1.5"),
            vec![real(RealSuffix::None), TokenKind::EndOfFile]
        );
        assert_eq!(
            kinds(".5"),
            vec![real(RealSuffix::None), TokenKind::EndOfFile]
        );
        assert_eq!(
            kinds("1e10"),
            vec![real(RealSuffix::None), TokenKind::EndOfFile]
        );
        assert_eq!(
            kinds("1.5E-3"),
            vec![real(RealSuffix::None), TokenKind::EndOfFile]
        );
        assert_eq!(
            kinds("1f"),
            vec![real(RealSuffix::Float), TokenKind::EndOfFile]
        );
        assert_eq!(
            kinds("2.0d"),
            vec![real(RealSuffix::Double), TokenKind::EndOfFile]
        );
        assert_eq!(
            kinds("3m"),
            vec![real(RealSuffix::Decimal), TokenKind::EndOfFile]
        );
    }

    #[test]
    fn a_dot_after_digits_is_member_access_not_a_fraction() {
        let significant: Vec<TokenKind> = tokenize("1.Foo")
            .tokens
            .into_iter()
            .map(|token| token.kind)
            .filter(|kind| !kind.is_trivia() && *kind != TokenKind::EndOfFile)
            .collect();
        assert_eq!(
            significant,
            vec![
                int(1, IntegerSuffix::None),
                TokenKind::Punctuator(Punctuator::Dot),
                ident("Foo")
            ]
        );
    }

    #[test]
    fn empty_source_is_just_end_of_file() {
        assert_eq!(kinds(""), vec![TokenKind::EndOfFile]);
    }

    #[test]
    fn keywords_and_identifiers_are_distinguished() {
        assert_eq!(
            kinds("class"),
            vec![TokenKind::Keyword(Keyword::Class), TokenKind::EndOfFile]
        );
        assert_eq!(kinds("Hello"), vec![ident("Hello"), TokenKind::EndOfFile]);
        assert_eq!(kinds("_x1"), vec![ident("_x1"), TokenKind::EndOfFile]);
    }

    #[test]
    fn verbatim_identifier_drops_the_at_and_is_never_a_keyword() {
        assert_eq!(kinds("@class"), vec![ident("class"), TokenKind::EndOfFile]);
        assert_eq!(kinds("@foo"), vec![ident("foo"), TokenKind::EndOfFile]);
    }

    #[test]
    fn line_terminators_collapse_crlf() {
        assert_eq!(kinds("\n"), vec![TokenKind::NewLine, TokenKind::EndOfFile]);
        assert_eq!(kinds("\r"), vec![TokenKind::NewLine, TokenKind::EndOfFile]);
        assert_eq!(
            kinds("\r\n"),
            vec![TokenKind::NewLine, TokenKind::EndOfFile]
        );
        assert_eq!(
            kinds("\u{2028}"),
            vec![TokenKind::NewLine, TokenKind::EndOfFile]
        );
        assert_eq!(
            kinds("\r\n\n"),
            vec![TokenKind::NewLine, TokenKind::NewLine, TokenKind::EndOfFile]
        );
    }

    #[test]
    fn whitespace_runs_are_one_token() {
        assert_eq!(
            kinds("  \t"),
            vec![TokenKind::Whitespace, TokenKind::EndOfFile]
        );
    }

    #[test]
    fn single_line_comment_stops_before_the_newline() {
        assert_eq!(
            kinds("// hi\nx"),
            vec![
                TokenKind::SingleLineComment,
                TokenKind::NewLine,
                ident("x"),
                TokenKind::EndOfFile
            ]
        );
    }

    #[test]
    fn delimited_comment_spans_multiple_lines() {
        assert_eq!(
            kinds("/* a\n b */x"),
            vec![
                TokenKind::DelimitedComment,
                ident("x"),
                TokenKind::EndOfFile
            ]
        );
    }

    #[test]
    fn unterminated_delimited_comment_reports_cs1035() {
        let result = tokenize("/* nope");
        assert_eq!(result.tokens[0].kind, TokenKind::DelimitedComment);
        assert_eq!(result.tokens[1].kind, TokenKind::EndOfFile);
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(result.diagnostics[0].code(), 1035);
    }

    #[test]
    fn operators_take_the_longest_match() {
        assert_eq!(
            kinds(">>="),
            vec![
                TokenKind::Punctuator(Punctuator::GreaterThanGreaterThanEquals),
                TokenKind::EndOfFile
            ]
        );
        assert_eq!(
            kinds(">>"),
            vec![
                TokenKind::Punctuator(Punctuator::GreaterThanGreaterThan),
                TokenKind::EndOfFile
            ]
        );
        assert_eq!(
            kinds(">"),
            vec![
                TokenKind::Punctuator(Punctuator::GreaterThan),
                TokenKind::EndOfFile
            ]
        );
        assert_eq!(
            kinds("->"),
            vec![
                TokenKind::Punctuator(Punctuator::Arrow),
                TokenKind::EndOfFile
            ]
        );
    }

    #[test]
    fn slash_is_division_when_not_a_comment() {
        assert_eq!(
            kinds("/"),
            vec![
                TokenKind::Punctuator(Punctuator::Slash),
                TokenKind::EndOfFile
            ]
        );
        assert_eq!(
            kinds("/="),
            vec![
                TokenKind::Punctuator(Punctuator::SlashEquals),
                TokenKind::EndOfFile
            ]
        );
    }

    #[test]
    fn a_small_declaration_tokenizes() {
        let significant: Vec<TokenKind> = tokenize("class Foo { }")
            .tokens
            .into_iter()
            .map(|token| token.kind)
            .filter(|kind| !kind.is_trivia() && *kind != TokenKind::EndOfFile)
            .collect();
        assert_eq!(
            significant,
            vec![
                TokenKind::Keyword(Keyword::Class),
                ident("Foo"),
                TokenKind::Punctuator(Punctuator::OpenBrace),
                TokenKind::Punctuator(Punctuator::CloseBrace),
            ]
        );
    }

    #[test]
    fn token_spans_cover_the_source_without_gaps() {
        let source = "public class A{ }\n// note\nstatic";
        let tokens = tokenize(source).tokens;
        let mut rebuilt = String::new();
        for token in &tokens {
            if token.kind == TokenKind::EndOfFile {
                continue;
            }
            rebuilt.push_str(token.span.slice(source));
        }
        assert_eq!(rebuilt, source);
    }

    #[test]
    fn a_hash_surfaces_as_unknown_without_a_diagnostic() {
        let result = tokenize("#");
        assert_eq!(result.tokens[0].kind, TokenKind::Unknown);
        assert!(result.diagnostics.is_empty());
    }

    fn character(value: u16) -> TokenKind {
        TokenKind::CharacterLiteral(value)
    }

    fn string(value: &str) -> TokenKind {
        TokenKind::StringLiteral(value.encode_utf16().collect())
    }

    #[test]
    fn plain_character_literals() {
        assert_eq!(
            kinds("'a'"),
            vec![character(u16::from(b'a')), TokenKind::EndOfFile]
        );
        assert_eq!(
            kinds("' '"),
            vec![character(u16::from(b' ')), TokenKind::EndOfFile]
        );
    }

    #[test]
    fn character_simple_escapes() {
        assert_eq!(
            kinds("'\\n'"),
            vec![character(0x000A), TokenKind::EndOfFile]
        );
        assert_eq!(
            kinds("'\\0'"),
            vec![character(0x0000), TokenKind::EndOfFile]
        );
        assert_eq!(
            kinds("'\\\\'"),
            vec![character(0x005C), TokenKind::EndOfFile]
        );
        assert_eq!(
            kinds("'\\''"),
            vec![character(0x0027), TokenKind::EndOfFile]
        );
    }

    #[test]
    fn character_hex_and_unicode_escapes() {
        assert_eq!(
            kinds("'\\x41'"),
            vec![character(0x0041), TokenKind::EndOfFile]
        );
        assert_eq!(
            kinds("'\\x0041'"),
            vec![character(0x0041), TokenKind::EndOfFile]
        );
        assert_eq!(
            kinds("'\\u00e9'"),
            vec![character(0x00E9), TokenKind::EndOfFile]
        );
        assert_eq!(
            kinds("'\\uD800'"),
            vec![character(0xD800), TokenKind::EndOfFile]
        );
    }

    #[test]
    fn empty_character_literal_reports_cs1011() {
        let result = tokenize("''");
        assert_eq!(result.tokens[0].kind, character(0));
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(result.diagnostics[0].code(), 1011);
    }

    #[test]
    fn overfull_character_literal_reports_cs1012() {
        let result = tokenize("'ab'");
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(result.diagnostics[0].code(), 1012);

        let astral = tokenize("'\\U00010000'");
        assert_eq!(astral.diagnostics.len(), 1);
        assert_eq!(astral.diagnostics[0].code(), 1012);
    }

    #[test]
    fn unrecognized_escape_reports_cs1009() {
        for source in ["'\\q'", "\"\\q\"", "\"\\x\"", "\"\\u12\"", "'\\U00110000'"] {
            let result = tokenize(source);
            assert_eq!(result.diagnostics.len(), 1, "source {source:?}");
            assert_eq!(result.diagnostics[0].code(), 1009, "source {source:?}");
        }
    }

    #[test]
    fn newline_or_eof_in_a_constant_reports_cs1010() {
        let with_newline = tokenize("'a\n");
        assert_eq!(with_newline.diagnostics.len(), 1);
        assert_eq!(with_newline.diagnostics[0].code(), 1010);
        assert_eq!(with_newline.tokens[1].kind, TokenKind::NewLine);

        for source in ["'a", "\"a", "\""] {
            let result = tokenize(source);
            assert_eq!(result.diagnostics[0].code(), 1010, "source {source:?}");
        }
    }

    #[test]
    fn plain_and_escaped_string_literals() {
        assert_eq!(
            kinds("\"hello\""),
            vec![string("hello"), TokenKind::EndOfFile]
        );
        assert_eq!(kinds("\"\""), vec![string(""), TokenKind::EndOfFile]);
        assert_eq!(
            kinds("\"a\\tb\""),
            vec![string("a\tb"), TokenKind::EndOfFile]
        );
        assert_eq!(
            kinds("\"\\U00010000\""),
            vec![string("\u{10000}"), TokenKind::EndOfFile]
        );
    }

    #[test]
    fn verbatim_strings_take_their_contents_literally() {
        assert_eq!(
            kinds("@\"a\\tb\""),
            vec![string("a\\tb"), TokenKind::EndOfFile]
        );
        assert_eq!(
            kinds("@\"a\"\"b\""),
            vec![string("a\"b"), TokenKind::EndOfFile]
        );
        assert_eq!(
            kinds("@\"a\nb\""),
            vec![string("a\nb"), TokenKind::EndOfFile]
        );
    }

    #[test]
    fn unterminated_verbatim_string_reports_cs1039() {
        let result = tokenize("@\"abc");
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(result.diagnostics[0].code(), 1039);
    }

    #[test]
    fn truly_unexpected_characters_report_cs1056() {
        let result = tokenize("$");
        assert_eq!(result.tokens[0].kind, TokenKind::Unknown);
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(result.diagnostics[0].code(), 1056);
    }

    #[test]
    fn trailing_control_z_is_dropped() {
        assert_eq!(
            kinds("class\u{1A}"),
            vec![TokenKind::Keyword(Keyword::Class), TokenKind::EndOfFile]
        );
    }
}
