//! The scanner: one token at a time, at the goal symbol the parser asks for.

use crate::{String, ToString};
use crate::source::{is_line_terminator, is_whitespace, Cursor, Span};
use crate::string_value::JsString;
use crate::token::{Goal, Punctuator, TemplateKind, TemplatePart, Token, TokenKind};
use crate::unicode;

/// A lexical error, with the span that caused it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LexError {
    pub kind: LexErrorKind,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LexErrorKind {
    UnexpectedCharacter(char),
    UnterminatedComment,
    UnterminatedString,
    UnterminatedTemplate,
    UnterminatedRegExp,
    /// A line terminator inside a string literal that is not a line continuation.
    LineTerminatorInString,
    InvalidEscape(&'static str),
    InvalidNumber(&'static str),
    /// A legacy octal literal (`0123`) or octal escape (`\07`).
    ///
    /// Refused rather than supported: these live in Annex B, which is web-browser surface, and the
    /// profile excludes Annex B. Refusing is loud; accepting them would make `010` evaluate to
    /// 8 on a tier that claims not to implement the annex.
    LegacyOctal,
    /// A code point the identifier tables cannot classify. See [`crate::unicode`].
    UnsupportedIdentifierCharacter(char, &'static str),
    /// A `BigInt` literal, recognized and refused.
    BigIntNotInProfile,
}

/// Scans one token at a time.
#[derive(Debug, Clone, Copy)]
pub struct Lexer<'a> {
    cursor: Cursor<'a>,
}

impl<'a> Lexer<'a> {
    #[must_use]
    pub fn new(source: &'a str) -> Self {
        Self { cursor: Cursor::new(source) }
    }

    #[must_use]
    pub fn offset(&self) -> usize {
        self.cursor.offset()
    }

    /// Rewinds to a previously observed offset, for the parser's backtracking points.
    pub fn reset_to(&mut self, offset: usize) {
        self.cursor.reset_to(offset);
    }

    /// Scans the next token, interpreting `/` and `}` according to `goal`.
    pub fn next_token(&mut self, goal: Goal) -> Result<Token, LexError> {
        let saw_line_terminator = self.skip_trivia()?;
        let start = self.cursor.offset();

        let Some(ch) = self.cursor.peek() else {
            return Ok(self.finish(TokenKind::EndOfFile, start, saw_line_terminator, false));
        };

        if ch == '}' && goal == Goal::TemplateTail {
            self.cursor.next();
            let part = self.scan_template_body(start, TemplateKind::Middle)?;
            return Ok(self.finish(TokenKind::Template(part), start, saw_line_terminator, false));
        }

        if ch == '/' && goal == Goal::RegExp {
            let (body, flags) = self.scan_regexp(start)?;
            return Ok(self.finish(
                TokenKind::RegExp { body, flags },
                start,
                saw_line_terminator,
                false,
            ));
        }

        match ch {
            '"' | '\'' => {
                let value = self.scan_string(start)?;
                Ok(self.finish(TokenKind::String(value), start, saw_line_terminator, false))
            }
            '`' => {
                self.cursor.next();
                let part = self.scan_template_body(start, TemplateKind::Head)?;
                Ok(self.finish(TokenKind::Template(part), start, saw_line_terminator, false))
            }
            '0'..='9' => {
                let kind = self.scan_number(start)?;
                Ok(self.finish(kind, start, saw_line_terminator, false))
            }
            '.' if self.cursor.peek_second().is_some_and(|c| c.is_ascii_digit()) => {
                let kind = self.scan_number(start)?;
                Ok(self.finish(kind, start, saw_line_terminator, false))
            }
            '#' => {
                self.cursor.next();
                let (name, had_escape) = self.scan_identifier_name(start)?;
                Ok(self.finish(TokenKind::PrivateName(name), start, saw_line_terminator, had_escape))
            }
            _ if self.starts_identifier(ch)? => {
                let (name, had_escape) = self.scan_identifier_name(start)?;
                Ok(self.finish(TokenKind::Identifier(name), start, saw_line_terminator, had_escape))
            }
            _ => {
                let punctuator = self.scan_punctuator(start)?;
                Ok(self.finish(
                    TokenKind::Punctuator(punctuator),
                    start,
                    saw_line_terminator,
                    false,
                ))
            }
        }
    }

    /// Resumes template text after a substitution's `}`, producing the Middle or Tail chunk.
    ///
    /// Exposed separately from [`Self::next_token`] because the parser reaches it by consuming a `}`
    /// it already has in hand in some paths.
    pub fn continue_template(&mut self) -> Result<Token, LexError> {
        let start = self.cursor.offset();
        let part = self.scan_template_body(start, TemplateKind::Middle)?;
        Ok(self.finish(TokenKind::Template(part), start, false, false))
    }

    fn finish(
        &self,
        kind: TokenKind,
        start: usize,
        preceded_by_line_terminator: bool,
        had_escape: bool,
    ) -> Token {
        Token {
            kind,
            span: Span::new(start, self.cursor.offset()),
            preceded_by_line_terminator,
            had_escape,
        }
    }

    /// Consumes whitespace and comments, reporting whether any line terminator was crossed.
    ///
    /// **A multi-line block comment counts as a line terminator for automatic semicolon
    /// insertion**, even though it is one token's worth of trivia: `a = 1 /*\n*/ b` inserts a
    /// semicolon. A scanner that only watches for `\n` between tokens misses it.
    fn skip_trivia(&mut self) -> Result<bool, LexError> {
        let mut saw_line_terminator = false;
        loop {
            let Some(ch) = self.cursor.peek() else { return Ok(saw_line_terminator) };
            if is_line_terminator(ch) {
                self.cursor.eat_line_terminator_sequence();
                saw_line_terminator = true;
                continue;
            }
            if is_whitespace(ch) {
                self.cursor.next();
                continue;
            }
            if ch == '/' {
                match self.cursor.peek_second() {
                    Some('/') => {
                        self.cursor.next();
                        self.cursor.next();
                        while let Some(c) = self.cursor.peek() {
                            if is_line_terminator(c) {
                                break;
                            }
                            self.cursor.next();
                        }
                        continue;
                    }
                    Some('*') => {
                        let start = self.cursor.offset();
                        self.cursor.next();
                        self.cursor.next();
                        loop {
                            let Some(c) = self.cursor.next() else {
                                return Err(LexError {
                                    kind: LexErrorKind::UnterminatedComment,
                                    span: Span::new(start, self.cursor.offset()),
                                });
                            };
                            if is_line_terminator(c) {
                                saw_line_terminator = true;
                            }
                            if c == '*' && self.cursor.peek() == Some('/') {
                                self.cursor.next();
                                break;
                            }
                        }
                        continue;
                    }
                    _ => return Ok(saw_line_terminator),
                }
            }
            return Ok(saw_line_terminator);
        }
    }

    fn starts_identifier(&self, ch: char) -> Result<bool, LexError> {
        if ch == '\\' {
            return Ok(true);
        }
        match unicode::is_id_start(ch) {
            Ok(answer) => Ok(answer),
            Err(unsupported) => Err(LexError {
                kind: LexErrorKind::UnsupportedIdentifierCharacter(
                    unsupported.code_point,
                    unsupported.reason,
                ),
                span: Span::new(self.cursor.offset(), self.identifier_run_end()),
            }),
        }
    }

    /// Where the identifier-shaped run at the cursor ends.
    ///
    /// # WARNING: A REFUSAL MUST COVER ITS WHOLE CONSTRUCT, EVEN WHEN IT CANNOT UNDERSTAND IT
    ///
    /// A non-ASCII identifier refusal that spans the ONE code point it cannot classify lets
    /// recovery resume on the second character of the same identifier and refuse again, and again
    /// -- one published absence arriving as a stream of them, and a report grouped by kind showing
    /// a program riddled with lexical errors instead of one unsupported name.
    ///
    /// Identifier-shaped is deliberately generous here: anything that is not whitespace, a line
    /// terminator, or ASCII punctuation. Which non-ASCII code points continue an identifier is
    /// exactly what this engine cannot say -- that is the whole gap -- so the run is bounded by
    /// what certainly does NOT.
    fn identifier_run_end(&self) -> usize {
        let mut probe = self.cursor;
        while let Some(ch) = probe.peek() {
            let ends_it = is_whitespace(ch)
                || is_line_terminator(ch)
                || (ch.is_ascii() && !ch.is_ascii_alphanumeric() && ch != '$' && ch != '_');
            if ends_it {
                break;
            }
            probe.next();
        }
        probe.offset()
    }

    /// Scans an `IdentifierName`, returning its value and whether any escape was used.
    fn scan_identifier_name(&mut self, start: usize) -> Result<(String, bool), LexError> {
        let mut name = String::new();
        let mut had_escape = false;
        let mut first = true;
        loop {
            let Some(ch) = self.cursor.peek() else { break };
            if ch == '\\' {
                let escape_start = self.cursor.offset();
                self.cursor.next();
                if !self.cursor.eat('u') {
                    return Err(LexError {
                        kind: LexErrorKind::InvalidEscape("only \\u escapes may appear in an identifier"),
                        span: Span::new(escape_start, self.cursor.offset()),
                    });
                }
                let decoded = self.scan_unicode_escape_value(escape_start)?;
                let ok = if first {
                    unicode::is_id_start(decoded)
                } else {
                    unicode::is_id_continue(decoded)
                };
                match ok {
                    Ok(true) => {}
                    Ok(false) => {
                        return Err(LexError {
                            kind: LexErrorKind::InvalidEscape(
                                "a \\u escape in an identifier must denote an identifier character",
                            ),
                            span: Span::new(escape_start, self.cursor.offset()),
                        })
                    }
                    Err(unsupported) => {
                        return Err(LexError {
                            kind: LexErrorKind::UnsupportedIdentifierCharacter(
                                unsupported.code_point,
                                unsupported.reason,
                            ),
                            span: Span::new(escape_start, self.cursor.offset()),
                        })
                    }
                }
                name.push(decoded);
                had_escape = true;
                first = false;
                continue;
            }
            let allowed = if first {
                unicode::is_id_start(ch)
            } else {
                unicode::is_id_continue(ch)
            };
            match allowed {
                Ok(true) => {
                    name.push(ch);
                    self.cursor.next();
                    first = false;
                }
                Ok(false) => break,
                Err(unsupported) => {
                    return Err(LexError {
                        kind: LexErrorKind::UnsupportedIdentifierCharacter(
                            unsupported.code_point,
                            unsupported.reason,
                        ),
                        span: Span::new(start, self.identifier_run_end()),
                    })
                }
            }
        }
        if name.is_empty() {
            return Err(LexError {
                kind: LexErrorKind::UnexpectedCharacter(self.cursor.peek().unwrap_or('\0')),
                span: Span::new(start, self.cursor.offset()),
            });
        }
        Ok((name, had_escape))
    }

    /// Reads the value of a `\u` escape, after the `u`. Handles both `\uXXXX` and `\u{...}`.
    fn scan_unicode_escape_value(&mut self, start: usize) -> Result<char, LexError> {
        let invalid = |cursor: &Cursor<'_>, message: &'static str| LexError {
            kind: LexErrorKind::InvalidEscape(message),
            span: Span::new(start, cursor.offset()),
        };
        if self.cursor.eat('{') {
            let mut value: u32 = 0;
            let mut digits = 0;
            while let Some(ch) = self.cursor.peek() {
                if ch == '}' {
                    break;
                }
                let Some(digit) = ch.to_digit(16) else {
                    return Err(invalid(&self.cursor, "a \\u{...} escape takes hexadecimal digits"));
                };
                self.cursor.next();
                digits += 1;
                value = value.saturating_mul(16).saturating_add(digit);
                if value > 0x10_FFFF {
                    return Err(invalid(&self.cursor, "a code point escape may not exceed 10FFFF"));
                }
            }
            if digits == 0 || !self.cursor.eat('}') {
                return Err(invalid(&self.cursor, "an unterminated \\u{...} escape"));
            }
            return char::from_u32(value)
                .ok_or_else(|| invalid(&self.cursor, "not a valid code point"));
        }
        let mut value: u32 = 0;
        for _ in 0..4 {
            let Some(digit) = self.cursor.peek().and_then(|c| c.to_digit(16)) else {
                return Err(invalid(&self.cursor, "a \\u escape takes four hexadecimal digits"));
            };
            self.cursor.next();
            value = value * 16 + digit;
        }
        char::from_u32(value).ok_or_else(|| invalid(&self.cursor, "a lone surrogate"))
    }

    /// Reads a `\u` escape **inside a string or template**, appending CODE UNITS.
    ///
    /// # THE DIFFERENCE FROM THE IDENTIFIER READER IS SEMANTIC, NOT STYLISTIC
    ///
    /// An ECMAScript String is a sequence of UTF-16 code units with **no well-formedness
    /// requirement**, so `'\uD834'` is a one-unit string holding an unpaired surrogate and
    /// `'\uD834\uDF06'.length` is 2. Decoding each escape to a Rust `char` cannot express either:
    /// the first is rejected outright and the second would collapse to one character.
    ///
    /// This was measured rather than reasoned about. The identifier reader was used for strings in
    /// the first version, and pointing the scanner at the pinned corpus that same day failed **36
    /// positive conformance files** on `String/length.js`, `codePointAt/*` and neighbours -- every
    /// one a legal program. **Not a missing feature; a different language.**
    fn scan_unicode_escape_units(
        &mut self,
        start: usize,
        value: &mut JsString,
    ) -> Result<(), LexError> {
        let invalid = |cursor: &Cursor<'_>, message: &'static str| LexError {
            kind: LexErrorKind::InvalidEscape(message),
            span: Span::new(start, cursor.offset()),
        };
        if self.cursor.eat('{') {
            let mut code: u32 = 0;
            let mut digits = 0;
            while let Some(ch) = self.cursor.peek() {
                if ch == '}' {
                    break;
                }
                let Some(digit) = ch.to_digit(16) else {
                    return Err(invalid(&self.cursor, "a \\u{...} escape takes hexadecimal digits"));
                };
                self.cursor.next();
                digits += 1;
                code = code.saturating_mul(16).saturating_add(digit);
                if code > 0x10_FFFF {
                    return Err(invalid(&self.cursor, "a code point escape may not exceed 10FFFF"));
                }
            }
            if digits == 0 || !self.cursor.eat('}') {
                return Err(invalid(&self.cursor, "an unterminated \\u{...} escape"));
            }
            push_code_point(value, code);
            return Ok(());
        }
        let mut code: u32 = 0;
        for _ in 0..4 {
            let Some(digit) = self.cursor.peek().and_then(|c| c.to_digit(16)) else {
                return Err(invalid(&self.cursor, "a \\u escape takes four hexadecimal digits"));
            };
            self.cursor.next();
            code = code * 16 + digit;
        }
        value.push_code_unit(code as u16);
        Ok(())
    }

    fn scan_punctuator(&mut self, start: usize) -> Result<Punctuator, LexError> {
        use Punctuator::*;
        let ch = self.cursor.next().expect("a character was peeked");
        let punctuator = match ch {
            '{' => OpenBrace,
            '}' => CloseBrace,
            '(' => OpenParen,
            ')' => CloseParen,
            '[' => OpenBracket,
            ']' => CloseBracket,
            ';' => Semicolon,
            ',' => Comma,
            ':' => Colon,
            '~' => Tilde,
            '.' => {
                if self.cursor.peek() == Some('.') && self.cursor.peek_second() == Some('.') {
                    self.cursor.next();
                    self.cursor.next();
                    Ellipsis
                } else {
                    Dot
                }
            }
            '?' => {
                if self.cursor.peek() == Some('.')
                    && !self.cursor.peek_second().is_some_and(|c| c.is_ascii_digit())
                {
                    self.cursor.next();
                    QuestionDot
                } else if self.cursor.eat('?') {
                    if self.cursor.eat('=') { QuestionQuestionEquals } else { QuestionQuestion }
                } else {
                    Question
                }
            }
            '<' => {
                if self.cursor.eat('<') {
                    if self.cursor.eat('=') { LessThanLessThanEquals } else { LessThanLessThan }
                } else if self.cursor.eat('=') {
                    LessThanEquals
                } else {
                    LessThan
                }
            }
            '>' => {
                if self.cursor.eat('>') {
                    if self.cursor.eat('>') {
                        if self.cursor.eat('=') {
                            GreaterThanGreaterThanGreaterThanEquals
                        } else {
                            GreaterThanGreaterThanGreaterThan
                        }
                    } else if self.cursor.eat('=') {
                        GreaterThanGreaterThanEquals
                    } else {
                        GreaterThanGreaterThan
                    }
                } else if self.cursor.eat('=') {
                    GreaterThanEquals
                } else {
                    GreaterThan
                }
            }
            '=' => {
                if self.cursor.eat('=') {
                    if self.cursor.eat('=') { EqualsEqualsEquals } else { EqualsEquals }
                } else if self.cursor.eat('>') {
                    Arrow
                } else {
                    Equals
                }
            }
            '!' => {
                if self.cursor.eat('=') {
                    if self.cursor.eat('=') { ExclamationEqualsEquals } else { ExclamationEquals }
                } else {
                    Exclamation
                }
            }
            '+' => {
                if self.cursor.eat('+') { PlusPlus }
                else if self.cursor.eat('=') { PlusEquals }
                else { Plus }
            }
            '-' => {
                if self.cursor.eat('-') { MinusMinus }
                else if self.cursor.eat('=') { MinusEquals }
                else { Minus }
            }
            '*' => {
                if self.cursor.eat('*') {
                    if self.cursor.eat('=') { StarStarEquals } else { StarStar }
                } else if self.cursor.eat('=') {
                    StarEquals
                } else {
                    Star
                }
            }
            '%' => if self.cursor.eat('=') { PercentEquals } else { Percent },
            '&' => {
                if self.cursor.eat('&') {
                    if self.cursor.eat('=') { AmpersandAmpersandEquals } else { AmpersandAmpersand }
                } else if self.cursor.eat('=') {
                    AmpersandEquals
                } else {
                    Ampersand
                }
            }
            '|' => {
                if self.cursor.eat('|') {
                    if self.cursor.eat('=') { BarBarEquals } else { BarBar }
                } else if self.cursor.eat('=') {
                    BarEquals
                } else {
                    Bar
                }
            }
            '^' => if self.cursor.eat('=') { CaretEquals } else { Caret },
            '/' => if self.cursor.eat('=') { SlashEquals } else { Slash },
            other => {
                return Err(LexError {
                    kind: LexErrorKind::UnexpectedCharacter(other),
                    span: Span::new(start, self.cursor.offset()),
                })
            }
        };
        Ok(punctuator)
    }

    /// Scans a string literal.
    ///
    /// # WARNING: A REFUSED ESCAPE STILL SCANS TO THE CLOSING QUOTE
    ///
    /// An Annex B escape is excluded surface, not a malformed program -- but returning at the escape
    /// leaves the cursor INSIDE the literal, so recovery resumes mid-string and reports a second,
    /// invented complaint about the remaining characters. The run then carries a mixed bag of kinds
    /// and stops classifying as a published absence.
    ///
    /// So the refusal is remembered and the scan continues to the closing quote, and the error that
    /// comes back spans the WHOLE literal. **A refusal must swallow its construct** -- the same rule
    /// the class and generator refusals needed, one layer further down.
    fn scan_string(&mut self, start: usize) -> Result<JsString, LexError> {
        let quote = self.cursor.next().expect("a quote was peeked");
        let mut value = JsString::new();
        let mut refused: Option<LexErrorKind> = None;
        loop {
            let Some(ch) = self.cursor.peek() else {
                return Err(LexError {
                    kind: LexErrorKind::UnterminatedString,
                    span: Span::new(start, self.cursor.offset()),
                });
            };
            if ch == quote {
                self.cursor.next();
                return match refused {
                    Some(kind) => {
                        Err(LexError { kind, span: Span::new(start, self.cursor.offset()) })
                    }
                    None => Ok(value),
                };
            }
            if ch == '\n' || ch == '\r' {
                return Err(LexError {
                    kind: LexErrorKind::LineTerminatorInString,
                    span: Span::new(start, self.cursor.offset()),
                });
            }
            if ch == '\\' {
                self.cursor.next();
                match self.scan_string_escape(start, &mut value) {
                    Ok(()) => {}
                    Err(error) if error.kind == LexErrorKind::LegacyOctal => {
                        refused.get_or_insert(error.kind);
                    }
                    Err(error) => return Err(error),
                }
                continue;
            }
            value.push_char(ch);
            self.cursor.next();
        }
    }

    /// Handles one escape sequence inside a string literal, appending to `value`.
    fn scan_string_escape(&mut self, start: usize, value: &mut JsString) -> Result<(), LexError> {
        let escape_start = self.cursor.offset();
        if self.cursor.eat_line_terminator_sequence() {
            return Ok(());
        }
        let Some(ch) = self.cursor.next() else {
            return Err(LexError {
                kind: LexErrorKind::UnterminatedString,
                span: Span::new(start, self.cursor.offset()),
            });
        };
        match ch {
            'b' => value.push_char('\u{0008}'),
            'f' => value.push_char('\u{000C}'),
            'n' => value.push_char('\n'),
            'r' => value.push_char('\r'),
            't' => value.push_char('\t'),
            'v' => value.push_char('\u{000B}'),
            '0' if !self.cursor.peek().is_some_and(|c| c.is_ascii_digit()) => value.push_char('\0'),
            '0'..='7' => {
                return Err(LexError {
                    kind: LexErrorKind::LegacyOctal,
                    span: Span::new(escape_start, self.cursor.offset()),
                })
            }
            '8' | '9' => {
                return Err(LexError {
                    kind: LexErrorKind::LegacyOctal,
                    span: Span::new(escape_start, self.cursor.offset()),
                })
            }
            'x' => {
                let mut code = 0u32;
                for _ in 0..2 {
                    let Some(digit) = self.cursor.peek().and_then(|c| c.to_digit(16)) else {
                        return Err(LexError {
                            kind: LexErrorKind::InvalidEscape("a \\x escape takes two hex digits"),
                            span: Span::new(escape_start, self.cursor.offset()),
                        });
                    };
                    self.cursor.next();
                    code = code * 16 + digit;
                }
                value.push_char(char::from_u32(code).expect("two hex digits are always a valid char"));
            }
            'u' => {
                self.scan_unicode_escape_units(escape_start, value)?;
            }
            other => value.push_char(other),
        }
        Ok(())
    }

    /// Scans template text from the current position to the next `${` or backtick.
    fn scan_template_body(
        &mut self,
        start: usize,
        opening_kind: TemplateKind,
    ) -> Result<TemplatePart, LexError> {
        let mut raw = String::new();
        let mut cooked = Some(JsString::new());
        loop {
            let Some(ch) = self.cursor.peek() else {
                return Err(LexError {
                    kind: LexErrorKind::UnterminatedTemplate,
                    span: Span::new(start, self.cursor.offset()),
                });
            };
            if ch == '`' {
                self.cursor.next();
                let kind = if opening_kind == TemplateKind::Head {
                    TemplateKind::NoSubstitution
                } else {
                    TemplateKind::Tail
                };
                return Ok(TemplatePart { kind, cooked, raw });
            }
            if ch == '$' && self.cursor.peek_second() == Some('{') {
                self.cursor.next();
                self.cursor.next();
                return Ok(TemplatePart { kind: opening_kind, cooked, raw });
            }
            if ch == '\r' {
                self.cursor.eat_line_terminator_sequence();
                raw.push('\n');
                if let Some(text) = &mut cooked {
                    text.push_char('\n');
                }
                continue;
            }
            if ch == '\\' {
                let escape_start = self.cursor.offset();
                self.cursor.next();
                let mut scratch = JsString::new();
                match self.scan_string_escape(start, &mut scratch) {
                    Ok(()) => {
                        if let Some(text) = &mut cooked {
                            text.extend_from(&scratch);
                        }
                    }
                    Err(_) => cooked = None,
                }
                let end = self.cursor.offset();
                let text = &self.cursor.source()[escape_start..end];
                if text.starts_with("\\\r") {
                    raw.push_str("\\\n");
                } else {
                    raw.push_str(text);
                }
                continue;
            }
            self.cursor.next();
            raw.push(ch);
            if let Some(text) = &mut cooked {
                text.push_char(ch);
            }
        }
    }

    /// Scans a RegExp literal's body and flags as text.
    ///
    /// The pattern grammar is a separate subject and is deliberately not parsed here: what the
    /// lexer needs is where the literal ENDS, which takes only the class and escape rules.
    /// Getting that wrong desynchronizes every token after it.
    fn scan_regexp(&mut self, start: usize) -> Result<(String, String), LexError> {
        self.cursor.next();
        let body_start = self.cursor.offset();
        let mut in_class = false;
        loop {
            let Some(ch) = self.cursor.peek() else {
                return Err(LexError {
                    kind: LexErrorKind::UnterminatedRegExp,
                    span: Span::new(start, self.cursor.offset()),
                });
            };
            if is_line_terminator(ch) {
                return Err(LexError {
                    kind: LexErrorKind::UnterminatedRegExp,
                    span: Span::new(start, self.cursor.offset()),
                });
            }
            if ch == '\\' {
                self.cursor.next();
                if self.cursor.peek().is_some_and(is_line_terminator) || self.cursor.at_end() {
                    return Err(LexError {
                        kind: LexErrorKind::UnterminatedRegExp,
                        span: Span::new(start, self.cursor.offset()),
                    });
                }
                self.cursor.next();
                continue;
            }
            if ch == '[' {
                in_class = true;
            } else if ch == ']' {
                in_class = false;
            } else if ch == '/' && !in_class {
                break;
            }
            self.cursor.next();
        }
        let body = self.cursor.source()[body_start..self.cursor.offset()].to_string();
        self.cursor.next();
        let flags_start = self.cursor.offset();
        while let Some(ch) = self.cursor.peek() {
            match unicode::is_id_continue(ch) {
                Ok(true) => {
                    self.cursor.next();
                }
                Ok(false) => break,
                Err(_) => break,
            }
        }
        let flags = self.cursor.source()[flags_start..self.cursor.offset()].to_string();
        Ok((body, flags))
    }

    fn scan_number(&mut self, start: usize) -> Result<TokenKind, LexError> {
        let invalid = |cursor: &Cursor<'_>, message: &'static str| LexError {
            kind: LexErrorKind::InvalidNumber(message),
            span: Span::new(start, cursor.offset()),
        };

        if self.cursor.peek() == Some('0') {
            match self.cursor.peek_second() {
                Some('b' | 'B') => return self.scan_radix(start, 2, "binary"),
                Some('o' | 'O') => return self.scan_radix(start, 8, "octal"),
                Some('x' | 'X') => return self.scan_radix(start, 16, "hexadecimal"),
                Some(c) if c.is_ascii_digit() => {
                    self.cursor.next();
                    while self.cursor.peek().is_some_and(|c| c.is_ascii_digit()) {
                        self.cursor.next();
                    }
                    return Err(LexError {
                        kind: LexErrorKind::LegacyOctal,
                        span: Span::new(start, self.cursor.offset()),
                    });
                }
                Some('_') => {
                    self.cursor.next();
                    self.cursor.next();
                    return Err(invalid(
                        &self.cursor,
                        "a numeric separator may not follow a leading zero",
                    ));
                }
                _ => {}
            }
        }

        let mut text = String::new();
        self.scan_decimal_digits(&mut text, &invalid)?;
        if self.cursor.peek() == Some('.') {
            self.cursor.next();
            text.push('.');
            if self.cursor.peek().is_some_and(|c| c.is_ascii_digit()) {
                self.scan_decimal_digits(&mut text, &invalid)?;
            } else if self.cursor.peek() == Some('_') {
                return Err(invalid(&self.cursor, "a separator may not follow a decimal point"));
            }
        }
        if matches!(self.cursor.peek(), Some('e' | 'E')) {
            self.cursor.next();
            text.push('e');
            if matches!(self.cursor.peek(), Some('+' | '-')) {
                text.push(self.cursor.next().expect("a sign was peeked"));
            }
            if !self.cursor.peek().is_some_and(|c| c.is_ascii_digit()) {
                return Err(invalid(&self.cursor, "an exponent needs at least one digit"));
            }
            self.scan_decimal_digits(&mut text, &invalid)?;
        }

        if self.cursor.peek() == Some('n') {
            self.cursor.next();
            return Err(LexError {
                kind: LexErrorKind::BigIntNotInProfile,
                span: Span::new(start, self.cursor.offset()),
            });
        }
        self.reject_identifier_immediately_after(start)?;

        let value = text.parse::<f64>().map_err(|_| invalid(&self.cursor, "not a number"))?;
        Ok(TokenKind::Number(value))
    }

    /// Digits with numeric separators. `1_000` is legal; a separator that is leading, trailing or
    /// doubled is not, and neither is one next to the decimal point.
    fn scan_decimal_digits(
        &mut self,
        text: &mut String,
        invalid: &impl Fn(&Cursor<'a>, &'static str) -> LexError,
    ) -> Result<(), LexError> {
        let mut last_was_separator = false;
        let mut any = false;
        loop {
            match self.cursor.peek() {
                Some('_') => {
                    if !any || last_was_separator {
                        return Err(invalid(
                            &self.cursor,
                            "a numeric separator must sit between two digits",
                        ));
                    }
                    self.cursor.next();
                    last_was_separator = true;
                }
                Some(c) if c.is_ascii_digit() => {
                    text.push(c);
                    self.cursor.next();
                    last_was_separator = false;
                    any = true;
                }
                _ => break,
            }
        }
        if last_was_separator {
            return Err(invalid(&self.cursor, "a numeric literal may not end with a separator"));
        }
        Ok(())
    }

    fn scan_radix(
        &mut self,
        start: usize,
        radix: u32,
        name: &'static str,
    ) -> Result<TokenKind, LexError> {
        self.cursor.next();
        self.cursor.next();
        let mut digits = String::new();
        let mut last_was_separator = false;
        loop {
            match self.cursor.peek() {
                Some('_') => {
                    if digits.is_empty() || last_was_separator {
                        return Err(LexError {
                            kind: LexErrorKind::InvalidNumber(
                                "a numeric separator must sit between two digits",
                            ),
                            span: Span::new(start, self.cursor.offset()),
                        });
                    }
                    self.cursor.next();
                    last_was_separator = true;
                }
                Some(c) if c.to_digit(radix).is_some() => {
                    digits.push(c);
                    self.cursor.next();
                    last_was_separator = false;
                }
                _ => break,
            }
        }
        if digits.is_empty() || last_was_separator {
            return Err(LexError {
                kind: LexErrorKind::InvalidNumber(match name {
                    "binary" => "a binary literal needs at least one digit",
                    "octal" => "an octal literal needs at least one digit",
                    _ => "a hexadecimal literal needs at least one digit",
                }),
                span: Span::new(start, self.cursor.offset()),
            });
        }
        if self.cursor.peek() == Some('n') {
            self.cursor.next();
            return Err(LexError {
                kind: LexErrorKind::BigIntNotInProfile,
                span: Span::new(start, self.cursor.offset()),
            });
        }
        self.reject_identifier_immediately_after(start)?;
        let mut value = 0.0f64;
        for ch in digits.chars() {
            value = value * f64::from(radix) + f64::from(ch.to_digit(radix).expect("checked"));
        }
        Ok(TokenKind::Number(value))
    }

    /// `3in` is a syntax error, not the number 3 followed by `in`.
    ///
    /// The grammar forbids an `IdentifierStart` immediately after a numeric literal. Without this
    /// the token stream silently splits into two valid tokens and a malformed program parses.
    fn reject_identifier_immediately_after(&mut self, start: usize) -> Result<(), LexError> {
        let Some(ch) = self.cursor.peek() else { return Ok(()) };
        let offending = match unicode::is_id_start(ch) {
            Ok(answer) => answer,
            Err(_) => true,
        };
        if offending || ch.is_ascii_digit() {
            return Err(LexError {
                kind: LexErrorKind::InvalidNumber(
                    "an identifier may not follow a numeric literal directly",
                ),
                span: Span::new(start, self.cursor.offset() + ch.len_utf8()),
            });
        }
        Ok(())
    }
}

/// Appends a code point as one or two UTF-16 units.
///
/// A `\u{...}` escape names a code POINT, and the standard's UTF16Encode is what turns it into the
/// units a String holds -- so an astral escape contributes two, which is what makes
/// `'\u{1D306}'.length` equal 2. A value in the surrogate range is written verbatim rather than
/// rejected: `\u{D834}` is as legal as `\uD834` and means the same thing.
fn push_code_point(value: &mut JsString, code: u32) {
    if code > 0xFFFF {
        let adjusted = code - 0x1_0000;
        value.push_code_unit(0xD800 + (adjusted >> 10) as u16);
        value.push_code_unit(0xDC00 + (adjusted & 0x3FF) as u16);
    } else {
        value.push_code_unit(code as u16);
    }
}
