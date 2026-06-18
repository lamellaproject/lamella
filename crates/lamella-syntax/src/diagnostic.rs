//! Diagnostics produced by the front end.

use crate::span::Span;
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
        }
    }

    /// Whether this diagnostic stops compilation.
    #[must_use]
    pub fn severity(&self) -> Severity {
        match self {
            DiagnosticKind::UnterminatedDelimitedComment
            | DiagnosticKind::UnexpectedCharacter { .. }
            | DiagnosticKind::IntegerLiteralTooLarge
            | DiagnosticKind::MalformedNumericLiteral
            | DiagnosticKind::UnrecognizedEscapeSequence
            | DiagnosticKind::NewlineInConstant
            | DiagnosticKind::EmptyCharacterLiteral
            | DiagnosticKind::TooManyCharactersInCharacterLiteral
            | DiagnosticKind::UnterminatedStringLiteral => Severity::Error,
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
