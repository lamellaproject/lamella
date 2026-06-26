//! The scanner: turning C# source text into a token stream.

use crate::diagnostic::{Diagnostic, DiagnosticKind};
use crate::span::Span;
use crate::token::{
    IntegerSuffix, Keyword, Punctuator, RealSuffix, Token, TokenKind, TypedRefKeyword,
};
use crate::version::{Feature, LanguageVersion};
use alloc::boxed::Box;
use alloc::collections::BTreeSet;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// The result of scanning a source file: the full token stream (ending in one
/// [`TokenKind::EndOfFile`]) and any diagnostics gathered along the way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tokenized {
    /// Every token in source order, trivia included, ending in `EndOfFile`.
    pub tokens: Vec<Token>,
    /// Lexical diagnostics, in source order.
    pub diagnostics: Vec<Diagnostic>,
    /// The preprocessor symbols defined by `#define` and not later `#undef`'d (9.5.3) -- the
    /// set a `[Conditional("X")]` call is checked against to decide inclusion (24.4.2).
    pub defined_symbols: BTreeSet<Box<str>>,
}

/// Scans `source` into a complete [`Tokenized`] stream.
///
/// How identifiers are compared (9.4.2). They differ only for a decomposed vs precomposed spelling
/// of one identifier: `None` keeps the raw code points, which csc does (it does NOT normalize, so
/// the two spellings are distinct -- this matches the differential oracle); `Nfc` folds to Unicode
/// Normalization Form C, which the ECMA-334 standard requires (the two spellings are then one
/// identifier). The compiler default is `None`; `Nfc` is the spec-strict knob.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Normalization {
    /// Raw code points -- the Roslyn/csc behaviour. The default.
    #[default]
    None,
    /// Unicode Normalization Form C, per ECMA-334 9.4.2.
    Nfc,
}

/// The lexer-level dialect knobs, gathered so they thread through the front end as one
/// value (and so a new knob is a field, not another parameter on every `*_with`).
///
/// [`Default`] is strict-ISO-meets-csc: identifiers are not normalized (matching the
/// Roslyn/csc oracle) and the csc typed-reference operators are off (they are not in
/// ECMA-334). The two knobs move independently of the oracle -- `normalization` toward
/// the spec, `typedref` toward csc -- because each closes a different gap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LexOptions {
    /// How identifiers are folded for comparison (9.4.2).
    pub normalization: Normalization,
    /// Whether the undocumented csc operators `__makeref`/`__refvalue`/`__reftype` are
    /// recognized as [`TypedRefKeyword`]s. Off in strict ISO-1 (they are not ECMA-334);
    /// on for csc parity, where they lower to `mkrefany`/`refanyval`/`refanytype`.
    pub typedref: bool,
    /// The language version being compiled. Defaults to [`LanguageVersion::CSharp1`] (the only
    /// implemented one). A post-1.0 operator (`=>`/`??`/`?.`/`?[`/`::`) under a version that
    /// predates it is rejected with a `Feature requires C# N` diagnostic instead of munching as
    /// the 1.0 tokens it would otherwise split into (so the error names the feature, not `=` `>`).
    pub version: LanguageVersion,
    /// Whether unmanaged native interop is enabled: `[DllImport]` P/Invoke (an `ImplMap`), and later
    /// explicit `[StructLayout]`/`[FieldOffset]` and `[MarshalAs]`. Off by default -- pure-managed
    /// code (and the NETMFv4_4 profile) does not need it, so a constrained target stays free of an
    /// unmanaged boundary it cannot honor; on for AOT mixed (managed + native) scenarios. When off,
    /// those attributes are rejected rather than emitted as inert metadata.
    pub native_interop: bool,
}

impl LexOptions {
    /// Options that fold identifiers per `normalization`, with every other knob default.
    #[must_use]
    pub fn with_normalization(normalization: Normalization) -> LexOptions {
        LexOptions {
            normalization,
            ..LexOptions::default()
        }
    }
}

/// The token stream is a gap-free cover of the source: concatenating the text of
/// every non-`EndOfFile` token reproduces the input, after the single trailing
/// Control-Z removal of 9.3.1. Identifiers are NOT normalized (matching csc); use
/// [`tokenize_with`] for the ECMA-334 9.4.2 NFC behaviour.
#[must_use]
pub fn tokenize(source: &str) -> Tokenized {
    tokenize_with(source, LexOptions::default())
}

/// Like [`tokenize`], but scans under `options` -- folding identifiers per its
/// [`Normalization`] (9.4.2) and recognizing the csc typed-reference operators when
/// [`LexOptions::typedref`] is set.
#[must_use]
pub fn tokenize_with(source: &str, options: LexOptions) -> Tokenized {
    let mut lexer = Lexer::new(source);
    lexer.options = options;
    let mut tokens = Vec::new();
    loop {
        let token = lexer.next_token();
        let reached_end = token.kind == TokenKind::EndOfFile;
        tokens.push(token);
        if reached_end {
            break;
        }
    }
    let defined_symbols = core::mem::take(&mut lexer.defined_symbols);
    Tokenized {
        tokens,
        defined_symbols,
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
    /// Whether only white space has been seen since the last line terminator, so
    /// the next `#` would begin its line. Pre-processing directives are valid
    /// only at the first non-white-space character of a line (9.5).
    line_start: bool,
    /// Whether a real token (9.4), as opposed to trivia or a directive, has been
    /// emitted yet. `#define` and `#undef` are valid only before it (9.5.3).
    seen_token: bool,
    /// The conditional compilation symbols currently defined (9.5.1), as built up
    /// by `#define` and torn down by `#undef` while scanning.
    defined_symbols: BTreeSet<Box<str>>,
    /// The stack of open `#if`/`#region` constructs, innermost last (9.5.4). Its
    /// top decides whether source is currently being included or skipped.
    conditionals: Vec<Conditional>,
    /// The dialect knobs: identifier folding (9.4.2) and whether the csc typed-reference
    /// operators are recognized. Defaults to the csc-matching, strict-ISO settings.
    options: LexOptions,
}

/// One open conditional construct: an `#if`/`#elif`/`#else`/`#endif` group or a
/// `#region`/`#endregion` pair (9.5.4, 9.5.6). A `#region` behaves lexically as
/// `#if true`, so both are tracked the same way.
#[derive(Debug)]
struct Conditional {
    /// True for a `#region`, false for an `#if` group. Decides whether `#endif`
    /// or `#endregion` closes it and which diagnostic an unterminated one gets.
    is_region: bool,
    /// Whether the enclosing context was including source. When false the whole
    /// construct is skipped, whatever its own conditions say.
    parent_active: bool,
    /// Whether a branch of this group has already been selected for inclusion, so
    /// no later `#elif`/`#else` branch may be.
    branch_taken: bool,
    /// Whether the branch now in effect is being included.
    including: bool,
    /// Whether an `#else` has been seen, after which `#elif`/`#else` is invalid.
    seen_else: bool,
}

/// A pre-processing directive name (9.5), the word right after the `#`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DirectiveKind {
    Define,
    Undef,
    If,
    Elif,
    Else,
    Endif,
    Line,
    Error,
    Warning,
    Region,
    EndRegion,
}

impl DirectiveKind {
    /// The directive named by `text`, if `text` is a directive name.
    fn from_text(text: &str) -> Option<DirectiveKind> {
        Some(match text {
            "define" => DirectiveKind::Define,
            "undef" => DirectiveKind::Undef,
            "if" => DirectiveKind::If,
            "elif" => DirectiveKind::Elif,
            "else" => DirectiveKind::Else,
            "endif" => DirectiveKind::Endif,
            "line" => DirectiveKind::Line,
            "error" => DirectiveKind::Error,
            "warning" => DirectiveKind::Warning,
            "region" => DirectiveKind::Region,
            "endregion" => DirectiveKind::EndRegion,
            _ => return None,
        })
    }
}

/// Records which diagnostics a single pre-processing expression has already
/// reported (9.5.2), so a malformed expression yields each at most once rather
/// than one per nesting level as the recursive descent unwinds.
struct PpExprErrors {
    /// Whether `CS1517` (invalid expression) has been reported for this line.
    invalid_reported: bool,
    /// Whether `CS1026` (missing `)`) has been reported for this line.
    close_paren_reported: bool,
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
            line_start: true,
            seen_token: false,
            defined_symbols: BTreeSet::new(),
            conditionals: Vec::new(),
            options: LexOptions::default(),
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
        let line_start = self.line_start;
        let Some(c) = self.peek() else {
            if let Some(top) = self.conditionals.last() {
                let kind = if top.is_region {
                    DiagnosticKind::EndRegionDirectiveExpected
                } else {
                    DiagnosticKind::EndIfDirectiveExpected
                };
                self.report(kind, start);
                self.conditionals.clear();
            }
            return Token::new(TokenKind::EndOfFile, Span::empty_at(start as u32));
        };

        let kind = if is_new_line(c) {
            self.scan_new_line()
        } else if is_whitespace(c) {
            self.scan_whitespace()
        } else if c == '#' && line_start {
            self.scan_directive(start)
        } else if c == '#' {
            self.bump();
            self.report(DiagnosticKind::DirectiveNotFirstOnLine, start);
            TokenKind::Unknown
        } else if !self.including() {
            self.scan_skipped_text()
        } else if c == '/' && matches!(self.peek_second(), Some('/' | '*')) {
            self.scan_comment(start)
        } else if is_identifier_start(c)
            || (c == '\\' && matches!(self.peek_second(), Some('u' | 'U')))
        {
            self.scan_identifier_or_keyword(start, false)
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
        } else if let Some(kind) = self.try_gate_post_1_0_operator(start) {
            kind
        } else if let Some(punctuator) = self.try_scan_operator() {
            TokenKind::Punctuator(punctuator)
        } else {
            self.bump();
            self.report(DiagnosticKind::UnexpectedCharacter { character: c }, start);
            TokenKind::Unknown
        };

        if matches!(kind, TokenKind::NewLine) {
            self.line_start = true;
        } else if !matches!(kind, TokenKind::Whitespace) {
            self.line_start = false;
        }
        if !kind.is_trivia() && !matches!(kind, TokenKind::EndOfFile) {
            self.seen_token = true;
        }

        Token::new(kind, Span::new(start as u32, self.position as u32))
    }

    /// Whether source at the current position is being included rather than
    /// skipped: true unless an open conditional's selected branch excludes it.
    fn including(&self) -> bool {
        self.conditionals.last().is_none_or(|c| c.including)
    }

    /// Consumes the rest of a line that conditional compilation is skipping
    /// (9.5.4), up to but not including the line terminator. Such text need not
    /// be lexically well formed, so it is taken as one opaque [`TokenKind::SkippedText`].
    fn scan_skipped_text(&mut self) -> TokenKind {
        self.consume_to_line_end();
        TokenKind::SkippedText
    }

    /// Scans one pre-processing directive line (9.5), beginning at the `#`, and
    /// applies its effect. `active` says whether the enclosing conditional
    /// section is being compiled. A directive must be lexically correct even in
    /// a skipped section (9.5.4), so malformed-directive diagnostics fire either
    /// way; only the *effects* -- defining symbols, evaluating which branch to
    /// include, raising `#error`/`#warning`, the first-token rule -- are gated on
    /// `active`. The structural stack of conditionals is always maintained, so
    /// nesting stays correct through skipped regions.
    fn scan_directive(&mut self, start: usize) -> TokenKind {
        let active = self.including();
        self.bump();
        self.skip_inline_whitespace();
        match DirectiveKind::from_text(self.read_directive_name()) {
            Some(DirectiveKind::Define) => self.scan_define_or_undef(start, active, true),
            Some(DirectiveKind::Undef) => self.scan_define_or_undef(start, active, false),
            Some(DirectiveKind::If) => self.scan_if(start, active),
            Some(DirectiveKind::Elif) => self.scan_elif(start),
            Some(DirectiveKind::Else) => self.scan_else(start),
            Some(DirectiveKind::Endif) => self.scan_endif(start),
            Some(DirectiveKind::Region) => self.scan_region(active),
            Some(DirectiveKind::EndRegion) => self.scan_endregion(start),
            Some(DirectiveKind::Error) => self.scan_error_or_warning(start, active, true),
            Some(DirectiveKind::Warning) => self.scan_error_or_warning(start, active, false),
            Some(DirectiveKind::Line) => self.scan_line(start),
            None => {
                self.report(DiagnosticKind::PreprocessorDirectiveExpected, start);
                self.consume_to_line_end();
            }
        }
        TokenKind::PreprocessingDirective
    }

    /// Processes a `#define` or `#undef` (9.5.3): the named symbol, which must be
    /// an identifier-or-keyword other than `true` or `false`, becomes defined or
    /// undefined for the rest of the file when this section is being compiled.
    fn scan_define_or_undef(&mut self, start: usize, active: bool, is_define: bool) {
        self.skip_inline_whitespace();
        let symbol = self.read_directive_name();
        if symbol.is_empty() || symbol == "true" || symbol == "false" {
            self.report(DiagnosticKind::IdentifierExpected, start);
            self.consume_to_line_end();
            return;
        }
        if active && self.seen_token {
            self.report(DiagnosticKind::SymbolAfterFirstToken, start);
        }
        if active {
            if is_define {
                self.defined_symbols.insert(symbol.into());
            } else {
                self.defined_symbols.remove(symbol);
            }
        }
        self.expect_directive_line_end(start);
    }

    /// Processes an `#if` (9.5.4): evaluates its pre-processing expression and
    /// opens a conditional whose first branch is included when the expression is
    /// true and the enclosing section is itself being compiled.
    fn scan_if(&mut self, start: usize, active: bool) {
        let condition = self.scan_pp_expression(start);
        self.expect_directive_line_end(start);
        let including = active && condition;
        self.conditionals.push(Conditional {
            is_region: false,
            parent_active: active,
            branch_taken: including,
            including,
            seen_else: false,
        });
    }

    /// Processes an `#elif` (9.5.4): selects its branch when no earlier branch of
    /// the group was taken and its expression is true. The expression is always
    /// parsed, so a malformed one is reported even in a skipped group.
    fn scan_elif(&mut self, start: usize) {
        let condition = self.scan_pp_expression(start);
        self.expect_directive_line_end(start);
        let Some(top) = self.conditionals.last_mut() else {
            self.report(DiagnosticKind::UnexpectedDirective, start);
            return;
        };
        if top.is_region {
            self.report(DiagnosticKind::EndRegionDirectiveExpected, start);
            return;
        }
        if top.seen_else {
            self.report(DiagnosticKind::EndIfDirectiveExpected, start);
            return;
        }
        let take = top.parent_active && !top.branch_taken && condition;
        top.including = take;
        top.branch_taken |= take;
    }

    /// Processes an `#else` (9.5.4): selects its branch when the group is active
    /// and no earlier branch was taken.
    fn scan_else(&mut self, start: usize) {
        self.expect_directive_line_end(start);
        let Some(top) = self.conditionals.last_mut() else {
            self.report(DiagnosticKind::UnexpectedDirective, start);
            return;
        };
        if top.is_region {
            self.report(DiagnosticKind::EndRegionDirectiveExpected, start);
            return;
        }
        if top.seen_else {
            self.report(DiagnosticKind::EndIfDirectiveExpected, start);
            return;
        }
        top.seen_else = true;
        let take = top.parent_active && !top.branch_taken;
        top.including = take;
        top.branch_taken |= take;
    }

    /// Processes an `#endif` (9.5.4): closes the innermost `#if` group. With a
    /// `#region` open instead, its `#endregion` was due (`CS1038`); the region is
    /// left open for its real `#endregion` or the end-of-file error, NOT closed
    /// here -- a wrong closer does not match, matching csc.
    fn scan_endif(&mut self, start: usize) {
        self.expect_directive_line_end(start);
        match self.conditionals.last() {
            Some(top) if top.is_region => {
                self.report(DiagnosticKind::EndRegionDirectiveExpected, start);
            }
            Some(_) => {
                self.conditionals.pop();
            }
            None => self.report(DiagnosticKind::UnexpectedDirective, start),
        }
    }

    /// Processes a `#region` (9.5.6), which behaves lexically as `#if true`: its
    /// body is included exactly when the enclosing section is. The label after
    /// the directive name is arbitrary text and carries no meaning.
    fn scan_region(&mut self, active: bool) {
        self.consume_to_line_end();
        self.conditionals.push(Conditional {
            is_region: true,
            parent_active: active,
            branch_taken: true,
            including: active,
            seen_else: false,
        });
    }

    /// Processes an `#endregion` (9.5.6): closes the innermost `#region`. With an
    /// `#if` open instead, its `#endif` was due (`CS1027`); the `#if` is left open
    /// for its real `#endif` or the end-of-file error, NOT closed here -- a wrong
    /// closer does not match, matching csc.
    fn scan_endregion(&mut self, start: usize) {
        self.consume_to_line_end();
        match self.conditionals.last() {
            Some(top) if !top.is_region => {
                self.report(DiagnosticKind::EndIfDirectiveExpected, start);
            }
            Some(_) => {
                self.conditionals.pop();
            }
            None => self.report(DiagnosticKind::UnexpectedDirective, start),
        }
    }

    /// Processes an `#error` or `#warning` (9.5.5): when this section is being
    /// compiled, raises a diagnostic carrying the rest of the line as its
    /// message. The message is arbitrary text, so no end-of-line check applies.
    fn scan_error_or_warning(&mut self, start: usize, active: bool, is_error: bool) {
        self.skip_inline_whitespace();
        let message_start = self.position;
        self.consume_to_line_end();
        if !active {
            return;
        }
        let message: Box<str> = self.source[message_start..self.position].trim().into();
        let kind = if is_error {
            DiagnosticKind::ErrorDirective { message }
        } else {
            DiagnosticKind::WarningDirective { message }
        };
        self.report(kind, start);
    }

    /// Processes a `#line` directive (9.5.7), validating its indicator: a line
    /// number with an optional file name, or `default`.
    fn scan_line(&mut self, start: usize) {
        self.skip_inline_whitespace();
        if self.peek().is_some_and(|c| c.is_ascii_digit()) {
            let digits_start = self.position;
            self.consume_decimal_digits();
            self.validate_line_number(&self.source[digits_start..self.position], start);
            self.skip_inline_whitespace();
            if self.peek() == Some('"') && !self.consume_line_file_name() {
                self.report(DiagnosticKind::NewlineInConstant, start);
                return;
            }
        } else if self.read_directive_name() != "default" {
            self.report(DiagnosticKind::InvalidLineDirective, start);
            self.consume_to_line_end();
            return;
        }
        self.expect_directive_line_end(start);
    }

    /// Checks a `#line` line number (9.5.7) against the range the reference
    /// compiler accepts. Zero is `CS1576`; a value past the limit but still
    /// within `int` range is `CS1687`; one that overflows `int` is `CS1021` and
    /// `CS1576`. All three boundaries were confirmed against `csc`.
    fn validate_line_number(&mut self, digits: &str, start: usize) {
        const MAX_LINE: u64 = 16_707_565;
        let value = digits.parse::<u64>().unwrap_or(u64::MAX);
        if value == 0 {
            self.report(DiagnosticKind::InvalidLineDirective, start);
        } else if value > i32::MAX as u64 {
            self.report(DiagnosticKind::IntegerLiteralTooLarge, start);
            self.report(DiagnosticKind::InvalidLineDirective, start);
        } else if value > MAX_LINE {
            self.report(DiagnosticKind::LineNumberOutOfRange, start);
        }
    }

    /// Consumes a `#line` file name (9.5.7): a `"`-delimited run with no escapes.
    /// Returns whether the closing quote was reached; a file name that runs to
    /// the line terminator or end of file is unterminated, and the scanner stops
    /// at the terminator without consuming it.
    fn consume_line_file_name(&mut self) -> bool {
        self.bump();
        while let Some(c) = self.peek() {
            if is_new_line(c) {
                return false;
            }
            self.bump();
            if c == '"' {
                return true;
            }
        }
        false
    }

    /// Reads the identifier-or-keyword starting at the current position, used for
    /// a directive name or a conditional symbol, returning its text (empty when
    /// no identifier is there). The text borrows the source, not the scanner.
    fn read_directive_name(&mut self) -> &'a str {
        let name_start = self.position;
        if self.peek().is_some_and(is_identifier_start) {
            self.bump();
            self.consume_identifier_part();
        }
        &self.source[name_start..self.position]
    }

    /// Skips white space within a line (9.3.3), stopping at a line terminator.
    fn skip_inline_whitespace(&mut self) {
        while self.peek().is_some_and(is_whitespace) {
            self.bump();
        }
    }

    /// Consumes everything up to, but not including, the next line terminator (or
    /// the end of the file).
    fn consume_to_line_end(&mut self) {
        while self.peek().is_some_and(|c| !is_new_line(c)) {
            self.bump();
        }
    }

    /// Requires the rest of a directive line to be empty but for white space and
    /// at most one trailing single-line comment, as pp-new-line demands (9.5.3).
    /// A delimited comment is not permitted on a directive line and so counts as
    /// unexpected content. Anything unexpected is reported once as `CS1025` and
    /// then consumed. Stops at, without consuming, the line terminator.
    fn expect_directive_line_end(&mut self, start: usize) {
        self.skip_inline_whitespace();
        if self.peek() == Some('/') && self.peek_second() == Some('/') {
            self.consume_to_line_end();
            return;
        }
        if self.peek().is_some_and(|c| !is_new_line(c)) {
            self.report(DiagnosticKind::EndOfLineExpected, start);
            self.consume_to_line_end();
        }
    }

    /// Evaluates the pre-processing expression of an `#if` or `#elif` against the
    /// defined symbols (9.5.2), leaving the scanner just past it. A malformed
    /// expression is reported and treated as false.
    fn scan_pp_expression(&mut self, start: usize) -> bool {
        let mut errors = PpExprErrors {
            invalid_reported: false,
            close_paren_reported: false,
        };
        self.pp_or(start, &mut errors)
    }

    fn pp_or(&mut self, start: usize, errors: &mut PpExprErrors) -> bool {
        let mut value = self.pp_and(start, errors);
        loop {
            self.skip_inline_whitespace();
            if self.try_consume_str("||") {
                value |= self.pp_and(start, errors);
            } else {
                return value;
            }
        }
    }

    fn pp_and(&mut self, start: usize, errors: &mut PpExprErrors) -> bool {
        let mut value = self.pp_equality(start, errors);
        loop {
            self.skip_inline_whitespace();
            if self.try_consume_str("&&") {
                value &= self.pp_equality(start, errors);
            } else {
                return value;
            }
        }
    }

    fn pp_equality(&mut self, start: usize, errors: &mut PpExprErrors) -> bool {
        let mut value = self.pp_unary(start, errors);
        loop {
            self.skip_inline_whitespace();
            if self.try_consume_str("==") {
                value = value == self.pp_unary(start, errors);
            } else if self.try_consume_str("!=") {
                value = value != self.pp_unary(start, errors);
            } else {
                return value;
            }
        }
    }

    fn pp_unary(&mut self, start: usize, errors: &mut PpExprErrors) -> bool {
        self.skip_inline_whitespace();
        if self.peek() == Some('!') && self.peek_second() != Some('=') {
            self.bump();
            return !self.pp_unary(start, errors);
        }
        self.pp_primary(start, errors)
    }

    fn pp_primary(&mut self, start: usize, errors: &mut PpExprErrors) -> bool {
        self.skip_inline_whitespace();
        match self.peek() {
            Some('(') => {
                self.bump();
                let value = self.pp_or(start, errors);
                self.skip_inline_whitespace();
                if self.peek() == Some(')') {
                    self.bump();
                } else if !errors.close_paren_reported {
                    self.report(DiagnosticKind::CloseParenExpected, start);
                    errors.close_paren_reported = true;
                }
                value
            }
            Some(c) if is_identifier_start(c) => match self.read_directive_name() {
                "true" => true,
                "false" => false,
                symbol => self.defined_symbols.contains(symbol),
            },
            _ => {
                if !errors.invalid_reported {
                    self.report(DiagnosticKind::InvalidPreprocessorExpression, start);
                    errors.invalid_reported = true;
                }
                false
            }
        }
    }

    /// Consumes `text` at the current position if it appears there exactly,
    /// reporting whether it did. Used for the multi-character pre-processing
    /// operators, which are all ASCII.
    fn try_consume_str(&mut self, text: &str) -> bool {
        if self.remaining().starts_with(text) {
            self.position += text.len();
            true
        } else {
            false
        }
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

    /// Scans an identifier or keyword (9.4.2). The common case -- no `\uXXXX` escape --
    /// stays a zero-allocation source slice; an escape (anywhere, including the first
    /// character) switches to building the decoded identifier text, which is what names
    /// the keyword/identifier (so `if` is the keyword `if`).
    /// An identifier's name, folded per the lexer's [`Normalization`] mode (9.4.2). ASCII is
    /// already in NFC, so it is returned unchanged regardless of mode.
    fn identifier_name(&self, text: &str) -> Box<str> {
        match self.options.normalization {
            Normalization::None => text.into(),
            Normalization::Nfc => nfc_identifier(text),
        }
    }

    fn scan_identifier_or_keyword(&mut self, start: usize, verbatim: bool) -> TokenKind {
        let mut decoded: Option<String> = None;
        if self.peek() == Some('\\') {
            match self.unicode_escape_char() {
                Some(c) if is_identifier_start(c) => decoded = Some(c.to_string()),
                Some(_) => {
                    self.report(DiagnosticKind::UnexpectedCharacter { character: '\\' }, start);
                    return TokenKind::Unknown;
                }
                None => {
                    self.bump();
                    self.report(DiagnosticKind::UnexpectedCharacter { character: '\\' }, start);
                    return TokenKind::Unknown;
                }
            }
        } else {
            self.bump();
        }
        loop {
            match self.peek() {
                Some(c) if is_identifier_part(c) => {
                    if let Some(text) = &mut decoded {
                        text.push(c);
                    }
                    self.bump();
                }
                Some('\\') if matches!(self.peek_second(), Some('u' | 'U')) => {
                    let prefix_end = self.position;
                    match self.unicode_escape_char() {
                        Some(c) if is_identifier_part(c) => decoded
                            .get_or_insert_with(|| self.source[start..prefix_end].to_string())
                            .push(c),
                        _ => break,
                    }
                }
                _ => break,
            }
        }
        let text: &str = match &decoded {
            Some(decoded) => decoded,
            None => &self.source[start..self.position],
        };
        if !verbatim {
            if let Some(keyword) = Keyword::from_text(text) {
                return TokenKind::Keyword(keyword);
            }
            if let Some(operator) = self.typedref_keyword(text) {
                return TokenKind::TypedRefKeyword(operator);
            }
        }
        TokenKind::Identifier(self.identifier_name(text))
    }

    /// The typed-reference operator `text` names, but only in csc-parity mode
    /// ([`LexOptions::typedref`]); `None` in strict ISO-1, where these scan as identifiers.
    fn typedref_keyword(&self, text: &str) -> Option<TypedRefKeyword> {
        self.options
            .typedref
            .then(|| TypedRefKeyword::from_text(text))
            .flatten()
    }

    fn scan_verbatim(&mut self, start: usize) -> TokenKind {
        match self.peek_second() {
            Some('"') => self.scan_verbatim_string(start),
            Some(c) if c == '\\' || is_identifier_start(c) => {
                self.bump();
                self.scan_identifier_or_keyword(self.position, true)
            }
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

    /// Decodes a Unicode-escape character within an identifier (9.4.2): a `\uXXXX`
    /// (4 hex digits) or `\UXXXXXXXX` (8 hex digits) sequence at the cursor (on the
    /// backslash) to a `char`, advancing past it. Returns `None` -- leaving the cursor
    /// unmoved, on the backslash -- when the digits are missing or name no Unicode
    /// scalar value, so the caller can treat the `\` as not part of the identifier.
    fn unicode_escape_char(&mut self) -> Option<char> {
        let width = match self.peek_second() {
            Some('u') => 4,
            Some('U') => 8,
            _ => return None,
        };
        let checkpoint = self.position;
        self.bump();
        self.bump();
        let mut value: u32 = 0;
        for _ in 0..width {
            match self.peek().and_then(|c| c.to_digit(16)) {
                Some(digit) => {
                    value = value * 16 + digit;
                    self.bump();
                }
                None => {
                    self.position = checkpoint;
                    return None;
                }
            }
        }
        match char::from_u32(value) {
            Some(c) => Some(c),
            None => {
                self.position = checkpoint;
                None
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
            let value_end = self.position;
            let suffix = self.try_consume_real_suffix().unwrap_or(RealSuffix::None);
            return self.numeric_real_token(&self.source[start..value_end], suffix, start);
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
            let value_end = self.position;
            let suffix = self.try_consume_real_suffix().unwrap_or(RealSuffix::None);
            self.numeric_real_token(&self.source[start..value_end], suffix, start)
        } else if let Some(suffix) = self.try_consume_real_suffix() {
            self.numeric_real_token(&self.source[start..integer_digits_end], suffix, start)
        } else {
            let digits = &self.source[digits_start..integer_digits_end];
            let suffix = self.consume_integer_suffix();
            let value = self.parse_integer_value(digits, 10, start);
            TokenKind::IntegerLiteral { value, suffix }
        }
    }

    /// Parses a real-literal's numeric text (without its suffix) to an `f64`,
    /// returning its bit pattern. A value the `f64` parser rejects is `MalformedNumericLiteral`.
    fn parse_real_value(&mut self, text: &str, start: usize) -> u64 {
        match text.parse::<f64>() {
            Ok(value) => value.to_bits(),
            Err(_) => {
                self.report(DiagnosticKind::MalformedNumericLiteral, start);
                0
            }
        }
    }

    /// Builds the token for a real-literal text (without its suffix). A `decimal` (`m`) literal
    /// keeps its EXACT 96-bit mantissa and scale; `float`/`double` narrow to `f64` bits.
    fn numeric_real_token(&mut self, text: &str, suffix: RealSuffix, start: usize) -> TokenKind {
        if suffix == RealSuffix::Decimal {
            if let Some((lo, mid, hi, scale)) = parse_decimal_literal(text) {
                return TokenKind::DecimalLiteral { lo, mid, hi, scale };
            }
            self.report(DiagnosticKind::MalformedNumericLiteral, start);
            return TokenKind::DecimalLiteral {
                lo: 0,
                mid: 0,
                hi: 0,
                scale: 0,
            };
        }
        let bits = self.parse_real_value(text, start);
        TokenKind::RealLiteral { bits, suffix }
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

    /// Recognizes a post-1.0 operator at the current position that the target version does not
    /// support (`=>` from C# 3.0, `??`/`::` from 2.0, `?.`/`?[` from 6.0), reports a `CS1644`
    /// feature diagnostic, consumes it as one token, and returns `Unknown`. Without this, maximal
    /// munch over the 1.0 operator set would split it (`=` then `>`, two `?`, ...) and the error
    /// would name those, not the feature. `?.` is NOT gated when a digit follows -- it is then a
    /// conditional `?` and a `.5`-style real literal (`a ?.5 : b`), valid in any version (9.4.4.3).
    fn try_gate_post_1_0_operator(&mut self, start: usize) -> Option<TokenKind> {
        const GATED: &[(&str, Feature)] = &[
            ("=>", Feature::LambdaArrow),
            ("??", Feature::NullCoalescing),
            ("?[", Feature::NullConditional),
            ("?.", Feature::NullConditional),
            ("::", Feature::NamespaceAlias),
        ];
        let rest = self.remaining();
        for &(spelling, feature) in GATED {
            if !rest.starts_with(spelling) || self.options.version.supports(feature) {
                continue;
            }
            if spelling == "?." && rest[spelling.len()..].starts_with(|c: char| c.is_ascii_digit()) {
                continue;
            }
            self.position += spelling.len();
            self.report(
                DiagnosticKind::FeatureRequiresLaterVersion {
                    feature: feature.description(),
                    required: feature.introduced_in().display_name(),
                },
                start,
            );
            return Some(TokenKind::Unknown);
        }
        None
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
/// Parses a `decimal`-literal's numeric text (digits with an optional `.` and `e`/`E` exponent, no
/// suffix or sign) to `(lo, mid, hi, scale)` -- the 96-bit integer mantissa split into three `u32`
/// and the power-of-ten scale, so the value is `mantissa x 10^-scale`. `None` if the mantissa
/// overflows 96 bits, the scale falls outside `0..=28`, or a character is not a digit. (The sign of
/// `-2.5m` is a separate unary minus, folded later, not part of the literal.)
fn parse_decimal_literal(text: &str) -> Option<(u32, u32, u32, u8)> {
    let (mantissa_text, exponent) = match text.split_once(['e', 'E']) {
        Some((mantissa, exp)) => (mantissa, exp.parse::<i32>().ok()?),
        None => (text, 0),
    };
    let mut mantissa: u128 = 0;
    let mut fractional_digits: i32 = 0;
    let mut after_point = false;
    for ch in mantissa_text.chars() {
        if ch == '.' {
            after_point = true;
            continue;
        }
        let digit = ch.to_digit(10)?;
        mantissa = mantissa.checked_mul(10)?.checked_add(u128::from(digit))?;
        if after_point {
            fractional_digits += 1;
        }
    }
    let mut scale = fractional_digits - exponent;
    while scale < 0 {
        mantissa = mantissa.checked_mul(10)?;
        scale += 1;
    }
    if scale > 28 || mantissa > 0xFFFF_FFFF_FFFF_FFFF_FFFF_FFFF {
        return None;
    }
    Some((
        mantissa as u32,
        (mantissa >> 32) as u32,
        (mantissa >> 64) as u32,
        scale as u8,
    ))
}

/// An identifier's name in Unicode Normalization Form C (9.4.2). ASCII is already NFC, so only a
/// non-ASCII spelling is normalized -- the common path stays allocation-light, and a build that
/// never lexes a non-ASCII identifier drops the normalization tables.
fn nfc_identifier(text: &str) -> Box<str> {
    if text.is_ascii() {
        text.into()
    } else {
        lamella_unicode::normalize(text, lamella_unicode::NormalizationForm::Nfc).into()
    }
}

/// A line terminator (9.3.2): carriage return (U+000D), line feed (U+000A), next line (U+0085),
/// line separator (U+2028), or paragraph separator (U+2029). These end a single-line comment and
/// advance the line count; they are NOT white space (9.3.3).
fn is_new_line(c: char) -> bool {
    matches!(c, '\r' | '\n' | '\u{0085}' | '\u{2028}' | '\u{2029}')
}

/// White space (9.3.3): a Unicode space separator (class Zs -- the space U+0020, NBSP, the
/// U+2000..U+200A set, etc.), or one of the spec's three explicit characters (horizontal tab,
/// vertical tab, form feed). Line terminators (9.3.2, e.g. CR/LF/U+2028/U+2029) are NOT white
/// space -- they are recognized separately by [`is_new_line`].
fn is_whitespace(c: char) -> bool {
    matches!(c, '\t' | '\u{000B}' | '\u{000C}')
        || lamella_unicode::general_category(c as u32)
            == lamella_unicode::GeneralCategory::SpaceSeparator
}

/// A letter-character (9.4.2): a Unicode letter (classes Lu, Ll, Lt, Lm, Lo) or letter-number
/// (class Nl, e.g. the Roman numerals).
fn is_letter_character(c: char) -> bool {
    let category = lamella_unicode::general_category(c as u32);
    category.is_letter() || category == lamella_unicode::GeneralCategory::LetterNumber
}

/// An identifier-start character (9.4.2): a letter-character, or the underscore `_` (U+005F).
fn is_identifier_start(c: char) -> bool {
    c == '_' || is_letter_character(c)
}

/// An identifier-part character (9.4.2): a letter-character, a decimal digit (class Nd), a
/// connecting character (class Pc -- includes `_`), a combining mark (Mn/Mc), or a formatting
/// character (class Cf).
fn is_identifier_part(c: char) -> bool {
    use lamella_unicode::GeneralCategory::{
        ConnectorPunctuation, DecimalDigitNumber, Format, NonSpacingMark, SpacingCombiningMark,
    };
    is_letter_character(c)
        || matches!(
            lamella_unicode::general_category(c as u32),
            DecimalDigitNumber | ConnectorPunctuation | NonSpacingMark | SpacingCombiningMark | Format
        )
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostic::Severity;
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

    fn real(value: f64, suffix: RealSuffix) -> TokenKind {
        TokenKind::RealLiteral {
            bits: value.to_bits(),
            suffix,
        }
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
            vec![real(1.5, RealSuffix::None), TokenKind::EndOfFile]
        );
        assert_eq!(
            kinds(".5"),
            vec![real(0.5, RealSuffix::None), TokenKind::EndOfFile]
        );
        assert_eq!(
            kinds("1e10"),
            vec![real(1e10, RealSuffix::None), TokenKind::EndOfFile]
        );
        assert_eq!(
            kinds("1.5E-3"),
            vec![real(1.5e-3, RealSuffix::None), TokenKind::EndOfFile]
        );
        assert_eq!(
            kinds("1f"),
            vec![real(1.0, RealSuffix::Float), TokenKind::EndOfFile]
        );
        assert_eq!(
            kinds("2.0d"),
            vec![real(2.0, RealSuffix::Double), TokenKind::EndOfFile]
        );
        assert_eq!(
            kinds("3m"),
            vec![
                TokenKind::DecimalLiteral {
                    lo: 3,
                    mid: 0,
                    hi: 0,
                    scale: 0
                },
                TokenKind::EndOfFile
            ]
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
    fn post_1_0_operators_report_cs1644_under_csharp1() {
        for src in ["a => b", "a ?? b", "a?.b", "a?[0]", "global::System"] {
            let diagnostics = tokenize(src).diagnostics;
            assert!(
                diagnostics.iter().any(|d| d.code() == 1644),
                "{src:?} should report CS1644, got {diagnostics:?}"
            );
        }
        assert!(
            tokenize("c?.5:.3").diagnostics.is_empty(),
            "c?.5:.3 is a valid 1.0 conditional: {:?}",
            tokenize("c?.5:.3").diagnostics
        );
    }

    #[test]
    fn a_verbatim_identifier_name_may_use_unicode_escapes() {
        assert_eq!(kinds("@\\u0041bc"), vec![ident("Abc"), TokenKind::EndOfFile]);
        assert_eq!(kinds("@\\u0069f"), vec![ident("if"), TokenKind::EndOfFile]);
        assert_eq!(kinds("@\\u0020"), vec![TokenKind::Unknown, TokenKind::EndOfFile]);
    }

    #[test]
    fn unicode_escapes_in_identifiers_denote_the_named_characters() {
        assert_eq!(kinds("\\u0061bc"), vec![ident("abc"), TokenKind::EndOfFile]);
        assert_eq!(kinds("x\\u0079z"), vec![ident("xyz"), TokenKind::EndOfFile]);
        assert_eq!(kinds("\\U00000071"), vec![ident("q"), TokenKind::EndOfFile]);
        assert_eq!(
            kinds("\\u0069f"),
            vec![TokenKind::Keyword(Keyword::If), TokenKind::EndOfFile]
        );
        assert_eq!(kinds("plain1"), vec![ident("plain1"), TokenKind::EndOfFile]);
        assert!(kinds("\\x41").contains(&TokenKind::Unknown));
    }

    #[test]
    fn unicode_identifiers_and_whitespace_by_general_category() {
        let significant = |src: &str| -> Vec<TokenKind> {
            tokenize(src)
                .tokens
                .into_iter()
                .map(|token| token.kind)
                .filter(|kind| !kind.is_trivia() && *kind != TokenKind::EndOfFile)
                .collect()
        };
        assert_eq!(significant("Διπλάσιο"), vec![ident("Διπλάσιο")]);
        assert_eq!(significant("café"), vec![ident("café")]);
        assert_eq!(significant("Ⅻ"), vec![ident("Ⅻ")]);
        assert_eq!(significant("a\u{0301}b"), vec![ident("a\u{0301}b")]);
        assert_eq!(significant("\u{20000}z"), vec![ident("\u{20000}z")]);
        assert_eq!(significant("x9"), vec![ident("x9")]);
        assert_eq!(significant("a\u{00A0}b"), vec![ident("a"), ident("b")]);
        let around_symbol = significant("a\u{00A4}b");
        assert_eq!(around_symbol.first(), Some(&ident("a")));
        assert_eq!(around_symbol.last(), Some(&ident("b")));
    }

    #[test]
    fn decimal_literals_keep_their_exact_mantissa_and_scale() {
        assert_eq!(parse_decimal_literal("1.5"), Some((15, 0, 0, 1)));
        assert_eq!(parse_decimal_literal("0.10"), Some((10, 0, 0, 2)));
        assert_eq!(parse_decimal_literal("100"), Some((100, 0, 0, 0)));
        assert_eq!(parse_decimal_literal("0.05"), Some((5, 0, 0, 2)));
        assert_eq!(parse_decimal_literal("4294967296"), Some((0, 1, 0, 0)));
        assert_eq!(parse_decimal_literal("1.5e2"), Some((150, 0, 0, 0)));
        assert_eq!(parse_decimal_literal("1.5e-1"), Some((15, 0, 0, 2)));
        assert_eq!(parse_decimal_literal("79228162514264337593543950336"), None);
    }

    #[test]
    fn nfc_knob_folds_decomposed_identifiers_only_when_enabled() {
        let significant = |tokenized: Tokenized| -> Vec<TokenKind> {
            tokenized
                .tokens
                .into_iter()
                .map(|token| token.kind)
                .filter(|kind| !kind.is_trivia() && *kind != TokenKind::EndOfFile)
                .collect()
        };
        assert_ne!(
            significant(tokenize("cafe\u{0301}")),
            significant(tokenize("caf\u{00e9}"))
        );
        assert_eq!(
            significant(tokenize_with(
                "cafe\u{0301}",
                LexOptions::with_normalization(Normalization::Nfc)
            )),
            vec![ident("caf\u{00e9}")]
        );
        assert_eq!(
            significant(tokenize_with(
                "cafe\u{0301}",
                LexOptions::with_normalization(Normalization::Nfc)
            )),
            significant(tokenize_with(
                "caf\u{00e9}",
                LexOptions::with_normalization(Normalization::Nfc)
            ))
        );
        assert_eq!(
            significant(tokenize_with(
                "plain",
                LexOptions::with_normalization(Normalization::Nfc)
            )),
            vec![ident("plain")]
        );
    }

    #[test]
    fn typedref_operators_are_keywords_only_when_the_knob_is_on() {
        let significant = |tokenized: Tokenized| -> Vec<TokenKind> {
            tokenized
                .tokens
                .into_iter()
                .map(|token| token.kind)
                .filter(|kind| !kind.is_trivia() && *kind != TokenKind::EndOfFile)
                .collect()
        };
        let typedref = LexOptions {
            typedref: true,
            ..LexOptions::default()
        };
        assert_eq!(significant(tokenize("__makeref")), vec![ident("__makeref")]);
        assert_eq!(
            significant(tokenize_with("__makeref", typedref)),
            vec![TokenKind::TypedRefKeyword(TypedRefKeyword::MakeRef)]
        );
        assert_eq!(
            significant(tokenize_with("__refvalue", typedref)),
            vec![TokenKind::TypedRefKeyword(TypedRefKeyword::RefValue)]
        );
        assert_eq!(
            significant(tokenize_with("__reftype", typedref)),
            vec![TokenKind::TypedRefKeyword(TypedRefKeyword::RefType)]
        );
        assert_eq!(
            significant(tokenize_with("__make", typedref)),
            vec![ident("__make")]
        );
        assert_eq!(
            significant(tokenize_with("@__makeref", typedref)),
            vec![ident("__makeref")]
        );
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
            kinds("\u{0085}"),
            vec![TokenKind::NewLine, TokenKind::EndOfFile]
        );
        assert_eq!(
            kinds("\u{2029}"),
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
    fn a_bare_hash_is_a_missing_directive_name() {
        let result = tokenize("#");
        assert_eq!(result.tokens[0].kind, TokenKind::PreprocessingDirective);
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(result.diagnostics[0].code(), 1024);
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

    /// The significant tokens of `source`: everything that is neither trivia
    /// (including directives and skipped text) nor the end-of-file marker.
    fn significant(source: &str) -> Vec<TokenKind> {
        tokenize(source)
            .tokens
            .into_iter()
            .map(|token| token.kind)
            .filter(|kind| !kind.is_trivia() && *kind != TokenKind::EndOfFile)
            .collect()
    }

    /// The diagnostic codes raised for `source`, sorted, so a test can compare
    /// the set of codes without depending on the order they were emitted in.
    fn sorted_codes(source: &str) -> Vec<u16> {
        let mut codes: Vec<u16> = tokenize(source)
            .diagnostics
            .iter()
            .map(Diagnostic::code)
            .collect();
        codes.sort_unstable();
        codes
    }

    #[test]
    fn a_defined_symbol_includes_its_if_branch() {
        assert_eq!(
            significant("#define A\n#if A\nclass C {}\n#endif"),
            vec![
                TokenKind::Keyword(Keyword::Class),
                ident("C"),
                TokenKind::Punctuator(Punctuator::OpenBrace),
                TokenKind::Punctuator(Punctuator::CloseBrace),
            ]
        );
        assert!(
            tokenize("#define A\n#if A\nclass C {}\n#endif")
                .diagnostics
                .is_empty()
        );
    }

    #[test]
    fn an_undefined_symbol_skips_its_if_branch() {
        let result = tokenize("#if A\nclass C {}\n#endif");
        assert!(significant("#if A\nclass C {}\n#endif").is_empty());
        assert!(result.diagnostics.is_empty());
        assert!(
            result
                .tokens
                .iter()
                .any(|t| t.kind == TokenKind::SkippedText)
        );
    }

    #[test]
    fn else_is_taken_when_the_if_is_false() {
        assert_eq!(
            significant("#if A\ntaken_away\n#else\nclass C {}\n#endif"),
            vec![
                TokenKind::Keyword(Keyword::Class),
                ident("C"),
                TokenKind::Punctuator(Punctuator::OpenBrace),
                TokenKind::Punctuator(Punctuator::CloseBrace),
            ]
        );
    }

    #[test]
    fn the_first_true_elif_branch_wins() {
        let source =
            "#define B\n#if A\nfirst\n#elif B\nsecond\n#elif C\nthird\n#else\nlast\n#endif";
        assert_eq!(significant(source), vec![ident("second")]);
    }

    #[test]
    fn undef_makes_a_symbol_undefined_again() {
        let result = tokenize("#define A\n#undef A\n#if A\nx\n#endif");
        assert!(significant("#define A\n#undef A\n#if A\nx\n#endif").is_empty());
        assert!(result.diagnostics.is_empty());
    }

    #[test]
    fn pre_processing_expression_operators_evaluate() {
        assert_eq!(significant("#if !A\nyes\n#endif"), vec![ident("yes")]);
        assert_eq!(
            significant("#define A\n#if A && !B\nyes\n#endif"),
            vec![ident("yes")]
        );
        assert_eq!(
            significant("#if (A == B) || C\nyes\n#endif"),
            vec![ident("yes")]
        );
        assert_eq!(
            significant("#if true != false\nyes\n#endif"),
            vec![ident("yes")]
        );
        assert_eq!(significant("#if A || B\nno\n#endif"), Vec::new());
    }

    #[test]
    fn nested_conditionals_in_a_skipped_branch_stay_skipped() {
        let source = "#if A\n#if true\nx\n#endif\n#endif\nclass C {}";
        assert_eq!(
            significant(source),
            vec![
                TokenKind::Keyword(Keyword::Class),
                ident("C"),
                TokenKind::Punctuator(Punctuator::OpenBrace),
                TokenKind::Punctuator(Punctuator::CloseBrace),
            ]
        );
        assert!(tokenize(source).diagnostics.is_empty());
    }

    #[test]
    fn a_skipped_section_need_not_be_well_formed() {
        let source = "#if A\n\"unterminated\n/* unclosed\n#endif\nclass C {}";
        let result = tokenize(source);
        assert!(result.diagnostics.is_empty());
        assert_eq!(
            significant(source),
            vec![
                TokenKind::Keyword(Keyword::Class),
                ident("C"),
                TokenKind::Punctuator(Punctuator::OpenBrace),
                TokenKind::Punctuator(Punctuator::CloseBrace),
            ]
        );
    }

    #[test]
    fn a_region_includes_its_body_like_if_true() {
        assert_eq!(
            significant("#region a label\nclass C {}\n#endregion done"),
            vec![
                TokenKind::Keyword(Keyword::Class),
                ident("C"),
                TokenKind::Punctuator(Punctuator::OpenBrace),
                TokenKind::Punctuator(Punctuator::CloseBrace),
            ]
        );
        assert!(
            tokenize("#region a label\nclass C {}\n#endregion done")
                .diagnostics
                .is_empty()
        );
    }

    #[test]
    fn a_directive_may_carry_leading_and_inner_white_space() {
        assert_eq!(
            significant("   #   define   A\n#if A\nyes\n#endif"),
            vec![ident("yes")]
        );
    }

    #[test]
    fn a_directive_line_may_end_with_a_single_line_comment() {
        let result = tokenize("#define A // fine\n#if A\nyes\n#endif");
        assert!(result.diagnostics.is_empty());
        assert_eq!(
            significant("#define A // fine\n#if A\nyes\n#endif"),
            vec![ident("yes")]
        );
    }

    #[test]
    fn a_delimited_comment_is_not_allowed_on_a_directive_line() {
        assert_eq!(sorted_codes("#define A /* no */\nclass C {}"), vec![1025]);
    }

    #[test]
    fn error_and_warning_carry_their_message_and_severity() {
        let error = tokenize("#error something bad");
        assert_eq!(error.diagnostics.len(), 1);
        assert_eq!(error.diagnostics[0].code(), 1029);
        assert_eq!(error.diagnostics[0].severity(), Severity::Error);
        assert_eq!(
            error.diagnostics[0].kind,
            DiagnosticKind::ErrorDirective {
                message: "something bad".into()
            }
        );

        let warning = tokenize("#warning be careful");
        assert_eq!(warning.diagnostics[0].code(), 1030);
        assert_eq!(warning.diagnostics[0].severity(), Severity::Warning);
    }

    #[test]
    fn diagnostics_in_a_skipped_branch_do_not_fire() {
        assert!(
            tokenize("#if A\n#error boom\n#warning meh\n#endif")
                .diagnostics
                .is_empty()
        );
    }

    #[test]
    fn a_directive_must_still_be_well_formed_when_skipped() {
        assert_eq!(sorted_codes("#if A\n#nonsense\n#endif"), vec![1024]);
        assert_eq!(sorted_codes("#if A\n#endif x\nclass C {}"), vec![1025]);
    }

    #[test]
    fn define_or_undef_after_the_first_token_is_an_error() {
        assert_eq!(sorted_codes("class C {}\n#define A"), vec![1032]);
        assert_eq!(sorted_codes("class C {}\n#undef A"), vec![1032]);
        assert!(
            tokenize("class C {}\n#if A\n#define B\n#endif")
                .diagnostics
                .is_empty()
        );
    }

    #[test]
    fn directive_diagnostics_match_the_reference_compiler() {
        assert_eq!(sorted_codes("#bad\nclass C {}"), vec![1024]);
        assert_eq!(sorted_codes("#define\nclass C {}"), vec![1001]);
        assert_eq!(sorted_codes("#define true\nclass C {}"), vec![1001]);
        assert_eq!(sorted_codes("#endif\nclass C {}"), vec![1028]);
        assert_eq!(sorted_codes("#else\nclass C {}"), vec![1028]);
        assert_eq!(sorted_codes("#elif A\nclass C {}"), vec![1028]);
        assert_eq!(sorted_codes("#endregion\nclass C {}"), vec![1028]);
        assert_eq!(sorted_codes("#if A\nclass C {}"), vec![1027]);
        assert_eq!(sorted_codes("#region r\nclass C {}"), vec![1038]);
        assert_eq!(sorted_codes("#if A\n#else\n#else\n#endif\nx"), vec![1027]);
        assert_eq!(sorted_codes("#if A\n#else\n#elif B\n#endif\nx"), vec![1027]);
        assert_eq!(sorted_codes("#line abc\nclass C {}"), vec![1576]);
    }

    #[test]
    fn well_formed_line_directives_are_accepted() {
        for source in [
            "#line default\nclass C {}",
            "#line 200\nclass C {}",
            "#line 200 \"foo.cs\"\nclass C {}",
        ] {
            assert!(tokenize(source).diagnostics.is_empty(), "source {source:?}");
        }
    }

    #[test]
    fn a_malformed_pre_processing_expression_matches_the_reference_compiler() {
        assert_eq!(sorted_codes("#if\n#endif\nx"), vec![1517]);
        assert_eq!(sorted_codes("#if A ==\n#endif\nx"), vec![1517]);
        assert_eq!(sorted_codes("#if 1\n#endif\nx"), vec![1025, 1517]);
        assert_eq!(sorted_codes("#if (A\n#endif\nx"), vec![1026]);
        assert_eq!(sorted_codes("#if (((\n#endif\nx"), vec![1026, 1517]);
    }

    #[test]
    fn a_hash_not_first_on_a_line_is_cs1040() {
        let result = tokenize("class C { int x = #; }");
        assert!(result.diagnostics.iter().any(|d| d.code() == 1040));
    }

    #[test]
    fn a_directive_inside_a_multi_line_token_is_not_processed() {
        let source = "#define D\nclass C {\nstring s = @\"a\n#if D\nb\n#endif\nc\";\n}\n#endif";
        assert_eq!(sorted_codes(source), vec![1028]);
    }

    #[test]
    fn directives_and_skipped_text_still_cover_the_source() {
        let source = "#define A\n#if A\nclass C {}\n#else\nbad ## text\n#endif\n#region r\nint x;\n#endregion\n";
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
    fn mismatched_and_unterminated_directives_match_csc() {

        assert_eq!(sorted_codes("#region\n#endif\n#endregion\nx"), vec![1038]);
        assert_eq!(sorted_codes("#region\n#endif\nx"), vec![1038, 1038]);
        assert_eq!(sorted_codes("#region\n#elif A\n#endregion\nx"), vec![1038]);
        assert_eq!(sorted_codes("#region\n#else\n#endregion\nx"), vec![1038]);
        assert_eq!(sorted_codes("#if X\n#endregion\n#endif\nx"), vec![1027]);
        assert_eq!(sorted_codes("#if X\n#endregion\nx"), vec![1027, 1027]);
        assert_eq!(
            sorted_codes("#region\n#if X\n#endregion\n#endif\nx"),
            vec![1027, 1038]
        );
        assert!(
            tokenize("#region\n#if X\n#endif\n#endregion\nx")
                .diagnostics
                .is_empty()
        );
        assert_eq!(sorted_codes("#if A\n#if B\nx"), vec![1027]);
        assert_eq!(sorted_codes("#region\n#region\nx"), vec![1038]);
        assert_eq!(sorted_codes("#if A\n#region\nx"), vec![1038]);
        assert_eq!(sorted_codes("#region\n#if A\nx"), vec![1027]);
        assert_eq!(sorted_codes("#endif\n#endif\nx"), vec![1028, 1028]);
    }

    #[test]
    fn line_directive_numbers_are_range_checked() {
        assert_eq!(sorted_codes("#line 0\nx"), vec![1576]);
        assert!(tokenize("#line 16707565\nx").diagnostics.is_empty());
        assert_eq!(sorted_codes("#line 16707566\nx"), vec![1687]);
        assert_eq!(sorted_codes("#line 2147483648\nx"), vec![1021, 1576]);
    }

    #[test]
    fn an_unterminated_line_directive_file_name_is_cs1010() {
        assert_eq!(sorted_codes("#line 5 \"oops\nx"), vec![1010]);
        assert!(tokenize("#line 5 \"ok.cs\"\nx").diagnostics.is_empty());
    }

    #[test]
    fn a_define_in_a_skipped_branch_has_no_effect() {
        let source = "#if false\n#define A\n#endif\n#if A\nyes\n#endif";
        assert!(significant(source).is_empty());
        assert!(tokenize(source).diagnostics.is_empty());
    }

    #[test]
    fn undef_of_an_undefined_symbol_and_redefinition_are_fine() {
        assert!(tokenize("#undef Never\nclass C {}").diagnostics.is_empty());
        assert!(
            tokenize("#define A\n#define A\nclass C {}")
                .diagnostics
                .is_empty()
        );
    }

    #[test]
    fn only_white_space_may_precede_a_directive_on_its_line() {
        assert!(
            tokenize("/* c */ #define A\nclass C {}")
                .diagnostics
                .iter()
                .any(|d| d.code() == 1040)
        );
        assert!(
            tokenize("// c\n#define A\n#if A\nyes\n#endif")
                .diagnostics
                .is_empty()
        );
    }
}
