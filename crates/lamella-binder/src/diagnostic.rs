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
    /// `CS0031`: a constant value is outside the range of the type it is being converted to
    /// (the constant-expression conversion, 13.1.7), e.g. `byte b = 256;`.
    ConstantOutOfRange {
        /// The constant value, as rendered.
        value: Box<str>,
        /// The target type.
        to: Box<str>,
    },
    /// `CS0220`: a constant arithmetic operation overflows the result type in a checked context
    /// (14.16), e.g. `int.MaxValue + 1`. Constant expressions are checked by default, so this
    /// fires unless an explicit `unchecked` context suppresses it.
    ConstantOverflowInCheckedContext,
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
    /// `CS0101`: the namespace already contains a definition for this type name (a
    /// duplicate type declaration -- C# 1.0 has no partial types).
    DuplicateTypeInNamespace {
        /// The namespace the type is declared in, or `<global namespace>` for the global one.
        namespace: Box<str>,
        /// The duplicated type name.
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
    /// `CS8022`: a language feature outside the strict C# 1.0 (ISO-1) dialect lcsc targets -- an
    /// automatically-implemented property (C# 3.0), a `static` class (C# 2.0), and so on. csc
    /// reports the same under `/langversion:ISO-1`; the message names the feature and the version it
    /// needs. lcsc GATES every post-1.0 feature here (strict C# 1.0 now), even ones whose emit path
    /// is already implemented -- see the `GATED FEATURE (ISO-N)` markers -- until a real
    /// language-version mode lifts the gate.
    FeatureRequiresLaterVersion {
        /// The feature name (e.g. "automatically implemented properties").
        feature: Box<str>,
        /// The minimum C# version, as rendered (e.g. "C# 3.0").
        required: Box<str>,
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
            DiagnosticKind::TypeNotFound { .. } => 246,
            DiagnosticKind::NameNotFound { .. } => 103,
            DiagnosticKind::ConstantOutOfRange { .. } => 31,
            DiagnosticKind::ConstantOverflowInCheckedContext => 220,
            DiagnosticKind::NoImplicitConversion { .. } => 29,
            DiagnosticKind::CannotConvertNullToValueType { .. } => 37,
            DiagnosticKind::TypeUsedAsValue { .. } => 119,
            DiagnosticKind::OperatorNotApplicable { .. } => 19,
            DiagnosticKind::UnaryOperatorNotApplicable { .. } => 23,
            DiagnosticKind::ConditionalTypeMismatch { .. } => 173,
            DiagnosticKind::NotAssignable => 131,
            DiagnosticKind::CannotAssignToMethodGroup { .. } => 1656,
            DiagnosticKind::MemberNotFound { .. } => 117,
            DiagnosticKind::NoOverloadForArgumentCount { .. } => 1501,
            DiagnosticKind::PredefinedTypeMissing { .. } => 518,
            DiagnosticKind::ArglistOutsideVarargMethod => 190,
            DiagnosticKind::ArglistOutsideCall => 226,
            DiagnosticKind::NoArgumentForArglist { .. } => 7036,
            DiagnosticKind::ArgumentConversion { .. } => 1503,
            DiagnosticKind::AmbiguousCall { .. } => 121,
            DiagnosticKind::Inaccessible { .. } => 122,
            DiagnosticKind::EventOutsideAddRemove { .. } => 70,
            DiagnosticKind::NoEnclosingLoop => 139,
            DiagnosticKind::MultipleEntryPoints => 17,
            DiagnosticKind::MethodGroupToNonDelegate { .. } => 428,
            DiagnosticKind::ConstantExpected => 150,
            DiagnosticKind::DuplicateCaseLabel { .. } => 152,
            DiagnosticKind::SwitchFallThrough => 163,
            DiagnosticKind::DuplicateLocal { .. } => 128,
            DiagnosticKind::LocalShadowsEnclosing { .. } => 136,
            DiagnosticKind::IllegalStatementExpression => 201,
            DiagnosticKind::DuplicateTypeInNamespace { .. } => 101,
            DiagnosticKind::DuplicateMember { .. } => 102,
            DiagnosticKind::DuplicateMethod { .. } => 111,
            DiagnosticKind::DuplicateLabel { .. } => 140,
            DiagnosticKind::UndefinedLabel { .. } => 159,
            DiagnosticKind::UnreferencedLabel => 164,
            DiagnosticKind::AbstractMethodWithBody { .. } => 500,
            DiagnosticKind::MethodMustHaveBody { .. } => 501,
            DiagnosticKind::FeatureRequiresLaterVersion { .. } => 8022,
            DiagnosticKind::AbstractMemberInNonAbstractType { .. } => 513,
            DiagnosticKind::VirtualOrAbstractMemberIsPrivate { .. } => 621,
            DiagnosticKind::ModifierNotValidForItem { .. } => 106,
            DiagnosticKind::SealedMemberIsNotOverride { .. } => 238,
            DiagnosticKind::ProtectedMemberInStruct { .. } => 666,
            DiagnosticKind::MemberNamedLikeType { .. } => 542,
            DiagnosticKind::StaticConstructorHasParameters { .. } => 132,
            DiagnosticKind::VoidField => 670,
            DiagnosticKind::ParamsNotLast => 231,
            DiagnosticKind::ParamsNotArray => 225,
            DiagnosticKind::InconsistentAccessibility { position, .. } => position.code(),
            DiagnosticKind::ConstFieldRequiresValue => 145,
            DiagnosticKind::InterfaceCannotContainInstanceField => 525,
            DiagnosticKind::ReadonlyAssignment { .. } => 191,
            DiagnosticKind::InterfaceMemberNotImplemented { .. } => 535,
            DiagnosticKind::NoMethodToOverride { .. } => 115,
            DiagnosticKind::CannotOverrideNonVirtual { .. } => 506,
            DiagnosticKind::OverrideReturnTypeMismatch { .. } => 508,
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
            DiagnosticKind::TypeNotFound { name } => {
                write!(f, "The type or namespace name '{name}' could not be found")
            }
            DiagnosticKind::NameNotFound { name } => {
                write!(f, "The name '{name}' does not exist in the current context")
            }
            DiagnosticKind::ConstantOverflowInCheckedContext => {
                write!(f, "The operation overflows at compile time in checked mode")
            }
            DiagnosticKind::ConstantOutOfRange { value, to } => {
                write!(f, "Constant value '{value}' cannot be converted to a '{to}'")
            }
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
            DiagnosticKind::MemberNotFound { type_name, member } => write!(
                f,
                "'{type_name}' does not contain a definition for '{member}'"
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
            DiagnosticKind::DuplicateTypeInNamespace { namespace, name } => write!(
                f,
                "The namespace '{namespace}' already contains a definition for '{name}'"
            ),
            DiagnosticKind::DuplicateMember { type_name, member } => write!(
                f,
                "The type '{type_name}' already contains a definition for '{member}'"
            ),
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
            DiagnosticKind::FeatureRequiresLaterVersion { feature, required } => write!(
                f,
                "Feature '{feature}' is not available in C# 1.0; it requires {required} or greater"
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
            DiagnosticKind::OverrideReturnTypeMismatch {
                method,
                return_type,
                base,
            } => write!(
                f,
                "'{method}': return type must be '{return_type}' to match overridden member \
                 '{base}'"
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
