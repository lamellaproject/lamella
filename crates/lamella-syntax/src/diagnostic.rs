//! Diagnostics produced by the front end.

use crate::span::Span;
use alloc::boxed::Box;
use core::fmt;

/// How serious a diagnostic is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// A problem that does not, by itself, prevent compilation.
    Warning,
    /// A problem that prevents successful compilation.
    Error,
}

/// A particular diagnostic, with any detail needed to render its message.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DiagnosticKind {
    /// A delimited comment was not closed with `*/` before the end of the file.
    UnterminatedDelimitedComment,
    /// A character was found that cannot begin any token.
    UnexpectedCharacter {
        /// The offending character.
        character: char,
    },
    /// An integer literal is larger than `ulong` can represent.
    IntegerLiteralTooLarge,
    /// A numeric literal is malformed, for example `0x` with no hex digits or an
    /// exponent with no digits. (Code to be confirmed against csc.)
    MalformedNumericLiteral,
    /// A backslash escape was not one of the recognised forms, or a `\x`, `\u`,
    /// or `\U` escape had too few hex digits or named a value above U+10FFFF.
    UnrecognizedEscapeSequence,
    /// A character or string literal ran to a line terminator or end of file
    /// before its closing quote.
    NewlineInConstant,
    /// A character literal had no character between its quotes.
    EmptyCharacterLiteral,
    /// A character literal held more than one character (more than one UTF-16
    /// code unit, counting an escape that expands to a surrogate pair as two).
    TooManyCharactersInCharacterLiteral,
    /// A verbatim string literal (`@"..."`) ran to end of file before its
    /// closing quote.
    UnterminatedStringLiteral,
    /// A `#` that is not the first non-white-space character on its line: a
    /// pre-processing directive must begin its own line (9.5).
    DirectiveNotFirstOnLine,
    /// A `#` was followed by something other than a known directive name (9.5).
    PreprocessorDirectiveExpected,
    /// A directive line carried tokens past its content where only white space,
    /// a single-line comment, or the end of the line was allowed (9.5).
    EndOfLineExpected,
    /// An identifier was required but not found: a `#define`/`#undef` symbol name
    /// (9.5.3), the member after a `.`, or any other identifier the grammar
    /// demands. (`true`/`false` are not valid symbol names.)
    IdentifierExpected,
    /// A `#define` or `#undef` appeared after the first real token of the file,
    /// which 9.5.3 forbids.
    SymbolAfterFirstToken,
    /// An `#elif`, `#else`, `#endif`, or `#endregion` had no open construct to
    /// match, or appeared where it was not allowed (9.5.4, 9.5.6).
    UnexpectedDirective,
    /// An `#if` (or `#region` whose body holds an `#if`) reached the end of the
    /// file, or a directive that may not appear, without its `#endif` (9.5.4).
    EndIfDirectiveExpected,
    /// A `#region` reached the end of the file, or an `#endif` where an
    /// `#endregion` was due, without its `#endregion` (9.5.6).
    EndRegionDirectiveExpected,
    /// A pre-processing expression in an `#if` or `#elif` was malformed (9.5.2).
    InvalidPreprocessorExpression,
    /// A parenthesised pre-processing expression was missing its `)` (9.5.2).
    CloseParenExpected,
    /// A `#line` directive had no valid line number, file name, or `default`
    /// indicator (9.5.7).
    InvalidLineDirective,
    /// A `#line` line number parsed as an integer but lay past the range a
    /// `#line` directive accepts (9.5.7).
    LineNumberOutOfRange,
    /// A `#error` directive, carrying its message text (9.5.5).
    ErrorDirective {
        /// The text following `#error` on the directive line.
        message: Box<str>,
    },
    /// A `#warning` directive, carrying its message text (9.5.5).
    WarningDirective {
        /// The text following `#warning` on the directive line.
        message: Box<str>,
    },
    /// A primary expression was expected, but the token there cannot begin one
    /// (ECMA-334 1st ed, 14.5).
    ExpressionExpected,
    /// A specific token the grammar required was not present.
    TokenExpected {
        /// The expected token's spelling, for example `]` or `:`.
        expected: &'static str,
    },
    /// A type was expected, for example inside `typeof( )` or after `is`/`as`
    /// (ECMA-334 1st ed, clause 11).
    TypeExpected,
    /// A statement was not terminated by the required `;` (clause 15).
    SemicolonExpected,
    /// A block or similar construct was not closed by the required `}`.
    CloseBraceExpected,
    /// A block was required (for example a `try`, `catch`, or `finally` body) but
    /// no `{` was found.
    OpenBraceExpected,
    /// A `try` block was followed by neither a `catch` nor a `finally` (15.10).
    ExpectedCatchOrFinally,
    /// A namespace member was expected to begin a type declaration but the
    /// `class`/`struct`/`interface`/`enum`/`delegate` keyword was missing (16.4).
    TypeDeclarationExpected,
    /// `operator` was not followed by an overloadable operator (17.9).
    OverloadableOperatorExpected,
    /// A `foreach` header was missing the `in` keyword (15.8.4).
    InExpected,
    /// A post-1.0 operator (`=>`, `??`, `?.`, `?[`, `::`) was used while targeting a language
    /// version that predates it. Carries the feature's description and the version that introduced
    /// it, both already rendered to text (the lexer builds them from [`crate::version::Feature`]),
    /// so this stays decoupled from the version model.
    FeatureRequiresLaterVersion {
        /// The feature's noun phrase, e.g. "the null-coalescing operator '??'".
        feature: &'static str,
        /// The version that introduced it, e.g. "C# 3.0".
        required: &'static str,
    },
}

impl DiagnosticKind {
    /// The C# compiler code for this diagnostic, that is, the number `N` in
    /// `CSN`. Codes match the reference compiler where an equivalent exists.
    #[must_use]
    pub fn code(&self) -> u16 {
        match self {
            DiagnosticKind::UnterminatedDelimitedComment => 1035,
            DiagnosticKind::UnexpectedCharacter { .. } => 1056,
            DiagnosticKind::IntegerLiteralTooLarge => 1021,
            DiagnosticKind::MalformedNumericLiteral => 1013,
            DiagnosticKind::UnrecognizedEscapeSequence => 1009,
            DiagnosticKind::NewlineInConstant => 1010,
            DiagnosticKind::EmptyCharacterLiteral => 1011,
            DiagnosticKind::TooManyCharactersInCharacterLiteral => 1012,
            DiagnosticKind::UnterminatedStringLiteral => 1039,
            DiagnosticKind::IdentifierExpected => 1001,
            DiagnosticKind::PreprocessorDirectiveExpected => 1024,
            DiagnosticKind::EndOfLineExpected => 1025,
            DiagnosticKind::CloseParenExpected => 1026,
            DiagnosticKind::EndIfDirectiveExpected => 1027,
            DiagnosticKind::UnexpectedDirective => 1028,
            DiagnosticKind::ErrorDirective { .. } => 1029,
            DiagnosticKind::WarningDirective { .. } => 1030,
            DiagnosticKind::SymbolAfterFirstToken => 1032,
            DiagnosticKind::EndRegionDirectiveExpected => 1038,
            DiagnosticKind::DirectiveNotFirstOnLine => 1040,
            DiagnosticKind::InvalidPreprocessorExpression => 1517,
            DiagnosticKind::InvalidLineDirective => 1576,
            DiagnosticKind::LineNumberOutOfRange => 1687,
            DiagnosticKind::ExpressionExpected => 1525,
            DiagnosticKind::TokenExpected { .. } => 1003,
            DiagnosticKind::TypeExpected => 1031,
            DiagnosticKind::SemicolonExpected => 1002,
            DiagnosticKind::CloseBraceExpected => 1513,
            DiagnosticKind::OpenBraceExpected => 1514,
            DiagnosticKind::ExpectedCatchOrFinally => 1524,
            DiagnosticKind::TypeDeclarationExpected => 1518,
            DiagnosticKind::OverloadableOperatorExpected => 1037,
            DiagnosticKind::InExpected => 1515,
            DiagnosticKind::FeatureRequiresLaterVersion { .. } => 1644,
        }
    }

    /// Whether this diagnostic stops compilation.
    #[must_use]
    pub fn severity(&self) -> Severity {
        match self {
            DiagnosticKind::WarningDirective { .. } => Severity::Warning,
            _ => Severity::Error,
        }
    }
}

impl fmt::Display for DiagnosticKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DiagnosticKind::UnterminatedDelimitedComment => f.write_str("End-of-comment expected"),
            DiagnosticKind::UnexpectedCharacter { character } => {
                write!(f, "Unexpected character '{character}'")
            }
            DiagnosticKind::IntegerLiteralTooLarge => f.write_str("Integer constant is too large"),
            DiagnosticKind::MalformedNumericLiteral => f.write_str("Invalid number"),
            DiagnosticKind::UnrecognizedEscapeSequence => f.write_str("Unrecognized escape sequence"),
            DiagnosticKind::NewlineInConstant => f.write_str("Newline in constant"),
            DiagnosticKind::EmptyCharacterLiteral => f.write_str("Empty character literal"),
            DiagnosticKind::TooManyCharactersInCharacterLiteral => {
                f.write_str("Too many characters in character literal")
            }
            DiagnosticKind::UnterminatedStringLiteral => {
                f.write_str("Unterminated string literal")
            }
            DiagnosticKind::IdentifierExpected => f.write_str("Identifier expected"),
            DiagnosticKind::PreprocessorDirectiveExpected => {
                f.write_str("Preprocessor directive expected")
            }
            DiagnosticKind::EndOfLineExpected => {
                f.write_str("Single-line comment or end-of-line expected")
            }
            DiagnosticKind::CloseParenExpected => f.write_str(") expected"),
            DiagnosticKind::EndIfDirectiveExpected => f.write_str("#endif directive expected"),
            DiagnosticKind::UnexpectedDirective => f.write_str("Unexpected preprocessor directive"),
            DiagnosticKind::ErrorDirective { message } => write!(f, "#error: '{message}'"),
            DiagnosticKind::WarningDirective { message } => write!(f, "#warning: '{message}'"),
            DiagnosticKind::SymbolAfterFirstToken => {
                f.write_str("Cannot define/undefine preprocessor symbols after first token in file")
            }
            DiagnosticKind::EndRegionDirectiveExpected => {
                f.write_str("#endregion directive expected")
            }
            DiagnosticKind::DirectiveNotFirstOnLine => f.write_str(
                "Preprocessor directives must appear as the first non-whitespace character on a line",
            ),
            DiagnosticKind::InvalidPreprocessorExpression => {
                f.write_str("Invalid preprocessor expression")
            }
            DiagnosticKind::InvalidLineDirective => {
                f.write_str("The line number specified for #line directive is missing or invalid")
            }
            DiagnosticKind::LineNumberOutOfRange => {
                f.write_str("The line number specified for #line directive is out of range")
            }
            DiagnosticKind::ExpressionExpected => f.write_str("Invalid expression term"),
            DiagnosticKind::TokenExpected { expected } => {
                write!(f, "Syntax error, '{expected}' expected")
            }
            DiagnosticKind::TypeExpected => f.write_str("Type expected"),
            DiagnosticKind::SemicolonExpected => f.write_str("; expected"),
            DiagnosticKind::CloseBraceExpected => f.write_str("} expected"),
            DiagnosticKind::OpenBraceExpected => f.write_str("{ expected"),
            DiagnosticKind::ExpectedCatchOrFinally => {
                f.write_str("Expected catch or finally")
            }
            DiagnosticKind::TypeDeclarationExpected => {
                f.write_str("Expected class, delegate, enum, interface, or struct")
            }
            DiagnosticKind::OverloadableOperatorExpected => {
                f.write_str("Overloadable operator expected")
            }
            DiagnosticKind::InExpected => f.write_str("'in' expected"),
            DiagnosticKind::FeatureRequiresLaterVersion { feature, required } => {
                write!(f, "Feature {feature} is not available in C# 1.0; it requires {required} or greater")
            }
        }
    }
}

/// A diagnostic: what went wrong ([`DiagnosticKind`]) and where ([`Span`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// The specific problem.
    pub kind: DiagnosticKind,
    /// The source location the diagnostic refers to.
    pub span: Span,
}

impl Diagnostic {
    /// Creates a diagnostic of `kind` at `span`.
    #[must_use]
    pub fn new(kind: DiagnosticKind, span: Span) -> Diagnostic {
        Diagnostic { kind, span }
    }

    /// The C# compiler code (`CSxxxx`) for this diagnostic.
    #[must_use]
    pub fn code(&self) -> u16 {
        self.kind.code()
    }

    /// This diagnostic's severity.
    #[must_use]
    pub fn severity(&self) -> Severity {
        self.kind.severity()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;

    #[test]
    fn codes_match_the_reference_compiler() {
        assert_eq!(DiagnosticKind::UnterminatedDelimitedComment.code(), 1035);
        assert_eq!(
            DiagnosticKind::UnexpectedCharacter { character: '#' }.code(),
            1056
        );
    }

    #[test]
    fn lexical_diagnostics_are_errors() {
        assert_eq!(
            DiagnosticKind::UnterminatedDelimitedComment.severity(),
            Severity::Error
        );
    }

    #[test]
    fn messages_render_their_detail() {
        let unexpected = DiagnosticKind::UnexpectedCharacter { character: '#' };
        assert_eq!(format!("{unexpected}"), "Unexpected character '#'");
        assert_eq!(
            format!("{}", DiagnosticKind::UnterminatedDelimitedComment),
            "End-of-comment expected"
        );
    }
}
