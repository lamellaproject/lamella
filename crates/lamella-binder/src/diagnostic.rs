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

    /// The severity, from the diagnostic's kind.
    #[must_use]
    pub fn severity(&self) -> Severity {
        self.kind.severity()
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
    /// `CS0117`: the type does not contain a definition for the named member.
    MemberNotFound {
        /// The type the member was looked for on.
        type_name: Box<str>,
        /// The member name that was not found.
        member: Box<str>,
    },
    /// `CS1501`: no overload of the method takes the given number of arguments.
    NoOverloadForArgumentCount {
        /// The method name.
        method: Box<str>,
        /// The number of arguments supplied.
        count: u32,
    },
    /// `CS1503`: an argument has no implicit conversion to its parameter type.
    ArgumentConversion {
        /// The 1-based argument position.
        index: u32,
        /// The argument's type.
        from: Box<str>,
        /// The parameter's type.
        to: Box<str>,
    },
    /// `CS0121`: a call is ambiguous between two or more overloads.
    AmbiguousCall {
        /// The method name.
        method: Box<str>,
    },
    /// `CS0122`: the member is inaccessible due to its protection level.
    Inaccessible {
        /// The qualified member name.
        member: Box<str>,
    },
    /// `CS0150`: a constant value was expected (e.g. a non-constant `case` label).
    ConstantExpected,
    /// `CS0152`: a `switch` has two labels with the same value (or two `default`s).
    DuplicateCaseLabel {
        /// The duplicated label, rendered as `case 5` or `default`.
        label: Box<str>,
    },
    /// `CS0163`: control can fall off the end of a non-empty `switch` section into
    /// the next (C# forbids implicit fall-through).
    SwitchFallThrough,
    /// `CS0128`: a local variable of this name is already declared in this scope.
    DuplicateLocal {
        /// The redeclared name.
        name: Box<str>,
    },
    /// `CS0136`: a local would shadow one already in an enclosing scope, which C#
    /// forbids.
    LocalShadowsEnclosing {
        /// The shadowing name.
        name: Box<str>,
    },
    /// `CS0201`: an expression that is not assignment, call, increment, decrement,
    /// or object creation was used as a statement.
    IllegalStatementExpression,
    /// `CS0102`: the type already contains a definition for this member name.
    DuplicateMember {
        /// The type that declares it twice.
        type_name: Box<str>,
        /// The duplicated member name.
        member: Box<str>,
    },
    /// `CS0266`: no implicit conversion exists, but an explicit one (a cast) does.
    ExplicitConversionExists {
        /// The source type.
        from: Box<str>,
        /// The target type.
        to: Box<str>,
    },
    /// `CS0168` (warning): a local is declared but never used.
    UnusedLocal {
        /// The local's name.
        name: Box<str>,
    },
    /// `CS0219` (warning): a local is assigned but its value is never used.
    UnusedLocalValue {
        /// The local's name.
        name: Box<str>,
    },
    /// `CS0162` (warning): a statement can never be reached.
    UnreachableCode,
    /// `CS0120`: an instance member was named through a type, with no object.
    ObjectReferenceRequired {
        /// The qualified member name.
        member: Box<str>,
    },
    /// `CS0176`: a static member was accessed through an instance.
    StaticMemberViaInstance {
        /// The qualified member name.
        member: Box<str>,
    },
    /// `CS0021`: a value of this type cannot be indexed with `[]`.
    CannotIndex {
        /// The type that was indexed.
        type_name: Box<str>,
    },
    /// `CS1729`: the type has no constructor taking the given number of arguments.
    NoConstructor {
        /// The type being constructed.
        type_name: Box<str>,
        /// The number of arguments supplied.
        count: u32,
    },
    /// `CS0127`: a `return` in a `void` method has an expression.
    ReturnValueInVoidMethod {
        /// The enclosing method's name.
        method: Box<str>,
    },
    /// `CS0126`: a `return` in a value-returning method has no expression.
    ReturnValueRequired {
        /// The required return type.
        ty: Box<str>,
    },
    /// `CS0161`: not every code path in a value-returning method returns a value.
    NotAllPathsReturn {
        /// The method's name.
        method: Box<str>,
    },
    /// `CS0030`: no explicit conversion exists for a cast.
    CannotCast {
        /// The operand's type.
        from: Box<str>,
        /// The cast's target type.
        to: Box<str>,
    },
    /// `CS0165`: a local variable is read before it is definitely assigned.
    UseOfUnassignedLocal {
        /// The local variable's name.
        name: Box<str>,
    },
    /// `CS0234`: a name does not exist in the given namespace.
    NamespaceMemberNotFound {
        /// The namespace that was searched.
        namespace: Box<str>,
        /// The name that was not found in it.
        name: Box<str>,
    },
    /// `CS0104`: a simple name is ambiguous between two imported namespaces.
    AmbiguousReference {
        /// The ambiguous simple name.
        name: Box<str>,
        /// One candidate's full name.
        first: Box<str>,
        /// Another candidate's full name.
        second: Box<str>,
    },
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
            DiagnosticKind::MemberNotFound { .. } => 117,
            DiagnosticKind::NoOverloadForArgumentCount { .. } => 1501,
            DiagnosticKind::ArgumentConversion { .. } => 1503,
            DiagnosticKind::AmbiguousCall { .. } => 121,
            DiagnosticKind::Inaccessible { .. } => 122,
            DiagnosticKind::ConstantExpected => 150,
            DiagnosticKind::DuplicateCaseLabel { .. } => 152,
            DiagnosticKind::SwitchFallThrough => 163,
            DiagnosticKind::DuplicateLocal { .. } => 128,
            DiagnosticKind::LocalShadowsEnclosing { .. } => 136,
            DiagnosticKind::IllegalStatementExpression => 201,
            DiagnosticKind::DuplicateMember { .. } => 102,
            DiagnosticKind::ExplicitConversionExists { .. } => 266,
            DiagnosticKind::UnusedLocal { .. } => 168,
            DiagnosticKind::UnusedLocalValue { .. } => 219,
            DiagnosticKind::UnreachableCode => 162,
            DiagnosticKind::ObjectReferenceRequired { .. } => 120,
            DiagnosticKind::StaticMemberViaInstance { .. } => 176,
            DiagnosticKind::CannotIndex { .. } => 21,
            DiagnosticKind::NoConstructor { .. } => 1729,
            DiagnosticKind::ReturnValueInVoidMethod { .. } => 127,
            DiagnosticKind::ReturnValueRequired { .. } => 126,
            DiagnosticKind::NotAllPathsReturn { .. } => 161,
            DiagnosticKind::CannotCast { .. } => 30,
            DiagnosticKind::UseOfUnassignedLocal { .. } => 165,
            DiagnosticKind::NamespaceMemberNotFound { .. } => 234,
            DiagnosticKind::AmbiguousReference { .. } => 104,
        }
    }

    /// Whether this diagnostic stops compilation. Most semantic diagnostics are
    /// errors; the unused-local diagnostics are warnings (CS0162 unreachable will
    /// join them once the reachability pass is break-aware).
    #[must_use]
    pub fn severity(&self) -> Severity {
        match self {
            DiagnosticKind::UnusedLocal { .. }
            | DiagnosticKind::UnusedLocalValue { .. }
            | DiagnosticKind::UnreachableCode => Severity::Warning,
            _ => Severity::Error,
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
            DiagnosticKind::MemberNotFound { type_name, member } => write!(
                f,
                "'{type_name}' does not contain a definition for '{member}'"
            ),
            DiagnosticKind::NoOverloadForArgumentCount { method, count } => write!(
                f,
                "No overload for method '{method}' takes {count} arguments"
            ),
            DiagnosticKind::ArgumentConversion { index, from, to } => write!(
                f,
                "Argument {index}: cannot convert from '{from}' to '{to}'"
            ),
            DiagnosticKind::AmbiguousCall { method } => {
                write!(f, "The call is ambiguous between overloads of '{method}'")
            }
            DiagnosticKind::Inaccessible { member } => {
                write!(f, "'{member}' is inaccessible due to its protection level")
            }
            DiagnosticKind::ConstantExpected => write!(f, "A constant value is expected"),
            DiagnosticKind::DuplicateCaseLabel { label } => write!(
                f,
                "The label '{label}:' already occurs in this switch statement"
            ),
            DiagnosticKind::SwitchFallThrough => {
                write!(
                    f,
                    "Control cannot fall through from one case label to another"
                )
            }
            DiagnosticKind::DuplicateLocal { name } => write!(
                f,
                "A local variable named '{name}' is already defined in this scope"
            ),
            DiagnosticKind::LocalShadowsEnclosing { name } => write!(
                f,
                "A local variable named '{name}' cannot be declared in this scope \
                 because it would give a different meaning to '{name}', which is \
                 already used in a 'parent or current' scope to denote something else"
            ),
            DiagnosticKind::IllegalStatementExpression => write!(
                f,
                "Only assignment, call, increment, decrement, and new object \
                 expressions can be used as a statement"
            ),
            DiagnosticKind::DuplicateMember { type_name, member } => write!(
                f,
                "The type '{type_name}' already contains a definition for '{member}'"
            ),
            DiagnosticKind::ExplicitConversionExists { from, to } => write!(
                f,
                "Cannot implicitly convert type '{from}' to '{to}'. \
                 An explicit conversion exists (are you missing a cast?)"
            ),
            DiagnosticKind::UnusedLocal { name } => {
                write!(f, "The variable '{name}' is declared but never used")
            }
            DiagnosticKind::UnusedLocalValue { name } => {
                write!(
                    f,
                    "The variable '{name}' is assigned but its value is never used"
                )
            }
            DiagnosticKind::UnreachableCode => write!(f, "Unreachable code detected"),
            DiagnosticKind::ObjectReferenceRequired { member } => write!(
                f,
                "An object reference is required for the non-static member '{member}'"
            ),
            DiagnosticKind::StaticMemberViaInstance { member } => write!(
                f,
                "Member '{member}' cannot be accessed with an instance reference; \
                 qualify it with a type name instead"
            ),
            DiagnosticKind::CannotIndex { type_name } => write!(
                f,
                "Cannot apply indexing with [] to an expression of type '{type_name}'"
            ),
            DiagnosticKind::NoConstructor { type_name, count } => write!(
                f,
                "'{type_name}' does not contain a constructor that takes {count} arguments"
            ),
            DiagnosticKind::ReturnValueInVoidMethod { method } => write!(
                f,
                "Since '{method}' returns void, a return keyword must not be followed by an \
                 object expression"
            ),
            DiagnosticKind::ReturnValueRequired { ty } => {
                write!(f, "An object of a type convertible to '{ty}' is required")
            }
            DiagnosticKind::NotAllPathsReturn { method } => {
                write!(f, "'{method}': not all code paths return a value")
            }
            DiagnosticKind::CannotCast { from, to } => {
                write!(f, "Cannot convert type '{from}' to '{to}'")
            }
            DiagnosticKind::UseOfUnassignedLocal { name } => {
                write!(f, "Use of unassigned local variable '{name}'")
            }
            DiagnosticKind::NamespaceMemberNotFound { namespace, name } => write!(
                f,
                "The type or namespace name '{name}' does not exist in the namespace '{namespace}'"
            ),
            DiagnosticKind::AmbiguousReference {
                name,
                first,
                second,
            } => write!(
                f,
                "'{name}' is an ambiguous reference between '{first}' and '{second}'"
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
