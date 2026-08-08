//! Semantic diagnostics (ECMA-334 1st ed; `CSxxxx` codes).

use alloc::boxed::Box;
use core::fmt;
use lamella_syntax::diagnostic::Severity;
use lamella_syntax::span::Span;
use lamella_syntax::version::LanguageVersion;

/// The language version this compilation is targeting -- **the one seam a selectable
/// `/langversion` will be wired through**, and deliberately a single function rather than a
/// literal repeated at each gate.
///
/// It answers [`LanguageVersion::DEFAULT`] today because ISO-1 is the only dialect lcsc implements,
/// so every feature gate fires unconditionally and every gate code is `CS8022`. The value is
/// nonetheless carried on the diagnostic rather than assumed where the message is formatted,
/// because the code and the message's "in C# N" both derive from it: a second dialect changes five
/// call sites through here, not thirty through a format string.
pub(crate) fn compiling_version() -> LanguageVersion {
    LanguageVersion::DEFAULT
}

/// Which of a C# compiler's two passes reported a diagnostic. A compilation is checked in the
/// order the language is defined: DECLARATIONS first -- signatures, base clauses, interface
/// implementation, const values, enum members, attributes -- and then METHOD BODIES, which are
/// checked AGAINST those declarations.
///
/// The order is not an implementation detail, because it decides what gets reported: a body is
/// only worth analyzing if the declarations it is checked against are sound, so when the
/// declaration pass reports an error the body pass is withheld entirely. See
/// [`crate::withhold_body_diagnostics_after_declaration_error`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DiagnosticPhase {
    /// Reported while checking declarations. Never withheld.
    #[default]
    Declaration,
    /// Reported while checking a method body, a field initializer, or a constructor initializer.
    Body,
}

/// A semantic diagnostic: its kind, the source range it concerns, and which pass reported it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// What went wrong, with the detail needed to render the message.
    pub kind: DiagnosticKind,
    /// The source range the diagnostic concerns.
    pub span: Span,
    /// Which pass reported this. Set by the binder as it leaves a body; everything else is a
    /// declaration, which is why the default is the conservative one -- a diagnostic nobody
    /// classified is never withheld.
    pub phase: DiagnosticPhase,
}

impl Diagnostic {
    /// Creates a diagnostic of `kind` over `span`, in the DECLARATION phase. The binder re-tags
    /// the range it reported while inside a body, so every construction site can stay unaware of
    /// the distinction.
    #[must_use]
    pub fn new(kind: DiagnosticKind, span: Span) -> Diagnostic {
        Diagnostic {
            kind,
            span,
            phase: DiagnosticPhase::Declaration,
        }
    }

    /// The numeric part of the code. Pair it with [`Diagnostic::namespace`] to render it.
    #[must_use]
    pub fn code(&self) -> u16 {
        self.kind.code()
    }

    /// Which diagnostic namespace the code belongs to -- `CS` for anything csc also has a concept
    /// of, `LAM` for the conditions it does not.
    #[must_use]
    pub fn namespace(&self) -> CodeNamespace {
        self.kind.namespace()
    }

    /// The severity, from the diagnostic's kind.
    #[must_use]
    pub fn severity(&self) -> Severity {
        self.kind.severity()
    }
}

#[cfg(test)]
mod namespace_tests {
    use super::{CodeNamespace, DiagnosticKind};
    use alloc::format;
    use lamella_syntax::version::LanguageVersion;

    #[test]
    fn the_lamella_namespace_is_for_conditions_csc_has_no_code_for() {
        let kind = DiagnosticKind::FeatureNotInThisBuild {
            feature: "generics".into(),
            permitted_by: LanguageVersion::CSharp7,
        };
        assert_eq!(kind.namespace(), CodeNamespace::Lam);
        assert_eq!(kind.code(), 1);
        assert_eq!(CodeNamespace::Lam.prefix(), "LAM");

        let rendered = format!("{kind}");
        assert_eq!(
            rendered,
            "Feature 'generics' is permitted by C# 7.0 but is not provided by this build of Lamella."
        );
        assert!(
            !rendered.contains("Please use language version"),
            "this diagnostic exists BECAUSE that sentence would be wrong here"
        );
        assert!(!rendered.contains("yet"), "a knob is a configuration, not a gap");
    }

    #[test]
    fn a_lamella_code_is_six_or_seven_characters_like_the_rest_of_the_ecosystem() {
        let rendered = format!("{}{:04}", CodeNamespace::Lam.prefix(), 1u16);
        assert_eq!(rendered, "LAM0001");
        assert_eq!(rendered.len(), 7);
        assert_eq!(format!("{}{:04}", CodeNamespace::Cs.prefix(), 649u16).len(), 6);
    }
}

impl DiagnosticKind {
    /// Which namespace this kind's code belongs to.
    ///
    /// `Cs` for everything csc also has a concept of, which is nearly all of it. A new arm here is
    /// a claim that csc has NO code for the condition -- check before adding one.
    #[must_use]
    pub fn namespace(&self) -> CodeNamespace {
        match self {
            DiagnosticKind::FeatureNotInThisBuild { .. }
            | DiagnosticKind::MemberSignatureNotSupported { .. } => CodeNamespace::Lam,
            _ => CodeNamespace::Cs,
        }
    }
}

/// The namespace a diagnostic code belongs to.
///
/// **`CS` MEANS WHAT csc MEANS BY IT, ALWAYS.** A code is a search key: a developer who hits
/// `CS0649` will look it up and land on
/// csc's documentation, so lcsc may only spell a condition `CS` when csc has that same concept. It
/// follows that a condition csc has NO concept of cannot borrow a `CS` number -- an unused one
/// today is one a future Roslyn release may claim, and then the same key means two things
/// depending on which compiler emitted it.
///
/// **`LAM` IS FOR EXACTLY THOSE CONDITIONS, AND IT IS A SMALL FAMILY.** Almost everything routes to
/// csc's own codes: a BCL surface we do not ship reports as `CS0246`/`CS0234` (which is also how
/// nanoFramework expresses a restricted platform, measured), a construct above the selected dialect
/// reports as the `CS8022` family, and a language feature missing a compiler-required member reports
/// as `CS0518`/`CS0656`. What is left is the condition none of those describe: the dialect permits
/// the construct and THIS BUILD cannot produce it.
///
/// **The user-visible payoff is that the prefix says which kind of problem it is.** A `CS` code is a
/// statement about the language; a `LAM` code is a statement about this build's coverage, and the
/// second kind changes as the compiler grows where the first does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeNamespace {
    /// csc's namespace, for conditions csc also has.
    Cs,
    /// Lamella's, for conditions csc has no concept of.
    Lam,
}

impl CodeNamespace {
    /// The literal prefix that precedes the digits: `CS` or `LAM`.
    ///
    /// Chosen against the widths a Problems pane already carries -- `CS0649` and `CA1822` at six
    /// characters, `IDE0051` and `MSB3021` at seven -- so `LAM0001` costs nothing in a pane already
    /// sized for the built-in analyzers, and one character in raw terminal output where the path
    /// dominates the line anyway. Four digits rather than three because the extra character buys
    /// ranges (compiler coverage, backend, linker, runtime) and three would not.
    #[must_use]
    pub fn prefix(self) -> &'static str {
        match self {
            CodeNamespace::Cs => "CS",
            CodeNamespace::Lam => "LAM",
        }
    }
}

/// A semantic-diagnostic kind, with any detail its message needs.
/// Which position in a member's signature a type occupies, for the accessibility-consistency
/// diagnostics (10.5.4): CS0050-CS0053 (method/field/property), CS0058/CS0059 (delegate),
/// CS0060/CS0061 (base class/interface), and CS7025 (event).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignaturePosition {
    /// A method's return type (CS0050).
    ReturnType,
    /// A method's parameter type (CS0051).
    ParameterType,
    /// A field's type (CS0052).
    FieldType,
    /// A property's type (CS0053).
    PropertyType,
    /// An indexer's element type (CS0054).
    IndexerType,
    /// An indexer's parameter type (CS0055).
    IndexerParameterType,
    /// An operator's return type (CS0056). Also a conversion operator's target type.
    OperatorReturnType,
    /// An operator's parameter type (CS0057).
    OperatorParameterType,
    /// A delegate's return type (CS0058). Here the "member" is the delegate itself.
    DelegateReturnType,
    /// A delegate's parameter type (CS0059). Here the "member" is the delegate itself.
    DelegateParameterType,
    /// An event's (delegate) type (CS7025).
    EventType,
    /// A class's base class (CS0060). Here the "member" is the derived class itself.
    BaseClass,
    /// An interface's base interface (CS0061). Here the "member" is the derived interface itself.
    BaseInterface,
}

impl SignaturePosition {
    /// The `CSxxxx` code for this position.
    fn code(self) -> u16 {
        match self {
            SignaturePosition::ReturnType => 50,
            SignaturePosition::ParameterType => 51,
            SignaturePosition::FieldType => 52,
            SignaturePosition::PropertyType => 53,
            SignaturePosition::IndexerType => 54,
            SignaturePosition::IndexerParameterType => 55,
            SignaturePosition::OperatorReturnType => 56,
            SignaturePosition::OperatorParameterType => 57,
            SignaturePosition::DelegateReturnType => 58,
            SignaturePosition::DelegateParameterType => 59,
            SignaturePosition::BaseClass => 60,
            SignaturePosition::BaseInterface => 61,
            SignaturePosition::EventType => 7025,
        }
    }

    /// The phrase naming the position in the message (`return type`, ..., `base class`).
    fn phrase(self) -> &'static str {
        match self {
            SignaturePosition::ReturnType | SignaturePosition::DelegateReturnType => "return type",
            SignaturePosition::ParameterType | SignaturePosition::DelegateParameterType => {
                "parameter type"
            }
            SignaturePosition::FieldType => "field type",
            SignaturePosition::PropertyType => "property type",
            SignaturePosition::IndexerType => "indexer return type",
            SignaturePosition::IndexerParameterType | SignaturePosition::OperatorParameterType => {
                "parameter type"
            }
            SignaturePosition::OperatorReturnType => "return type",
            SignaturePosition::EventType => "event type",
            SignaturePosition::BaseClass => "base class",
            SignaturePosition::BaseInterface => "base interface",
        }
    }

    /// The kind of member the position belongs to (`method`, `field`, `property`, `class`).
    fn member_kind(self) -> &'static str {
        match self {
            SignaturePosition::ReturnType | SignaturePosition::ParameterType => "method",
            SignaturePosition::FieldType => "field",
            SignaturePosition::PropertyType => "property",
            SignaturePosition::IndexerType => "indexer",
            SignaturePosition::IndexerParameterType => "indexer or property",
            SignaturePosition::OperatorReturnType | SignaturePosition::OperatorParameterType => {
                "operator"
            }
            SignaturePosition::DelegateReturnType | SignaturePosition::DelegateParameterType => {
                "delegate"
            }
            SignaturePosition::EventType => "event",
            SignaturePosition::BaseClass => "class",
            SignaturePosition::BaseInterface => "interface",
        }
    }
}

/// Which noun CS0305/CS0308 name, because csc's message templates are parameterized by it:
/// *"Using the generic **{0}** '{1}' requires {2} type arguments"*. Both codes serve types and
/// methods, and the two messages differ ONLY in this word -- so it is carried rather than baked in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenericMember {
    /// A generic type: csc says "type" and quotes `Box<T>`.
    Type,
    /// A generic method: csc says "method" and quotes the full signature `C.Id<T>(T)`.
    Method,
}

impl GenericMember {
    /// The word csc puts in the message.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            GenericMember::Type => "type",
            GenericMember::Method => "method",
        }
    }
}

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
    /// `CS0305`: a generic TYPE or METHOD was used with the wrong number of type arguments. It IS
    /// in scope -- at a different arity -- so this is what stands in place of a CS0246 the
    /// programmer could not act on.
    GenericArityMismatch {
        /// The candidate as csc quotes it: the name with its declared parameters, `Box<T>` for a
        /// type and the full signature `C.Id<T>(T)` for a method.
        candidate: Box<str>,
        /// How many type arguments that candidate requires.
        required: usize,
        /// Which noun csc puts in the message. See [`GenericMember`].
        member: GenericMember,
    },
    /// `CS0308`: a NON-generic type or method was used with type arguments (`Plain<int>`). A
    /// separate code from CS0305 in csc, measured because there is no arity to suggest.
    NonGenericTypeWithTypeArguments {
        /// The type or method named, as csc quotes it.
        name: Box<str>,
        /// Which noun csc puts in the message. See [`GenericMember`].
        member: GenericMember,
    },
    /// `CS0031`: a constant value is outside the range of the type it is being converted to
    /// (the constant-expression conversion, 13.1.7), e.g. `byte b = 256;`.
    ConstantOutOfRange {
        /// The constant value, as rendered.
        value: Box<str>,
        /// The target type.
        to: Box<str>,
    },
    /// `CS0133`: the value of an `enum` member must be a compile-time constant (21.4), so a
    /// non-constant initializer -- a method call, object creation, an instance member -- is rejected.
    NonConstantEnumMember {
        /// The qualified member name, e.g. `E.A`.
        member: Box<str>,
    },
    /// `CS0220`: a constant arithmetic operation overflows the result type in a checked context
    /// (14.16), e.g. `int.MaxValue + 1`. Constant expressions are checked by default, so this
    /// fires unless an explicit `unchecked` context suppresses it.
    ConstantOverflowInCheckedContext,
    /// `CS0221`: a constant CONVERSION (an explicit cast, or a narrowing) overflows the target type
    /// in a checked context (14.16 / 13.1.7). Constant conversions are checked by default, so this
    /// fires unless an explicit `unchecked` context suppresses it -- e.g. `checked((byte)300)`.
    CheckedConstantConversionOverflow {
        /// The constant value, as rendered.
        value: Box<str>,
        /// The target type.
        to: Box<str>,
    },
    /// `CS0677`: a `volatile` field must be of a permitted type (17.4.3) -- a reference type, one of
    /// byte/sbyte/short/ushort/int/uint/char/float/bool, or an enum with one of those bases. Any
    /// other type (long, ulong, double, decimal, a struct, an enum with a 64-bit base) is rejected.
    VolatileFieldType {
        /// The qualified field name, e.g. `C.field`.
        field: Box<str>,
        /// The field's disallowed type, as rendered.
        ty: Box<str>,
    },
    /// `CS1599`: a method or delegate may not RETURN a restricted type.
    ///
    /// `System.TypedReference`, `System.ArgIterator` and `System.RuntimeArgumentHandle` are the
    /// CLI's restricted types: a value of one carries a managed pointer into the frame that made
    /// it, so it must never outlive that frame. This rule and its three siblings
    /// ([`DiagnosticKind::RestrictedTypeField`], [`DiagnosticKind::RestrictedTypeArrayElement`],
    /// [`DiagnosticKind::RestrictedTypeByReference`]) are what enforce that structurally --
    /// returning one lets it escape upward. A local and a BY-VALUE parameter stay legal, which is
    /// where such a value is meant to live.
    RestrictedTypeReturn {
        /// The restricted type's simple name, as the message quotes it (`TypedReference`).
        ty: Box<str>,
    },
    /// `CS0610`: a field or property may not be OF a restricted type -- that would put a
    /// frame-bound pointer on the heap. See [`DiagnosticKind::RestrictedTypeReturn`].
    RestrictedTypeField {
        /// The restricted type's simple name, as the message quotes it.
        ty: Box<str>,
    },
    /// `CS0611`: an array element may not be OF a restricted type, for the same reason a field may
    /// not be. See [`DiagnosticKind::RestrictedTypeReturn`].
    RestrictedTypeArrayElement {
        /// The restricted type's simple name, as the message quotes it.
        ty: Box<str>,
    },
    /// `CS1601`: a `ref` or `out` parameter may not be OF a restricted type -- an out-slot is a way
    /// out of the frame. A BY-VALUE parameter of the same type is legal. See
    /// [`DiagnosticKind::RestrictedTypeReturn`].
    RestrictedTypeByReference {
        /// The restricted type's simple name, as the message quotes it.
        ty: Box<str>,
    },
    /// `CS0558`: a user-defined operator must be declared `static` and `public` (17.9.1); one
    /// missing either modifier is rejected.
    OperatorMustBeStaticAndPublic {
        /// The operator's signature, as csc names it (`C.operator +(C, C)`).
        signature: Box<str>,
    },
    /// `CS0418`: a class declared both `abstract` and `sealed` is contradictory -- an abstract type
    /// must be extended, a sealed one cannot be.
    AbstractTypeSealedOrStatic {
        /// The offending type's name.
        type_name: Box<str>,
    },
    /// `CS0112`: a `static` member cannot also be `virtual`, `abstract`, or `override` -- static
    /// members are not part of the virtual dispatch a derived type overrides.
    StaticMemberCannotBeVirtual {
        /// The offending inheritance modifier (`virtual` / `abstract` / `override`).
        modifier: Box<str>,
    },
    /// `CS1008`: an enum's underlying type must be one of the eight integer types (21.1); a base
    /// that resolves to anything else (bool, char, a floating type, a struct) is rejected.
    EnumUnderlyingTypeExpected,
    /// `CS1721`: a class may name at most one base class (10.1.4); two class bases in the base list
    /// are rejected.
    MultipleClassBases {
        /// The deriving type's name.
        type_name: Box<str>,
        /// The first base class named.
        first: Box<str>,
        /// The second base class named.
        second: Box<str>,
    },
    /// `CS0066`: an event's type must be a delegate type (17.7); a non-delegate type is rejected.
    EventTypeMustBeDelegate {
        /// The event's qualified name (`C.E`).
        event: Box<str>,
    },
    /// `CS1020`: a two-parameter operator declaration must name a binary-overloadable operator
    /// (17.9.2), so a unary-only operator (`!`, `~`, `++`, `--`, `true`, `false`) given two
    /// parameters is rejected.
    OverloadableBinaryOperatorExpected,
    /// `CS0578`: the `[Conditional]` attribute (24.4.2) requires a `void`-returning method; a method
    /// with a non-`void` return type is rejected.
    ConditionalMethodMustReturnVoid {
        /// The method's display signature (`C.M()`).
        method: Box<str>,
    },
    /// `CS0509`: a class cannot derive from a `sealed` type (10.1.1.2).
    DeriveFromSealed {
        /// The deriving type's name (`C`).
        derived: Box<str>,
        /// The sealed base type's name (`B`).
        base: Box<str>,
    },
    /// `CS1527`: a type defined directly in a namespace (10.5) may be `public` or `internal`, but
    /// not `private`, `protected`, or `protected internal`.
    NamespaceElementBadAccessibility,
    /// `CS0029`: no implicit conversion exists between two types.
    NoImplicitConversion {
        /// The source type.
        from: Box<str>,
        /// The target type.
        to: Box<str>,
    },
    /// `CS0037`: the `null` literal is assigned to a non-nullable value type.
    CannotConvertNullToValueType {
        /// The non-nullable value type.
        to: Box<str>,
    },
    /// `CS0119`: a type name is used where a value is required.
    TypeUsedAsValue {
        /// The type's name.
        type_name: Box<str>,
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
    /// `CS1656`: assignment to a method group, which is not a variable.
    CannotAssignToMethodGroup {
        /// The method group's name.
        name: Box<str>,
    },
    /// `CS1656`: an assignment to a local the language owns rather than the body -- a `foreach`
    /// iteration variable (rebound from the collection each pass, 15.8.4) or a `using` resource
    /// (disposed on the way out, 15.13). The message names which kind it was.
    CannotAssignToReadonlyLocal {
        /// The local's name.
        name: Box<str>,
        /// What holds it, as the message quotes it (`foreach iteration variable`).
        kind: &'static str,
    },
    /// `CS0117`: the type does not contain a definition for the named member.
    MemberNotFound {
        /// The type the member was looked for on.
        type_name: Box<str>,
        /// The member name that was not found.
        member: Box<str>,
    },
    /// `LAM0002`: the member EXISTS on a referenced type and this build could not decode its
    /// signature, so it could not be bound.
    MemberSignatureNotSupported {
        /// The type the member was looked for on.
        type_name: Box<str>,
        /// The member name, as the assembly spells it.
        member: Box<str>,
    },
    /// `CS1501`: no overload of the method takes the given number of arguments.
    NoOverloadForArgumentCount {
        /// The method name.
        method: Box<str>,
        /// The number of arguments supplied.
        count: u32,
    },
    /// `CS0518`: a predefined type's backing `System` type is not defined or imported --
    /// the compilation has a corlib, but that corlib does not declare this type (e.g. a
    /// no-float corlib and `double`).
    PredefinedTypeMissing {
        /// The backing type's full name, e.g. `System.Double`.
        full_name: Box<str>,
    },
    /// `CS0190`: a bare `__arglist` outside a vararg member's body.
    ArglistOutsideVarargMethod,
    /// `CS0226`: an `__arglist(...)` expression somewhere other than the final argument
    /// of a call or object creation.
    ArglistOutsideCall,
    /// `CS7036`: a call to a vararg member supplied no `__arglist(...)` at the sentinel
    /// position (csc reports the sentinel as a missing required parameter).
    NoArgumentForArglist {
        /// The member's display signature, e.g. `P.M(int, __arglist)`.
        method: Box<str>,
    },
    /// `CS7036`: a call supplied too FEW arguments and exactly ONE candidate exists, so csc
    /// names the first parameter left without an argument instead of reporting a bare count
    /// mismatch. With two or more candidates it falls back to the count (`CS1501` for a method,
    /// `CS1729` for a constructor) -- measured, both ways.
    MissingArgumentForParameter {
        /// The first parameter with no corresponding argument.
        parameter: Box<str>,
        /// The candidate's display signature, e.g. `B.B(int)` or `C.M(int, out int)`.
        method: Box<str>,
    },
    /// `CS1620`: an argument reached a by-reference parameter under the WRONG modifier --
    /// `out` where the parameter is `ref`, or the reverse. Both spellings give the argument
    /// the same `ByRef` type, so overload resolution cannot tell them apart and the call
    /// resolves; only the parameter's recorded mode distinguishes them. csc names the mode
    /// the PARAMETER wants, not the one the argument wrote.
    ///
    /// A missing modifier where the parameter is byref is also this code in csc, but it
    /// cannot arise here: an unmodified argument does not share the parameter's `ByRef`
    /// type, so no candidate is applicable and the call is `CS1503` instead.
    ArgumentModeRequired {
        /// The 1-based argument position.
        index: u32,
        /// The keyword the parameter requires -- `ref` or `out`.
        keyword: Box<str>,
    },
    /// `CS0663`: two members differ ONLY in whether a by-reference parameter is `ref` or `out`.
    /// Both spellings give the parameter the same by-reference type, so they do not overload --
    /// and csc gives that its own code rather than the generic duplicate-member one, because the
    /// repair is different: one of the two modifiers has to change, not one of the signatures.
    ///
    /// It is also the rule that makes `CS1620` single-valued: a resolved call's by-reference
    /// parameter has exactly one mode, so "which keyword did the parameter want" has one answer.
    OverloadDiffersOnlyByRefOut {
        /// The declaring type.
        type_name: Box<str>,
        /// `method` or `constructor` -- csc changes the noun.
        member_kind: &'static str,
        /// The modifier on the LATER declaration. csc names this one first.
        current: Box<str>,
        /// The modifier on the earlier one.
        previous: Box<str>,
    },
    /// `CS1615`: an argument carried a `ref`/`out` modifier its parameter does not take. The
    /// mirror of [`Self::ArgumentModeRequired`], and it names the keyword the ARGUMENT wrote
    /// rather than one the parameter wants -- the parameter wants none.
    ArgumentModeForbidden {
        /// The 1-based argument position.
        index: u32,
        /// The keyword the argument carried -- `ref` or `out`.
        keyword: Box<str>,
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
    /// `CS0181`: an attribute constructor parameter whose type cannot appear in metadata as an
    /// attribute argument (24.1.3). An attribute's arguments are baked into the assembly, so the
    /// legal set is what the metadata blob can encode.
    InvalidAttributeParameterType {
        /// The parameter's declared name, as csc quotes it.
        parameter: Box<str>,
        /// The parameter's type.
        type_name: Box<str>,
    },
    /// `CS0617`: a named attribute argument naming a member that exists and is reachable but can
    /// never be assigned in an attribute -- a non-public, static, readonly or const field; a
    /// property that is not public, is static, or lacks an accessor; a method; a nested type.
    NotAValidNamedAttributeArgument {
        /// The member name as written, unqualified -- csc names it bare here.
        name: Box<str>,
    },
    /// `CS1540`: a `protected` instance member reached from a derived class through a qualifier
    /// whose type is not that class or one derived from it (10.5.3). Deriving from a class grants
    /// access to its protected members *through instances of the deriving class* -- not through an
    /// arbitrary instance of the base, which need not be one of ours.
    ProtectedQualifier {
        /// The qualified member name.
        member: Box<str>,
        /// The static type of the qualifier the access was written through.
        qualifier: Box<str>,
        /// The type the access is written in, as csc names it.
        accessing: Box<str>,
    },
    /// `CS0070`: a field-like event used from outside its declaring type anywhere other
    /// than the left of `+=`/`-=`.
    EventOutsideAddRemove {
        /// The qualified event name.
        event: Box<str>,
    },
    /// `CS0139`: a `break`/`continue` with no enclosing loop (or switch, for `break`).
    NoEnclosingLoop,
    /// `CS0017`: the program declares more than one entry point (two or more valid `static Main`).
    MultipleEntryPoints,
    /// `CS0428`: a method group is used where a non-delegate type is expected (it was not
    /// invoked and does not convert to the target).
    MethodGroupToNonDelegate {
        /// The method group's name.
        method: Box<str>,
        /// The non-delegate target type.
        target: Box<str>,
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
    SwitchFallThrough {
        /// The offending section's LAST label, rendered as `case 5:` or `default:`.
        label: Box<str>,
    },
    /// `CS8070`: control can fall out of the switch entirely, because the offending
    /// section is the LAST one and so has no following section to fall into. The
    /// first-edition rule is the same one behind [`Self::SwitchFallThrough`] (a switch
    /// section's end point must not be reachable); only the reported code differs.
    SwitchFallOutFinal {
        /// The final section's LAST label, rendered as `case 5:` or `default:`.
        label: Box<str>,
    },
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
    /// `CS0101`: the namespace already contains a definition for this type name (a
    /// duplicate type declaration -- C# 1.0 has no partial types).
    DuplicateTypeInNamespace {
        /// The namespace the type is declared in, or `<global namespace>` for the global one.
        namespace: Box<str>,
        /// The duplicated type name.
        name: Box<str>,
    },
    /// `CS0737`: a class member matches an interface member's signature but is not public, so it
    /// cannot implement it (13.4.4). An implicit implementation is public or it is nothing.
    InterfaceImplementationNotPublic {
        /// The class that fails to implement the interface.
        type_name: Box<str>,
        /// The interface member left unimplemented (`I.M()`).
        interface_member: Box<str>,
        /// The class member that would have implemented it (`C.M()`).
        member: Box<str>,
    },
    /// `CS0738`: a class member matches an interface member's signature and is public, but returns
    /// a different type, so it implements nothing.
    InterfaceImplementationReturnType {
        /// The class that fails to implement the interface.
        type_name: Box<str>,
        /// The interface member left unimplemented (`I.M()`).
        interface_member: Box<str>,
        /// The class member that would have implemented it (`C.M()`).
        member: Box<str>,
        /// The return type the interface requires.
        return_type: Box<str>,
    },
    /// `CS0768`: a constructor reaches itself through a chain of `: this(...)` initializers
    /// (17.10.1), so no constructor in the chain could ever finish.
    ConstructorInitializerCycle {
        /// The constructor, as the message names it (`C.C(int)`).
        constructor: Box<str>,
    },
    /// `CS1674`: a `using` resource whose type does not implement `System.IDisposable` (15.13),
    /// so there is nothing for the generated `finally` to dispose.
    UsingRequiresDisposable {
        /// The resource type, as the message names it.
        ty: Box<str>,
    },
    /// `CS0616`: a type used as an attribute that does not derive from `System.Attribute` (24.2),
    /// so it is not an attribute class at all.
    NotAnAttributeClass {
        /// The type named in the attribute, as the message quotes it.
        type_name: Box<str>,
    },
    /// `CS0579`: the same attribute is applied twice to one target (24.2).
    DuplicateAttribute {
        /// The attribute's simple name, as the message quotes it.
        name: Box<str>,
    },
    /// `CS0182`: an attribute argument that is not a constant, a `typeof`, or an array creation.
    /// An attribute is baked into metadata, so nothing evaluated at run time can supply one.
    NonConstantAttributeArgument,
    /// `CS0227`: the source contains `unsafe` but the compilation was not given `/unsafe`. The
    /// language supports unsafe code in full; a compilation opts IN to containing it, exactly as
    /// csc requires.
    UnsafeCodeRequiresOption,
    /// `CS0133`: a `const` field's initializer is not a constant expression. Its value is baked
    /// into every use site (17.4.2), so nothing evaluated at run time can supply it.
    NonConstantFieldInitializer {
        /// The field, qualified as the message names it (`C.Value`).
        field: Box<str>,
    },
    /// `CS1019`: an operator declared in a unary form takes other than one parameter -- here, a
    /// conversion operator, which is always unary (17.9.4).
    OverloadableUnaryOperatorExpected,
    /// `CS1017`: a `catch` clause after the general one. A general `catch` catches every
    /// exception (15.10), so a clause behind it could never run.
    CatchAfterGeneralCatch,
    /// `CS0556`: a user-defined conversion that neither converts to nor from the type declaring
    /// it. A conversion operator exists to bridge ITS type and another (17.9.4).
    ConversionMustInvolveEnclosingType,
    /// `CS1579`: a `foreach` collection that is not enumerable -- it declares no public
    /// `GetEnumerator` and is not an array (15.8.4), so the loop has nothing to iterate.
    ForEachNotEnumerable {
        /// The collection's type, as the message names it (twice).
        ty: Box<str>,
    },
    /// `CS1536`: a parameter declared `void`. `void` is the absence of a value, so it names no
    /// storage a parameter could hold -- the same reason a `void` local is `CS1547`.
    VoidParameter,
    /// `CS0847`: a rectangular array initializer list has the wrong length -- it disagrees either
    /// with the written dimension or with the other lists at its level (19.6). The shape is
    /// rectangular, so every list at one level holds the same count.
    ArrayInitializerLength {
        /// The length the list was required to have.
        length: u64,
    },
    /// `CS0153`: a `goto case` outside any `switch`. The target is a case of the enclosing
    /// switch (15.10), so outside one it names nothing.
    GotoCaseOutsideSwitch,
    /// `CS0156`: a bare `throw;` outside a `catch` clause. It re-throws the exception being
    /// handled (15.9.5), and outside a catch there is none in flight.
    RethrowOutsideCatch,
    /// `CS0157`: a `return`, `break` or `continue` that would transfer control out of a
    /// `finally` block. A finally runs precisely because control is already leaving (15.10).
    ControlLeavesFinally,
    /// `CS1537`: two `using` directives in one namespace give the same alias. An alias names one
    /// type (10.4.1), so the second declares nothing.
    DuplicateUsingAlias {
        /// The repeated alias.
        alias: Box<str>,
    },
    /// `CS0020`: a division or remainder by a constant zero. The operation has no value, and it is
    /// a compile-time fact rather than a run-time one.
    DivisionByConstantZero,
    /// `CS0185`: the operand of a `lock` is a value type. A monitor is taken on a reference
    /// (15.12); boxing a value would lock a fresh box each time and guard nothing.
    LockRequiresReferenceType {
        /// The operand's type, as the message names it.
        ty: Box<str>,
    },
    /// `CS0144`: `new` on an abstract class or an interface. Neither can be instantiated
    /// (14.5.10.1) -- there is no complete implementation to construct.
    CannotCreateAbstractInstance {
        /// The abstract type named, as the message spells it.
        type_name: Box<str>,
    },
    /// `CS0515`: a static constructor declares an accessibility modifier. It is never called by
    /// user code -- the runtime runs it -- so it has no accessibility to declare (17.11).
    StaticConstructorAccessibility {
        /// The static constructor, as the message names it (`C.C()`).
        member: Box<str>,
    },
    /// `CS1520`: a member that is not a constructor and declares no return type. A method
    /// declaration whose name does not repeat the enclosing type's is missing its return type
    /// (17.5), which is how csc reads it.
    MethodMustHaveReturnType,
    /// `CS0527`: a type in a struct's or interface's base list is not an interface. Neither may
    /// name a base class (11.2, 13.1.3) -- a struct's is always `System.ValueType`.
    BaseTypeNotInterface {
        /// The offending base type, as the message names it.
        base: Box<str>,
    },
    /// `CS0574`: a destructor's name does not repeat its class's. A destructor is named for the
    /// type it finalizes (17.12), so any other name declares nothing.
    DestructorNameMismatch,
    /// `CS0575`: a destructor declared somewhere other than a class. Only a class is finalized
    /// (17.12); a struct has no finalizer slot.
    DestructorNotInClass,
    /// `CS0100`: a parameter list declares the same name twice. The parameters of a member share
    /// one declaration space (10.3), so the second is not a new parameter.
    DuplicateParameterName {
        /// The repeated parameter name.
        name: Box<str>,
    },
    /// `CS0102`: the type already contains a definition for this member name.
    DuplicateMember {
        /// The type that declares it twice.
        type_name: Box<str>,
        /// The duplicated member name.
        member: Box<str>,
    },
    /// `CS0111`: the type already defines a method with the same name and parameter
    /// types (a duplicate, not a valid overload).
    DuplicateMethod {
        /// The type that declares it twice.
        type_name: Box<str>,
        /// The duplicated method name.
        member: Box<str>,
    },
    /// `CS0140`: two labels in the same method body share a name.
    DuplicateLabel {
        /// The duplicated label.
        label: Box<str>,
    },
    /// `CS0159`: a `goto` targets a label that does not exist in scope.
    UndefinedLabel {
        /// The label the `goto` named.
        label: Box<str>,
    },
    /// `CS0164` (warning): a declared label that no `goto` targets.
    UnreferencedLabel,
    /// `CS0500`: an `abstract` method declares a body.
    AbstractMethodWithBody {
        /// The method name.
        member: Box<str>,
    },
    /// `CS0501`: a non-abstract, non-extern method in a class or struct declares no body.
    MethodMustHaveBody {
        /// The method's qualified signature (`C.M()`).
        method: Box<str>,
    },
    /// A language feature outside the dialect being compiled -- an automatically-implemented
    /// property (C# 3.0), a `static` class (C# 2.0), and so on. lcsc GATES every feature above the
    /// selected version, even ones whose emit path is already implemented -- see the
    /// `GATED FEATURE (ISO-N)` markers.
    ///
    /// **THE CODE IS NOT FIXED. It names the version being COMPILED**, not the one the feature
    /// needs: `CS8022` at C# 1, `CS8023` at C# 2, and so on up to `CS8059` at C# 6 -- see
    /// [`LanguageVersion::feature_gate_code`] for the measured table. The REQUIRED version appears
    /// only in the message. A single hard-coded code was right while ISO-1 was the only selectable
    /// dialect and becomes wrong the moment a second one exists, which is why `current` is carried
    /// here rather than assumed at the format site.
    FeatureRequiresLaterVersion {
        /// The feature name (e.g. "automatically implemented properties").
        feature: Box<str>,
        /// The minimum C# version, as rendered (e.g. "C# 3.0").
        required: Box<str>,
        /// The version being compiled, which selects both the code and the message's "in C# N".
        current: LanguageVersion,
    },
    /// `LAM0001`: the selected dialect PERMITS this construct and this build cannot produce it.
    ///
    /// **Deliberately covers two causes with one message, because they are one fact to the person
    /// reading it**: the feature is not implemented, or a capability knob turned it off. Either way
    /// this build cannot compile the construct, and which of the two it is changes nothing they can
    /// do about it in the source. That is also why the text must not say "yet" -- it would be wrong
    /// for the knob case, and a knob is a supported configuration rather than a gap.
    ///
    /// **It exists because [`Self::FeatureRequiresLaterVersion`] would be a LIE here.** Telling
    /// someone to "use language version 7 or greater" when they already passed exactly that sends
    /// them looking for a switch that cannot help. The message names the permitting dialect
    /// precisely so they stop suspecting the language version.
    FeatureNotInThisBuild {
        /// The feature name, spelled as csc spells it (e.g. "generics").
        feature: Box<str>,
        /// The dialect that permits the construct -- the one the user already selected.
        permitted_by: LanguageVersion,
    },
    /// `CS8703`: an interface member declares an access modifier. Every interface member is
    /// implicitly public in C# 1.0 (13.2), so the modifier is not merely redundant -- it is a
    /// later-version form, and csc gives it its own code because the repair is to delete it.
    InterfaceMemberModifier {
        /// The offending modifier, as the message names it.
        modifier: Box<str>,
    },
    /// `CS0513`: an `abstract` member is declared in a non-abstract type.
    AbstractMemberInNonAbstractType {
        /// The member's qualified signature (`C.M()`).
        member: Box<str>,
        /// The containing type's name.
        type_name: Box<str>,
    },
    /// `CS0621`: a `virtual` or `abstract` member is private (explicitly or by default).
    VirtualOrAbstractMemberIsPrivate {
        /// The member's qualified signature (`C.M()`).
        member: Box<str>,
    },
    /// `CS0106`: a modifier is not valid for the item it is applied to -- here, a
    /// non-private `virtual` or `abstract` member of a struct.
    ModifierNotValidForItem {
        /// The offending modifier keyword (`virtual` / `abstract`).
        modifier: Box<str>,
    },
    /// `CS0238`: a `sealed` member is not an `override`, so there is nothing to seal.
    SealedMemberIsNotOverride {
        /// The member's qualified signature (`C.M()` / `C.P`).
        member: Box<str>,
    },
    /// `CS0666`: a struct declares a `protected` (or `protected internal`) member.
    ProtectedMemberInStruct {
        /// The member's qualified name (`S.x` / `S.M()` / `S.P`).
        member: Box<str>,
    },
    /// `CS0542`: a member is named the same as its enclosing type (only a constructor may be).
    MemberNamedLikeType {
        /// The enclosing type's name (which the member illegally repeats).
        type_name: Box<str>,
    },
    /// `CS0132`: a static constructor declares parameters.
    StaticConstructorHasParameters {
        /// The constructor's qualified signature (`C.C(int)`).
        constructor: Box<str>,
    },
    /// `CS0670`: a field is declared with `void` type.
    VoidField,
    /// `CS1547`: a local variable is declared with `void` type -- `void` is not a
    /// local-variable-type (15.5.1), so the keyword "cannot be used in this context".
    VoidLocal,
    /// `CS0151`: a `switch` governing expression is not an integral type, char, string, or enum
    /// (15.7.2), and no user-defined implicit conversion reaches one.
    SwitchGoverningType,
    /// `CS0236`: a field initializer references an instance member (a non-static field, method, or
    /// property) of the containing type through the implicit `this` (17.4.5) -- a field initializer
    /// runs with no instance, so there is no `this` to read it from. A static member, an external
    /// member, an instance member reached through an explicit object, and a literal are all fine.
    FieldInitializerReference {
        /// The referenced member, qualified by its declaring type (`C.first`, `C.M()`).
        member: Box<str>,
    },
    /// `CS0231`: a `params` parameter is not the last parameter in the list.
    ParamsNotLast,
    /// `CS0225`: a `params` parameter is not a single-dimensional array.
    ParamsNotArray,
    /// `CS0050`-`CS0053`: a type in a public member's signature is less accessible than the member.
    InconsistentAccessibility {
        /// Where the offending type appears in the signature.
        position: SignaturePosition,
        /// The less-accessible type's qualified name.
        type_name: Box<str>,
        /// The member's qualified signature or name.
        member: Box<str>,
    },
    /// `CS0145`: a `const` field is declared without a value.
    ConstFieldRequiresValue,
    /// `CS0525`: an interface declares an instance (non-const) field.
    InterfaceCannotContainInstanceField,
    /// `CS0191`: a `readonly` field is assigned outside a constructor.
    ReadonlyAssignment {
        /// The field name.
        field: Box<str>,
    },
    /// `CS0200`: a property with no `set` accessor is assigned.
    PropertyCannotBeAssigned {
        /// The property, qualified as csc renders it (`C.P`).
        property: Box<str>,
    },
    /// `CS1061`: a member is not found on the type of an EXPRESSION (as opposed to `CS0117`, which
    /// is the same absence on a type named directly).
    MemberNotFoundOnExpression {
        /// The type the member was looked for on.
        type_name: Box<str>,
        /// The member name.
        member: Box<str>,
    },
    /// `CS1922`: a collection initializer targets a type that does not implement `IEnumerable`.
    ///
    /// Separate from the missing-`Add` case (`CS1061`) because the two name different repairs --
    /// implement the interface, or supply the method -- and csc reports them separately. Measured.
    NotACollectionInitializerTarget {
        /// The type being created.
        type_name: Box<str>,
    },
    /// `CS1914`: a STATIC field or property is named in an object initializer.
    ///
    /// Its own code rather than the ordinary "cannot assign" family, because the mistake is
    /// specific: an object initializer assigns members OF THE NEW OBJECT, and a static member does
    /// not belong to one. MEASURED -- csc reports this even when the initializer is also refused by
    /// the language-version gate, so it is not suppressed by that gate.
    StaticMemberInObjectInitializer {
        /// The member, qualified as csc renders it (`C.F`).
        member: Box<str>,
    },
    /// `CS9034`: a `required` member cannot be assigned -- a `readonly` field, or a property with
    /// no `set` accessor.
    ///
    /// The point of `required` is that every construction must assign the member, so a member
    /// nothing can assign is a contradiction rather than a missing assignment. Distinct from
    /// `CS0106`: `required` IS valid on a field or property, and it is this one that is not
    /// settable.
    RequiredMemberMustBeSettable {
        /// The member, qualified as csc renders it (`C.F`).
        member: Box<str>,
    },
    /// `CS9032`: a `required` member is less visible than the type that declares it.
    ///
    /// A caller that cannot see the member cannot satisfy it, so the type would be
    /// unconstructible from where it is visible.
    RequiredMemberLessVisible {
        /// The member, qualified as csc renders it (`C.F`).
        member: Box<str>,
        /// The declaring type, as csc names it at the end of the sentence.
        containing_type: Box<str>,
    },
    /// `CS9035`: an object creation leaves a `required` member unset.
    RequiredMemberMustBeSet {
        /// The member, qualified as csc renders it (`C.P`).
        member: Box<str>,
    },
    /// `CS9036`: a `required` member is given a NESTED member or collection initializer rather
    /// than a value.
    RequiredMemberNeedsValue {
        /// The member, qualified as csc renders it (`C.F`).
        member: Box<str>,
    },
    /// `CS9030`: an `override` of a `required` member drops the `required`.
    ///
    /// `required` is part of the contract the base slot imposes on every construction, and an
    /// override cannot narrow it -- otherwise constructing the derived type would escape a
    /// requirement the base declared.
    OverrideMustBeRequired {
        /// The overriding member, qualified as csc renders it (`D.P`).
        member: Box<str>,
        /// The base member it overrides (`B.P`).
        base_member: Box<str>,
    },
    /// `CS0535`: a class does not implement an inherited interface member.
    InterfaceMemberNotImplemented {
        /// The class that is missing the implementation.
        type_name: Box<str>,
        /// The interface member it must implement (`I.M`).
        member: Box<str>,
    },
    /// `CS0115`: an `override` method matches no base-class method to override.
    NoMethodToOverride {
        /// The offending method's qualified signature (`C.M()`).
        method: Box<str>,
    },
    /// `CS0506`: an `override` matches a base method that is not `virtual`, `abstract`, or
    /// `override`, so there is no slot to override.
    CannotOverrideNonVirtual {
        /// The overriding method's qualified signature (`D.M()`).
        method: Box<str>,
        /// The base member it cannot override (`B.M()`).
        base: Box<str>,
    },
    /// `CS0216`: a user-defined operator that must be declared in a PAIR was declared alone.
    /// `==`/`!=`, `<`/`>`, `<=`/`>=` and `true`/`false` each require their partner (17.9.2),
    /// so a type cannot support one direction of a comparison without the other.
    OperatorRequiresMatchingOperator {
        /// The declared operator's signature (`C.operator ==(C, C)`).
        operator: Box<str>,
        /// The partner it requires, as a source symbol (`!=`).
        partner: &'static str,
    },
    /// `CS0155`: a `catch` clause names, or a `throw` throws, a type that does not derive
    /// from `System.Exception`. Reported only when the type can be PROVEN not to (15.9.5,
    /// 15.10), so a type this compilation cannot resolve is left alone.
    CaughtTypeMustBeException,
    /// `CS0239`: an `override` whose base member is `sealed`. The base slot is overridable
    /// in principle -- it IS a virtual/override -- but `sealed` closed it (17.5.5), so no
    /// further derived class may take it.
    CannotOverrideSealed {
        /// The overriding method's qualified signature (`D.M()`).
        method: Box<str>,
        /// The sealed base member (`B.M()`).
        base: Box<str>,
    },
    /// `CS0507`: an `override` that declares a different accessibility from the member it
    /// overrides. An override takes the base member's accessibility exactly; it may neither
    /// widen nor narrow it.
    OverrideChangesAccess {
        /// The overriding method's qualified signature (`D.M()`).
        method: Box<str>,
        /// The base member's accessibility keyword, as the message quotes it (`public`).
        access: Box<str>,
        /// The base member overridden (`B.M()`).
        base: Box<str>,
    },
    /// `CS0508`: an `override`'s return type differs from the base member it overrides (C# 1.0
    /// has no covariant return types).
    OverrideReturnTypeMismatch {
        /// The overriding method's qualified signature (`D.M()`).
        method: Box<str>,
        /// The return type the override must have to match the base member.
        return_type: Box<str>,
        /// The base member overridden (`B.M()`).
        base: Box<str>,
    },
    /// `CS1715`: an `override` property or indexer whose TYPE differs from the member it
    /// overrides. The rule is the return-type rule of `CS0508` one member kind over, and csc
    /// gives it its own code and wording because a property has a type rather than a return
    /// type.
    OverridePropertyTypeMismatch {
        /// The overriding member's qualified name (`D.P`, `D.this[int]`).
        property: Box<str>,
        /// The type the override must have to match the base member.
        ty: Box<str>,
        /// The base member overridden (`B.P`).
        base: Box<str>,
    },
    /// `CS0534`: a non-abstract class does not implement an inherited abstract member.
    AbstractMemberNotImplemented {
        /// The non-abstract class missing the implementation.
        type_name: Box<str>,
        /// The inherited abstract member it must implement (`B.M()`).
        member: Box<str>,
    },
    /// `CS0146`: a circular base-class dependency (A : B, B : A).
    CircularBase {
        /// The type whose base chain is circular.
        type_name: Box<str>,
    },
    /// `CS0110`: a circular constant definition (`const A = B; const B = A;`).
    CircularConstant {
        /// The qualified const field name (`C.A`) whose evaluation is circular.
        member: Box<str>,
    },
    /// `CS0529`: a circular base-interface dependency (interface I : J, J : I).
    CircularInterface {
        /// The interface whose base-interface hierarchy is circular.
        type_name: Box<str>,
        /// The directly-inherited interface that leads back into the cycle.
        base: Box<str>,
    },
    /// `CS0523`: a struct field whose type cycles back through value-type fields to the struct
    /// itself (`struct S { S f; }`) -- an infinitely-sized layout.
    StructLayoutCycle {
        /// The qualified field name (`S.f`).
        member: Box<str>,
        /// The field's type, which cycles back to the enclosing struct.
        type_name: Box<str>,
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
    /// `CS0414` (warning): a private field is assigned but its value is never used.
    UnusedField {
        /// The field's qualified name (`C.f`).
        field: Box<str>,
    },
    /// `CS0169` (warning): a private field is never used -- neither read nor written.
    FieldNeverUsed {
        /// The field's qualified name (`C.f`).
        field: Box<str>,
    },
    /// `CS0649` (warning): a private field is read but never assigned, so it keeps its type's
    /// default value.
    FieldNeverAssigned {
        /// The field's qualified name (`C.f`).
        field: Box<str>,
        /// The type's default value, as csc renders it (`0` / `false` / `null`, or empty for a
        /// `char`, an enum, or a struct).
        default: Box<str>,
    },
    /// `CS0162` (warning): a statement can never be reached.
    UnreachableCode,
    /// `CS0120`: an instance member was named with no object -- through a type, or through
    /// an implicit `this` in a static method (where there is none).
    ObjectReferenceRequired {
        /// The qualified member name (`C.x`, or `C.Foo()` for a method).
        member: Box<str>,
    },
    /// `CS0026`: the `this` keyword used in a static method, static property, or static
    /// field initializer, where there is no instance.
    ThisInStaticContext,
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
    /// `CS0149`: a delegate-creation argument is not a method group or a compatible delegate
    /// value (or extra arguments follow it).
    MethodNameExpected,
    /// `CS0123`: the named method (or a delegate value's `Invoke`) matches no overload with the
    /// delegate's signature.
    NoOverloadMatchesDelegate {
        /// The method group's name, or `Type.Invoke` for a delegate-value operand.
        method: Box<str>,
        /// The delegate type being created.
        delegate: Box<str>,
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
    /// `CS0039`: no conversion exists for an `as` expression -- the operand does not become
    /// the target via a reference, boxing, unboxing, wrapping, or null-type conversion.
    AsConversionMissing {
        /// The operand's type.
        from: Box<str>,
        /// The `as` target type.
        to: Box<str>,
    },
    /// `CS0165`: a local variable is read before it is definitely assigned.
    UseOfUnassignedLocal {
        /// The local variable's name.
        name: Box<str>,
    },
    /// `CS0177`: an `out` parameter is not definitely assigned before control leaves the method.
    OutParameterNotAssigned {
        /// The `out` parameter's name.
        parameter: Box<str>,
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
            DiagnosticKind::FeatureNotInThisBuild { .. } => 1,
            DiagnosticKind::TypeNotFound { .. } => 246,
            DiagnosticKind::NameNotFound { .. } => 103,
            DiagnosticKind::GenericArityMismatch { .. } => 305,
            DiagnosticKind::NonGenericTypeWithTypeArguments { .. } => 308,
            DiagnosticKind::ConstantOutOfRange { .. } => 31,
            DiagnosticKind::NonConstantEnumMember { .. } => 133,
            DiagnosticKind::ConstantOverflowInCheckedContext => 220,
            DiagnosticKind::CheckedConstantConversionOverflow { .. } => 221,
            DiagnosticKind::VolatileFieldType { .. } => 677,
            DiagnosticKind::RestrictedTypeReturn { .. } => 1599,
            DiagnosticKind::RestrictedTypeField { .. } => 610,
            DiagnosticKind::RestrictedTypeArrayElement { .. } => 611,
            DiagnosticKind::RestrictedTypeByReference { .. } => 1601,
            DiagnosticKind::OperatorMustBeStaticAndPublic { .. } => 558,
            DiagnosticKind::AbstractTypeSealedOrStatic { .. } => 418,
            DiagnosticKind::StaticMemberCannotBeVirtual { .. } => 112,
            DiagnosticKind::EnumUnderlyingTypeExpected => 1008,
            DiagnosticKind::MultipleClassBases { .. } => 1721,
            DiagnosticKind::EventTypeMustBeDelegate { .. } => 66,
            DiagnosticKind::OverloadableBinaryOperatorExpected => 1020,
            DiagnosticKind::ConditionalMethodMustReturnVoid { .. } => 578,
            DiagnosticKind::DeriveFromSealed { .. } => 509,
            DiagnosticKind::NamespaceElementBadAccessibility => 1527,
            DiagnosticKind::NoImplicitConversion { .. } => 29,
            DiagnosticKind::CannotConvertNullToValueType { .. } => 37,
            DiagnosticKind::TypeUsedAsValue { .. } => 119,
            DiagnosticKind::OperatorNotApplicable { .. } => 19,
            DiagnosticKind::UnaryOperatorNotApplicable { .. } => 23,
            DiagnosticKind::ConditionalTypeMismatch { .. } => 173,
            DiagnosticKind::NotAssignable => 131,
            DiagnosticKind::CannotAssignToMethodGroup { .. } => 1656,
            DiagnosticKind::CannotAssignToReadonlyLocal { .. } => 1656,
            DiagnosticKind::MemberNotFound { .. } => 117,
            DiagnosticKind::MemberSignatureNotSupported { .. } => 2,
            DiagnosticKind::NoOverloadForArgumentCount { .. } => 1501,
            DiagnosticKind::PredefinedTypeMissing { .. } => 518,
            DiagnosticKind::ArglistOutsideVarargMethod => 190,
            DiagnosticKind::ArglistOutsideCall => 226,
            DiagnosticKind::NoArgumentForArglist { .. } => 7036,
            DiagnosticKind::MissingArgumentForParameter { .. } => 7036,
            DiagnosticKind::ArgumentModeRequired { .. } => 1620,
            DiagnosticKind::ArgumentModeForbidden { .. } => 1615,
            DiagnosticKind::OverloadDiffersOnlyByRefOut { .. } => 663,
            DiagnosticKind::ArgumentConversion { .. } => 1503,
            DiagnosticKind::AmbiguousCall { .. } => 121,
            DiagnosticKind::Inaccessible { .. } => 122,
            DiagnosticKind::InvalidAttributeParameterType { .. } => 181,
            DiagnosticKind::NotAValidNamedAttributeArgument { .. } => 617,
            DiagnosticKind::ProtectedQualifier { .. } => 1540,
            DiagnosticKind::EventOutsideAddRemove { .. } => 70,
            DiagnosticKind::NoEnclosingLoop => 139,
            DiagnosticKind::MultipleEntryPoints => 17,
            DiagnosticKind::MethodGroupToNonDelegate { .. } => 428,
            DiagnosticKind::ConstantExpected => 150,
            DiagnosticKind::DuplicateCaseLabel { .. } => 152,
            DiagnosticKind::SwitchFallThrough { .. } => 163,
            DiagnosticKind::SwitchFallOutFinal { .. } => 8070,
            DiagnosticKind::DuplicateLocal { .. } => 128,
            DiagnosticKind::LocalShadowsEnclosing { .. } => 136,
            DiagnosticKind::IllegalStatementExpression => 201,
            DiagnosticKind::DuplicateTypeInNamespace { .. } => 101,
            DiagnosticKind::DuplicateMember { .. } => 102,
            DiagnosticKind::DuplicateParameterName { .. } => 100,
            DiagnosticKind::InterfaceImplementationNotPublic { .. } => 737,
            DiagnosticKind::InterfaceImplementationReturnType { .. } => 738,
            DiagnosticKind::ConstructorInitializerCycle { .. } => 768,
            DiagnosticKind::UsingRequiresDisposable { .. } => 1674,
            DiagnosticKind::NotAnAttributeClass { .. } => 616,
            DiagnosticKind::DuplicateAttribute { .. } => 579,
            DiagnosticKind::NonConstantAttributeArgument => 182,
            DiagnosticKind::UnsafeCodeRequiresOption => 227,
            DiagnosticKind::NonConstantFieldInitializer { .. } => 133,
            DiagnosticKind::OverloadableUnaryOperatorExpected => 1019,
            DiagnosticKind::CatchAfterGeneralCatch => 1017,
            DiagnosticKind::ConversionMustInvolveEnclosingType => 556,
            DiagnosticKind::ForEachNotEnumerable { .. } => 1579,
            DiagnosticKind::VoidParameter => 1536,
            DiagnosticKind::ArrayInitializerLength { .. } => 847,
            DiagnosticKind::GotoCaseOutsideSwitch => 153,
            DiagnosticKind::RethrowOutsideCatch => 156,
            DiagnosticKind::ControlLeavesFinally => 157,
            DiagnosticKind::DuplicateUsingAlias { .. } => 1537,
            DiagnosticKind::DivisionByConstantZero => 20,
            DiagnosticKind::LockRequiresReferenceType { .. } => 185,
            DiagnosticKind::CannotCreateAbstractInstance { .. } => 144,
            DiagnosticKind::StaticConstructorAccessibility { .. } => 515,
            DiagnosticKind::MethodMustHaveReturnType => 1520,
            DiagnosticKind::BaseTypeNotInterface { .. } => 527,
            DiagnosticKind::DestructorNameMismatch => 574,
            DiagnosticKind::DestructorNotInClass => 575,
            DiagnosticKind::DuplicateMethod { .. } => 111,
            DiagnosticKind::DuplicateLabel { .. } => 140,
            DiagnosticKind::UndefinedLabel { .. } => 159,
            DiagnosticKind::UnreferencedLabel => 164,
            DiagnosticKind::AbstractMethodWithBody { .. } => 500,
            DiagnosticKind::MethodMustHaveBody { .. } => 501,
            DiagnosticKind::FeatureRequiresLaterVersion { current, .. } => current.feature_gate_code(),
            DiagnosticKind::InterfaceMemberModifier { .. } => 8703,
            DiagnosticKind::AbstractMemberInNonAbstractType { .. } => 513,
            DiagnosticKind::VirtualOrAbstractMemberIsPrivate { .. } => 621,
            DiagnosticKind::ModifierNotValidForItem { .. } => 106,
            DiagnosticKind::SealedMemberIsNotOverride { .. } => 238,
            DiagnosticKind::ProtectedMemberInStruct { .. } => 666,
            DiagnosticKind::MemberNamedLikeType { .. } => 542,
            DiagnosticKind::StaticConstructorHasParameters { .. } => 132,
            DiagnosticKind::VoidField => 670,
            DiagnosticKind::VoidLocal => 1547,
            DiagnosticKind::SwitchGoverningType => 151,
            DiagnosticKind::FieldInitializerReference { .. } => 236,
            DiagnosticKind::ParamsNotLast => 231,
            DiagnosticKind::ParamsNotArray => 225,
            DiagnosticKind::InconsistentAccessibility { position, .. } => position.code(),
            DiagnosticKind::ConstFieldRequiresValue => 145,
            DiagnosticKind::InterfaceCannotContainInstanceField => 525,
            DiagnosticKind::ReadonlyAssignment { .. } => 191,
            DiagnosticKind::PropertyCannotBeAssigned { .. } => 200,
            DiagnosticKind::MemberNotFoundOnExpression { .. } => 1061,
            DiagnosticKind::NotACollectionInitializerTarget { .. } => 1922,
            DiagnosticKind::StaticMemberInObjectInitializer { .. } => 1914,
            DiagnosticKind::RequiredMemberMustBeSettable { .. } => 9034,
            DiagnosticKind::RequiredMemberLessVisible { .. } => 9032,
            DiagnosticKind::RequiredMemberMustBeSet { .. } => 9035,
            DiagnosticKind::RequiredMemberNeedsValue { .. } => 9036,
            DiagnosticKind::OverrideMustBeRequired { .. } => 9030,
            DiagnosticKind::InterfaceMemberNotImplemented { .. } => 535,
            DiagnosticKind::NoMethodToOverride { .. } => 115,
            DiagnosticKind::CannotOverrideNonVirtual { .. } => 506,
            DiagnosticKind::CaughtTypeMustBeException => 155,
            DiagnosticKind::OperatorRequiresMatchingOperator { .. } => 216,
            DiagnosticKind::CannotOverrideSealed { .. } => 239,
            DiagnosticKind::OverrideChangesAccess { .. } => 507,
            DiagnosticKind::OverrideReturnTypeMismatch { .. } => 508,
            DiagnosticKind::OverridePropertyTypeMismatch { .. } => 1715,
            DiagnosticKind::AbstractMemberNotImplemented { .. } => 534,
            DiagnosticKind::CircularBase { .. } => 146,
            DiagnosticKind::CircularConstant { .. } => 110,
            DiagnosticKind::StructLayoutCycle { .. } => 523,
            DiagnosticKind::CircularInterface { .. } => 529,
            DiagnosticKind::ExplicitConversionExists { .. } => 266,
            DiagnosticKind::UnusedLocal { .. } => 168,
            DiagnosticKind::UnusedLocalValue { .. } => 219,
            DiagnosticKind::UnusedField { .. } => 414,
            DiagnosticKind::FieldNeverUsed { .. } => 169,
            DiagnosticKind::FieldNeverAssigned { .. } => 649,
            DiagnosticKind::UnreachableCode => 162,
            DiagnosticKind::ObjectReferenceRequired { .. } => 120,
            DiagnosticKind::ThisInStaticContext => 26,
            DiagnosticKind::StaticMemberViaInstance { .. } => 176,
            DiagnosticKind::CannotIndex { .. } => 21,
            DiagnosticKind::NoConstructor { .. } => 1729,
            DiagnosticKind::MethodNameExpected => 149,
            DiagnosticKind::NoOverloadMatchesDelegate { .. } => 123,
            DiagnosticKind::ReturnValueInVoidMethod { .. } => 127,
            DiagnosticKind::ReturnValueRequired { .. } => 126,
            DiagnosticKind::NotAllPathsReturn { .. } => 161,
            DiagnosticKind::CannotCast { .. } => 30,
            DiagnosticKind::AsConversionMissing { .. } => 39,
            DiagnosticKind::UseOfUnassignedLocal { .. } => 165,
            DiagnosticKind::OutParameterNotAssigned { .. } => 177,
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
            | DiagnosticKind::UnusedField { .. }
            | DiagnosticKind::FieldNeverUsed { .. }
            | DiagnosticKind::FieldNeverAssigned { .. }
            | DiagnosticKind::UnreachableCode
            | DiagnosticKind::UnreferencedLabel => Severity::Warning,
            _ => Severity::Error,
        }
    }
}

impl fmt::Display for DiagnosticKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DiagnosticKind::TypeNotFound { name } => write!(
                f,
                "The type or namespace name '{name}' could not be found \
                 (are you missing a using directive or an assembly reference?)"
            ),
            DiagnosticKind::NameNotFound { name } => {
                write!(f, "The name '{name}' does not exist in the current context")
            }
            DiagnosticKind::GenericArityMismatch {
                candidate,
                required,
                member,
            } => write!(
                f,
                "Using the generic {} '{candidate}' requires {required} type arguments",
                member.as_str()
            ),
            DiagnosticKind::NonGenericTypeWithTypeArguments { name, member } => write!(
                f,
                "The non-generic {} '{name}' cannot be used with type arguments",
                member.as_str()
            ),
            DiagnosticKind::ConstantOverflowInCheckedContext => {
                write!(f, "The operation overflows at compile time in checked mode")
            }
            DiagnosticKind::CheckedConstantConversionOverflow { value, to } => write!(
                f,
                "Constant value '{value}' cannot be converted to a '{to}' (use 'unchecked' syntax to override)"
            ),
            DiagnosticKind::ConstantOutOfRange { value, to } => {
                write!(f, "Constant value '{value}' cannot be converted to a '{to}'")
            }
            DiagnosticKind::NonConstantEnumMember { member } => {
                write!(f, "The expression being assigned to '{member}' must be constant")
            }
            DiagnosticKind::VolatileFieldType { field, ty } => {
                write!(f, "'{field}': a volatile field cannot be of the type '{ty}'")
            }
            DiagnosticKind::RestrictedTypeReturn { ty } => write!(
                f,
                "The return type of a method, delegate, or function pointer cannot be '{ty}'"
            ),
            DiagnosticKind::RestrictedTypeField { ty } => {
                write!(f, "Field or property cannot be of type '{ty}'")
            }
            DiagnosticKind::RestrictedTypeArrayElement { ty } => {
                write!(f, "Array elements cannot be of type '{ty}'")
            }
            DiagnosticKind::RestrictedTypeByReference { ty } => {
                write!(f, "Cannot make reference to variable of type '{ty}'")
            }
            DiagnosticKind::OperatorMustBeStaticAndPublic { signature } => write!(
                f,
                "User-defined operator '{signature}' must be declared static and public"
            ),
            DiagnosticKind::AbstractTypeSealedOrStatic { type_name } => write!(
                f,
                "'{type_name}': an abstract type cannot be sealed or static"
            ),
            DiagnosticKind::StaticMemberCannotBeVirtual { modifier } => {
                write!(f, "A static member cannot be marked as '{modifier}'")
            }
            DiagnosticKind::EnumUnderlyingTypeExpected => {
                write!(f, "Type byte, sbyte, short, ushort, int, uint, long, or ulong expected")
            }
            DiagnosticKind::MultipleClassBases {
                type_name,
                first,
                second,
            } => write!(
                f,
                "Class '{type_name}' cannot have multiple base classes: '{first}' and '{second}'"
            ),
            DiagnosticKind::EventTypeMustBeDelegate { event } => {
                write!(f, "'{event}': event must be of a delegate type")
            }
            DiagnosticKind::OverloadableBinaryOperatorExpected => {
                write!(f, "Overloadable binary operator expected")
            }
            DiagnosticKind::ConditionalMethodMustReturnVoid { method } => {
                write!(
                    f,
                    "The Conditional attribute is not valid on '{method}' because its return type is not void"
                )
            }
            DiagnosticKind::DeriveFromSealed { derived, base } => {
                write!(f, "'{derived}': cannot derive from sealed type '{base}'")
            }
            DiagnosticKind::NamespaceElementBadAccessibility => write!(
                f,
                "Elements defined in a namespace cannot be explicitly declared as private, protected, protected internal, or private protected"
            ),
            DiagnosticKind::NoImplicitConversion { from, to } => {
                write!(f, "Cannot implicitly convert type '{from}' to '{to}'")
            }
            DiagnosticKind::CannotConvertNullToValueType { to } => write!(
                f,
                "Cannot convert null to '{to}' because it is a non-nullable value type"
            ),
            DiagnosticKind::TypeUsedAsValue { type_name } => write!(
                f,
                "'{type_name}' is a type, which is not valid in the given context"
            ),
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
            DiagnosticKind::CannotAssignToMethodGroup { name } => write!(
                f,
                "Cannot assign to '{name}' because it is a 'method group'"
            ),
            DiagnosticKind::CannotAssignToReadonlyLocal { name, kind } => {
                write!(f, "Cannot assign to '{name}' because it is a '{kind}'")
            }
            DiagnosticKind::MemberNotFound { type_name, member } => write!(
                f,
                "'{type_name}' does not contain a definition for '{member}'"
            ),
            DiagnosticKind::MemberSignatureNotSupported { type_name, member } => write!(
                f,
                "'{type_name}' defines '{member}', but its signature uses generics, \
                 which this build of the compiler cannot read"
            ),
            DiagnosticKind::NoOverloadForArgumentCount { method, count } => write!(
                f,
                "No overload for method '{method}' takes {count} arguments"
            ),
            DiagnosticKind::PredefinedTypeMissing { full_name } => write!(
                f,
                "Predefined type '{full_name}' is not defined or imported"
            ),
            DiagnosticKind::ArglistOutsideVarargMethod => write!(
                f,
                "The __arglist construct is valid only within a variable argument method"
            ),
            DiagnosticKind::ArglistOutsideCall => write!(
                f,
                "An __arglist expression may only appear inside of a call or new expression"
            ),
            DiagnosticKind::NoArgumentForArglist { method } => write!(
                f,
                "There is no argument given that corresponds to the required parameter '__arglist' of '{method}'"
            ),
            DiagnosticKind::MissingArgumentForParameter { parameter, method } => write!(
                f,
                "There is no argument given that corresponds to the required parameter '{parameter}' of '{method}'"
            ),
            DiagnosticKind::ArgumentModeRequired { index, keyword } => write!(
                f,
                "Argument {index} must be passed with the '{keyword}' keyword"
            ),
            DiagnosticKind::ArgumentModeForbidden { index, keyword } => write!(
                f,
                "Argument {index} may not be passed with the '{keyword}' keyword"
            ),
            DiagnosticKind::OverloadDiffersOnlyByRefOut {
                type_name,
                member_kind,
                current,
                previous,
            } => write!(
                f,
                "'{type_name}' cannot define an overloaded {member_kind} that differs only on \
                 parameter modifiers '{current}' and '{previous}'"
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
            DiagnosticKind::InvalidAttributeParameterType {
                parameter,
                type_name,
            } => write!(
                f,
                "Attribute constructor parameter '{parameter}' has type '{type_name}', \
                 which is not a valid attribute parameter type"
            ),
            DiagnosticKind::NotAValidNamedAttributeArgument { name } => write!(
                f,
                "'{name}' is not a valid named attribute argument. Named attribute arguments \
                 must be fields which are not readonly, static, or const, or read-write \
                 properties which are public and not static."
            ),
            DiagnosticKind::ProtectedQualifier {
                member,
                qualifier,
                accessing,
            } => write!(
                f,
                "Cannot access protected member '{member}' via a qualifier of type '{qualifier}'; \
                 the qualifier must be of type '{accessing}' (or derived from it)"
            ),
            DiagnosticKind::EventOutsideAddRemove { event } => write!(
                f,
                "The event '{event}' can only appear on the left hand side of += or -= \
                 (except when used from within the type that declares it)"
            ),
            DiagnosticKind::NoEnclosingLoop => {
                write!(f, "No enclosing loop out of which to break or continue")
            }
            DiagnosticKind::MultipleEntryPoints => write!(
                f,
                "Program has more than one entry point defined. Compile with /main to specify \
                 the type that contains the entry point"
            ),
            DiagnosticKind::MethodGroupToNonDelegate { method, target } => write!(
                f,
                "Cannot convert method group '{method}' to non-delegate type '{target}'"
            ),
            DiagnosticKind::ConstantExpected => write!(f, "A constant value is expected"),
            DiagnosticKind::DuplicateCaseLabel { label } => write!(
                f,
                "The label '{label}:' already occurs in this switch statement"
            ),
            DiagnosticKind::SwitchFallThrough { label } => write!(
                f,
                "Control cannot fall through from one case label ('{label}') to another"
            ),
            DiagnosticKind::SwitchFallOutFinal { label } => write!(
                f,
                "Control cannot fall out of switch from final case label ('{label}')"
            ),
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
            DiagnosticKind::DuplicateTypeInNamespace { namespace, name } => write!(
                f,
                "The namespace '{namespace}' already contains a definition for '{name}'"
            ),
            DiagnosticKind::DuplicateMember { type_name, member } => write!(
                f,
                "The type '{type_name}' already contains a definition for '{member}'"
            ),
            DiagnosticKind::DuplicateParameterName { name } => {
                write!(f, "The parameter name '{name}' is a duplicate")
            }
            DiagnosticKind::InterfaceImplementationNotPublic {
                type_name,
                interface_member,
                member,
            } => write!(
                f,
                "'{type_name}' does not implement interface member '{interface_member}'. \
                 '{member}' cannot implement an interface member because it is not public."
            ),
            DiagnosticKind::InterfaceImplementationReturnType {
                type_name,
                interface_member,
                member,
                return_type,
            } => write!(
                f,
                "'{type_name}' does not implement interface member '{interface_member}'. \
                 '{member}' cannot implement '{interface_member}' because it does not have the \
                 matching return type of '{return_type}'."
            ),
            DiagnosticKind::ConstructorInitializerCycle { constructor } => write!(
                f,
                "Constructor '{constructor}' cannot call itself through another constructor"
            ),
            DiagnosticKind::UsingRequiresDisposable { ty } => write!(
                f,
                "'{ty}': type used in a using statement must implement 'System.IDisposable'."
            ),
            DiagnosticKind::NotAnAttributeClass { type_name } => {
                write!(f, "'{type_name}' is not an attribute class")
            }
            DiagnosticKind::DuplicateAttribute { name } => {
                write!(f, "Duplicate '{name}' attribute")
            }
            DiagnosticKind::NonConstantAttributeArgument => write!(
                f,
                "An attribute argument must be a constant expression, typeof expression or \
                 array creation expression of an attribute parameter type"
            ),
            DiagnosticKind::UnsafeCodeRequiresOption => {
                write!(f, "Unsafe code may only appear if compiling with /unsafe")
            }
            DiagnosticKind::NonConstantFieldInitializer { field } => {
                write!(f, "The expression being assigned to '{field}' must be constant")
            }
            DiagnosticKind::OverloadableUnaryOperatorExpected => {
                write!(f, "Overloadable unary operator expected")
            }
            DiagnosticKind::CatchAfterGeneralCatch => write!(
                f,
                "Catch clauses cannot follow the general catch clause of a try statement"
            ),
            DiagnosticKind::ConversionMustInvolveEnclosingType => write!(
                f,
                "User-defined conversion must convert to or from the enclosing type"
            ),
            DiagnosticKind::ForEachNotEnumerable { ty } => write!(
                f,
                "foreach statement cannot operate on variables of type '{ty}' because '{ty}' \
                 does not contain a public instance or extension definition for 'GetEnumerator'"
            ),
            DiagnosticKind::VoidParameter => write!(f, "Invalid parameter type 'void'"),
            DiagnosticKind::ArrayInitializerLength { length } => {
                write!(f, "An array initializer of length '{length}' is expected")
            }
            DiagnosticKind::GotoCaseOutsideSwitch => {
                write!(f, "A goto case is only valid inside a switch statement")
            }
            DiagnosticKind::RethrowOutsideCatch => write!(
                f,
                "A throw statement with no arguments is not allowed outside of a catch clause"
            ),
            DiagnosticKind::ControlLeavesFinally => {
                write!(f, "Control cannot leave the body of a finally clause")
            }
            DiagnosticKind::DuplicateUsingAlias { alias } => write!(
                f,
                "The using alias '{alias}' appeared previously in this namespace"
            ),
            DiagnosticKind::DivisionByConstantZero => write!(f, "Division by constant zero"),
            DiagnosticKind::LockRequiresReferenceType { ty } => write!(
                f,
                "'{ty}' is not a reference type as required by the lock statement"
            ),
            DiagnosticKind::CannotCreateAbstractInstance { type_name } => write!(
                f,
                "Cannot create an instance of the abstract type or interface '{type_name}'"
            ),
            DiagnosticKind::StaticConstructorAccessibility { member } => write!(
                f,
                "'{member}': access modifiers are not allowed on static constructors"
            ),
            DiagnosticKind::MethodMustHaveReturnType => {
                write!(f, "Method must have a return type")
            }
            DiagnosticKind::BaseTypeNotInterface { base } => {
                write!(f, "Type '{base}' in interface list is not an interface")
            }
            DiagnosticKind::DestructorNameMismatch => {
                write!(f, "Name of destructor must match name of type")
            }
            DiagnosticKind::DestructorNotInClass => {
                write!(f, "Only class types can contain destructors")
            }
            DiagnosticKind::DuplicateMethod { type_name, member } => write!(
                f,
                "Type '{type_name}' already defines a member called '{member}' \
                 with the same parameter types"
            ),
            DiagnosticKind::DuplicateLabel { label } => {
                write!(f, "The label '{label}' is a duplicate")
            }
            DiagnosticKind::UndefinedLabel { label } => {
                write!(f, "No such label '{label}' within the scope of the goto statement")
            }
            DiagnosticKind::UnreferencedLabel => {
                write!(f, "This label has not been referenced")
            }
            DiagnosticKind::AbstractMethodWithBody { member } => write!(
                f,
                "'{member}' cannot declare a body because it is marked abstract"
            ),
            DiagnosticKind::MethodMustHaveBody { method } => write!(
                f,
                "'{method}' must declare a body because it is not marked abstract, extern, or partial"
            ),
            DiagnosticKind::InterfaceMemberModifier { modifier } => write!(
                f,
                "The modifier '{modifier}' is not valid for this item in C# 1.0; \
                 it requires C# 8.0 or greater"
            ),
            DiagnosticKind::FeatureRequiresLaterVersion {
                feature,
                required,
                current,
            } => write!(
                f,
                "Feature '{feature}' is not available in C# {}. Please use language version {required} or greater.",
                current.message_name()
            ),
            DiagnosticKind::FeatureNotInThisBuild {
                feature,
                permitted_by,
            } => write!(
                f,
                "Feature '{feature}' is permitted by C# {} but is not provided by this build of Lamella.",
                permitted_by.message_name()
            ),
            DiagnosticKind::AbstractMemberInNonAbstractType { member, type_name } => write!(
                f,
                "'{member}' is abstract but it is contained in non-abstract type '{type_name}'"
            ),
            DiagnosticKind::VirtualOrAbstractMemberIsPrivate { member } => write!(
                f,
                "'{member}': virtual or abstract members cannot be private"
            ),
            DiagnosticKind::ModifierNotValidForItem { modifier } => {
                write!(f, "The modifier '{modifier}' is not valid for this item")
            }
            DiagnosticKind::SealedMemberIsNotOverride { member } => write!(
                f,
                "'{member}' cannot be sealed because it is not an override"
            ),
            DiagnosticKind::ProtectedMemberInStruct { member } => {
                write!(f, "'{member}': new protected member declared in struct")
            }
            DiagnosticKind::MemberNamedLikeType { type_name } => write!(
                f,
                "'{type_name}': member names cannot be the same as their enclosing type"
            ),
            DiagnosticKind::StaticConstructorHasParameters { constructor } => write!(
                f,
                "'{constructor}': a static constructor must be parameterless"
            ),
            DiagnosticKind::VoidField => write!(f, "Field cannot have void type"),
            DiagnosticKind::VoidLocal => {
                write!(f, "Keyword 'void' cannot be used in this context")
            }
            DiagnosticKind::SwitchGoverningType => write!(
                f,
                "A switch governing type must be sbyte, byte, short, ushort, int, uint, long, ulong, char, string, or an enum type"
            ),
            DiagnosticKind::FieldInitializerReference { member } => write!(
                f,
                "A field initializer cannot reference the non-static field, method, or property '{member}'"
            ),
            DiagnosticKind::ParamsNotLast => write!(
                f,
                "A params parameter must be the last parameter in a parameter list"
            ),
            DiagnosticKind::ParamsNotArray => {
                write!(f, "The params parameter must have a valid collection type")
            }
            DiagnosticKind::InconsistentAccessibility {
                position,
                type_name,
                member,
            } => write!(
                f,
                "Inconsistent accessibility: {} '{type_name}' is less accessible than {} '{member}'",
                position.phrase(),
                position.member_kind()
            ),
            DiagnosticKind::ConstFieldRequiresValue => {
                write!(f, "A const field requires a value to be provided")
            }
            DiagnosticKind::InterfaceCannotContainInstanceField => {
                write!(f, "Interfaces cannot contain instance fields")
            }
            DiagnosticKind::ReadonlyAssignment { field } => write!(
                f,
                "A readonly field '{field}' cannot be assigned to (except in a constructor)"
            ),
            DiagnosticKind::PropertyCannotBeAssigned { property } => write!(
                f,
                "Property or indexer '{property}' cannot be assigned to -- it is read only"
            ),
            DiagnosticKind::MemberNotFoundOnExpression { type_name, member } => write!(
                f,
                "'{type_name}' does not contain a definition for '{member}' and no accessible \
                 extension method '{member}' accepting a first argument of type '{type_name}' \
                 could be found (are you missing a using directive or an assembly reference?)"
            ),
            DiagnosticKind::NotACollectionInitializerTarget { type_name } => write!(
                f,
                "Cannot initialize type '{type_name}' with a collection initializer because it \
                 does not implement 'System.Collections.IEnumerable'"
            ),
            DiagnosticKind::StaticMemberInObjectInitializer { member } => write!(
                f,
                "Static field or property '{member}' cannot be assigned in an object initializer"
            ),
            DiagnosticKind::RequiredMemberMustBeSettable { member } => {
                write!(f, "Required member '{member}' must be settable.")
            }
            DiagnosticKind::RequiredMemberLessVisible {
                member,
                containing_type,
            } => write!(
                f,
                "Required member '{member}' cannot be less visible or have a setter less visible \
                 than the containing type '{containing_type}'."
            ),
            DiagnosticKind::RequiredMemberMustBeSet { member } => write!(
                f,
                "Required member '{member}' must be set in the object initializer or attribute \
                 constructor."
            ),
            DiagnosticKind::RequiredMemberNeedsValue { member } => write!(
                f,
                "Required member '{member}' must be assigned a value, it cannot use a nested \
                 member or collection initializer."
            ),
            DiagnosticKind::OverrideMustBeRequired {
                member,
                base_member,
            } => write!(
                f,
                "'{member}' must be required because it overrides required member '{base_member}'"
            ),
            DiagnosticKind::InterfaceMemberNotImplemented { type_name, member } => write!(
                f,
                "'{type_name}' does not implement interface member '{member}'"
            ),
            DiagnosticKind::NoMethodToOverride { method } => {
                write!(f, "'{method}': no suitable method found to override")
            }
            DiagnosticKind::CannotOverrideNonVirtual { method, base } => write!(
                f,
                "'{method}': cannot override inherited member '{base}' because it is not marked \
                 virtual, abstract, or override"
            ),
            DiagnosticKind::OperatorRequiresMatchingOperator { operator, partner } => write!(
                f,
                "The operator '{operator}' requires a matching operator '{partner}' to also \
                 be defined"
            ),
            DiagnosticKind::CaughtTypeMustBeException => {
                write!(f, "The type caught or thrown must be derived from System.Exception")
            }
            DiagnosticKind::CannotOverrideSealed { method, base } => write!(
                f,
                "'{method}': cannot override inherited member '{base}' because it is sealed"
            ),
            DiagnosticKind::OverrideChangesAccess {
                method,
                access,
                base,
            } => write!(
                f,
                "'{method}': cannot change access modifiers when overriding '{access}' \
                 inherited member '{base}'"
            ),
            DiagnosticKind::OverrideReturnTypeMismatch {
                method,
                return_type,
                base,
            } => write!(
                f,
                "'{method}': return type must be '{return_type}' to match overridden member \
                 '{base}'"
            ),
            DiagnosticKind::OverridePropertyTypeMismatch { property, ty, base } => write!(
                f,
                "'{property}': type must be '{ty}' to match overridden member '{base}'"
            ),
            DiagnosticKind::AbstractMemberNotImplemented { type_name, member } => write!(
                f,
                "'{type_name}' does not implement inherited abstract member '{member}'"
            ),
            DiagnosticKind::CircularBase { type_name } => write!(
                f,
                "Circular base class dependency involving '{type_name}'"
            ),
            DiagnosticKind::CircularConstant { member } => write!(
                f,
                "The evaluation of the constant value for '{member}' involves a circular definition"
            ),
            DiagnosticKind::CircularInterface { type_name, base } => write!(
                f,
                "Inherited interface '{base}' causes a cycle in the interface hierarchy of '{type_name}'"
            ),
            DiagnosticKind::StructLayoutCycle { member, type_name } => write!(
                f,
                "Struct member '{member}' of type '{type_name}' causes a cycle in the struct layout"
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
            DiagnosticKind::UnusedField { field } => {
                write!(
                    f,
                    "The field '{field}' is assigned but its value is never used"
                )
            }
            DiagnosticKind::FieldNeverUsed { field } => {
                write!(f, "The field '{field}' is never used")
            }
            DiagnosticKind::FieldNeverAssigned { field, default } => write!(
                f,
                "Field '{field}' is never assigned to, and will always have its default value {default}"
            ),
            DiagnosticKind::UnreachableCode => write!(f, "Unreachable code detected"),
            DiagnosticKind::ObjectReferenceRequired { member } => write!(
                f,
                "An object reference is required for the non-static field, method, or property '{member}'"
            ),
            DiagnosticKind::ThisInStaticContext => write!(
                f,
                "Keyword 'this' is not valid in a static property, static method, or static field initializer"
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
            DiagnosticKind::MethodNameExpected => write!(f, "Method name expected"),
            DiagnosticKind::NoOverloadMatchesDelegate { method, delegate } => {
                write!(f, "No overload for '{method}' matches delegate '{delegate}'")
            }
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
            DiagnosticKind::AsConversionMissing { from, to } => write!(
                f,
                "Cannot convert type '{from}' to '{to}' via a reference conversion, boxing \
                 conversion, unboxing conversion, wrapping conversion, or null type conversion"
            ),
            DiagnosticKind::UseOfUnassignedLocal { name } => {
                write!(f, "Use of unassigned local variable '{name}'")
            }
            DiagnosticKind::OutParameterNotAssigned { parameter } => write!(
                f,
                "The out parameter '{parameter}' must be assigned to before control leaves the current method"
            ),
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
        assert_eq!(
            DiagnosticKind::ModifierNotValidForItem {
                modifier: "virtual".into()
            }
            .code(),
            106
        );
        assert_eq!(
            DiagnosticKind::SealedMemberIsNotOverride {
                member: "C.M()".into()
            }
            .code(),
            238
        );
        assert_eq!(
            DiagnosticKind::ProtectedMemberInStruct {
                member: "S.x".into()
            }
            .code(),
            666
        );
        assert_eq!(DiagnosticKind::ConstantOverflowInCheckedContext.code(), 220);
        assert_eq!(
            DiagnosticKind::TypeNotFound { name: "Foo".into() }.namespace(),
            CodeNamespace::Cs
        );
        assert_eq!(
            DiagnosticKind::ConstantOverflowInCheckedContext.namespace(),
            CodeNamespace::Cs
        );
        assert_eq!(
            DiagnosticKind::MemberNamedLikeType {
                type_name: "C".into()
            }
            .code(),
            542
        );
        assert_eq!(
            DiagnosticKind::StaticConstructorHasParameters {
                constructor: "C.C(int)".into()
            }
            .code(),
            132
        );
        assert_eq!(DiagnosticKind::VoidField.code(), 670);
        assert_eq!(DiagnosticKind::VoidLocal.code(), 1547);
        assert_eq!(DiagnosticKind::SwitchGoverningType.code(), 151);
        assert_eq!(
            DiagnosticKind::FieldInitializerReference {
                member: "C.first".into()
            }
            .code(),
            236
        );
        assert_eq!(DiagnosticKind::ParamsNotLast.code(), 231);
        assert_eq!(DiagnosticKind::ParamsNotArray.code(), 225);
        assert_eq!(
            DiagnosticKind::InconsistentAccessibility {
                position: SignaturePosition::FieldType,
                type_name: "C.Priv".into(),
                member: "C.f".into()
            }
            .code(),
            52
        );
        assert_eq!(
            DiagnosticKind::InconsistentAccessibility {
                position: SignaturePosition::BaseClass,
                type_name: "Base".into(),
                member: "C".into()
            }
            .code(),
            60
        );
        assert_eq!(
            DiagnosticKind::InconsistentAccessibility {
                position: SignaturePosition::ReturnType,
                type_name: "C.Priv".into(),
                member: "C.Get()".into()
            }
            .to_string(),
            "Inconsistent accessibility: return type 'C.Priv' is less accessible than method 'C.Get()'"
        );
        assert_eq!(
            DiagnosticKind::FieldNeverAssigned {
                field: "C.x".into(),
                default: "0".into()
            }
            .code(),
            649
        );
        assert_eq!(
            DiagnosticKind::FieldNeverAssigned {
                field: "C.x".into(),
                default: "null".into()
            }
            .to_string(),
            "Field 'C.x' is never assigned to, and will always have its default value null"
        );
    }

    #[test]
    fn messages_render_their_detail() {
        assert_eq!(
            DiagnosticKind::TypeNotFound {
                name: "Widget".into()
            }
            .to_string(),
            "The type or namespace name 'Widget' could not be found \
             (are you missing a using directive or an assembly reference?)"
        );
        assert_eq!(
            DiagnosticKind::NoImplicitConversion {
                from: "string".into(),
                to: "int".into()
            }
            .to_string(),
            "Cannot implicitly convert type 'string' to 'int'"
        );
        assert_eq!(
            DiagnosticKind::ModifierNotValidForItem {
                modifier: "virtual".into()
            }
            .to_string(),
            "The modifier 'virtual' is not valid for this item"
        );
        assert_eq!(
            DiagnosticKind::ProtectedMemberInStruct {
                member: "S.x".into()
            }
            .to_string(),
            "'S.x': new protected member declared in struct"
        );
        assert_eq!(
            DiagnosticKind::ConstantOverflowInCheckedContext.to_string(),
            "The operation overflows at compile time in checked mode"
        );
    }
}
