//! Semantic diagnostics (ECMA-334 1st ed; `CSxxxx` codes).

use alloc::boxed::Box;
use core::fmt;
use lamella_syntax::diagnostic::Severity;
use lamella_syntax::span::Span;

/// A semantic diagnostic: its kind and the source range it concerns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// What went wrong, with the detail needed to render the message.
    pub kind: DiagnosticKind,
    /// The source range the diagnostic concerns.
    pub span: Span,
}

impl Diagnostic {
    /// Creates a diagnostic of `kind` over `span`.
    #[must_use]
    pub fn new(kind: DiagnosticKind, span: Span) -> Diagnostic {
        Diagnostic { kind, span }
    }

    /// The `CSxxxx` numeric code.
    #[must_use]
    pub fn code(&self) -> u16 {
        self.kind.code()
    }

    /// The severity (every binder diagnostic is an error so far).
    #[must_use]
    pub fn severity(&self) -> Severity {
        Severity::Error
    }
}

/// A semantic-diagnostic kind, with any detail its message needs.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DiagnosticKind {
    /// `CS0246`: a type or namespace name could not be found.
    TypeNotFound {
        /// The unresolved name as written.
        name: Box<str>,
    },
    /// `CS0103`: the name does not exist in the current context.
    NameNotFound {
        /// The unresolved name.
        name: Box<str>,
    },
    /// `CS0029`: no implicit conversion exists between two types.
    NoImplicitConversion {
        /// The source type.
        from: Box<str>,
        /// The target type.
        to: Box<str>,
    },
    /// `CS0019`: an operator cannot be applied to operands of the given types.
    OperatorNotApplicable {
        /// The operator symbol, e.g. `+`.
        operator: Box<str>,
        /// The left operand's type.
        left: Box<str>,
        /// The right operand's type.
        right: Box<str>,
    },
    /// `CS0023`: a unary operator cannot be applied to an operand of this type.
    UnaryOperatorNotApplicable {
        /// The operator symbol, e.g. `-`.
        operator: Box<str>,
        /// The operand's type.
        operand: Box<str>,
    },
    /// `CS0173`: a conditional expression's two branches have no common type.
    ConditionalTypeMismatch {
        /// The `true` branch's type.
        left: Box<str>,
        /// The `false` branch's type.
        right: Box<str>,
    },
    /// `CS0131`: the target of an assignment is not a variable, property, or indexer.
    NotAssignable,
}

impl DiagnosticKind {
    /// The `CSxxxx` numeric code (confirmed against csc).
    #[must_use]
    pub fn code(&self) -> u16 {
        match self {
            DiagnosticKind::TypeNotFound { .. } => 246,
            DiagnosticKind::NameNotFound { .. } => 103,
            DiagnosticKind::NoImplicitConversion { .. } => 29,
            DiagnosticKind::OperatorNotApplicable { .. } => 19,
            DiagnosticKind::UnaryOperatorNotApplicable { .. } => 23,
            DiagnosticKind::ConditionalTypeMismatch { .. } => 173,
            DiagnosticKind::NotAssignable => 131,
        }
    }
}

impl fmt::Display for DiagnosticKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DiagnosticKind::TypeNotFound { name } => {
                write!(f, "The type or namespace name '{name}' could not be found")
            }
            DiagnosticKind::NameNotFound { name } => {
                write!(f, "The name '{name}' does not exist in the current context")
            }
            DiagnosticKind::NoImplicitConversion { from, to } => {
                write!(f, "Cannot implicitly convert type '{from}' to '{to}'")
            }
            DiagnosticKind::OperatorNotApplicable {
                operator,
                left,
                right,
            } => write!(
                f,
                "Operator '{operator}' cannot be applied to operands of type '{left}' and '{right}'"
            ),
            DiagnosticKind::UnaryOperatorNotApplicable { operator, operand } => write!(
                f,
                "Operator '{operator}' cannot be applied to operand of type '{operand}'"
            ),
            DiagnosticKind::ConditionalTypeMismatch { left, right } => write!(
                f,
                "Type of conditional expression cannot be determined because there is no \
                 implicit conversion between '{left}' and '{right}'"
            ),
            DiagnosticKind::NotAssignable => write!(
                f,
                "The left-hand side of an assignment must be a variable, property or indexer"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;

    #[test]
    fn codes_match_the_reference_compiler() {
        assert_eq!(
            DiagnosticKind::TypeNotFound { name: "Foo".into() }.code(),
            246
        );
        assert_eq!(
            DiagnosticKind::NameNotFound { name: "x".into() }.code(),
            103
        );
        assert_eq!(
            DiagnosticKind::NoImplicitConversion {
                from: "string".into(),
                to: "int".into()
            }
            .code(),
            29
        );
    }

    #[test]
    fn messages_render_their_detail() {
        assert_eq!(
            DiagnosticKind::TypeNotFound {
                name: "Widget".into()
            }
            .to_string(),
            "The type or namespace name 'Widget' could not be found"
        );
        assert_eq!(
            DiagnosticKind::NoImplicitConversion {
                from: "string".into(),
                to: "int".into()
            }
            .to_string(),
            "Cannot implicitly convert type 'string' to 'int'"
        );
    }
}
