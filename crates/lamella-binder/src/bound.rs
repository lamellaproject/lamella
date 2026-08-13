//! The bound expression tree and the expression binder (ECMA-334 1st ed,
//! clause 14).

use crate::bind::bind_type;
use crate::conversion::{can_cast, converts, no_conversion_operator_can_exist};
use crate::diagnostic::{Diagnostic, DiagnosticKind, GenericMember};
use crate::infer::{infer_expanded_type_arguments, infer_method_type_arguments};
use crate::resolve::{Shadowed, TypeTable, resolve_type};
use crate::special::SpecialType;
use crate::symbols::{
    Accessibility, EventSymbol, MethodSymbol, Model, ParameterMode, PropertySymbol, TypeInfo,
    TypeKind, unmangled_type_name,
};
use crate::types::TypeSymbol;
use lamella_syntax::version::{Feature, LanguageVersion};
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use lamella_syntax::ast::{
    AssignmentOperator, BinaryOperator, Expr, ExprKind, Initializer, Literal, MemberInitializer,
    MemberInitializerValue, PostfixOperator, TypeParameter, TypeParameterConstraintClause, TypeRef,
    TypeRefKind, TypeTestOperation, UnaryOperator,
};
use lamella_syntax::span::Span;
use lamella_syntax::token::{IntegerSuffix, RealSuffix};

/// Why a class does or does not implement one interface method. Three of the four outcomes are
/// failures, and csc gives each its own code because each names a different repair: declare the
/// member (`CS0535`), make it public (`CS0737`), or fix its return type (`CS0738`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InterfaceMemberStatus {
    /// A public member of the right signature and return type provides it.
    Implemented,
    /// Nothing of that name and parameter list exists.
    Missing,
    /// A member matches, but it is not public, so it cannot implement anything.
    NotPublic,
    /// A public member matches by signature but returns a different type.
    WrongReturnType,
}

/// The base slot an `override` property or indexer resolved to, reduced to the facts the
/// override-legality rules read. Both member kinds produce one, from different tables: a
/// property from the base type's property list, an indexer from its `get_Item` accessor.
struct BaseSlot {
    /// Whether the base member is `virtual`.
    is_virtual: bool,
    /// Whether the base member is `abstract`.
    is_abstract: bool,
    /// Whether the base member is itself an `override`.
    is_override: bool,
    /// Whether `sealed` closed the base member's slot.
    is_sealed: bool,
    /// The base member's accessibility, which the override must repeat.
    accessibility: Accessibility,
    /// The base member's type (a property's type, an indexer's element type).
    ty: TypeSymbol,
    /// Whether `accessibility` was read from a declaration or from metadata rather than
    /// synthesized. `CS0507` is withheld when it was not, so a placeholder never accuses a
    /// correct override of changing access.
    accessibility_is_known: bool,
    /// Whether the base member is declared in a REFERENCED assembly, which changes what
    /// accessibility the override must declare -- see [`required_override_accessibility`].
    base_is_external: bool,
}

/// The accessibility an `override` must DECLARE in order to match a base member whose own
/// accessibility is `base_access`.
///
/// Normally it is simply the same: an override may neither widen nor narrow. The exception is
/// the one that decides whether a driver can subclass a seam at all. `protected internal` means
/// "protected OR internal", and the `internal` half names the assembly that DECLARES the member.
/// An override in a different assembly cannot claim it, so C# requires the override to be spelled
/// `protected` there -- and requires `protected internal` when the two are in the same assembly.
/// The two spellings are mutually exclusive, which is why one source file cannot serve both the
/// folded and the referenced shape, and why a seam meant to be subclassed from another assembly
/// wants `public` members (the only accessibility whose spelling is the same on both sides).
///
/// `FamANDAssem` (`private protected`) needs no case: C# 1.0 cannot spell it, so an override can
/// never match it and reporting the mismatch is correct.
fn required_override_accessibility(
    base_access: Accessibility,
    base_is_external: bool,
) -> Accessibility {
    if base_is_external && base_access == Accessibility::ProtectedInternal {
        Accessibility::Protected
    } else {
        base_access
    }
}

/// A bound expression: its kind and its resolved type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundExpr {
    /// What the expression is, after binding.
    pub kind: BoundExprKind,
    /// The expression's type (`TypeSymbol::Error` when binding failed).
    pub ty: TypeSymbol,
}

/// A bound object or collection initializer (C# 3.0).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoundInitializer {
    /// `{ F = 1, P = 2 }` -- assignments into the new object's own members.
    Object(Vec<BoundMemberInitializer>),
    /// `{ 1, 2 }` -- elements handed to the type's `Add` method.
    Collection(Vec<BoundExpr>),
}

/// What an object initializer's member name resolved to.
///
/// Carries what EMISSION needs rather than what binding produced: a field emits `stfld` and needs
/// its reference, a property emits a call to `set_<name>` and needs the type DECLARING that setter
/// (which is not always the receiver's type -- a partially overridden property declares its two
/// accessors on different types) plus the property's own type for the signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoundInitializerTarget {
    /// A field. **Also what the definite-assignment walk reads**: `new C { F = 1 }` genuinely
    /// assigns `F`, and without the reference here the walk cannot see it -- which produced a
    /// `CS0649` "never assigned" on a field the program does assign, measured against csc.
    Field(FieldReference),
    /// A settable property.
    Property {
        /// The type declaring the `set_` accessor.
        setter_declaring_type: TypeSymbol,
        /// The property's type -- the setter's single parameter.
        ty: TypeSymbol,
    },
    /// The name did not resolve, or resolved to something unassignable. Already reported; kept so
    /// the tree still describes what was written.
    Unresolved,
}

/// One bound `name = value` of an object initializer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundMemberInitializer {
    /// The member's name, kept whether or not it resolved.
    pub name: Box<str>,
    /// What the name resolved to.
    pub target: BoundInitializerTarget,
    /// What is assigned to it.
    pub value: BoundMemberInitializerValue,
}

/// The bound right-hand side of a member initializer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoundMemberInitializerValue {
    /// `F = expr`.
    Expression(BoundExpr),
    /// `F = { ... }` -- assigns INTO the object `F` already refers to and constructs nothing.
    Nested(BoundInitializer),
}

/// The field an access resolved to, recorded so emission can name it with a
/// metadata token and choose `ldfld`/`stfld` versus `ldsfld`/`stsfld`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldReference {
    /// The type that declares the field.
    pub declaring_type: TypeSymbol,
    /// The field name.
    pub name: Box<str>,
    /// The field's type.
    pub ty: TypeSymbol,
    /// Whether the field is `static`.
    pub is_static: bool,
    /// Whether the field is `readonly` (`initonly`). Its address may not be taken with
    /// `ldsflda`/`ldflda` outside a constructor, so a value-type method call on a readonly field
    /// copies it to a temp first.
    pub is_readonly: bool,
    /// Whether the field is `volatile` (17.4.3): emission prefixes its load/store with `volatile.`.
    pub is_volatile: bool,
    /// The field's accessibility.
    pub accessibility: Accessibility,
    /// The compile-time constant value of a `const` field or enum member, so emission folds
    /// the access to a constant load (not an `ldsfld`). `None` for an ordinary field.
    pub constant: Option<Literal>,
    /// The DECLARING TYPE's open form, when this field was named through an instantiated generic
    /// type -- `None` for a field of an ordinary type. See [`FieldInstantiation`].
    pub declaring_instantiation: Option<Box<FieldInstantiation>>,
}

/// The open form of the type a FIELD was named through -- the `Counter<T>` behind a
/// `Counter<int>` -- so emission can write the `MemberRef` ECMA-335 4th ed II.23.2.1 requires: a
/// parent `TypeSpec` carrying the arguments, and the DEFINITION's field signature.
///
/// **THE STATIC CASE IS WHY THIS IS DATA CORRUPTION AND NOT A METADATA NICETY.** II.9.7 gives each
/// instantiation of a generic type its OWN copy of each static field. Name the definition's
/// `FieldDef` instead and `Counter<int>.Total` and `Counter<string>.Total` become one cell -- the
/// program runs, reports nothing, and answers with the two counts added together.
///
/// The sibling of [`TypeInstantiation`], separate because a field has a TYPE where a method has a
/// parameter list and a return: folding a field into that shape would leave `parameters` always
/// empty and a field's type living in a member called `return_type`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldInstantiation {
    /// The declaring TYPE's own type parameter names, in declaration order. A name's POSITION is
    /// the `n` of the `!n` its signature spells it with (II.23.1.16).
    pub type_parameters: Vec<Box<str>>,
    /// The field's type BEFORE substitution -- `T` for `Box<T>.item`, and plainly `int` for a
    /// `Counter<T>.Total` that never mentions `T`. **The second case is not the trivial one:** its
    /// signature is identical open and closed, so nothing about the SIGNATURE reveals whether the
    /// instantiation was carried, and only the parent row does.
    pub ty: TypeSymbol,
}

/// The method an invocation resolved to, recorded so emission can name it with a
/// metadata token and choose `call` versus `callvirt`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodReference {
    /// The type through which the method is named.
    pub declaring_type: TypeSymbol,
    /// The method name.
    pub name: Box<str>,
    /// The parameter types, in order. For a vararg member these are the FIXED
    /// parameters only; the variable arguments ride the call's trailing
    /// [`BoundExprKind::ArgListLiteral`] argument.
    pub parameters: Vec<TypeSymbol>,
    /// The return type.
    pub return_type: TypeSymbol,
    /// Whether the method is `static`.
    pub is_static: bool,
    /// Whether the member uses the CLI vararg calling convention (`__arglist`), so
    /// emission mints the vararg def/call-site signatures instead of DEFAULT.
    pub is_vararg: bool,
    /// The generic instantiation this call site named, when it named one -- `None` for an
    /// ordinary call, which is every C# 1.0 call. See [`MethodInstantiation`].
    ///
    /// **Everything else on this reference is the SUBSTITUTED signature**, which is what
    /// overload resolution and conversion ran against; this is the only field that still knows
    /// the call was generic at all.
    pub instantiation: Option<Box<MethodInstantiation>>,
    /// The DECLARING TYPE's open form, when this member was named through an instantiated generic
    /// type -- `None` for a member of an ordinary type. See [`TypeInstantiation`].
    ///
    /// Independent of [`Self::instantiation`], and both can be set at once:
    /// `Holder<int>.Echo<string>` is a generic method on a generic type, and its signature spells
    /// `!0` and `!!0` from two separate numbering spaces.
    pub declaring_instantiation: Option<Box<TypeInstantiation>>,
}

/// The open form of the type a member was named through -- the `Box<T>` behind a `Box<int>` -- so
/// emission can write the `MemberRef` that ECMA-335 4th ed II.23.2.1 requires for a member of an
/// instantiated generic type: a parent `TypeSpec` carrying the arguments, and the DEFINITION's
/// signature with `!n` where the declaring type's parameters appear.
///
/// **THIS IS [`MethodInstantiation`]'s ARGUMENT ONE AXIS OVER, AND IT IS NOT INTERCHANGEABLE WITH
/// IT.** That type exists because a generic METHOD's open signature cannot be recovered from the
/// closed one; the same is true of a generic TYPE's, and neither carries the other's names. A
/// `new Box<int>(41)` has no generic method anywhere in it, so `instantiation` is `None` and the
/// only record that `!0` was ever involved is this.
///
/// The ARGUMENTS are deliberately absent: they are already on the
/// [`TypeSymbol::Instantiation`] in [`MethodReference::declaring_type`], and a second copy is a
/// second thing to keep in step.
///
/// **A WRONG `!n` IS SILENT, EXACTLY AS A WRONG `!!n` IS.** `!0` written where the substituted type
/// was meant produces a signature that decodes cleanly and names a different type; so does a
/// parent that names the definition instead of the instantiation, which is what this compiler
/// emitted before this type existed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeInstantiation {
    /// The declaring TYPE's own type parameter names, in declaration order -- `["T"]` for
    /// `Box<T>`, never the method's. A name's POSITION here is the `n` of the `!n` a metadata
    /// signature spells it with (II.23.1.16), because the binder substitutes BY NAME and metadata
    /// numbers instead.
    pub type_parameters: Vec<Box<str>>,
    /// The member's parameter types BEFORE substitution -- `[T]` for `Box<T>.Box(T value)`.
    pub parameters: Vec<TypeSymbol>,
    /// The member's return type before substitution -- `T` for `Box<T>.Get()`.
    pub return_type: TypeSymbol,
}

/// The generic instantiation a call site named -- the `<int>` of `Id<int>(x)` -- carried beside
/// the substituted signature in [`MethodReference`] so emission can mint the `MethodSpec` row
/// (ECMA-335 4th ed II.22.29) that such a call must name instead of the definition's own token.
///
/// **THE OPEN SIGNATURE CANNOT BE RECOVERED FROM THE CLOSED ONE, WHICH IS THE WHOLE REASON
/// THIS TYPE EXISTS.** After substitution `Id<int>` reads `int Id(int)`, indistinguishable from an
/// ordinary `int Plain(int)` -- nothing in it records that the parameter was `T`. Emission needs
/// the OPEN form to write the definition's `!!0` signature and the ARGUMENTS to write the
/// instantiation blob, and neither half is derivable from the other.
///
/// **Getting either half wrong is silent.** A `MethodSpec` whose parent is right and whose
/// arguments are wrong binds the call to a different instantiation; a call emitted against the
/// definition's token binds to the OPEN method with `!!0` never substituted. Both produce metadata
/// that decodes cleanly (`lamella-pe`'s `module.rs` `method_spec` doc says the same from the
/// writer's side), so neither announces itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodInstantiation {
    /// The type arguments named at the call site, in order -- `[int]` for `Id<int>(x)`. Never
    /// empty: a call with no type argument list has no instantiation at all.
    pub arguments: Vec<TypeSymbol>,
    /// The method's OWN type parameter names, in declaration order -- `["T"]` for `T Id<T>(T)`,
    /// never the declaring type's. The binder substitutes BY NAME, so this is what maps a `T` in
    /// the open signature below onto the `!!0` a metadata signature spells it with. Always the
    /// same length as `arguments` -- the arity filter (14.5.5.1) ran before this was built.
    pub type_parameters: Vec<Box<str>>,
    /// The definition's parameter types BEFORE substitution -- `[T]` for `T Id<T>(T)`.
    pub parameters: Vec<TypeSymbol>,
    /// The definition's return type before substitution.
    pub return_type: TypeSymbol,
}

/// What a CLOSED candidate was closed FROM: its open definition, and the type arguments that closed
/// it -- whether those were written at the call site or inferred by 25.6.4.
///
/// **NOTHING ELSE CAN SUPPLY IT, BECAUSE SUBSTITUTION IS NOT INVERTIBLE.** After `Id<int>` is built,
/// its parameter list is `(int)` and no amount of looking at it recovers `(T)` or the `int` that
/// replaced `T` -- and both are needed to mint the `MethodSpec` row the call emits.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ClosedFrom {
    /// The generic method as declared, with its own type parameters still open.
    definition: MethodSymbol,
    /// The type arguments, in the definition's declaration order. Never empty.
    arguments: Vec<TypeSymbol>,
}

/// The candidates a call will be resolved against, plus what each was closed from.
///
/// `candidates` and `origins` are PARALLEL, index for index; `origins[i]` is `None` exactly when
/// `candidates[i]` is an ordinary non-generic method. Keeping them parallel rather than pairing
/// them into one list is what lets `candidates` be handed to the existing overload resolution
/// unchanged -- it knows nothing about generics and should not have to.
#[derive(Debug, Default)]
struct CandidateSet {
    /// Every candidate, CLOSED. Overload resolution sees real types throughout.
    candidates: Vec<MethodSymbol>,
    /// Parallel to `candidates`.
    origins: Vec<Option<ClosedFrom>>,
    /// Generic methods at this call's argument count for which type inference FAILED. Not an
    /// error on its own -- the caller reports `CS0411` only when nothing else can be said about
    /// the call, which is the boundary csc draws (measured: a same-arity non-generic candidate's
    /// `CS1503` is reported INSTEAD).
    uninferable: Vec<MethodSymbol>,
}

impl CandidateSet {
    /// Applies the half of ECMA-334 4th ed **14.4.2.2** that only the OPEN signatures can settle:
    /// *"In case the expanded parameter types ... are identical, the following tie-breaking rules
    /// are applied ... by comparing the given uninstantiated and unexpanded parameter types."*
    ///
    /// **WITHOUT THIS, ADDING INFERENCE BREAKS PROGRAMS THAT COMPILED BEFORE IT.** `M(1)` against
    /// `void M(int)` beside `void M<T>(T)` bound to the non-generic only because the generic one
    /// was dropped for want of inference. Inferred, the two close to the SAME signature `(int)`,
    /// neither converts better than the other, and the call becomes CS0121 -- a working program
    /// turned ambiguous by a feature that was supposed to accept more. Rule 1 settles it: *"If one
    /// of MP and MQ is non-generic, but the other is generic, then the non-generic is better."*
    ///
    /// Dropping here rather than ranking inside `is_better` is deliberate, and it is safe for one
    /// reason worth stating: applicability depends only on the parameter list and the `params` /
    /// vararg shape, so two candidates identical in all three are applicable in exactly the same
    /// situations. Dropping one can therefore never lose a call that would otherwise resolve, and
    /// it keeps `candidates` free of EQUAL entries -- which `instantiation_of` needs, because two
    /// equal candidates are indistinguishable to it once a winner has been cloned out.
    fn apply_identical_signature_tie_break(&mut self) {
        let mut beaten: Vec<bool> = alloc::vec![false; self.candidates.len()];
        for left in 0..self.candidates.len() {
            for right in 0..self.candidates.len() {
                if left == right || beaten[left] || beaten[right] {
                    continue;
                }
                if !Self::same_call_shape(&self.candidates[left], &self.candidates[right]) {
                    continue;
                }
                if Self::beats(
                    self.origins[left].as_ref(),
                    self.origins[right].as_ref(),
                ) {
                    beaten[right] = true;
                }
            }
        }
        let mut keep = beaten.iter().map(|beaten| !beaten);
        self.candidates.retain(|_| keep.next() == Some(true));
        let mut keep = beaten.iter().map(|beaten| !beaten);
        self.origins.retain(|_| keep.next() == Some(true));
    }

    /// Whether two candidates are applicable in exactly the same situations: the same parameter
    /// types AND the same trailing-argument shape. `params` and vararg are part of it because
    /// `M(int[])` and `M(params int[])` accept different call sites despite one parameter list.
    fn same_call_shape(left: &MethodSymbol, right: &MethodSymbol) -> bool {
        left.parameters == right.parameters
            && left.is_params == right.is_params
            && left.is_vararg == right.is_vararg
    }

    /// Whether `left` wins 14.4.2.2's tie-break over `right`. `None` is a non-generic candidate.
    fn beats(left: Option<&ClosedFrom>, right: Option<&ClosedFrom>) -> bool {
        match (left, right) {
            (None, Some(_)) => true,
            (Some(_), None) | (None, None) => false,
            (Some(left), Some(right)) => {
                more_specific(&left.definition.parameters, &right.definition.parameters)
            }
        }
    }
}

/// Whether parameter list `left` is MORE SPECIFIC than `right` (14.4.2.2): *"each given parameter
/// RX is not less specific than SX, and at least one given parameter RX is more specific than SX.
/// A type parameter is less specific than a non-type parameter. Recursively, a constructed type is
/// more specific than another constructed type (with the same number of type arguments) if at least
/// one type argument is more specific and no type argument is less specific ... An array type is
/// more specific than another array type (with the same number of dimensions) if the element type
/// of the first is more specific."*
fn more_specific(left: &[TypeSymbol], right: &[TypeSymbol]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| !type_more_specific(right, left))
        && left
            .iter()
            .zip(right)
            .any(|(left, right)| type_more_specific(left, right))
}

/// The per-type half of [`more_specific`].
fn type_more_specific(left: &TypeSymbol, right: &TypeSymbol) -> bool {
    let is_parameter = |ty: &TypeSymbol| matches!(ty, TypeSymbol::Named(parts) if parts.len() == 1);
    match (left, right) {
        (
            TypeSymbol::Array {
                element: left,
                rank: left_rank,
            },
            TypeSymbol::Array {
                element: right,
                rank: right_rank,
            },
        ) if left_rank == right_rank => type_more_specific(left, right),
        (
            TypeSymbol::Instantiation {
                definition: left_definition,
                arguments: left_arguments,
            },
            TypeSymbol::Instantiation {
                definition: right_definition,
                arguments: right_arguments,
            },
        ) if left_definition == right_definition
            && left_arguments.len() == right_arguments.len() =>
        {
            more_specific(left_arguments, right_arguments)
        }
        _ => !is_parameter(left) && is_parameter(right),
    }
}

/// A user-defined `++`/`--` whose operand or result type differs from the variable's (14.14.2): the
/// operator method to call and any implicit conversion of its result back to the variable's type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConvertingStep {
    /// The `op_Increment`/`op_Decrement` to call.
    pub operator: MethodReference,
    /// The conversion of the operator's result back to the operand type, if it is not already that
    /// type or a reference-convertible one.
    pub result_conversion: Option<MethodReference>,
}

/// What an inserted [`BoundExprKind::Conversion`] does at emit time (13.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversionKind {
    /// A widening numeric conversion: emit `conv.*` to the target type.
    ImplicitNumeric,
    /// A value type to `object`: emit `box`.
    Boxing,
    /// A reference upcast (derived to base or interface): a no-op in CIL.
    ImplicitReference,
}

/// The kind of a [`BoundExpr`]. Grows as the binder learns more expression forms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoundExprKind {
    /// A constant literal, retyped from the syntax (9.4.4).
    Literal(Literal),
    /// A reference to a local variable or parameter (14.5.2).
    Local(Box<str>),
    /// A `ref`/`out` argument (17.5.1): the address of the inner variable, passed to a
    /// byref parameter. Its type is the variable's type. `out` assigns the variable (so
    /// it need not be assigned beforehand); `ref` requires it already assigned.
    Ref {
        /// `true` for `out`, `false` for `ref`.
        out: bool,
        /// The variable whose address is passed.
        operand: Box<BoundExpr>,
    },
    /// The `this` access (14.5.7); its type is the enclosing type.
    This,
    /// A `base` access (14.5.8); its type is the enclosing type's base class, used
    /// as the receiver of a non-virtual `base.member`.
    Base,
    /// A type name used as the receiver of a static member access (14.5.4). Its
    /// type is the named type so member lookup reaches the type's members.
    TypeReference(TypeSymbol),
    /// A namespace name used as the receiver of a qualified name (10.8). Not a
    /// value; only a step in resolving a qualified type name.
    NamespaceReference(Box<str>),
    /// Access to an instance or static field through a receiver (14.5.4); the
    /// expression's type is the field's.
    FieldAccess {
        /// The receiver the field is read from.
        receiver: Box<BoundExpr>,
        /// The field name.
        name: Box<str>,
        /// The resolved field, recorded for emission.
        field: Option<FieldReference>,
    },
    /// Access to a property through a receiver (14.5.4); the expression's type is
    /// the property's.
    PropertyAccess {
        /// The receiver the property is read from.
        receiver: Box<BoundExpr>,
        /// The type that declares the `get_` accessor (a base, for an inherited getter) -- it is
        /// named on that type, not the receiver's static type. A partially-overridden property
        /// (e.g. a `sealed override { set; }` that inherits its getter) declares its two accessors
        /// on DIFFERENT types, so the setter carries its own (14.5.4).
        declaring_type: TypeSymbol,
        /// The type that declares the `set_` accessor.
        setter_declaring_type: TypeSymbol,
        /// The `get_` accessor's OPEN signature, when the property was named through an
        /// instantiated generic type -- `None` for a property of an ordinary type.
        ///
        /// **WITHOUT THIS THE ACCESSOR IS NAMED BY THE DEFINITION'S OWN TOKEN**, which is a
        /// `callvirt` at the open `get_Item` returning `!0` unsubstituted -- and identical for
        /// every instantiation, so `B<int>.Item` and `B<string>.Item` call one method. The
        /// substituted signature on this node is what overload resolution ran against; this is the
        /// only field that still knows the property was reached through a constructed type. Same
        /// role [`MethodReference::declaring_instantiation`] plays for a call.
        getter_instantiation: Option<Box<TypeInstantiation>>,
        /// The `set_` accessor's open signature. SEPARATE from the getter's, because a partially
        /// overridden property declares its two accessors on DIFFERENT types (14.5.4) and so may
        /// reach them through different instantiations.
        setter_instantiation: Option<Box<TypeInstantiation>>,
        /// The property name.
        name: Box<str>,
    },
    /// A method group named through a receiver (14.5.4) -- not a value on its own,
    /// only the target of an invocation, so its type is the error type.
    MethodGroup {
        /// The receiver the method is called on.
        receiver: Box<BoundExpr>,
        /// The method name.
        name: Box<str>,
    },
    /// A method call (14.5.5); its type is the chosen overload's return type.
    Call {
        /// The callee (a method group).
        callee: Box<BoundExpr>,
        /// The bound arguments, in order.
        arguments: Vec<BoundExpr>,
        /// The method overload resolution chose, when it succeeded.
        method: Option<MethodReference>,
    },
    /// An element access `receiver[indices]` (14.5.6); its type is the array's
    /// element type.
    ElementAccess {
        /// The indexed receiver.
        receiver: Box<BoundExpr>,
        /// The index arguments.
        indices: Vec<BoundExpr>,
    },
    /// An indexer store target `receiver[indices]` bound as the LHS of a simple `=` (14.5.6.2 /
    /// 14.14.1). An indexer READ lowers to a plain `get_` [`Call`], but an assignment yields the
    /// assigned value (its type is the indexer's element type), so the store is a first-class
    /// lvalue rather than a void `set_` call. Only ever an [`BoundExprKind::Assignment`] target for
    /// a simple `=`; emission mirrors a property store with the indices pushed between the receiver
    /// and the value.
    IndexerAccess {
        /// The indexed receiver.
        receiver: Box<BoundExpr>,
        /// The index arguments, converted to the setter's index-parameter types.
        indices: Vec<BoundExpr>,
        /// The resolved `set_` accessor (`set_Item`, or a `[IndexerName]`-renamed setter).
        setter: MethodReference,
    },
    /// An array creation `new T[...]` (14.5.10.2); its type is the array type.
    ArrayCreation {
        /// The dimension-length expressions (empty when the size comes from
        /// `elements`).
        lengths: Vec<BoundExpr>,
        /// The `{ ... }` initializer elements, converted to the element type; empty for
        /// a sized-but-uninitialized array.
        elements: Vec<BoundExpr>,
    },
    /// An object creation `new T(args)` (14.5.10.1); its type is the created type.
    ObjectCreation {
        /// The constructor arguments.
        arguments: Vec<BoundExpr>,
        /// The constructor overload resolution chose, when it succeeded.
        constructor: Option<MethodReference>,
        /// The object or collection initializer `{ ... }` (C# 3.0), bound.
        initializer: Option<BoundInitializer>,
    },
    /// A delegate creation `new D(methodGroup)` (14.5.10.3): a method group converts to
    /// the delegate `D`. Its type is `D`. Emits `ldftn target` (with the receiver, or
    /// `ldnull` for a static target) then `newobj D::.ctor`.
    DelegateCreation {
        /// The delegate type being created.
        delegate_type: TypeSymbol,
        /// The method the delegate targets.
        target: MethodReference,
        /// The receiver for an instance target; `None` for a static one.
        receiver: Option<Box<BoundExpr>>,
    },
    /// A binary operation on two bound operands (14.7-14.12).
    Binary {
        /// The operator.
        operator: BinaryOperator,
        /// The left operand.
        left: Box<BoundExpr>,
        /// The right operand.
        right: Box<BoundExpr>,
        /// Whether the operation is in a `checked` context, so emission uses the
        /// overflow-checking `add.ovf`/`sub.ovf`/`mul.ovf` (14.5.12).
        checked: bool,
    },
    /// A prefix unary operation (14.6).
    Unary {
        /// The operator.
        operator: UnaryOperator,
        /// The operand.
        operand: Box<BoundExpr>,
    },
    /// A postfix increment or decrement (14.5.9).
    Postfix {
        /// The operator.
        operator: PostfixOperator,
        /// The operand.
        operand: Box<BoundExpr>,
        /// A user-defined `op_Increment`/`op_Decrement` whose parameter/result type differs from the
        /// operand's (14.14.2): the operator to call and any conversion of its result back to the
        /// operand type. `None` for a numeric/enum/pointer step or an exact same-type user operator.
        step: Option<Box<ConvertingStep>>,
    },
    /// A cast to the expression's type (14.6.6).
    Cast {
        /// The operand being cast.
        operand: Box<BoundExpr>,
        /// Whether the cast is in a `checked` context, so a narrowing integer
        /// conversion uses `conv.ovf.*` (14.5.12).
        checked: bool,
    },
    /// An implicit conversion the binder inserts so emission knows to widen a
    /// numeric, box a value type, or treat a reference upcast as a no-op (13.1).
    /// The expression's type is the conversion's target.
    Conversion {
        /// The value being converted.
        operand: Box<BoundExpr>,
        /// What kind of conversion to perform.
        conversion: ConversionKind,
    },
    /// An `is`/`as` type test (14.9.9, 14.9.10); the tested type is the result
    /// type for `as` and `bool` for `is`.
    TypeTest {
        /// Whether this is `is` or `as`.
        operation: TypeTestOperation,
        /// The operand.
        operand: Box<BoundExpr>,
        /// The type tested against (`isinst` names it).
        target: TypeSymbol,
    },
    /// An assignment, simple or compound (14.14); its type is the target's.
    Assignment {
        /// The assignment operator.
        operator: AssignmentOperator,
        /// The assignment target (an lvalue).
        target: Box<BoundExpr>,
        /// The assigned value.
        value: Box<BoundExpr>,
        /// Whether the assignment is in a `checked` context, so a compound assignment's
        /// implicit narrowing of its result back to a sub-int target uses `conv.ovf.*`
        /// (14.14.2 / 14.5.12) -- e.g. `checked { short s; s += 32000; }` overflows.
        checked: bool,
    },
    /// A conditional expression `c ? a : b` (14.13).
    Conditional {
        /// The condition.
        condition: Box<BoundExpr>,
        /// The value when true.
        when_true: Box<BoundExpr>,
        /// The value when false.
        when_false: Box<BoundExpr>,
    },
    /// A `typeof` expression (14.5.11), naming the type it reflects; its type is
    /// `System.Type`.
    TypeOf(TypeSymbol),
    /// A `sizeof(T)` (III.4.25): the byte size of `T` as an `int`.
    SizeOf(TypeSymbol),
    /// A `default(T)` expression: the target type's zero. Emission chooses the form from the
    /// type -- `ldnull`, a zero literal, or `initobj` into a temporary.
    ///
    /// **The TYPE is carried rather than a folded value, because the value depends on a fact the
    /// binder does not always have.** For a TYPE PARAMETER the answer is `null` at a reference
    /// instantiation and an all-zero struct at a value one, and the same `default(T)` is both.
    /// Folding it here would pick one before `T` is known.
    DefaultValue(TypeSymbol),
    /// A `__makeref(variable)` (csc typed-reference operator): its type is
    /// `System.TypedReference`. Emits the operand's address then `mkrefany <operand type>`.
    MakeRef(Box<BoundExpr>),
    /// A `__reftype(reference)`: the runtime `System.Type` of a typed reference. Emits
    /// `refanytype` then `Type.GetTypeFromHandle`, so its type is `System.Type`.
    RefType(Box<BoundExpr>),
    /// A `__refvalue(reference, T)`: the referent of a typed reference, viewed as `T` and
    /// usable as an lvalue (its type is `T`). Emits `refanyval <T>` then a load, or a store
    /// through the recovered pointer when it is an assignment target.
    RefValue {
        /// The typed-reference operand.
        reference: Box<BoundExpr>,
        /// The asserted referent type (`refanyval` names it; it is the expression's type).
        target: TypeSymbol,
    },
    /// A bare `__arglist` inside a vararg member's body: the handle to the current
    /// method's variable arguments, of type `System.RuntimeArgumentHandle`. Emits the
    /// `arglist` opcode.
    ArgListValue,
    /// An `__arglist(argument, ...)` at a call site: the variable arguments passed past a
    /// vararg member's fixed parameters. Rides as the FINAL element of a `Call`/
    /// `ObjectCreation` argument list; emission pushes each element and encodes its type
    /// after the sentinel in the call-site signature. Never a value by itself (CS0226).
    ArgListLiteral(Vec<BoundExpr>),
    /// A `stackalloc T[count]` (unsafe): a `T*` to `count * sizeof(T)` stack bytes.
    StackAlloc {
        /// The element type.
        element: TypeSymbol,
        /// The element count.
        count: Box<BoundExpr>,
    },
    /// A pointer indirection `*operand` (unsafe): reads/writes the element the pointer
    /// addresses. An lvalue when it is an assignment target.
    Dereference {
        /// The pointer being dereferenced.
        operand: Box<BoundExpr>,
    },
    /// The address-of `&operand` (unsafe, 18.5.4): a `T*` to a fixed variable (a local,
    /// value parameter, field, or array element). The inverse of [`Dereference`].
    AddressOf {
        /// The fixed variable whose address is taken.
        operand: Box<BoundExpr>,
    },
    /// A `checked` expression (14.5.12); the type is the operand's.
    Checked(Box<BoundExpr>),
    /// An `unchecked` expression (14.5.12); the type is the operand's.
    Unchecked(Box<BoundExpr>),
    /// An expression that could not be bound (yet), for recovery.
    Error,
}

/// The method currently being bound: its name (for `CS0127`), declared return
/// type (for checking `return`), whether it is `static` (for `CS0120`/`CS0026`,
/// which forbid an implicit `this` where there is no object), and whether it takes
/// CLI varargs (for `CS0190`: bare `__arglist` is legal only inside a vararg member).
#[derive(Debug, Clone)]
struct MethodContext {
    name: Box<str>,
    return_type: TypeSymbol,
    is_static: bool,
    is_vararg: bool,
}

/// The result of binding one REPL submission ([`Binder::bind_submission`]): the bound
/// `Submit$N` body, its return type, and the session variables it introduced.
#[derive(Debug, Clone)]
pub struct SubmissionBinding {
    /// The bound submission body -- a block of the session-field stores and statements,
    /// ending in a boxed `return` for a trailing display expression.
    pub body: crate::statement::BoundStmt,
    /// The `Submit$N` method's return type: `object` for a display expression, else `void`.
    pub return_type: TypeSymbol,
    /// The session variables this submission introduced, in declaration order, each with
    /// its stable `__Repl` field name -- the caller commits them so later submissions see
    /// them (and rebinds the source name on a redefinition). A field the runtime cannot
    /// resolve against the loaded `__Repl` it adds (inference).
    pub new_fields: Vec<DeclaredField>,
}

/// A session variable a submission introduced: its source name, the stable `__Repl` field
/// name it was assigned (the source name, or `x$2` on a redefinition), and its type.
#[derive(Debug, Clone)]
pub struct DeclaredField {
    /// The C# name the user wrote.
    pub source: Box<str>,
    /// The stable `__Repl` field name (the source name, or a fresh `x$2` on redefinition).
    pub stable: Box<str>,
    /// The field's type.
    pub ty: TypeSymbol,
}

/// Binds expressions, accumulating the semantic diagnostics found. Holds a stack
/// of local-variable scopes for name resolution.
#[derive(Debug, Default)]
pub struct Binder {
    diagnostics: Vec<Diagnostic>,
    /// The dialect being compiled -- `/langversion`, or [`LanguageVersion::DEFAULT`] when no driver
    /// parsed one. Read only through [`Binder::gate_feature`], so the two-bit rule lives in one
    /// place rather than at each construct.
    language_version: LanguageVersion,
    scopes: Vec<BTreeMap<String, TypeSymbol>>,
    /// The type parameters currently in scope, innermost last, each with the constraints its
    /// declaration wrote. Pushed by [`Binder::enter_type_parameters`] and popped by
    /// [`Binder::exit_type_parameters`], so it tracks the SAME scope the type table does.
    ///
    /// **A type parameter is not a row in the model** -- it is a shadowing entry in the type table
    /// and nothing more ([`Binder::enter_type_parameters`] explains why it is not its own
    /// `TypeSymbol` variant). So "is this name a type parameter, and what was promised about it"
    /// is a question nothing else in the binder could answer, which is what CS0304 needs.
    type_parameters_in_scope: Vec<(Box<str>, crate::symbols::TypeParameterConstraints)>,
    world: TypeTable,
    model: Model,
    current_type: Option<TypeSymbol>,
    current_method: Option<MethodContext>,
    /// Whether the NEXT [`Binder::bind_method`] call binds a vararg member's body, so a
    /// bare `__arglist` is legal there (CS0190 elsewhere). Set by the caller just before
    /// `bind_method`, which consumes (clears) it -- like `set_canon`, pre-set state the
    /// caller owns; self-clearing so a forgotten reset cannot leak into the next member.
    next_method_vararg: bool,
    imported_namespaces: Vec<Box<str>>,
    /// `using X = N.T;` aliases in scope, each the alias name and its target type, so an
    /// unqualified `X` resolves to the target (16.4.1). Scoped per namespace block alongside
    /// `imported_namespaces`.
    aliases: Vec<(Box<str>, TypeSymbol)>,
    /// Locals to EXEMPT from the unused-local check (`CS0168`/`CS0219`) of the method being
    /// bound: a local referenced only in a `switch` case-label expression (folded out of the
    /// bound tree, so its use is otherwise invisible), and a local whose initializer failed to
    /// convert (csc lets that conversion error stand alone rather than also warning it unused).
    /// Reset per method.
    case_label_uses: alloc::collections::BTreeSet<Box<str>>,
    /// Whether expressions are currently bound in a `checked` context (14.5.12),
    /// tracked as the binder descends so each arithmetic/cast node records whether
    /// emission should use the overflow-checking form. C# 1.0 defaults to unchecked.
    pub(crate) checked_context: bool,
    /// Whether expressions are currently in an explicit `unchecked` context (14.16). Distinct
    /// from `checked_context`: a CONSTANT operation is checked by DEFAULT, so a constant overflow
    /// is CS0220 even at the top level (where `checked_context` is false), and only an explicit
    /// `unchecked` block/expression suppresses it. False at the top level and inside `checked`.
    pub(crate) unchecked_context: bool,
    /// In REPL session mode, the name of the parameter standing in for the persistent
    /// `__Repl` instance (`s`). When set, an unqualified name that resolves to a member
    /// of the enclosing type reads through `s` (a parameter) instead of `this` -- the
    /// submission method is a static `Submit$N(__Repl s)`, so session locals are fields
    /// of `s`, not of a non-existent `this`. `None` in ordinary binding.
    session_receiver: Option<Box<str>>,
    /// In REPL session mode, each session variable's source name -> (its stable `__Repl`
    /// field name, its type). An unqualified name found here reads `s.<stable>` (14.5.2
    /// through `s`). It is keyed by SOURCE name and maps to the STABLE field name so a
    /// type-changing redefinition -- which adds a fresh field `x$2` and rebinds source `x`
    /// to it -- resolves correctly. Empty (so a no-op) in ordinary binding.
    session_fields: BTreeMap<String, (Box<str>, TypeSymbol)>,
    /// How many enclosing loops (`for`/`while`/`do`/`foreach`) the binder is inside, so a
    /// `break`/`continue` with no enclosing loop is `CS0139`. Reset per method.
    loop_depth: u32,
    /// How many enclosing `switch` statements the binder is inside (a `break` is also valid
    /// in a switch). Reset per method.
    switch_depth: u32,
    /// Whether the driver parsed a command line that OMITTED `/unsafe`. Phrased as the negative
    /// so the DERIVED default (`false`) is the permissive one: a caller that parsed no command
    /// line -- a test, an in-process compile -- has no such policy to apply. The driver sets it,
    /// and `unsafe` written under it is `CS0227`.
    unsafe_option_missing: bool,
    /// How many enclosing `catch` clauses the binder is inside, so a bare `throw;` outside one
    /// is `CS0156` -- there is no exception in flight to re-throw. Reset per method.
    catch_depth: u32,
    /// How many enclosing `finally` blocks the binder is inside, so a `return` that would leave
    /// one is `CS0157`. Reset per method.
    finally_depth: u32,
    /// For each open `finally`, the loop+switch depth OUTSIDE it, so a `break` can be told from
    /// one that binds to a loop opened within the finally and never leaves it.
    finally_floor: Vec<u32>,
    /// The locals the LANGUAGE owns rather than the body: a `foreach` iteration variable and a
    /// `using` resource. Assigning to either is `CS1656`, and the entry carries the kind csc
    /// names. A stack, since these nest and an inner name may shadow an outer.
    readonly_locals: Vec<(Box<str>, &'static str)>,
    /// The preprocessor symbols defined for this compilation (from `#define`), so a call to a
    /// `[Conditional("X")]` method with no `X` here is omitted (24.4.2). Empty by default.
    defined_symbols: alloc::collections::BTreeSet<Box<str>>,
    /// Local constants in scope (15.5.1) mapped to their folded value and declared type: a
    /// reference to one binds to its constant, not a variable load, and the declaration emits
    /// nothing. Reset per method. Keyed by name; a local constant is also declared as a local
    /// so a redeclaration is still `CS0128`/`CS0136`.
    const_locals: BTreeMap<String, (Literal, TypeSymbol, usize)>,
    /// CS0414 field-use tracking, accumulated across a whole compilation unit. Each pair is a
    /// field's declaring type (dotted name) and its name. A WRITE is a simple `=` target or an
    /// initializer; a READ is every other access. A private, non-const field that the unit
    /// writes but never reads is warned. Deferred to a final pass (not per method or per type)
    /// because a nested type may read an enclosing field either side of its declaration; reset
    /// per unit by [`Binder::report_unused_fields`].
    field_reads: alloc::collections::BTreeSet<(Box<str>, Box<str>)>,
    field_writes: alloc::collections::BTreeSet<(Box<str>, Box<str>)>,
    /// Candidate CS0414/CS0169 fields, one per declarator. The final pass warns a
    /// written-never-read field CS0414, and an eligible never-written-never-read one CS0169.
    private_fields: Vec<PrivateField>,
    /// Fields whose only assignment is itself an error -- a readonly violation (CS0191) or a value
    /// that does not convert (CS0029). csc suppresses every unused-field warning (CS0414/CS0169/
    /// CS0649) for such a field: it is referenced (so not "never used") but not validly written (so
    /// not "assigned but unused"). Keyed like [`Self::field_writes`]; cleared per unit.
    fields_with_errors: alloc::collections::BTreeSet<(Box<str>, Box<str>)>,
    /// True while binding a field's variable initializer (17.4.5): the expression runs with no
    /// instance, so an implicit-`this` reference to a non-static field, method, or property of the
    /// containing type is `CS0236` -- the field-initializer twin of the static-method `CS0120`. A
    /// field initializer contains no method bodies in ISO-1 (no lambdas/anonymous methods), so the
    /// flag never nests; it is saved/restored regardless, for robustness.
    in_field_initializer: bool,
}

/// One candidate for the unused-field warnings (CS0414 / CS0169 / CS0649).
#[derive(Debug)]
struct PrivateField {
    /// The declaring type's dotted name, as a lookup KEY -- it must match the key every field
    /// read and write is recorded under, so it keeps the metadata spelling.
    declaring: Box<str>,
    /// The declaring type's name as a DIAGNOSTIC READS IT, which is not the key: a generic type is
    /// keyed `` Box`1 `` and spelled `Box<T>`. Two jobs that were one string until a generic type
    /// could declare a field, at which point every message about one quoted a name that appears
    /// nowhere in the source it is describing.
    declaring_display: Box<str>,
    /// The field's name.
    name: Box<str>,
    /// The declarator's own byte range -- where the warning is reported.
    span: Span,
    /// The byte range from the start of the field DECLARATION up to the first declarator: the
    /// modifiers, the attributes and the TYPE, which every declarator in the declaration shares.
    /// A diagnostic landing in here belongs to all of them; see [`Binder::report_unused_fields`].
    shared_prefix: Span,
    /// Whether the field is eligible for the CS0169 "never used" warning: additionally a
    /// resolved type, no initializer, and not a duplicate.
    eligible_never_used: bool,
    /// Whether the field is `private`, which decides WHICH of the three warnings it can draw.
    ///
    /// CS0169 (never used) and CS0414 (assigned, never read) are private-only, because a
    /// non-private field may be used by an assembly this compilation cannot see -- so "unused" is
    /// not a claim we can make.
    is_private: bool,
    /// Whether an assembly OTHER than this one could ASSIGN the field -- the rule CS0649 keys on.
    ///
    /// The same excuse `is_private` encodes for CS0169/CS0414, applied to writing rather than
    /// reading: if code we cannot see may assign the field, "never assigned" is not a claim we can
    /// make. It is the field's accessibility narrowed by every type it is nested in, so a `public`
    /// field of an `internal` type is NOT assignable from outside and does warn.
    assignable_from_outside: bool,
    /// The rendered default value, for the CS0649 message.
    default_value: Box<str>,
}

/// Whether an assembly other than this one could ASSIGN a field of `access` declared in `declaring`
/// -- the condition CS0649 is withheld under.
///
/// **Measured against csc `/langversion:ISO-1`, one compilation per row**:
///
/// | field | enclosing type | csc |
/// |---|---|---|
/// | `private` / `internal` | public | CS0649 |
/// | `protected` / `protected internal` / `public` | public | -- |
/// | `public` | internal | **CS0649** |
///
/// That last row is the one that separates the two candidate rules. The earlier measurement varied
/// the field's accessibility while leaving the enclosing type at C#'s default -- which is
/// `internal` -- so every row warned, "csc reports it at every accessibility" fit the data
/// perfectly, and the instrument could not tell the two apart.
fn assignable_from_outside_assembly(model: &Model, declaring: &TypeSymbol, access: Accessibility) -> bool {
    reaches_outside(access) && type_is_externally_visible(model, declaring)
}

/// Whether `access` lets code in another assembly touch the member at all, IGNORING where it is
/// declared. `protected` counts: a class in another assembly may derive and assign it.
fn reaches_outside(access: Accessibility) -> bool {
    matches!(
        access,
        Accessibility::Public | Accessibility::Protected | Accessibility::ProtectedInternal
    )
}

/// Whether `symbol` is reachable from outside this assembly -- its own accessibility, and every
/// enclosing type's, all the way out.
///
/// A type this model cannot resolve is treated as VISIBLE, which SUPPRESSES the warning rather than
/// inventing one. That is the same safe direction the rest of the unused-field checks take: a
/// missing warning is a gap, a wrong one is a defect.
fn type_is_externally_visible(model: &Model, symbol: &TypeSymbol) -> bool {
    let mut info = model.get_by_symbol(symbol);
    while let Some(type_info) = info {
        if !reaches_outside(type_info.accessibility) {
            return false;
        }
        match type_info.enclosing.as_deref() {
            None => return true,
            Some(enclosing) => info = model.get_by_symbol(&named_symbol_from_dotted(enclosing)),
        }
    }
    true
}

impl Binder {
    /// A fresh binder with an empty reference world.
    #[must_use]
    pub fn new() -> Binder {
        Binder::default()
    }

    /// Compiles this unit as `version` rather than as [`LanguageVersion::DEFAULT`].
    ///
    /// Only a caller that actually parsed a `/langversion` sets this; every other entry point
    /// leaves the default, because a compilation with no command line behind it has not selected a
    /// dialect. (The same reasoning as `unsafe_option_missing`.)
    pub fn set_language_version(&mut self, version: LanguageVersion) {
        self.language_version = version;
    }

    /// Refuses `feature` unless this compilation can admit it, and says WHICH of the two reasons.
    ///
    /// **The whole two-bit rule, in one place.** A construct needs its dialect to PERMIT it and this
    /// build to have BUILT it, and the two failures want different diagnostics:
    ///
    /// * not permitted -> the `CS8022` family, whose code names the dialect being compiled and whose
    ///   message names the one that would work. Moving up the language version fixes it.
    /// * permitted but not built -> `LAM0001`. **Moving up the language version does NOT fix it**,
    ///   which is exactly why it must not borrow the other message.
    ///
    /// Sites for features introduced above [`LanguageVersion::SELECTABLE_MAX`] do not call this:
    /// `supports` is false for them under every selectable dialect, so the version diagnostic is
    /// unconditionally right and there is no second bit to consult.
    pub(crate) fn gate_feature(&mut self, feature: Feature, span: Span) {
        let current = self.language_version;
        let kind = if !current.supports(feature) {
            DiagnosticKind::FeatureRequiresLaterVersion {
                feature: feature.description().into(),
                required: feature.introduced_in().required_name().into(),
                current,
            }
        } else if !feature.is_implemented() {
            DiagnosticKind::FeatureNotInThisBuild {
                feature: feature.description().into(),
                permitted_by: current,
            }
        } else {
            return;
        };
        self.report(Diagnostic::new(kind, span));
    }

    /// A binder that resolves named types against `world` (existence only; member
    /// lookup needs [`Binder::with_model`]).
    #[must_use]
    pub fn with_world(world: TypeTable) -> Binder {
        Binder {
            world,
            ..Binder::default()
        }
    }

    /// Brings a generic declaration's type parameters into scope AS TYPES, for the duration of its
    /// body, and returns the names actually added so they can be withdrawn again.
    ///
    /// **Why the type table rather than a `TypeSymbol` variant.** A type parameter IS a type inside
    /// its declaration, and `resolve_type` already answers "is this name a type" from this table --
    /// so scoping the name is the whole change. The alternative, a dedicated `TypeSymbol` variant,
    /// reaches **662 sites across 17 files**, and the largest consumers carry wildcard arms
    /// (`bound.rs` 67, `program.rs` 35) that would ABSORB it silently rather than failing to
    /// compile. Under bake-time lowering a parameter becomes a concrete type anyway; what the
    /// binder needs is for `T` to resolve, not for it to be a new kind of thing.
    ///
    /// **THE PARAMETER SHADOWS A REAL TYPE OF THE SAME NAME**, and the displaced entry is carried
    /// in the returned scope so [`Binder::exit_type_parameters`] can put it back. The rule itself
    /// is stated once, on [`TypeTable::shadow`]; this is its use site, not a second copy of it.
    ///
    /// the parameter inside the declaration, so `class T {} class Box<T> { T Value; }` gave
    /// `Value` the CLASS. That is not a message difference -- it binds a different type, and every
    /// signature built from it is wrong. `shadow`/`unshadow` gets both halves.
    /// **PUBLIC BECAUSE THE EMITTER RE-BINDS BODIES AND MUST DO IT IN THE SAME SCOPE.**
    /// `lamella-assemble`'s `emit_type` binds every method body a second time; without entering
    /// this scope there, `T` resolved in the diagnostic pass and became the ERROR type at emit --
    /// two phases, two answers, and the emit-side diagnostic reaches no one.
    ///
    /// **`clauses` IS A REQUIRED ARGUMENT AND NOT AN OPTION, WHICH IS THE POINT.** A caller that
    /// entered a scope without them would leave every parameter looking unconstrained, and the
    /// CS0304 check reads exactly that -- so `new T()` under a written `where T : new()` would be
    /// refused. Making it a parameter means the four call sites had to supply it rather than
    /// acquire it later; an empty slice is the honest answer where there is no syntax to read.
    pub fn enter_type_parameters(
        &mut self,
        parameters: &[TypeParameter],
        clauses: &[TypeParameterConstraintClause],
    ) -> Vec<(Box<str>, Shadowed)> {
        let names: Vec<Box<str>> = parameters
            .iter()
            .map(|parameter| parameter.name.clone())
            .collect();
        let constraints = crate::declaration::constraints_by_parameter(&names, clauses);
        let entered: Vec<(Box<str>, Shadowed)> = parameters
            .iter()
            .map(|parameter| {
                let displaced = self.world.shadow("", &parameter.name);
                (parameter.name.clone(), displaced)
            })
            .collect();
        for (name, constraint) in names.into_iter().zip(constraints) {
            self.type_parameters_in_scope.push((name, constraint));
        }
        entered
    }

    /// The constraints written for the in-scope type parameter named `name`, innermost first, or
    /// `None` if no such parameter is in scope.
    ///
    /// Searched from the END so a method's `T` shadows its type's `T` -- the same direction
    /// [`Binder::exit_type_parameters`] unwinds in, and for the same reason.
    #[must_use]
    pub(crate) fn type_parameter_in_scope(
        &self,
        name: &str,
    ) -> Option<&crate::symbols::TypeParameterConstraints> {
        self.type_parameters_in_scope
            .iter()
            .rev()
            .find(|(parameter, _)| &**parameter == name)
            .map(|(_, constraints)| constraints)
    }

    /// The type a member lookup on `ty` actually searches: `System.Array` for an array, a TYPE
    /// PARAMETER's effective base class for a bare parameter, and `ty` itself otherwise.
    ///
    /// **A BARE TYPE PARAMETER IS NOT A TYPE THE MODEL HOLDS, SO A LOOKUP THAT STARTS AT IT FINDS
    /// NOTHING AND THE CALL IS REFUSED.** `t.ToString()` on an unconstrained `T` and `a.Legs()`
    /// under `where T : Animal` are the same failure: `type_info_of` has no entry for `T`, the
    /// chain walk visits nothing, and emission reports *a call that did not resolve*. The members
    /// a parameter offers are those of its EFFECTIVE BASE CLASS (25.7) -- `System.Object` when
    /// nothing is written, `System.ValueType` under `where T : struct`, and the named class when
    /// there is one.
    ///
    /// **IT IS A METHOD RATHER THAN A FREE FUNCTION BECAUSE THE ANSWER NEEDS THE SCOPE**, and it
    /// replaced a free `member_lookup_type` at TEN call sites for that reason: a rule with ten
    /// implementations gains a new case in none of them, and this case had to reach every walk that
    /// resolves a member -- methods, fields, properties, events, extension lookup and the accessor
    /// declarers -- or a constrained call would work in one position and refuse in the next.
    ///
    /// **INTERFACE CONSTRAINTS ARE NOT SEARCHED: `where T : IFoo` then `t.Foo()` is refused.** Only
    /// the effective base class is consulted, so a member reached through an interface constraint
    /// is a known gap rather than a silent wrong answer.
    #[must_use]
    pub(crate) fn lookup_type_of(&self, ty: &TypeSymbol) -> TypeSymbol {
        let TypeSymbol::Named(parts) = ty else {
            return member_lookup_type(ty);
        };
        let [only] = &parts[..] else {
            return member_lookup_type(ty);
        };
        let Some(constraints) = self.type_parameter_in_scope(only) else {
            return member_lookup_type(ty);
        };
        let named_class = constraints.types.iter().find(|constraint| {
            self.type_info_of(constraint)
                .is_some_and(|info| info.kind != TypeKind::Interface)
        });
        match named_class {
            Some(class) => class.clone(),
            None if constraints.value_type => type_symbol_in("System", "ValueType"),
            None => type_symbol_in("System", "Object"),
        }
    }

    /// Closes the scope [`Binder::enter_type_parameters`] opened, restoring whatever each parameter
    /// displaced.
    ///
    /// **In reverse**, so that two parameters spelled the same way -- illegal C# (CS0692), but the
    /// binder still has to unwind a program it is refusing -- restore to the entry that was there
    /// before the FIRST of them, rather than to the one the first displaced.
    pub fn exit_type_parameters(&mut self, entered: Vec<(Box<str>, Shadowed)>) {
        let remaining = self.type_parameters_in_scope.len().saturating_sub(entered.len());
        self.type_parameters_in_scope.truncate(remaining);
        for (name, displaced) in entered.into_iter().rev() {
            self.world.unshadow("", &name, displaced);
        }
    }

    /// A binder that resolves type names and looks members up against `model`.
    #[must_use]
    pub fn with_model(model: Model) -> Binder {
        Binder {
            world: model.type_table(),
            model,
            ..Binder::default()
        }
    }

    /// Resolves `ty` but DISCARDS any diagnostic the attempt produces. For a check that needs to
    /// know what a name means and is not the place its absence should be reported -- an attribute
    /// name, say, whose CS0246 belongs to whatever else names that type. Without this, adding a
    /// rule that merely CONSULTS a type starts reporting its absence as a side effect.
    pub(crate) fn resolve_named_type_quietly(&mut self, ty: &TypeSymbol, span: Span) -> TypeSymbol {
        let before = self.diagnostics.len();
        let resolved = self.resolve_named_type(ty, span);
        self.diagnostics.truncate(before);
        resolved
    }

    /// Refuses a constructed generic type at a USE site: `LAM0001`, once per written reference.
    ///
    /// SHALLOW -- it tests `ty` itself and does not look inside an array, pointer or byref. That is
    /// right for a caller that already recurses into element types, which
    /// [`resolve_named_type`](Self::resolve_named_type) does: `List<int>[]` reaches this twice,
    /// once as the array and once as its element, and only the second fires. A caller that does not
    /// recurse wants [`gate_generic_use_including_elements`](Self::gate_generic_use_including_elements).
    ///
    /// Neither form looks inside an instantiation's ARGUMENTS. `List<List<int>>` is ONE reference
    /// the programmer wrote and draws ONE refusal; reporting per level would make a nested type
    /// noisier than a flat one for no added information.
    pub(crate) fn gate_generic_use(&mut self, ty: &TypeSymbol, span: Span) {
        if matches!(ty, TypeSymbol::Instantiation { .. }) {
            self.gate_feature(Feature::Generics, span);
        }
    }

    /// [`gate_generic_use`](Self::gate_generic_use) for a caller that does NOT recurse into element
    /// types, so it walks them itself.
    ///
    /// **THIS EXISTS BECAUSE A POSITION CAN BE UNGATED WITHOUT BEING UNBOUND, AND ONE IS.** A
    /// delegate's return and parameter types are never resolved by the binder -- `delegate Missing
    /// D();` draws no `CS0246` at all, which is a pre-existing under-report, separate from generics
    /// and not fixed here (`bind_namespace_body`'s member loop has the same deliberate omission for
    /// an event's type, and for the same cascade reason). The feature gate must still be total, so
    /// that position gates through here rather than through resolution.
    pub(crate) fn gate_generic_use_including_elements(&mut self, ty: &TypeSymbol, span: Span) {
        match ty {
            TypeSymbol::Array { element, .. }
            | TypeSymbol::Pointer(element)
            | TypeSymbol::ByRef(element) => {
                self.gate_generic_use_including_elements(element, span);
            }
            _ => self.gate_generic_use(ty, span),
        }
    }

    /// Records that the driver's command line omitted `/unsafe`, so any `unsafe` in the source is
    /// `CS0227`. Called only by a driver that actually parsed options.
    pub fn set_unsafe_option_missing(&mut self, missing: bool) {
        self.unsafe_option_missing = missing;
    }

    /// Whether `unsafe` may appear at all in this compilation.
    pub(crate) fn unsafe_option_missing(&self) -> bool {
        self.unsafe_option_missing
    }

    /// The binder's type model, for the assembling step (base classes, member kinds).
    #[must_use]
    pub fn model(&self) -> &Model {
        &self.model
    }

    /// Sets the `#define`d preprocessor symbols for this compilation, so a call to a
    /// `[Conditional("X")]` method with no `X` here is omitted (24.4.2).
    pub fn set_defined_symbols(&mut self, symbols: alloc::collections::BTreeSet<Box<str>>) {
        self.defined_symbols = symbols;
    }

    /// Records a diagnostic.
    pub(crate) fn report(&mut self, diagnostic: Diagnostic) {
        self.diagnostics.push(diagnostic);
    }

    /// Marks every diagnostic reported since `first` as BODY phase, so the declaration gate can
    /// withhold them (see [`crate::withhold_body_diagnostics_after_declaration_error`]).
    ///
    /// TAGGING A RANGE ON THE WAY OUT, rather than at each of the ~200 construction sites, is what
    /// keeps this from touching the whole binder -- and it is also the only form that survives the
    /// speculative binds. Several paths bind an expression, dislike the result and `truncate` the
    /// diagnostics back to a checkpoint; marking as we go would leave tags on entries that no
    /// longer exist. Marking afterwards, over whatever survived, cannot.
    fn mark_body_phase(&mut self, first: usize) {
        for diagnostic in &mut self.diagnostics[first..] {
            diagnostic.phase = crate::diagnostic::DiagnosticPhase::Body;
        }
    }

    /// A declaring type's name as a DIAGNOSTIC should render it: `Box<T>`, not `` Box`1 ``.
    ///
    /// The metadata spelling is right for a lookup key and wrong in a message -- it names a type
    /// the programmer did not write, using a character C# has no meaning for. csc quotes
    /// `Box<T>.item`, so a message quoting `` Box`1.item `` diverges on the one part of the
    /// diagnostic a reader searches their own source for.
    ///
    /// A type with no parameters renders exactly as before, so no non-generic message moves.
    fn declaring_display_name(&self, declaring: &TypeSymbol) -> Box<str> {
        let Some(key) = crate::flow::field_type_key(declaring) else {
            return Box::from("");
        };
        let parameters = self
            .model
            .get_by_symbol(declaring)
            .map(|info| info.type_parameters.clone())
            .unwrap_or_default();
        if parameters.is_empty() {
            return key;
        }
        let (prefix, last) = match key.rfind('.') {
            Some(dot) => (&key[..=dot], &key[dot + 1..]),
            None => ("", &key[..]),
        };
        let bare = crate::symbols::unmangled_type_name(last);
        let mut rendered = String::from(prefix);
        rendered.push_str(&bare);
        rendered.push('<');
        for (index, parameter) in parameters.iter().enumerate() {
            if index > 0 {
                rendered.push(',');
            }
            rendered.push_str(parameter);
        }
        rendered.push('>');
        rendered.into()
    }

    /// Records a private, non-const field (its declaring type, name, declarator span, and CS0169
    /// eligibility) as a candidate for the unused-field warnings, judged in the final pass.
    pub(crate) fn record_private_field(
        &mut self,
        declaring: &TypeSymbol,
        name: &str,
        span: Span,
        shared_prefix: Span,
        eligible_never_used: bool,
        access: Accessibility,
        default_value: Box<str>,
    ) {
        let assignable_from_outside =
            assignable_from_outside_assembly(&self.model, declaring, access);
        let declaring_display = self.declaring_display_name(declaring);
        if let Some(declaring) = crate::flow::field_type_key(declaring) {
            self.private_fields.push(PrivateField {
                declaring,
                declaring_display,
                name: name.into(),
                span,
                shared_prefix,
                eligible_never_used,
                is_private: access == Accessibility::Private,
                assignable_from_outside,
                default_value,
            });
        }
    }

    /// Records that an attribute named argument (`[A(F = 1)]`) both READS and WRITES the field it
    /// names, so none of the three unused-field warnings fires for it.
    ///
    /// BOTH, and neither is padding. It is an assignment, so CS0649 ("never assigned") must not
    /// fire -- that is the false positive this exists to prevent, and it appeared the moment
    /// CS0649 stopped being private-only, because an attribute argument is the one assignment
    /// that lives outside every method body. It is also a USE, so CS0169 ("never used") stays
    /// silent, matching csc: a private field named only by an attribute draws CS0122 alone.
    /// Recording only the write would trade one wrong warning for another (CS0414, "assigned but
    /// never read").
    pub(crate) fn record_attribute_named_argument(&mut self, declaring: &TypeSymbol, field: &str) {
        if let Some(key) = crate::flow::field_type_key(declaring) {
            self.field_reads.insert((key.clone(), field.into()));
            self.field_writes.insert((key, field.into()));
        }
    }

    /// Records that a field's assignment is itself an error (a readonly violation, or a value that
    /// does not convert), so the final pass suppresses the unused-field warnings for it -- csc does
    /// not warn a field unused when its only write is erroneous (it is referenced, but not validly
    /// assigned).
    fn record_field_write_error(&mut self, declaring: &TypeSymbol, name: &str) {
        if let Some(key) = crate::flow::field_type_key(declaring) {
            self.fields_with_errors.insert((key, name.into()));
        }
    }

    /// Emits `CS0414` for every private, non-const field the unit assigned (an initializer or a
    /// simple `=`) but never read, then clears the field-use accumulators for the next unit. A
    /// private field's every access is within its own type, so once the whole unit is bound the
    /// read set is complete -- including a read from a nested type on either side of the field's
    /// declaration, which a per-type pass would miss.
    pub(crate) fn report_unused_fields(&mut self) {
        let first_unused_field_diagnostic = self.diagnostics.len();
        let declaration_errors: alloc::vec::Vec<Span> =
            self.diagnostics.iter().map(|d| d.span).collect();
        let suppressed = |field: &PrivateField| {
            declaration_errors.iter().any(|s| {
                let within = |r: Span| s.start >= r.start && s.start < r.end;
                within(field.span) || within(field.shared_prefix)
            })
        };
        for field in core::mem::take(&mut self.private_fields) {
            if suppressed(&field) {
                continue;
            }
            let PrivateField {
                declaring,
                declaring_display,
                name,
                span,
                eligible_never_used,
                is_private,
                assignable_from_outside,
                default_value,
                ..
            } = field;
            let key = (declaring.clone(), name.clone());
            if self.fields_with_errors.contains(&key) {
                continue;
            }
            let read = self.field_reads.contains(&key);
            let written = self.field_writes.contains(&key);
            let kind = if is_private && written && !read {
                Some(DiagnosticKind::UnusedField {
                    field: format!("{declaring_display}.{name}").into(),
                })
            } else if is_private && eligible_never_used && !written && !read {
                Some(DiagnosticKind::FieldNeverUsed {
                    field: format!("{declaring_display}.{name}").into(),
                })
            } else if eligible_never_used && !written && !assignable_from_outside {
                Some(DiagnosticKind::FieldNeverAssigned {
                    field: format!("{declaring_display}.{name}").into(),
                    default: default_value,
                })
            } else {
                None
            };
            if let Some(kind) = kind {
                self.report(Diagnostic::new(kind, span));
            }
        }
        self.mark_body_phase(first_unused_field_diagnostic);
        self.field_reads.clear();
        self.field_writes.clear();
        self.fields_with_errors.clear();
    }

    /// Records the locals a `switch` case-label expression references, so the
    /// unused-local check (`CS0168`/`CS0219`) is not misled by a label folded out of
    /// the bound tree.
    pub(crate) fn record_case_label_uses(&mut self, expr: &BoundExpr) {
        crate::flow::collect_uses(expr, &mut self.case_label_uses);
    }

    /// Exempts a local from the unused-local check (`CS0168`/`CS0219`): a local whose initializer
    /// failed to convert, which csc does not also warn unused.
    pub(crate) fn exempt_local_from_unused(&mut self, name: &str) {
        self.case_label_uses.insert(name.into());
    }

    /// Resolves a syntactic type name to its canonical type via the namespaces and aliases
    /// in scope (e.g. `Type` with `using System;` -> `System.Type`), for the emitter to mint
    /// an external type's `TypeRef` in a signature. Resolution-only; reports no diagnostic.
    #[must_use]
    pub fn resolve_type(&self, ty: &TypeSymbol) -> TypeSymbol {
        if let TypeSymbol::Named(parts) = ty {
            if parts.len() == 1 {
                let name: &str = &parts[0];
                if let Some(target) = self.alias_target(name) {
                    return target.fold_builtin();
                }
                let hits = self.type_namespaces_containing(name);
                if let Some((namespace, _)) = hits.first() {
                    return type_symbol_in(namespace, name).fold_builtin();
                }
            }
        }
        ty.clone().fold_builtin()
    }

    /// Resolves a type against the reference world, reporting `CS0246` if unknown.
    ///
    /// **THIS IS ALSO THE GENERIC USE SITE'S FEATURE GATE, AND THE PLACEMENT IS THE WHOLE OF ITS
    /// CORRECTNESS.** A generic USE can appear in a program that declares no generic type, so the
    /// declaration-level gate in `bind_type_bodies` never fires for `void M() { List<int> x; }`. A
    /// gate on signatures but not body locals would ship an accepts-invalid, which for a construct
    /// no emitter can encode is a miscompile rather than a missing message.
    ///
    /// **THIS IS 21 OF THE 23 POSITIONS AND NOT ALL OF THEM, WHICH IS A MEASUREMENT AND NOT AN
    /// ESTIMATE.** A delegate's return and parameter types never reach here, so they gate at their
    /// own declaration through
    /// [`gate_generic_use_including_elements`](Self::gate_generic_use_including_elements). The
    /// division is recorded by `a_generic_use_is_refused_in_every_position_a_type_can_appear` and
    /// its control, which is a table rather than an example because "every position" is a claim an
    /// example cannot support -- and which found that exception rather than having it written in.
    pub(crate) fn resolve_named_type(&mut self, ty: &TypeSymbol, span: Span) -> TypeSymbol {
        let resolved = self.resolve_named_type_unchecked(ty, span);
        self.check_type_argument_constraints(&resolved, span);
        resolved
    }

    /// Resolves the operand of `typeof(List<>)` -- an `unbound-type-name` (14.5.11) already bound
    /// by [`bind_type`] to the spec's `object`-substituted instantiation -- to the UNBOUND GENERIC
    /// TYPE it names.
    ///
    /// Two things separate this from [`resolve_named_type`](Self::resolve_named_type), and 14.5.11
    /// states both:
    ///
    /// - **Constraints are ignored.** *Evaluate the resulting type-name, while ignoring all type
    ///   parameter constraints.* The substituted `object`s satisfy nothing in particular, so
    ///   checking them would refuse `typeof(Nullable<>)` -- which csc compiles -- for a violation by
    ///   an argument the programmer did not write. This is the ONE caller outside the wrapper, and
    ///   it is here rather than at a `skip_constraints` flag so that the exemption is attached to
    ///   the construct the standard grants it to.
    /// - **The answer is the DEFINITION, not the instantiation.** *The unbound-type-name resolves to
    ///   the unbound generic type associated with the resulting constructed type.* So the resolved
    ///   `` List`1<object> `` becomes plain `` List`1 ``, through
    ///   [`definition_symbol`](crate::symbols::definition_symbol) -- the one spelling a token table
    ///   and the model both hold a generic definition under. Emission then needs no case of its own:
    ///   `ldtoken` of a [`TypeSymbol::Named`] is the `TypeRef`/`TypeDef` row it has always been.
    ///
    /// **MEASURED AGAINST csc RATHER THAN REASONED ABOUT** (`System.Reflection.Metadata` over csc's
    /// own output for this program): `typeof(List<>)` is `ldtoken` of the **TypeRef** `` List`1 ``
    /// and `typeof(Dictionary<,>)` of `` Dictionary`2 ``, while `typeof(List<int>)` beside them is a
    /// `TypeSpec`. The unbound form names no `TypeSpec` at all.
    ///
    /// A definition that does not resolve has already drawn its own diagnostic (CS0246, or CS0305
    /// where the name is in scope at another arity), and `Error` falls through unchanged.
    pub(crate) fn resolve_unbound_generic_type(&mut self, ty: &TypeSymbol, span: Span) -> TypeSymbol {
        match self.resolve_named_type_unchecked(ty, span) {
            TypeSymbol::Instantiation {
                definition,
                arguments,
            } => crate::symbols::definition_symbol(&definition, arguments.len()),
            other => other,
        }
    }

    /// [`resolve_named_type`](Self::resolve_named_type) without the constraint check -- the
    /// resolution itself. The split exists so the check has exactly one place to sit, not so
    /// callers may choose to skip it: the ONE other caller is
    /// [`resolve_unbound_generic_type`](Self::resolve_unbound_generic_type), which skips it because
    /// ECMA-334 4th ed 14.5.11 says to and quotes the sentence.
    fn resolve_named_type_unchecked(&mut self, ty: &TypeSymbol, span: Span) -> TypeSymbol {
        self.gate_generic_use(ty, span);
        if let TypeSymbol::Special(special) = ty {
            return self.resolve_special_type(*special, span);
        }
        if let TypeSymbol::Named(parts) = ty {
            if parts.len() == 1 {
                let name: &str = &parts[0];
                if let Some(target) = self.alias_target(name) {
                    return target.fold_builtin();
                }
                let hits = self.type_namespaces_containing(name);
                if let Some((namespace, _)) = hits.first() {
                    return type_symbol_in(namespace, name).fold_builtin();
                }
            }
        }
        if let TypeSymbol::Instantiation {
            definition,
            arguments,
        } = ty
            && let [only] = &definition[..]
        {
            let mangled = crate::symbols::definition_metadata_name(definition, arguments.len());
            if let Some((namespace, _)) = self.type_namespaces_containing(&mangled).first() {
                let mut parts: Vec<Box<str>> = Vec::new();
                if !namespace.is_empty() {
                    parts.extend(namespace.split('.').map(Box::from));
                }
                parts.push(only.clone());
                let resolved: Vec<TypeSymbol> = arguments
                    .iter()
                    .map(|argument| self.resolve_named_type(argument, span))
                    .collect();
                if resolved.iter().any(TypeSymbol::is_error) {
                    return TypeSymbol::Error;
                }
                return TypeSymbol::Instantiation {
                    definition: parts.into(),
                    arguments: resolved.into(),
                };
            }
        }
        if let TypeSymbol::Array { element, rank } = ty {
            return TypeSymbol::Array {
                element: Box::new(self.resolve_named_type(element, span)),
                rank: *rank,
            };
        }
        if let TypeSymbol::Pointer(element) = ty {
            return TypeSymbol::Pointer(Box::new(self.resolve_named_type(element, span)));
        }
        if let TypeSymbol::Named(parts) = ty {
            if let [prefix @ .., name] = &parts[..] {
                if !prefix.is_empty() {
                    let prefix_ns = prefix.join(".");
                    if let Some(full_ns) = self.resolve_partial_namespace(&prefix_ns) {
                        if self.model.get(&full_ns, name).is_some() {
                            return qualified_type_symbol(&full_ns, name).fold_builtin();
                        }
                    }
                }
            }
        }
        if let TypeSymbol::Named(parts) = ty {
            if let [outer, name] = &parts[..] {
                let enclosing: Vec<(Box<str>, ())> = self
                    .type_namespaces_containing(outer)
                    .into_iter()
                    .map(|(namespace, _)| (namespace, ()))
                    .collect();
                for (namespace, ()) in enclosing {
                    let enclosing_full = if namespace.is_empty() {
                        outer.to_string()
                    } else {
                        alloc::format!("{namespace}.{outer}")
                    };
                    if self.model.get(&enclosing_full, name).is_some() {
                        return qualified_type_symbol(&enclosing_full, name).fold_builtin();
                    }
                }
            }
        }
        match resolve_type(&self.world, ty, &mut self.diagnostics, span).fold_builtin() {
            TypeSymbol::Special(special) => self.resolve_special_type(special, span),
            resolved => resolved,
        }
    }

    /// Reports every constraint (25.7.1) that a constructed type's arguments fail to satisfy.
    ///
    /// **THIS RUNS WHERE THE ARITY CHECK RUNS, AND THAT PLACEMENT IS THE WHOLE OF ITS COVERAGE.**
    /// `resolve_named_type` is the 21-of-23 position seam the generic use-site gate already
    /// documents, so a constraint violation is caught in a local's type, a parameter, a return, a
    /// field, a base clause and a `new` -- rather than in whichever one a test happened to write.
    /// The two positions it does not reach are a delegate's return and parameter types, which
    /// cannot name a constructed type in this parser at all.
    ///
    /// **THE ARGUMENTS ARE CHECKED AGAINST THE DEFINITION'S OWN CONSTRAINTS, NOT AN INSTANTIATED
    /// COPY'S.** `Model::get_by_symbol` on an instantiation BUILDS a substituted `TypeInfo`, which
    /// is the right answer for member lookup and the wrong one here: after substitution the
    /// parameter list belongs to the closed type and the constraint on `T` no longer has a `T` to
    /// be about. The definition row is fetched directly for that reason.
    ///
    /// **Silence when the definition is absent is deliberate and is not a hole**: an
    /// instantiation whose definition does not resolve has already drawn CS0246 or CS0305 from
    /// `definition_refusal`, and a second message about constraints on a type that was never found
    /// would be a cascade.
    fn check_type_argument_constraints(&mut self, resolved: &TypeSymbol, span: Span) {
        let TypeSymbol::Instantiation {
            definition,
            arguments,
        } = resolved
        else {
            return;
        };
        let (namespace, name) = crate::resolve::split_name(definition);
        let metadata_name = crate::symbols::definition_metadata_name(definition, arguments.len());
        let Some(info) = self.model.get(&namespace, &metadata_name).cloned() else {
            return;
        };
        let declaration =
            crate::resolve::quote_candidate(name, info.type_parameters.len(), &info.type_parameters);
        for (index, argument) in arguments.iter().enumerate() {
            if argument.is_error() {
                continue;
            }
            let Some(constraints) = info.constraints_on(index) else {
                continue;
            };
            if constraints.is_empty() {
                continue;
            }
            let parameter: Box<str> = info.type_parameters[index].clone();
            self.check_one_type_argument(
                argument,
                &constraints.clone(),
                &parameter,
                &declaration,
                span,
            );
        }
    }

    /// The four constraint tests for ONE type argument of a generic TYPE.
    fn check_one_type_argument(
        &mut self,
        argument: &TypeSymbol,
        constraints: &crate::symbols::TypeParameterConstraints,
        parameter: &str,
        declaration: &str,
        span: Span,
    ) {
        self.check_one_type_argument_as(
            argument,
            constraints,
            parameter,
            declaration,
            GenericMember::Type,
            span,
        );
    }

    /// The four constraint tests for ONE type argument, in csc's reporting order.
    ///
    /// **ONE IMPLEMENTATION FOR TYPES AND METHODS.** The rules are identical and only the noun in
    /// the message differs -- csc says *"in the generic type 'Box<T>'"* and *"in the generic method
    /// 'C.M<T>()'"* under the SAME four codes. `member` is that noun. Writing the tests twice is
    /// how the next constraint kind would land on one of them.
    fn check_one_type_argument_as(
        &mut self,
        argument: &TypeSymbol,
        constraints: &crate::symbols::TypeParameterConstraints,
        parameter: &str,
        declaration: &str,
        member: GenericMember,
        span: Span,
    ) {
        if constraints.reference_type && !crate::conversion::is_reference_type(&self.model, argument)
        {
            self.diagnostics.push(Diagnostic::new(
                DiagnosticKind::TypeArgumentMustBeReferenceType {
                    argument: alloc::format!("{argument}").into(),
                    parameter: parameter.into(),
                    declaration: declaration.into(),
                    member,
                },
                span,
            ));
        }
        if constraints.value_type && !self.is_value_type(argument) {
            self.diagnostics.push(Diagnostic::new(
                DiagnosticKind::TypeArgumentMustBeValueType {
                    argument: alloc::format!("{argument}").into(),
                    parameter: parameter.into(),
                    declaration: declaration.into(),
                    member,
                },
                span,
            ));
        }
        if constraints.default_constructor
            && !self.has_accessible_parameterless_constructor(argument)
        {
            self.diagnostics.push(Diagnostic::new(
                DiagnosticKind::TypeArgumentNeedsDefaultConstructor {
                    argument: alloc::format!("{argument}").into(),
                    parameter: parameter.into(),
                    declaration: declaration.into(),
                    member,
                },
                span,
            ));
        }
        for constraint in &constraints.types {
            let constraint = self.resolve_named_type_quietly(constraint, span);
            if constraint.is_error() {
                continue;
            }
            if !crate::conversion::converts(&self.model, argument, &constraint) {
                self.diagnostics.push(Diagnostic::new(
                    DiagnosticKind::TypeArgumentNoConversionToConstraint {
                        argument: alloc::format!("{argument}").into(),
                        parameter: parameter.into(),
                        declaration: declaration.into(),
                        constraint: alloc::format!("{constraint}").into(),
                        member,
                    },
                    span,
                ));
            }
        }
    }

    /// Whether `ty` satisfies a `new()` constraint: a value type (which always has one), or a
    /// non-abstract type declaring a public parameterless constructor -- including one that
    /// declares NO constructor at all, which C# gives an implicit public parameterless one (17.10).
    ///
    /// **A type this model cannot see answers `true`.** That is the same safe under-report
    /// `is_sealed` and `is_abstract` take for a referenced or synthetic type: a missed CS0310 is a
    /// gap, whereas a false one would refuse a legal program against an assembly we merely failed
    /// to decode.
    fn has_accessible_parameterless_constructor(&self, ty: &TypeSymbol) -> bool {
        if self.is_value_type(ty) {
            return true;
        }
        let Some(info) = self.model.get_by_symbol(ty) else {
            return true;
        };
        if info.is_abstract {
            return false;
        }
        if info.constructors.is_empty() {
            return true;
        }
        info.constructors.iter().any(|constructor| {
            constructor.parameters.is_empty()
                && matches!(constructor.accessibility, Accessibility::Public)
        })
    }

    /// Resolves a predefined type's mention: [`TypeSymbol::Special`] when its `System`
    /// backing type is defined or imported (or when no corlib is present at all), else
    /// csc's CS0518 at `span` and the error type. The null type has no `System` identity
    /// and always resolves.
    pub(crate) fn resolve_special_type(&mut self, special: SpecialType, span: Span) -> TypeSymbol {
        if matches!(special, SpecialType::Null)
            || !self.corlib_present()
            || self.special_backing_defined(special)
        {
            return TypeSymbol::Special(special);
        }
        let (namespace, name) = special.full_name();
        self.diagnostics.push(Diagnostic::new(
            DiagnosticKind::PredefinedTypeMissing {
                full_name: alloc::format!("{namespace}.{name}").into(),
            },
            span,
        ));
        TypeSymbol::Error
    }

    /// Whether a predefined type's `System` backing type is declared in source or a
    /// reference (4.1.4).
    fn special_backing_defined(&self, special: SpecialType) -> bool {
        let (namespace, name) = special.full_name();
        self.model.get(namespace, name).is_some() || self.world.contains(namespace, name)
    }

    /// Whether the compilation carries a corlib: `System.Object` AND `System.Int32` are
    /// declared in source or a reference. Every real corlib -- including a no-float one --
    /// carries the integral core, so the anchor is invisible in practice; requiring BOTH
    /// keeps the check off for minimal hand-built models (a test fixture declaring only
    /// `System.Object` to give classes a root) and for model-less binding, where csc has
    /// no equivalent mode (it always demands a corlib), so no parity is lost.
    fn corlib_present(&self) -> bool {
        (self.model.get("System", "Object").is_some()
            || self.world.contains("System", "Object"))
            && (self.model.get("System", "Int32").is_some()
                || self.world.contains("System", "Int32"))
    }

    /// Rewrites a single-part named type to its canonical fully-qualified symbol when that
    /// simple name is unambiguous in the model, so a body-bound type (e.g. a method's declared
    /// return type, structurally bound from syntax) is the SAME [`TypeSymbol`] as the qualified
    /// form a `new`/cast produces. Mirrors [`Model::canonicalize_signatures`] for the types the
    /// body re-binds from syntax; non-reporting (an unresolved name stays as is for the normal
    /// resolver to diagnose). Arrays and pointers canonicalize their element type.
    pub(crate) fn canonicalize(&self, ty: &TypeSymbol) -> TypeSymbol {
        match ty {
            TypeSymbol::Named(parts) if parts.len() == 1 => self
                .model
                .type_with_simple_name(&parts[0])
                .unwrap_or_else(|| ty.clone())
                .fold_builtin(),
            TypeSymbol::Array { element, rank } => TypeSymbol::Array {
                element: Box::new(self.canonicalize(element)),
                rank: *rank,
            },
            TypeSymbol::Pointer(inner) => TypeSymbol::Pointer(Box::new(self.canonicalize(inner))),
            TypeSymbol::Instantiation {
                definition,
                arguments,
            } => TypeSymbol::Instantiation {
                definition: crate::symbols::canonical_definition(
                    definition,
                    arguments.len(),
                    |mangled| self.model.type_with_simple_name(mangled),
                ),
                arguments: arguments
                    .iter()
                    .map(|argument| self.canonicalize(argument))
                    .collect::<Vec<_>>()
                    .into(),
            },
            _ => ty.clone(),
        }
    }

    /// Whether `from` implicitly converts to `to`, including reference conversions
    /// that walk the model's inheritance graph (13.1).
    pub(crate) fn converts(&self, from: &TypeSymbol, to: &TypeSymbol) -> bool {
        converts(&self.model, from, to)
            || self.user_conversion(from, to, "op_Implicit").is_some()
    }

    /// Whether `value` is the integer literal `0` and `target` is an enum type -- the implicit
    /// enumeration conversion (13.1.3), which lets `E e = 0;`. ONLY the literal `0` (the spec's
    /// "decimal-integer-literal 0") converts: NOT a floating or decimal zero (`E e = 0.0;`), and NOT
    /// a named or folded constant that happens to be zero (`const int Z = 0; E e = Z;`). Those are
    /// later reference-compiler relaxations, rejected under strict C# 1.0.
    fn enum_from_zero(&self, value: &BoundExpr, target: &TypeSymbol) -> bool {
        self.type_info_of(target).map(|info| info.kind) == Some(TypeKind::Enum)
            && matches!(
                value.kind,
                BoundExprKind::Literal(Literal::Integer { value: 0, .. })
            )
    }

    /// Whether `value` is assignable to `target`: it implicitly converts by type, it is a
    /// constant whose value fits a narrower integral `target` (13.1.7), or it is the constant
    /// `0` assigned to an enum (13.1.3). Use this at an assignment context that has the value
    /// expression, not just its type.
    pub(crate) fn assignable(&self, value: &BoundExpr, target: &TypeSymbol) -> bool {
        self.converts(&value.ty, target)
            || implicit_constant_conversion(value, target)
            || self.enum_from_zero(value, target)
    }

    /// Reports a failed conversion (`CS0266`/`CS0029`) at an assignment context unless
    /// `value` is assignable to `target` (including the constant-expression rule). Error
    /// types are skipped so a prior failure does not cascade.
    pub(crate) fn check_assignable(&mut self, value: &BoundExpr, target: &TypeSymbol, span: Span) {
        if target.is_error() {
            return;
        }
        if let BoundExprKind::MethodGroup { name, .. } = &value.kind {
            let to_delegate = self
                .type_info_of(target)
                .is_some_and(|info| info.kind == TypeKind::Delegate);
            if !to_delegate {
                self.report(Diagnostic::new(
                    DiagnosticKind::MethodGroupToNonDelegate {
                        method: name.clone(),
                        target: target.to_string().into(),
                    },
                    span,
                ));
            }
            return;
        }
        if let BoundExprKind::TypeReference(ty) = &value.kind {
            self.report(Diagnostic::new(
                DiagnosticKind::TypeUsedAsValue {
                    type_name: ty.to_string().into(),
                },
                span,
            ));
            return;
        }
        if value.ty.is_error() {
            return;
        }
        if !self.assignable(value, target) {
            if let Some(value_text) = constant_out_of_range(value, target) {
                self.report(Diagnostic::new(
                    DiagnosticKind::ConstantOutOfRange {
                        value: value_text,
                        to: target.to_string().into(),
                    },
                    span,
                ));
            } else if matches!(value.kind, BoundExprKind::Literal(Literal::Null))
                && self.is_value_type(target)
            {
                self.report(Diagnostic::new(
                    DiagnosticKind::CannotConvertNullToValueType {
                        to: target.to_string().into(),
                    },
                    span,
                ));
            } else {
                self.report_no_implicit_conversion(&value.ty, target, span);
            }
        }
    }

    /// A user-defined conversion method (`op_Implicit`/`op_Explicit`) taking `from` and
    /// returning `to`, declared on either the source or target type (17.9.3). The static
    /// call a `from -> to` conversion lowers to.
    ///
    /// A pair for which 17.9.3 forbids declaring an operator is refused before the search runs,
    /// because the return-type test below is vacuous there: it accepts an operator whose return
    /// type merely CONVERTS to the target, and every type converts to `object`. Against the real
    /// BCL that selected `System.String.op_Implicit(String) -> ReadOnlySpan<char>` as the
    /// conversion from `string` to `object`, and emission -- which looks an operator up keyed by
    /// its return type -- then missed and refused with *"call to a method outside this module"*.
    /// `(object)aString` was nine of the C# differential's nine failures.
    ///
    /// The guard is in this function rather than at the fourteen call sites that reach it. Only the
    /// cast expression could actually select the wrong operator -- [`Self::convert`] is safe
    /// because it tests `converts` first -- but a rule delivered to one caller of fourteen is a
    /// rule the other thirteen do not have, and which of them can reach the case is not a property
    /// anyone re-derives when adding the fifteenth.
    pub(crate) fn user_conversion(
        &self,
        from: &TypeSymbol,
        to: &TypeSymbol,
        name: &str,
    ) -> Option<MethodReference> {
        if no_conversion_operator_can_exist(&self.model, from, to) {
            return None;
        }
        let mut fallback: Option<MethodReference> = None;
        for owner in [from, to] {
            for method in self.methods_in_chain(owner, name) {
                if method.parameters.len() != 1
                    || !(&method.return_type == to || converts(&self.model, &method.return_type, to))
                    || !converts(&self.model, from, &method.parameters[0])
                {
                    continue;
                }
                let exact = &method.parameters[0] == from && &method.return_type == to;
                let declaring_type = self.declaring_type_in_chain(owner, name, &method.parameters);
                let declaring_instantiation =
                    self.declaring_instantiation_of(&declaring_type, name, &method.parameters);
                let reference = MethodReference {
                    declaring_type,
                    name: name.into(),
                    parameters: method.parameters,
                    return_type: method.return_type,
                    is_static: true,
                    is_vararg: false,
                    instantiation: None,
                    declaring_instantiation,
                };
                if exact {
                    return Some(reference);
                }
                fallback.get_or_insert(reference);
            }
        }
        fallback
    }

    /// Reports a failed implicit conversion at `span`: `CS0266` when an explicit
    /// conversion (a cast) exists, otherwise `CS0029`. Use this at every assignment
    /// context (initializer, assignment, return, field initializer); a context with
    /// no cast escape (a non-`bool` condition) reports `CS0029` directly.
    pub(crate) fn report_no_implicit_conversion(
        &mut self,
        from: &TypeSymbol,
        to: &TypeSymbol,
        span: Span,
    ) {
        if is_arglist_marker(from) {
            self.report(Diagnostic::new(DiagnosticKind::ArglistOutsideCall, span));
            return;
        }
        let kind = if can_cast(&self.model, from, to) {
            DiagnosticKind::ExplicitConversionExists {
                from: from.to_string().into(),
                to: to.to_string().into(),
            }
        } else {
            DiagnosticKind::NoImplicitConversion {
                from: from.to_string().into(),
                to: to.to_string().into(),
            }
        };
        self.report(Diagnostic::new(kind, span));
    }

    /// Wraps `expr` in the implicit conversion to `target` so emission widens,
    /// boxes, or upcasts as needed (13.1). Returns `expr` unchanged when the types
    /// match or no implicit conversion applies (the site reports any error).
    pub(crate) fn convert(&self, expr: BoundExpr, target: &TypeSymbol) -> BoundExpr {
        if matches!(expr.kind, BoundExprKind::MethodGroup { .. })
            && self
                .type_info_of(target)
                .is_some_and(|info| info.kind == TypeKind::Delegate)
        {
            return self.bind_delegate_creation(target, &[expr], Span::empty_at(0));
        }
        if matches!(target, TypeSymbol::ByRef(_))
            && matches!(expr.kind, BoundExprKind::Ref { .. })
        {
            return expr;
        }
        if expr.ty == *target || expr.ty.is_error() || target.is_error() {
            return expr;
        }
        if self.enum_from_zero(&expr, target) {
            return BoundExpr {
                kind: BoundExprKind::Literal(integer_literal(0)),
                ty: target.clone(),
            };
        }
        if matches!(target, TypeSymbol::Special(SpecialType::Decimal))
            && !matches!(expr.ty, TypeSymbol::Special(SpecialType::Decimal))
        {
            if let Some(method) = self.user_conversion(&expr.ty, target, "op_Implicit") {
                return BoundExpr {
                    ty: target.clone(),
                    kind: BoundExprKind::Call {
                        callee: Box::new(error_expr()),
                        arguments: alloc::vec![expr],
                        method: Some(method),
                    },
                };
            }
        }
        if converts(&self.model, &expr.ty, target) {
            let conversion = self.conversion_kind(&expr.ty, target);
            return BoundExpr {
                kind: BoundExprKind::Conversion {
                    operand: Box::new(expr),
                    conversion,
                },
                ty: target.clone(),
            };
        }
        if implicit_constant_conversion(&expr, target) {
            return BoundExpr {
                kind: BoundExprKind::Conversion {
                    operand: Box::new(expr),
                    conversion: ConversionKind::ImplicitNumeric,
                },
                ty: target.clone(),
            };
        }
        if let Some(method) = self.user_conversion(&expr.ty, target, "op_Implicit") {
            let produced = method.return_type.clone();
            let call = BoundExpr {
                ty: produced.clone(),
                kind: BoundExprKind::Call {
                    callee: Box::new(error_expr()),
                    arguments: alloc::vec![expr],
                    method: Some(method),
                },
            };
            if produced == *target {
                return call;
            }
            return self.convert_numeric_or_relabel(call, target);
        }
        expr
    }

    /// Applies the standard conversion that follows a user-defined one. Kept separate from
    /// [`Self::convert`] so it cannot re-enter the user-conversion search and recurse: the second
    /// step of 6.4.4 is a STANDARD conversion by definition.
    fn convert_numeric_or_relabel(&self, expr: BoundExpr, target: &TypeSymbol) -> BoundExpr {
        if let (TypeSymbol::Special(from), TypeSymbol::Special(to)) = (&expr.ty, target) {
            if from.is_numeric() && to.is_numeric() {
                return BoundExpr {
                    ty: target.clone(),
                    kind: BoundExprKind::Conversion {
                        operand: Box::new(expr),
                        conversion: ConversionKind::ImplicitNumeric,
                    },
                };
            }
        }
        BoundExpr {
            ty: target.clone(),
            ..expr
        }
    }

    /// Binds an array initializer `{ e, ... }` against `array_ty`, converting each
    /// element to the array's element type. Used for `new T[]{...}` and `T[] a = {...}`.
    pub(crate) fn bind_array_initializer(
        &mut self,
        init: &Expr,
        array_ty: &TypeSymbol,
    ) -> Vec<BoundExpr> {
        let ExprKind::ArrayInitializer(elements) = &init.kind else {
            return Vec::new();
        };
        let element_ty = match array_ty {
            TypeSymbol::Array { element, .. } => (**element).clone(),
            _ => TypeSymbol::Error,
        };
        elements
            .iter()
            .map(|element| {
                let bound = self.bind_expression(element);
                self.check_assignable(&bound, &element_ty, element.span);
                self.convert(bound, &element_ty)
            })
            .collect()
    }

    /// Binds a rectangular array initializer `{{ .. }, { .. }}` (19.6): the leaf elements
    /// flattened in row-major order and converted to the element type, paired with each
    /// dimension's length, inferred from the initializer's shape (the sub-list count at each
    /// level). `None` when `array_ty` is not a rank >= 2 array, so the caller keeps the
    /// single-dimension path. The inferred lengths stand in for omitted (`new T[,]{...}`) and
    /// bare (`T[,] a = {...}`) creations alike, and equal the written lengths otherwise.
    pub(crate) fn bind_rectangular_array(
        &mut self,
        init: &Expr,
        array_ty: &TypeSymbol,
        written_lengths: &[BoundExpr],
    ) -> Option<(Vec<BoundExpr>, Vec<BoundExpr>)> {
        let TypeSymbol::Array { element, rank } = array_ty else {
            return None;
        };
        let rank = *rank as usize;
        if rank < 2 {
            return None;
        }
        let element_ty = (**element).clone();
        let mut dimensions = alloc::vec![0usize; rank];
        for (slot, written) in dimensions.iter_mut().zip(written_lengths) {
            if let BoundExprKind::Literal(Literal::Integer { value, .. }) = &written.kind {
                *slot = *value as usize;
            }
        }
        let mut elements = Vec::new();
        self.flatten_rectangular_level(init, 0, rank, &element_ty, &mut dimensions, &mut elements);
        let lengths = dimensions
            .into_iter()
            .map(|length| BoundExpr {
                kind: BoundExprKind::Literal(Literal::Integer {
                    value: length as u64,
                    suffix: IntegerSuffix::None,
                }),
                ty: TypeSymbol::Special(SpecialType::Int32),
            })
            .collect();
        Some((lengths, elements))
    }

    /// Descends one nesting level of a rectangular initializer (19.6): fixes `dimensions[depth]`
    /// from this level's sub-list count on first encounter, then recurses into each sub-list
    /// until the leaf level (`depth + 1 == rank`), where each item is a bound, converted element.
    fn flatten_rectangular_level(
        &mut self,
        init: &Expr,
        depth: usize,
        rank: usize,
        element_ty: &TypeSymbol,
        dimensions: &mut [usize],
        elements: &mut Vec<BoundExpr>,
    ) {
        let ExprKind::ArrayInitializer(items) = &init.kind else {
            return;
        };
        if dimensions[depth] == 0 {
            dimensions[depth] = items.len();
        } else if dimensions[depth] != items.len() {
            self.diagnostics.push(Diagnostic::new(
                DiagnosticKind::ArrayInitializerLength {
                    length: dimensions[depth] as u64,
                },
                init.span,
            ));
        }
        if depth + 1 == rank {
            for item in items {
                let bound = self.bind_expression(item);
                elements.push(self.convert(bound, element_ty));
            }
        } else {
            for item in items {
                self.flatten_rectangular_level(item, depth + 1, rank, element_ty, dimensions, elements);
            }
        }
    }

    /// Converts `value` to the enclosing method's return type, for a `return`.
    pub(crate) fn convert_to_return_type(&self, value: BoundExpr) -> BoundExpr {
        match &self.current_method {
            Some(method) => {
                let target = method.return_type.clone();
                self.convert(value, &target)
            }
            None => value,
        }
    }

    fn conversion_kind(&self, from: &TypeSymbol, to: &TypeSymbol) -> ConversionKind {
        if as_special(from).is_some_and(SpecialType::is_numeric)
            && as_special(to).is_some_and(SpecialType::is_numeric)
        {
            ConversionKind::ImplicitNumeric
        } else if self.is_value_type(from) && !self.is_value_type(to) {
            ConversionKind::Boxing
        } else if self.is_bare_type_parameter(from) && !self.is_value_type(to) {
            ConversionKind::Boxing
        } else {
            ConversionKind::ImplicitReference
        }
    }

    /// Whether `ty` is a BARE type parameter of the declaration being bound -- `T`, not `Box<T>`
    /// and not `T[]`, both of which are ordinary types that merely mention one.
    #[must_use]
    pub(crate) fn is_bare_type_parameter(&self, ty: &TypeSymbol) -> bool {
        matches!(ty, TypeSymbol::Named(parts)
            if matches!(&parts[..], [only] if self.type_parameter_in_scope(only).is_some()))
    }

    /// Whether a type is a value type (boxed when converted to `object`).
    pub(crate) fn is_value_type(&self, ty: &TypeSymbol) -> bool {
        match ty {
            TypeSymbol::Special(
                SpecialType::Object | SpecialType::String | SpecialType::Null,
            ) => false,
            TypeSymbol::Special(_) => true,
            TypeSymbol::Named(_) => {
                !is_reference_base_class(ty)
                    && matches!(
                        self.type_info_of(ty).map(|info| info.kind),
                        Some(TypeKind::Struct | TypeKind::Enum)
                    )
            }
            TypeSymbol::Instantiation { .. }
            | TypeSymbol::Array { .. }
            | TypeSymbol::Pointer(_)
            | TypeSymbol::ByRef(_)
            | TypeSymbol::Error => false,
        }
    }

    /// The result of `==`/`!=` when one operand is the null literal (14.9.6): the null type
    /// is reference-comparable with any reference type (and with the null type itself),
    /// giving `bool`. It is not comparable with a value type -- which has no null -- so that
    /// returns `None` and falls through to the not-applicable diagnostic. `None` also when
    /// neither operand is the null type.
    fn null_equality_result(
        &self,
        operator: BinaryOperator,
        left: &TypeSymbol,
        right: &TypeSymbol,
    ) -> Option<TypeSymbol> {
        if !matches!(operator, BinaryOperator::Equal | BinaryOperator::NotEqual) {
            return None;
        }
        let null = TypeSymbol::Special(SpecialType::Null);
        let other = if *left == null {
            right
        } else if *right == null {
            left
        } else {
            return None;
        };
        (!self.is_value_type(other)).then(|| TypeSymbol::Special(SpecialType::Boolean))
    }

    /// The diagnostics gathered so far.
    #[must_use]
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    /// Consumes the binder, returning its diagnostics.
    #[must_use]
    pub fn into_diagnostics(self) -> Vec<Diagnostic> {
        self.diagnostics
    }

    /// Takes the diagnostics reported so far, leaving the binder empty -- so a
    /// multi-unit compilation attributes each unit's diagnostics to its own file.
    pub fn take_diagnostics(&mut self) -> Vec<Diagnostic> {
        core::mem::take(&mut self.diagnostics)
    }

    /// Sets the enclosing type whose members an unqualified name and `this`
    /// resolve against, for binding that type's method bodies.
    pub fn enter_type(&mut self, ty: TypeSymbol) {
        self.current_type = Some(ty);
    }

    /// Clears the enclosing type.
    pub fn exit_type(&mut self) {
        self.current_type = None;
    }

    /// Brings a namespace into scope for unqualified type-name resolution (a
    /// `using` directive, 16.3).
    pub fn import_namespace(&mut self, namespace: &str) {
        self.imported_namespaces.push(namespace.into());
    }

    /// Brings a `using X = N.T;` alias into scope: an unqualified `X` resolves to `target`
    /// (16.4.1).
    pub fn import_alias(&mut self, name: &str, target: TypeSymbol) {
        self.aliases.push((name.into(), target));
    }

    /// The target type of an in-scope alias `name`, if any (the most recent wins).
    fn alias_target(&self, name: &str) -> Option<TypeSymbol> {
        self.aliases
            .iter()
            .rev()
            .find(|(alias, _)| &**alias == name)
            .map(|(_, target)| target.clone())
    }

    /// A marker for the current set of imported namespaces and aliases, to scope the usings
    /// of a namespace block: snapshot before, restore after.
    #[must_use]
    pub fn import_scope(&self) -> (usize, usize) {
        (self.imported_namespaces.len(), self.aliases.len())
    }

    /// Restores the imported namespaces and aliases to an earlier [`Binder::import_scope`].
    pub fn restore_import_scope(&mut self, scope: (usize, usize)) {
        self.imported_namespaces.truncate(scope.0);
        self.aliases.truncate(scope.1);
    }

    /// The current type's namespace, if any, for unqualified type resolution.
    fn current_namespace(&self) -> Option<Box<str>> {
        match &self.current_type {
            Some(TypeSymbol::Named(parts)) if parts.len() > 1 => {
                Some(parts[..parts.len() - 1].join(".").into())
            }
            _ => None,
        }
    }

    /// Resolves a namespace name written PARTIALLY against the enclosing namespaces (10.8): a name
    /// `A` used inside `namespace N.B` finds the sibling namespace `N.A`. Returns the full namespace
    /// name, most-specific enclosing scope first, or `None`. A fallback after a direct namespace
    /// lookup, so it only ever ADDS the enclosing-scope resolution -- it never changes a direct hit.
    fn resolve_partial_namespace(&self, name: &str) -> Option<Box<str>> {
        let current = self.current_namespace().unwrap_or_default();
        let mut scope: Option<&str> = Some(&current);
        while let Some(prefix) = scope {
            let candidate = if prefix.is_empty() {
                name.to_string()
            } else {
                alloc::format!("{prefix}.{name}")
            };
            if self.model.is_namespace(&candidate) {
                return Some(candidate.into());
            }
            scope = if prefix.is_empty() {
                None
            } else {
                Some(prefix.rsplit_once('.').map_or("", |(head, _)| head))
            };
        }
        None
    }

    /// The distinct in-scope namespaces (current, global, and imported) that hold
    /// a type with this name.
    /// The namespaces (in scope) that declare a type of this simple name, ordered
    /// most-specific scope first. The flag marks a hit that comes from a `using`-imported
    /// namespace: those alone share one precedence level, so two of them are a CS0104
    /// ambiguity, whereas a more-specific scope (an enclosing type, the current namespace,
    /// the global namespace) unambiguously SHADOWS every less-specific hit (3.8, 10.8).
    fn type_namespaces_containing(&self, name: &str) -> Vec<(Box<str>, bool)> {
        let mut search: Vec<(Box<str>, bool)> = Vec::new();
        if let Some(TypeSymbol::Named(parts)) = &self.current_type {
            let mut enclosing = String::new();
            for part in parts.iter() {
                if !enclosing.is_empty() {
                    enclosing.push('.');
                }
                enclosing.push_str(part);
            }
            search.push((enclosing.into(), false));
        }
        if let Some(current) = self.current_namespace() {
            search.push((current, false));
        }
        search.push((Box::from(""), false));
        search.extend(self.imported_namespaces.iter().cloned().map(|ns| (ns, true)));
        let mut hits: Vec<(Box<str>, bool)> = Vec::new();
        for (namespace, imported) in search {
            if self.model.get(&namespace, name).is_some()
                && !hits.iter().any(|(seen, _)| *seen == namespace)
            {
                hits.push((namespace, imported));
            }
        }
        hits
    }

    /// Marks the next [`Binder::bind_method`] call as binding a vararg member's body
    /// (`M(..., __arglist)` / `T(..., __arglist)`), so a bare `__arglist` binds to the
    /// runtime argument handle instead of reporting CS0190. `bind_method` consumes the
    /// mark, so it never leaks past the one member it was set for.
    pub fn set_next_method_vararg(&mut self) {
        self.next_method_vararg = true;
    }

    /// Binds a method body end to end: the enclosing type is in scope for `this`
    /// and unqualified names, the parameters are declared as locals, and `return`
    /// statements are checked against `return_type` (15.9.4). Returns the bound
    /// body.
    pub fn bind_method(
        &mut self,
        enclosing_type: Option<TypeSymbol>,
        name: &str,
        return_type: TypeSymbol,
        parameters: &[(Box<str>, TypeSymbol)],
        out_parameters: &[Box<str>],
        is_static: bool,
        body: &lamella_syntax::ast::Stmt,
    ) -> crate::statement::BoundStmt {
        let return_type = self.canonicalize(&return_type);
        let returns_value = !return_type.is_void();
        let body_span = body.span;
        let diagnostics_before_body = self.diagnostics.len();
        self.current_type = enclosing_type;
        self.current_method = Some(MethodContext {
            name: name.into(),
            return_type,
            is_static,
            is_vararg: core::mem::take(&mut self.next_method_vararg),
        });
        self.enter_scope();
        self.case_label_uses.clear();
        self.const_locals.clear();
        self.loop_depth = 0;
        self.switch_depth = 0;
        self.catch_depth = 0;
        self.finally_depth = 0;
        self.finally_floor.clear();
        for (parameter, ty) in parameters {
            self.declare_local(parameter, self.canonicalize(ty));
        }
        let bound = self.bind_statement(body);
        self.exit_scope();
        if returns_value && !crate::flow::method_body_always_exits(&bound) {
            self.diagnostics.push(Diagnostic::new(
                DiagnosticKind::NotAllPathsReturn {
                    method: name.into(),
                },
                body_span,
            ));
        }
        let parameter_names: Vec<Box<str>> = parameters
            .iter()
            .map(|(parameter, _)| parameter.clone())
            .collect();
        let unassigned = crate::flow::check_definite_assignment(
            &bound,
            &parameter_names,
            out_parameters,
            &self.model,
        );
        self.diagnostics.extend(unassigned);
        self.diagnostics.extend(crate::flow::check_unused_locals(
            &bound,
            &self.case_label_uses,
        ));
        self.diagnostics
            .extend(crate::flow::check_unreachable(&bound));
        self.diagnostics.extend(crate::flow::check_labels(&bound));
        crate::flow::collect_field_accesses(&bound, &mut self.field_reads, &mut self.field_writes);
        self.mark_body_phase(diagnostics_before_body);
        self.current_method = None;
        self.current_type = None;
        bound
    }

    /// Binds one REPL submission as the body of a `Submit$N(__Repl s)` method, in
    /// session mode: the enclosing type is `__Repl` and the implicit receiver is the
    /// parameter `receiver` (`s`), so an unqualified session variable reads/writes a
    /// field of `s` (14.5.2 against `s` rather than `this`). `initial_fields` maps each
    /// PRIOR session variable's source name to its stable field name + type (for reads);
    /// `occurrences` counts how many times each source name has already been declared (so
    /// a redefinition picks a fresh `x$2`). The introduced variables come back in
    /// [`SubmissionBinding::new_fields`] for the caller to commit.
    ///
    /// A TOP-LEVEL local declaration is not a real local: it is a persistent field, so
    /// `T x = init;` lowers to the field store `s.<stable> = init` (a declarator with no
    /// initializer just registers the field, which keeps its zero default), and a
    /// redefinition `T x = ...;` adds a fresh field `x$2` and rebinds source `x` to it.
    /// Every other statement -- including a declaration nested inside a block, an ordinary
    /// local of that block -- is bound normally. Diagnostics accumulate as usual; the
    /// caller drains them with [`Binder::into_diagnostics`].
    pub fn bind_submission(
        &mut self,
        repl_type: TypeSymbol,
        receiver: &str,
        statements: &[lamella_syntax::ast::Stmt],
        trailing: Option<&Expr>,
        initial_fields: BTreeMap<String, (Box<str>, TypeSymbol)>,
        mut occurrences: BTreeMap<String, u32>,
    ) -> SubmissionBinding {
        use crate::statement::{BoundStmt, BoundStmtKind};
        use lamella_syntax::ast::StmtKind;

        let body_span = statements
            .first()
            .map(|statement| statement.span)
            .or_else(|| trailing.map(|expr| expr.span))
            .unwrap_or(Span::empty_at(0));
        self.current_type = Some(repl_type.clone());
        self.session_receiver = Some(receiver.into());
        self.session_fields = initial_fields;
        self.current_method = Some(MethodContext {
            name: "Submit".into(),
            return_type: TypeSymbol::Special(SpecialType::Void),
            is_static: true,
            is_vararg: false,
        });
        self.enter_scope();
        self.case_label_uses.clear();
        self.const_locals.clear();
        self.loop_depth = 0;
        self.switch_depth = 0;
        self.catch_depth = 0;
        self.finally_depth = 0;
        self.finally_floor.clear();

        let mut bound = Vec::new();
        let mut new_fields = Vec::new();
        for statement in statements {
            match &statement.kind {
                StmtKind::LocalDeclaration {
                    ty, declarators, ..
                } => {
                    let field_ty = self.resolve_named_type(&bind_type(ty), ty.span);
                    for declarator in declarators {
                        let source: &str = &declarator.name;
                        let value = declarator.initializer.as_ref().map(|initializer| {
                            let value = self.bind_expression(initializer);
                            self.check_assignable(&value, &field_ty, declarator.span);
                            self.convert(value, &field_ty)
                        });
                        let count = occurrences.get(source).copied().unwrap_or(0);
                        let stable: Box<str> = if count == 0 {
                            source.into()
                        } else {
                            format!("{source}${}", count + 1).into()
                        };
                        occurrences.insert(source.into(), count + 1);
                        self.session_fields
                            .insert(source.into(), (stable.clone(), field_ty.clone()));
                        new_fields.push(DeclaredField {
                            source: source.into(),
                            stable: stable.clone(),
                            ty: field_ty.clone(),
                        });
                        if let Some(value) = value {
                            let target =
                                self.session_field_access(receiver, &repl_type, &stable, &field_ty);
                            let assignment = BoundExpr {
                                ty: field_ty.clone(),
                                kind: BoundExprKind::Assignment {
                                    operator: AssignmentOperator::Assign,
                                    target: Box::new(target),
                                    value: Box::new(value),
                                    checked: self.checked_context,
                                },
                            };
                            bound.push(BoundStmt {
                                kind: BoundStmtKind::Expression(assignment),
                                span: declarator.span,
                            });
                        }
                    }
                }
                _ => bound.push(self.bind_statement(statement)),
            }
        }

        let mut return_type = TypeSymbol::Special(SpecialType::Void);
        if let Some(expr) = trailing {
            let value = self.bind_expression(expr);
            if value.ty.is_void() || value.ty.is_error() {
                bound.push(BoundStmt {
                    kind: BoundStmtKind::Expression(value),
                    span: expr.span,
                });
            } else {
                let object = TypeSymbol::Special(SpecialType::Object);
                let display = self.convert(value, &object);
                bound.push(BoundStmt {
                    kind: BoundStmtKind::Return(Some(display)),
                    span: expr.span,
                });
                return_type = object;
            }
        }

        self.exit_scope();
        self.session_receiver = None;
        self.session_fields = BTreeMap::new();
        self.current_type = None;
        self.current_method = None;
        SubmissionBinding {
            body: BoundStmt {
                kind: BoundStmtKind::Block(bound),
                span: body_span,
            },
            return_type,
            new_fields,
        }
    }

    /// A read/write of session field `name` of type `ty`, declared on `repl_type` and
    /// reached through the session receiver parameter `receiver` (`s`). The field is a
    /// public instance field of `__Repl`, so emission lowers it to `ldarg.0` (the `s`
    /// instance) + `ldfld`/`stfld` of the `<repl>.__Repl::name` reference.
    fn session_field_access(
        &self,
        receiver: &str,
        repl_type: &TypeSymbol,
        name: &str,
        ty: &TypeSymbol,
    ) -> BoundExpr {
        BoundExpr {
            ty: ty.clone(),
            kind: BoundExprKind::FieldAccess {
                receiver: Box::new(BoundExpr {
                    kind: BoundExprKind::Local(receiver.into()),
                    ty: repl_type.clone(),
                }),
                name: name.into(),
                field: Some(FieldReference {
                    declaring_type: repl_type.clone(),
                    name: name.into(),
                    ty: ty.clone(),
                    is_static: false,
                    is_readonly: false,
                    is_volatile: false,
                    accessibility: Accessibility::Public,
                    constant: None,
                    declaring_instantiation: None,
                }),
            },
        }
    }

    /// Binds a constructor initializer `: this(args)` / `: base(args)`: the arguments are
    /// bound in a scope with the constructor's parameters, then matched to a constructor
    /// of the sibling (`this`) or base type. Returns the target `.ctor` reference and the
    /// bound arguments, or `None` if it does not resolve.
    pub fn bind_constructor_chain(
        &mut self,
        enclosing: &TypeSymbol,
        parameters: &[(Box<str>, TypeSymbol)],
        initializer: &lamella_syntax::ast::ConstructorInitializer,
    ) -> Option<(MethodReference, Vec<BoundExpr>)> {
        let diagnostics_before = self.diagnostics.len();
        self.current_type = Some(enclosing.clone());
        self.enter_scope();
        for (name, ty) in parameters {
            self.declare_local(name, self.resolve_type(ty));
        }
        let arguments: Vec<BoundExpr> = initializer
            .arguments
            .iter()
            .map(|argument| self.bind_expression(argument))
            .collect();
        self.exit_scope();
        self.current_type = None;
        self.mark_body_phase(diagnostics_before);
        let target = match initializer.kind {
            lamella_syntax::ast::ConstructorInitializerKind::This => enclosing.clone(),
            lamella_syntax::ast::ConstructorInitializerKind::Base => {
                self.type_info_of(enclosing)?.base.clone()?
            }
        };
        let constructors = self.type_info_of(&target)?.constructors.clone();
        let argument_types: Vec<TypeSymbol> =
            arguments.iter().map(argument_type).collect();
        let arg_constants: Vec<Option<i64>> =
            arguments.iter().map(constant_int_value).collect();
        let chosen =
            match resolve_overload(&self.model, &constructors, &argument_types, &arg_constants) {
            OverloadResult::Resolved(method) => method,
            _ => return None,
        };
        Some((
            MethodReference {
                declaring_instantiation: self.declaring_instantiation_of(
                    &target,
                    ".ctor",
                    &chosen.parameters,
                ),
                declaring_type: target,
                name: ".ctor".into(),
                is_vararg: chosen.is_vararg,
                parameters: chosen.parameters,
                return_type: TypeSymbol::Special(SpecialType::Void),
                is_static: false,
                instantiation: None,
            },
            arguments,
        ))
    }

    /// Binds a field initializer in `enclosing`'s context and checks it converts
    /// to the field's type (`CS0029`).
    ///
    /// `is_const` decides which side of the declaration/body line the initializer falls on, and
    /// the two cases genuinely differ: a `const` field's value is part of the DECLARATION -- other
    /// declarations are checked against it -- while an instance field's initializer is code that
    /// runs in a constructor. csc splits them exactly there, which is measurable: a const
    /// initializer's type error withholds an unrelated body diagnostic and an instance one does
    /// not. They look identical in the source, which is why this is a parameter rather than
    /// something inferred here.
    pub fn bind_field_initializer(
        &mut self,
        enclosing: TypeSymbol,
        field_name: &str,
        field_type: &TypeSymbol,
        initializer: &Expr,
        is_const: bool,
    ) {
        self.current_type = Some(enclosing.clone());
        self.enter_scope();
        let was_in_initializer = self.in_field_initializer;
        self.in_field_initializer = true;
        let diagnostics_before = self.diagnostics.len();
        let value = self.bind_expression(initializer);
        self.check_assignable(&value, field_type, initializer.span);
        self.in_field_initializer = was_in_initializer;
        self.exit_scope();
        self.current_type = None;
        if !is_const {
            self.mark_body_phase(diagnostics_before);
        }
        let initializer_referenced_self = self.diagnostics[diagnostics_before..]
            .iter()
            .any(|diagnostic| {
                matches!(diagnostic.kind, DiagnosticKind::FieldInitializerReference { .. })
            });
        if initializer_referenced_self {
            self.record_field_write_error(&enclosing, field_name);
        }
        if !value.ty.is_error() && self.assignable(&value, field_type) {
            if let Some(key) = crate::flow::field_type_key(&enclosing) {
                self.field_writes.insert((key, field_name.into()));
            }
        }
        crate::flow::collect_field_uses(&value, &mut self.field_reads, &mut self.field_writes);
    }

    /// Checks a `return` statement against the enclosing method's return type
    /// (15.9.4): `CS0127` for a value in a `void` method, `CS0126` for a missing
    /// value, `CS0029` for a value that does not convert.
    pub(crate) fn check_return(&mut self, value: Option<&BoundExpr>, span: Span) {
        let Some(method) = self.current_method.clone() else {
            return;
        };
        if method.return_type.is_void() {
            if value.is_some_and(|expr| !expr.ty.is_error()) {
                self.diagnostics.push(Diagnostic::new(
                    DiagnosticKind::ReturnValueInVoidMethod {
                        method: method.name,
                    },
                    span,
                ));
            }
        } else {
            match value {
                None => self.diagnostics.push(Diagnostic::new(
                    DiagnosticKind::ReturnValueRequired {
                        ty: method.return_type.to_string().into(),
                    },
                    span,
                )),
                Some(expr) => self.check_assignable(expr, &method.return_type, span),
            }
        }
    }

    /// Opens a nested scope (a block or method body).
    pub fn enter_scope(&mut self) {
        self.scopes.push(BTreeMap::new());
    }

    /// Closes the innermost scope.
    pub fn exit_scope(&mut self) {
        self.scopes.pop();
        let depth = self.scopes.len();
        self.const_locals.retain(|_, (_, _, declared_at)| *declared_at <= depth);
    }

    /// Enters / leaves a loop body, so `break`/`continue` know they have an enclosing loop.
    pub(crate) fn enter_loop(&mut self) {
        self.loop_depth += 1;
    }
    pub(crate) fn exit_loop(&mut self) {
        self.loop_depth = self.loop_depth.saturating_sub(1);
    }
    /// Enters / leaves a `switch`, so `break` knows it has an enclosing switch.
    pub(crate) fn enter_switch(&mut self) {
        self.switch_depth += 1;
    }
    pub(crate) fn exit_switch(&mut self) {
        self.switch_depth = self.switch_depth.saturating_sub(1);
    }
    /// Whether a `continue` is valid here (inside a loop).
    pub(crate) fn in_loop(&self) -> bool {
        self.loop_depth > 0
    }
    /// Whether a `break` is valid here (inside a loop or a switch).
    pub(crate) fn in_loop_or_switch(&self) -> bool {
        self.loop_depth > 0 || self.switch_depth > 0
    }
    /// Enters / leaves a `catch` clause, so a bare `throw;` knows it has an exception in flight.
    pub(crate) fn enter_catch(&mut self) {
        self.catch_depth += 1;
    }
    pub(crate) fn exit_catch(&mut self) {
        self.catch_depth = self.catch_depth.saturating_sub(1);
    }
    /// Whether a bare `throw;` is valid here (inside a `catch`).
    pub(crate) fn in_catch(&self) -> bool {
        self.catch_depth > 0
    }
    /// Whether a `goto case` is valid here (inside a `switch`).
    pub(crate) fn in_switch(&self) -> bool {
        self.switch_depth > 0
    }
    /// Enters / leaves a construct that OWNS a local -- a `foreach` body or a `using` -- so an
    /// assignment to it is CS1656. `kind` is the noun csc puts in the message.
    pub(crate) fn enter_readonly_local(&mut self, name: &str, kind: &'static str) {
        self.readonly_locals.push((name.into(), kind));
    }
    pub(crate) fn exit_readonly_local(&mut self) {
        self.readonly_locals.pop();
    }
    /// What owns `name`, if anything in scope does.
    pub(crate) fn readonly_local_kind(&self, name: &str) -> Option<&'static str> {
        self.readonly_locals
            .iter()
            .rev()
            .find(|(held, _)| **held == *name)
            .map(|(_, kind)| *kind)
    }
    /// Enters a `finally` block, remembering how many loops and switches were open OUTSIDE it.
    /// A `break` that binds to one of those leaves the finally; one that binds to a loop opened
    /// INSIDE it does not, which is the whole distinction `CS0157` turns on.
    pub(crate) fn enter_finally(&mut self) {
        self.finally_depth += 1;
        self.finally_floor.push(self.loop_depth + self.switch_depth);
    }
    pub(crate) fn exit_finally(&mut self) {
        self.finally_depth = self.finally_depth.saturating_sub(1);
        self.finally_floor.pop();
    }
    /// Whether a `return` here would leave a `finally` (`CS0157`). A return always leaves it.
    pub(crate) fn return_leaves_finally(&self) -> bool {
        self.finally_depth > 0
    }
    /// Whether a `break`/`continue` here would leave a `finally` (`CS0157`) rather than bind to a
    /// loop or switch opened inside it.
    pub(crate) fn jump_leaves_finally(&self) -> bool {
        self.finally_floor
            .last()
            .is_some_and(|floor| self.loop_depth + self.switch_depth <= *floor)
    }

    /// Declares a local variable or parameter in the innermost scope.
    pub fn declare_local(&mut self, name: &str, ty: TypeSymbol) {
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(name.into(), ty);
        }
    }

    /// Declares a local constant (15.5.1): its name resolves to the folded `value` (a
    /// constant load, not a variable read), and the name is also a local so a later
    /// redeclaration is diagnosed. The declaration itself emits nothing.
    pub(crate) fn declare_const_local(&mut self, name: &str, value: Literal, ty: TypeSymbol) {
        self.declare_local(name, ty.clone());
        let declared_at = self.scopes.len();
        self.const_locals.insert(name.into(), (value, ty, declared_at));
    }

    /// Looks a name up through the scope stack, innermost first.
    fn lookup_local(&self, name: &str) -> Option<&TypeSymbol> {
        self.scopes.iter().rev().find_map(|scope| scope.get(name))
    }

    /// Whether a local of this name is already declared in the innermost scope: a
    /// redeclaration (CS0128).
    pub(crate) fn local_in_current_scope(&self, name: &str) -> bool {
        self.scopes
            .last()
            .is_some_and(|scope| scope.contains_key(name))
    }

    /// Whether a local of this name is declared in an enclosing (not innermost)
    /// scope, which a new local would shadow (CS0136).
    pub(crate) fn local_in_enclosing_scope(&self, name: &str) -> bool {
        let innermost = self.scopes.len().saturating_sub(1);
        self.scopes[..innermost]
            .iter()
            .any(|scope| scope.contains_key(name))
    }

    /// Binds an expression (14).
    pub fn bind_expression(&mut self, expr: &Expr) -> BoundExpr {
        match &expr.kind {
            ExprKind::Literal(literal) => {
                let ty = match literal_type(literal) {
                    TypeSymbol::Special(special) => self.resolve_special_type(special, expr.span),
                    other => other,
                };
                BoundExpr {
                    kind: BoundExprKind::Literal(literal.clone()),
                    ty,
                }
            }
            ExprKind::Name(name) => self.bind_name(name, expr.span),
            ExprKind::This => {
                if self.in_static_method() {
                    self.report(Diagnostic::new(
                        DiagnosticKind::ThisInStaticContext,
                        expr.span,
                    ));
                    error_expr()
                } else {
                    self.this_expr()
                }
            }
            ExprKind::Base => self.base_expr(),
            ExprKind::MemberAccess { receiver, name } => {
                self.bind_member_access(receiver, name, expr.span)
            }
            ExprKind::Invocation {
                receiver,
                type_arguments,
                arguments,
            } => {
                if !type_arguments.is_empty() {
                    self.gate_feature(Feature::Generics, expr.span);
                }
                self.bind_invocation(receiver, type_arguments, arguments, expr.span)
            }
            ExprKind::ElementAccess {
                receiver,
                arguments,
            } => self.bind_element_access(receiver, arguments, expr.span),
            ExprKind::ObjectCreation {
                target,
                arguments,
                initializer,
            } => {
                if let Some(initializer) = initializer {
                    let feature = match initializer {
                        Initializer::Object(_) => Feature::ObjectInitializer,
                        Initializer::Collection(_) => Feature::CollectionInitializer,
                    };
                    if self.language_version.supports(feature) {
                        self.gate_feature(feature, expr.span);
                    }
                }
                self.bind_object_creation(target, arguments, initializer.as_ref(), expr.span)
            }
            ExprKind::ArrayCreation {
                element,
                lengths,
                rank,
                extra_ranks,
                initializer,
            } => {
                let mut ty = self.resolve_named_type(&bind_type(element), element.span);
                if !ty.is_error() {
                    for &extra in extra_ranks.iter().rev() {
                        ty = ty.into_array(extra);
                    }
                    ty = ty.into_array(*rank);
                }
                let lengths: Vec<BoundExpr> = lengths
                    .iter()
                    .map(|length| {
                        let bound = self.bind_expression(length);
                        self.check_index_or_length(&bound, length.span);
                        bound
                    })
                    .collect();
                if let Some((lengths, elements)) = initializer
                    .as_ref()
                    .and_then(|init| self.bind_rectangular_array(init, &ty, &lengths))
                {
                    return BoundExpr {
                        kind: BoundExprKind::ArrayCreation { lengths, elements },
                        ty,
                    };
                }
                let elements = initializer
                    .as_ref()
                    .map(|init| self.bind_array_initializer(init, &ty))
                    .unwrap_or_default();
                BoundExpr {
                    kind: BoundExprKind::ArrayCreation { lengths, elements },
                    ty,
                }
            }
            ExprKind::Binary {
                operator,
                left,
                right,
            } => self.bind_binary(*operator, left, right, expr.span),
            ExprKind::Unary { operator, operand } => self.bind_unary(*operator, operand, expr.span),
            ExprKind::RefArgument { out, operand } => {
                let operand = self.bind_expression(operand);
                let ty = operand.ty.clone();
                BoundExpr {
                    kind: BoundExprKind::Ref {
                        out: *out,
                        operand: Box::new(operand),
                    },
                    ty,
                }
            }
            ExprKind::PostfixUnary { operator, operand } => {
                self.bind_postfix(*operator, operand, expr.span)
            }
            ExprKind::Cast { target, operand } => {
                let operand = self.bind_expression(operand);
                let ty = self.resolve_named_type(&bind_type(target), target.span);
                if !self.unchecked_context {
                    if let Some(value_text) = constant_out_of_range(&operand, &ty) {
                        self.report(Diagnostic::new(
                            DiagnosticKind::CheckedConstantConversionOverflow {
                                value: value_text,
                                to: ty.to_string().into(),
                            },
                            target.span,
                        ));
                    }
                }
                if matches!(operand.kind, BoundExprKind::MethodGroup { .. }) && !ty.is_error() {
                    let candidates: Vec<(Box<str>, MethodSymbol)> = self
                        .type_info_of(&ty)
                        .map(|info| {
                            info.methods
                                .iter()
                                .filter(|m| {
                                    (&*m.name == "op_Explicit" || &*m.name == "op_Implicit")
                                        && m.parameters.len() == 1
                                        && m.return_type == ty
                                        && self
                                            .type_info_of(&m.parameters[0])
                                            .is_some_and(|d| d.kind == TypeKind::Delegate)
                                })
                                .map(|m| (m.name.clone(), m.clone()))
                                .collect()
                        })
                        .unwrap_or_default();
                    for (op_name, method) in candidates {
                        let delegate = method.parameters[0].clone();
                        let as_delegate = self.bind_delegate_creation(
                            &delegate,
                            core::slice::from_ref(&operand),
                            target.span,
                        );
                        if matches!(as_delegate.kind, BoundExprKind::DelegateCreation { .. }) {
                            let declaring_type =
                                self.declaring_type_in_chain(&ty, &op_name, &method.parameters);
                            let declaring_instantiation = self.declaring_instantiation_of(
                                &declaring_type,
                                &op_name,
                                &method.parameters,
                            );
                            return BoundExpr {
                                ty: ty.clone(),
                                kind: BoundExprKind::Call {
                                    callee: Box::new(error_expr()),
                                    arguments: alloc::vec![as_delegate],
                                    method: Some(MethodReference {
                                        declaring_type,
                                        name: op_name,
                                        parameters: method.parameters,
                                        return_type: method.return_type,
                                        is_static: true,
                                        is_vararg: false,
                                        instantiation: None,
                                        declaring_instantiation,
                                    }),
                                },
                            };
                        }
                    }
                }
                if !operand.ty.is_error() && !ty.is_error() {
                    if matches!(ty, TypeSymbol::Special(SpecialType::Decimal))
                        != matches!(operand.ty, TypeSymbol::Special(SpecialType::Decimal))
                    {
                        if let Some(method) = self
                            .user_conversion(&operand.ty, &ty, "op_Implicit")
                            .or_else(|| self.user_conversion(&operand.ty, &ty, "op_Explicit"))
                        {
                            let argument = self.convert(operand, &method.parameters[0].clone());
                            return BoundExpr {
                                ty,
                                kind: BoundExprKind::Call {
                                    callee: Box::new(error_expr()),
                                    arguments: alloc::vec![argument],
                                    method: Some(method),
                                },
                            };
                        }
                    }
                    if let Some(method) = self
                        .user_conversion(&operand.ty, &ty, "op_Explicit")
                        .or_else(|| self.user_conversion(&operand.ty, &ty, "op_Implicit"))
                    {
                        return BoundExpr {
                            ty,
                            kind: BoundExprKind::Call {
                                callee: Box::new(error_expr()),
                                arguments: alloc::vec![operand],
                                method: Some(method),
                            },
                        };
                    }
                    if let Some(value) = constant_int_value(&operand) {
                        let via = self.type_info_of(&ty).and_then(|info| {
                            info.methods.iter().find_map(|m| {
                                ((&*m.name == "op_Explicit" || &*m.name == "op_Implicit")
                                    && m.parameters.len() == 1
                                    && m.return_type == ty
                                    && matches!(
                                        m.parameters.first(),
                                        Some(TypeSymbol::Special(t)) if constant_fits(value, *t)
                                    ))
                                .then(|| (m.name.clone(), m.clone()))
                            })
                        });
                        if let Some((op_name, method)) = via {
                            let param = method.parameters[0].clone();
                            let argument = self.convert(operand, &param);
                            let declaring_type =
                                self.declaring_type_in_chain(&ty, &op_name, &method.parameters);
                            let declaring_instantiation = self.declaring_instantiation_of(
                                &declaring_type,
                                &op_name,
                                &method.parameters,
                            );
                            return BoundExpr {
                                ty: ty.clone(),
                                kind: BoundExprKind::Call {
                                    callee: Box::new(error_expr()),
                                    arguments: alloc::vec![argument],
                                    method: Some(MethodReference {
                                        declaring_type,
                                        name: op_name,
                                        parameters: method.parameters,
                                        return_type: method.return_type,
                                        is_static: true,
                                        is_vararg: false,
                                        instantiation: None,
                                        declaring_instantiation,
                                    }),
                                },
                            };
                        }
                    }
                    if let Some(underlying) = self.enum_underlying_type(&ty) {
                        if let Some(method) = self
                            .user_conversion(&operand.ty, &underlying, "op_Explicit")
                            .or_else(|| self.user_conversion(&operand.ty, &underlying, "op_Implicit"))
                        {
                            return BoundExpr {
                                ty,
                                kind: BoundExprKind::Call {
                                    callee: Box::new(error_expr()),
                                    arguments: alloc::vec![operand],
                                    method: Some(method),
                                },
                            };
                        }
                    }
                    if let Some(underlying) = self.enum_underlying_type(&operand.ty) {
                        if let Some(method) = self
                            .user_conversion(&underlying, &ty, "op_Implicit")
                            .or_else(|| self.user_conversion(&underlying, &ty, "op_Explicit"))
                        {
                            let mut as_underlying = operand;
                            as_underlying.ty = underlying;
                            return BoundExpr {
                                ty,
                                kind: BoundExprKind::Call {
                                    callee: Box::new(error_expr()),
                                    arguments: alloc::vec![as_underlying],
                                    method: Some(method),
                                },
                            };
                        }
                    }
                    if !can_cast(&self.model, &operand.ty, &ty) {
                        self.diagnostics.push(Diagnostic::new(
                            DiagnosticKind::CannotCast {
                                from: operand.ty.to_string().into(),
                                to: ty.to_string().into(),
                            },
                            target.span,
                        ));
                    }
                }
                BoundExpr {
                    kind: BoundExprKind::Cast {
                        operand: Box::new(operand),
                        checked: self.checked_context,
                    },
                    ty,
                }
            }
            ExprKind::TypeTest {
                operation,
                operand,
                target,
            } => {
                let span = target.span;
                let operand = self.bind_expression(operand);
                let resolved = self.resolve_named_type(&bind_type(target), span);
                if operand.ty.is_void() && matches!(operation, TypeTestOperation::As) {
                    self.diagnostics.push(Diagnostic::new(
                        DiagnosticKind::AsConversionMissing {
                            from: operand.ty.to_string().into(),
                            to: resolved.to_string().into(),
                        },
                        span,
                    ));
                    return error_expr();
                }
                let both_classes = |binder: &Self, a: &TypeSymbol, b: &TypeSymbol| {
                    binder
                        .type_info_of(a)
                        .is_some_and(|info| info.kind == TypeKind::Class)
                        && binder
                            .type_info_of(b)
                            .is_some_and(|info| info.kind == TypeKind::Class)
                };
                if matches!(operation, TypeTestOperation::As)
                    && !operand.ty.is_error()
                    && both_classes(self, &operand.ty, &resolved)
                    && !self.converts(&operand.ty, &resolved)
                    && !self.converts(&resolved, &operand.ty)
                {
                    self.diagnostics.push(Diagnostic::new(
                        DiagnosticKind::AsConversionMissing {
                            from: operand.ty.to_string().into(),
                            to: resolved.to_string().into(),
                        },
                        span,
                    ));
                    return error_expr();
                }
                let ty = match operation {
                    TypeTestOperation::Is => TypeSymbol::Special(SpecialType::Boolean),
                    TypeTestOperation::As => resolved.clone(),
                };
                BoundExpr {
                    kind: BoundExprKind::TypeTest {
                        operation: *operation,
                        operand: Box::new(operand),
                        target: resolved,
                    },
                    ty,
                }
            }
            ExprKind::TypeOf(target) => {
                let target_ty = if matches!(target.kind, TypeRefKind::Unbound { .. }) {
                    self.resolve_unbound_generic_type(&bind_type(target), target.span)
                } else {
                    self.resolve_named_type(&bind_type(target), target.span)
                };
                BoundExpr {
                    kind: BoundExprKind::TypeOf(target_ty),
                    ty: system_type(),
                }
            }
            ExprKind::SizeOf(target) => {
                let target_ty = self.resolve_named_type(&bind_type(target), target.span);
                BoundExpr {
                    kind: BoundExprKind::SizeOf(target_ty),
                    ty: TypeSymbol::Special(SpecialType::Int32),
                }
            }
            ExprKind::DefaultValue(target) => {
                self.gate_feature(Feature::DefaultOperator, expr.span);
                let target_ty = self.resolve_named_type(&bind_type(target), target.span);
                BoundExpr {
                    kind: BoundExprKind::DefaultValue(target_ty.clone()),
                    ty: target_ty,
                }
            }
            ExprKind::MakeRef(operand) => {
                let operand = self.bind_expression(operand);
                if !operand.ty.is_error() && !is_lvalue(&operand) {
                    self.diagnostics
                        .push(Diagnostic::new(DiagnosticKind::NotAssignable, expr.span));
                }
                BoundExpr {
                    kind: BoundExprKind::MakeRef(Box::new(operand)),
                    ty: typed_reference(),
                }
            }
            ExprKind::RefType(reference) => {
                let reference = self.bind_expression(reference);
                BoundExpr {
                    kind: BoundExprKind::RefType(Box::new(reference)),
                    ty: system_type(),
                }
            }
            ExprKind::RefValue { reference, target } => {
                let reference = self.bind_expression(reference);
                let target_ty = self.resolve_named_type(&bind_type(target), target.span);
                BoundExpr {
                    kind: BoundExprKind::RefValue {
                        reference: Box::new(reference),
                        target: target_ty.clone(),
                    },
                    ty: target_ty,
                }
            }
            ExprKind::ArgListHandle => {
                if !self
                    .current_method
                    .as_ref()
                    .is_some_and(|method| method.is_vararg)
                {
                    self.diagnostics.push(Diagnostic::new(
                        DiagnosticKind::ArglistOutsideVarargMethod,
                        expr.span,
                    ));
                    return error_expr();
                }
                BoundExpr {
                    kind: BoundExprKind::ArgListValue,
                    ty: runtime_argument_handle(),
                }
            }
            ExprKind::ArgListCall(arguments) => {
                let arguments: Vec<BoundExpr> = arguments
                    .iter()
                    .map(|argument| self.bind_expression(argument))
                    .collect();
                BoundExpr {
                    kind: BoundExprKind::ArgListLiteral(arguments),
                    ty: arglist_marker(),
                }
            }
            ExprKind::StackAlloc { element, count } => {
                let element_ty = self.resolve_named_type(&bind_type(element), element.span);
                let count = self.bind_expression(count);
                BoundExpr {
                    ty: TypeSymbol::Pointer(Box::new(element_ty.clone())),
                    kind: BoundExprKind::StackAlloc {
                        element: element_ty,
                        count: Box::new(count),
                    },
                }
            }
            ExprKind::Dereference(operand) => {
                let pointer = self.bind_expression(operand);
                let ty = match &pointer.ty {
                    TypeSymbol::Pointer(element) => (**element).clone(),
                    _ => TypeSymbol::Error,
                };
                BoundExpr {
                    kind: BoundExprKind::Dereference {
                        operand: Box::new(pointer),
                    },
                    ty,
                }
            }
            ExprKind::AddressOf(operand) => {
                let variable = self.bind_expression(operand);
                let ty = if variable.ty.is_error() {
                    TypeSymbol::Error
                } else {
                    TypeSymbol::Pointer(Box::new(variable.ty.clone()))
                };
                BoundExpr {
                    kind: BoundExprKind::AddressOf {
                        operand: Box::new(variable),
                    },
                    ty,
                }
            }
            ExprKind::Checked(inner) => {
                let saved_checked = self.checked_context;
                let saved_unchecked = self.unchecked_context;
                self.checked_context = true;
                self.unchecked_context = false;
                let inner = self.bind_expression(inner);
                self.checked_context = saved_checked;
                self.unchecked_context = saved_unchecked;
                let ty = inner.ty.clone();
                BoundExpr {
                    kind: BoundExprKind::Checked(Box::new(inner)),
                    ty,
                }
            }
            ExprKind::Unchecked(inner) => {
                let saved_checked = self.checked_context;
                let saved_unchecked = self.unchecked_context;
                self.checked_context = false;
                self.unchecked_context = true;
                let inner = self.bind_expression(inner);
                self.checked_context = saved_checked;
                self.unchecked_context = saved_unchecked;
                let ty = inner.ty.clone();
                BoundExpr {
                    kind: BoundExprKind::Unchecked(Box::new(inner)),
                    ty,
                }
            }
            ExprKind::Conditional {
                condition,
                when_true,
                when_false,
            } => self.bind_conditional(condition, when_true, when_false),
            ExprKind::Assignment {
                operator,
                target,
                value,
            } => self.bind_assignment(*operator, target, value, expr.span),
            ExprKind::PredefinedType(predefined) => {
                let ty = TypeSymbol::Special(SpecialType::from_predefined(*predefined));
                BoundExpr {
                    kind: BoundExprKind::TypeReference(ty.clone()),
                    ty,
                }
            }
            ExprKind::ConstructedType {
                name,
                type_arguments,
            } => {
                let Some(parts) = dotted_name_parts(name) else {
                    return BoundExpr {
                        kind: BoundExprKind::Error,
                        ty: TypeSymbol::Error,
                    };
                };
                let arguments: Vec<TypeSymbol> =
                    type_arguments.iter().map(|arg| bind_type(arg)).collect();
                let constructed = TypeSymbol::Instantiation {
                    definition: parts.into(),
                    arguments: arguments.into(),
                };
                let ty = self.resolve_named_type(&constructed, expr.span);
                BoundExpr {
                    kind: BoundExprKind::TypeReference(ty.clone()),
                    ty,
                }
            }
            ExprKind::Parenthesized(inner) => self.bind_expression(inner),
            _ => BoundExpr {
                kind: BoundExprKind::Error,
                ty: TypeSymbol::Error,
            },
        }
    }

    fn bind_binary(
        &mut self,
        operator: BinaryOperator,
        left_expr: &Expr,
        right_expr: &Expr,
        span: Span,
    ) -> BoundExpr {
        let left = self.bind_expression(left_expr);
        let right = self.bind_expression(right_expr);
        if matches!(operator, BinaryOperator::Divide | BinaryOperator::Modulo)
            && matches!(
                right.kind,
                BoundExprKind::Literal(Literal::Integer { value: 0, .. })
            )
        {
            self.diagnostics.push(Diagnostic::new(
                DiagnosticKind::DivisionByConstantZero,
                span,
            ));
        }
        if matches!(operator, BinaryOperator::Equal | BinaryOperator::NotEqual)
            && !left.ty.is_error()
            && !right.ty.is_error()
        {
            let left_kind = self.type_info_of(&left.ty).map(|info| info.kind);
            let right_kind = self.type_info_of(&right.ty).map(|info| info.kind);
            if matches!(left_kind, Some(TypeKind::Struct | TypeKind::Class))
                || matches!(right_kind, Some(TypeKind::Struct | TypeKind::Class))
            {
                if let Some(call) = self.bind_user_binary_operator(operator, &left, &right) {
                    return call;
                }
            }
            if matches!(left_kind, Some(TypeKind::Delegate))
                && matches!(right_kind, Some(TypeKind::Delegate))
            {
                return self.bind_delegate_equality(operator, left, right);
            }
        }
        if matches!(operator, BinaryOperator::Add | BinaryOperator::Subtract)
            && !left.ty.is_error()
            && !right.ty.is_error()
            && matches!(
                self.type_info_of(&left.ty).map(|info| info.kind),
                Some(TypeKind::Delegate)
            )
            && matches!(
                self.type_info_of(&right.ty).map(|info| info.kind),
                Some(TypeKind::Delegate)
            )
        {
            return self.bind_delegate_combination(operator, left, right);
        }
        let (left, right) = self.adjust_binary_constant(operator, left, right);
        let ty = if left.ty.is_error() || right.ty.is_error() {
            TypeSymbol::Error
        } else if let Some(result) = self.enum_binary_result(operator, &left.ty, &right.ty) {
            result
        } else if let Some(result) = pointer_binary_result(operator, &left.ty, &right.ty) {
            result
        } else if let Some(result) = self.null_equality_result(operator, &left.ty, &right.ty) {
            result
        } else if let Some(result) = binary_result_type(operator, &left.ty, &right.ty) {
            result
        } else {
            if let Some(call) = self.bind_user_binary_operator(operator, &left, &right) {
                return call;
            }
            self.diagnostics.push(Diagnostic::new(
                DiagnosticKind::OperatorNotApplicable {
                    operator: operator_symbol(operator).into(),
                    left: left.ty.to_string().into(),
                    right: right.ty.to_string().into(),
                },
                span,
            ));
            TypeSymbol::Error
        };
        if matches!(
            operator,
            BinaryOperator::Add | BinaryOperator::Subtract | BinaryOperator::Multiply
        ) {
            let fold = |operand: &BoundExpr| {
                constant_literal_value(operand).and_then(|literal| literal_int_value(&literal))
            };
            if let (Some(left_value), Some(right_value)) = (fold(&left), fold(&right)) {
                let (left_value, right_value) = (i128::from(left_value), i128::from(right_value));
                let value = match operator {
                    BinaryOperator::Add => left_value + right_value,
                    BinaryOperator::Subtract => left_value - right_value,
                    BinaryOperator::Multiply => left_value * right_value,
                    _ => unreachable!(),
                };
                self.report_constant_overflow(value, &ty, span);
            }
        }
        let (left, right) = if matches!(operator, BinaryOperator::Add)
            && matches!(ty, TypeSymbol::Special(SpecialType::String))
        {
            (self.to_concat_operand(left), self.to_concat_operand(right))
        } else if let Some(common) = binary_operand_promotion(operator, &left.ty, &right.ty) {
            let target = TypeSymbol::Special(common);
            (self.convert(left, &target), self.convert(right, &target))
        } else {
            (left, right)
        };
        BoundExpr {
            kind: BoundExprKind::Binary {
                operator,
                left: Box::new(left),
                right: Box::new(right),
                checked: self.checked_context,
            },
            ty,
        }
    }

    /// Applies a binary operator's constant expression conversion (13.1.7): when one operand is
    /// `uint` and the other a fitting `int` constant, the constant is `uint` too, so the operation
    /// stays unsigned rather than promoting `uint op <signed>` to `long` (14.2.6.2). `uint` is the
    /// only case that diverges -- every other unsigned type already promotes to itself or to `int`.
    /// Any other operand pair is returned unchanged, so numeric promotion proceeds as before.
    /// Reports CS0220 when a constant integer `+`/`-`/`*` (or unary `-`) overflows its result
    /// type in a checked context. Constant expressions are checked by DEFAULT (14.16), so this
    /// fires unless an explicit `unchecked` context suppresses it. `ty` is the operation's own
    /// result type (the binder's numeric promotion), and `value` its true mathematical result in
    /// i128 -- so a fitting constant is never flagged and this cannot misfire on a valid program
    /// (a valid program has no overflowing constant). sbyte/short never reach here (they promote
    /// to int for arithmetic); ulong is not folded (its range exceeds i64), a deliberate
    /// under-report; a checked CAST overflow is CS0221, a separate rule left untouched.
    fn report_constant_overflow(&mut self, value: i128, ty: &TypeSymbol, span: Span) {
        if self.unchecked_context {
            return;
        }
        let TypeSymbol::Special(target) = ty else {
            return;
        };
        let (min, max) = match target {
            SpecialType::Int32 => (i128::from(i32::MIN), i128::from(i32::MAX)),
            SpecialType::UInt32 => (0, i128::from(u32::MAX)),
            SpecialType::Int64 => (i128::from(i64::MIN), i128::from(i64::MAX)),
            _ => return,
        };
        if value < min || value > max {
            self.diagnostics.push(Diagnostic::new(
                DiagnosticKind::ConstantOverflowInCheckedContext,
                span,
            ));
        }
    }

    fn adjust_binary_constant(
        &self,
        operator: BinaryOperator,
        left: BoundExpr,
        right: BoundExpr,
    ) -> (BoundExpr, BoundExpr) {
        use BinaryOperator as Op;
        if !matches!(
            operator,
            Op::Add
                | Op::Subtract
                | Op::Multiply
                | Op::Divide
                | Op::Modulo
                | Op::LessThan
                | Op::GreaterThan
                | Op::LessThanOrEqual
                | Op::GreaterThanOrEqual
                | Op::Equal
                | Op::NotEqual
                | Op::BitwiseAnd
                | Op::BitwiseOr
                | Op::BitwiseXor
        ) {
            return (left, right);
        }
        let uint = TypeSymbol::Special(SpecialType::UInt32);
        if is_uint(&left.ty) && int_constant_fits(&right, SpecialType::UInt32) {
            let right = self.convert(right, &uint);
            return (left, right);
        }
        if is_uint(&right.ty) && int_constant_fits(&left, SpecialType::UInt32) {
            let left = self.convert(left, &uint);
            return (left, right);
        }
        (left, right)
    }

    /// A string-concatenation operand in `String.Concat` argument form: a string stays a
    /// string; any other type becomes `object` (a value type boxes), so concatenation
    /// uses the `Concat(object, object)` overload and calls the operand's `ToString`.
    fn to_concat_operand(&self, operand: BoundExpr) -> BoundExpr {
        if matches!(operand.ty, TypeSymbol::Special(SpecialType::String)) {
            operand
        } else {
            self.convert(operand, &TypeSymbol::Special(SpecialType::Object))
        }
    }

    /// Resolves a binary operator to a user-defined `op_*` method on either operand's
    /// type, as a static call -- the lowering of `a + b` for overloaded operators.
    fn bind_user_binary_operator(
        &mut self,
        operator: BinaryOperator,
        left: &BoundExpr,
        right: &BoundExpr,
    ) -> Option<BoundExpr> {
        let name = operator.overload_method_name()?;
        let argument_types = [left.ty.clone(), right.ty.clone()];
        for owner in [&left.ty, &right.ty] {
            let candidates = self.methods_in_chain(owner, name);
            if let OverloadResult::Resolved(method) =
                resolve_overload(&self.model, &candidates, &argument_types, &[])
            {
                let declaring_type = self.declaring_type_in_chain(owner, name, &method.parameters);
                let declaring_instantiation =
                    self.declaring_instantiation_of(&declaring_type, name, &method.parameters);
                let left_arg = self.convert(left.clone(), &method.parameters[0]);
                let right_arg = self.convert(right.clone(), &method.parameters[1]);
                return Some(BoundExpr {
                    ty: method.return_type.clone(),
                    kind: BoundExprKind::Call {
                        callee: Box::new(error_expr()),
                        arguments: alloc::vec![left_arg, right_arg],
                        method: Some(MethodReference {
                            declaring_type,
                            name: name.into(),
                            parameters: method.parameters,
                            return_type: method.return_type,
                            is_static: true,
                            is_vararg: false,
                            instantiation: None,
                            declaring_instantiation,
                        }),
                    },
                });
            }
        }
        None
    }

    /// Lowers `==` / `!=` on two delegate operands to a call of `System.Delegate`'s
    /// `op_Equality` / `op_Inequality` (14.9.8). Delegate equality is value equality -- two
    /// delegates are equal when their invocation lists are the same length and pairwise
    /// equal (same target and method) -- so it cannot be a reference `ceq`. The operands are
    /// derived delegate types passed where `System.Delegate` is expected (an implicit
    /// reference conversion the emitter needs no instruction for).
    fn bind_delegate_equality(
        &self,
        operator: BinaryOperator,
        left: BoundExpr,
        right: BoundExpr,
    ) -> BoundExpr {
        let delegate_base = TypeSymbol::Named([Box::from("System"), Box::from("Delegate")].into());
        let accessor = if matches!(operator, BinaryOperator::Equal) {
            "op_Equality"
        } else {
            "op_Inequality"
        };
        let bool_type = TypeSymbol::Special(SpecialType::Boolean);
        let method = MethodReference {
            declaring_instantiation: None,
            declaring_type: delegate_base.clone(),
            name: accessor.into(),
            parameters: alloc::vec![delegate_base.clone(), delegate_base],
            return_type: bool_type.clone(),
            is_static: true,
            is_vararg: false,
            instantiation: None,
        };
        BoundExpr {
            kind: BoundExprKind::Call {
                callee: Box::new(error_expr()),
                arguments: alloc::vec![left, right],
                method: Some(method),
            },
            ty: bool_type,
        }
    }

    /// Lowers `+` / `-` on two delegate operands to a `System.Delegate` `Combine` / `Remove` call,
    /// cast back to the delegate type (14.7.4 / 15.4). Combine/Remove take and return
    /// `System.Delegate`: the operands upcast to it implicitly (no instruction), and the
    /// `System.Delegate` result downcasts (`castclass`) to the specific delegate type.
    fn bind_delegate_combination(
        &self,
        operator: BinaryOperator,
        left: BoundExpr,
        right: BoundExpr,
    ) -> BoundExpr {
        let delegate_ty = left.ty.clone();
        let delegate_base = TypeSymbol::Named([Box::from("System"), Box::from("Delegate")].into());
        let accessor = if matches!(operator, BinaryOperator::Add) {
            "Combine"
        } else {
            "Remove"
        };
        let method = MethodReference {
            declaring_instantiation: None,
            declaring_type: delegate_base.clone(),
            name: accessor.into(),
            parameters: alloc::vec![delegate_base.clone(), delegate_base.clone()],
            return_type: delegate_base.clone(),
            is_static: true,
            is_vararg: false,
            instantiation: None,
        };
        let combined = BoundExpr {
            kind: BoundExprKind::Call {
                callee: Box::new(error_expr()),
                arguments: alloc::vec![left, right],
                method: Some(method),
            },
            ty: delegate_base,
        };
        BoundExpr {
            kind: BoundExprKind::Cast {
                operand: Box::new(combined),
                checked: false,
            },
            ty: delegate_ty,
        }
    }

    /// Coerces a `switch` governing expression to its governing type (15.7.2): when the expression
    /// type is not itself a governing type, the single user-defined implicit conversion to one is
    /// applied, so `switch (t)` where `t` has `implicit operator int` switches on the `int`.
    pub(crate) fn coerce_switch_governing(&self, expression: BoundExpr) -> BoundExpr {
        if expression.ty.is_error() || self.is_switch_governing_type(&expression.ty) {
            return expression;
        }
        for target in [
            SpecialType::SByte,
            SpecialType::Byte,
            SpecialType::Int16,
            SpecialType::UInt16,
            SpecialType::Int32,
            SpecialType::UInt32,
            SpecialType::Int64,
            SpecialType::UInt64,
            SpecialType::Char,
            SpecialType::String,
        ] {
            let target = TypeSymbol::Special(target);
            if self
                .user_conversion(&expression.ty, &target, "op_Implicit")
                .is_some()
            {
                return self.convert(expression, &target);
            }
        }
        expression
    }

    /// Whether `ty` is a valid `switch` governing type (15.7.2): an integral type, `char`, `string`,
    /// or an enum. NOT `bool` -- switching on a boolean is a C# 2.0 addition, gated separately at the
    /// switch statement. (Other types reach a governing type only via a user-defined conversion.)
    pub(crate) fn is_switch_governing_type(&self, ty: &TypeSymbol) -> bool {
        match ty {
            TypeSymbol::Special(special) => {
                is_integral(*special) || matches!(special, SpecialType::String)
            }
            _ => self
                .type_info_of(ty)
                .is_some_and(|info| info.kind == TypeKind::Enum),
        }
    }

    /// The underlying integral type of an enum -- its `value__` field's type, defaulting to `int`
    /// (21.4) -- or `None` if `ty` is not an enum.
    fn enum_underlying_type(&self, ty: &TypeSymbol) -> Option<TypeSymbol> {
        let info = self.type_info_of(ty)?;
        if info.kind != TypeKind::Enum {
            return None;
        }
        Some(match info.find_field("value__").map(|field| &field.ty) {
            Some(TypeSymbol::Special(special)) => TypeSymbol::Special(*special),
            _ => TypeSymbol::Special(SpecialType::Int32),
        })
    }

    /// Whether `ty` is an enum type declared in the model.
    fn is_enum_type(&self, ty: &TypeSymbol) -> bool {
        self.type_info_of(ty)
            .is_some_and(|info| info.kind == TypeKind::Enum)
    }

    /// The result of a binary operator on enum operands of the same type (14.7):
    /// the bitwise operators yield the enum; the relational operators yield `bool`.
    /// `==`/`!=` fall through to the general path. `None` if it does not apply.
    fn is_enum_type_pair(&self, left: &TypeSymbol, right: &TypeSymbol) -> bool {
        left == right && self.is_enum_type(left)
    }

    fn enum_binary_result(
        &self,
        operator: BinaryOperator,
        left: &TypeSymbol,
        right: &TypeSymbol,
    ) -> Option<TypeSymbol> {
        use BinaryOperator as Op;
        if self.is_enum_type_pair(left, right) {
            return match operator {
                Op::BitwiseAnd | Op::BitwiseOr | Op::BitwiseXor => Some(left.clone()),
                Op::LessThan | Op::GreaterThan | Op::LessThanOrEqual | Op::GreaterThanOrEqual => {
                    Some(TypeSymbol::Special(SpecialType::Boolean))
                }
                _ => None,
            };
        }
        let integral =
            |ty: &TypeSymbol| matches!(ty, TypeSymbol::Special(special) if is_integral(*special));
        match operator {
            Op::Add | Op::Subtract if self.is_enum_type(left) && integral(right) => {
                Some(left.clone())
            }
            Op::Add if self.is_enum_type(right) && integral(left) => Some(right.clone()),
            _ => None,
        }
    }

    fn bind_unary(
        &mut self,
        operator: UnaryOperator,
        operand_expr: &Expr,
        span: Span,
    ) -> BoundExpr {
        let operand = self.bind_expression(operand_expr);
        if operator == UnaryOperator::Minus
            && matches!(
                &operand.kind,
                BoundExprKind::Literal(Literal::Integer {
                    value: 9_223_372_036_854_775_808,
                    suffix: IntegerSuffix::None,
                })
            )
        {
            return BoundExpr {
                kind: BoundExprKind::Literal(Literal::Integer {
                    value: 9_223_372_036_854_775_808,
                    suffix: IntegerSuffix::Long,
                }),
                ty: TypeSymbol::Special(SpecialType::Int64),
            };
        }
        if operator == UnaryOperator::Minus
            && matches!(
                &operand.kind,
                BoundExprKind::Literal(Literal::Integer {
                    value: 2_147_483_648,
                    suffix: IntegerSuffix::None,
                })
            )
        {
            return BoundExpr {
                kind: BoundExprKind::Literal(Literal::Integer {
                    value: (-2_147_483_648i64) as u64,
                    suffix: IntegerSuffix::None,
                }),
                ty: TypeSymbol::Special(SpecialType::Int32),
            };
        }
        let ty = if operand.ty.is_error() {
            TypeSymbol::Error
        } else if operator == UnaryOperator::Complement && self.is_enum_type(&operand.ty) {
            operand.ty.clone()
        } else if let Some(result) = unary_result_type(operator, &operand.ty) {
            result
        } else if matches!(
            operator,
            UnaryOperator::PreIncrement | UnaryOperator::PreDecrement
        ) && matches!(operand.ty, TypeSymbol::Pointer(_))
        {
            operand.ty.clone()
        } else if matches!(
            operator,
            UnaryOperator::PreIncrement | UnaryOperator::PreDecrement
        ) && self.is_enum_type(&operand.ty)
        {
            operand.ty.clone()
        } else if matches!(
            operator,
            UnaryOperator::PreIncrement | UnaryOperator::PreDecrement
        ) && self.has_step_operator(
            &operand.ty,
            if operator == UnaryOperator::PreIncrement {
                "op_Increment"
            } else {
                "op_Decrement"
            },
        ) {
            operand.ty.clone()
        } else {
            if let Some(call) = self.bind_user_unary_operator(operator, &operand) {
                return call;
            }
            self.report_unary(unary_operator_symbol(operator), &operand.ty, span);
            TypeSymbol::Error
        };
        if operator == UnaryOperator::Minus
            && self.checked_context
            && matches!(ty, TypeSymbol::Special(SpecialType::Int32 | SpecialType::Int64))
            && constant_literal_value(&operand).is_none()
        {
            let zero = BoundExpr {
                kind: BoundExprKind::Literal(Literal::Integer {
                    value: 0,
                    suffix: if matches!(ty, TypeSymbol::Special(SpecialType::Int64)) {
                        IntegerSuffix::Long
                    } else {
                        IntegerSuffix::None
                    },
                }),
                ty: ty.clone(),
            };
            let operand = self.convert(operand, &ty);
            return BoundExpr {
                kind: BoundExprKind::Binary {
                    operator: BinaryOperator::Subtract,
                    left: Box::new(zero),
                    right: Box::new(operand),
                    checked: true,
                },
                ty,
            };
        }
        if operator == UnaryOperator::Minus {
            if let Some(value) =
                constant_literal_value(&operand).and_then(|literal| literal_int_value(&literal))
            {
                self.report_constant_overflow(-i128::from(value), &ty, span);
            }
        }
        BoundExpr {
            kind: BoundExprKind::Unary {
                operator,
                operand: Box::new(operand),
            },
            ty,
        }
    }

    /// Resolves a unary operator to a user-defined `op_*` method on the operand's type,
    /// as a static call.
    fn bind_user_unary_operator(
        &mut self,
        operator: UnaryOperator,
        operand: &BoundExpr,
    ) -> Option<BoundExpr> {
        let name = operator.overload_method_name()?;
        let argument_types = [operand.ty.clone()];
        let candidates = self.methods_in_chain(&operand.ty, name);
        if let OverloadResult::Resolved(method) =
            resolve_overload(&self.model, &candidates, &argument_types, &[])
        {
            let declaring_type = self.declaring_type_in_chain(&operand.ty, name, &method.parameters);
            let declaring_instantiation =
                self.declaring_instantiation_of(&declaring_type, name, &method.parameters);
            return Some(BoundExpr {
                ty: method.return_type.clone(),
                kind: BoundExprKind::Call {
                    callee: Box::new(error_expr()),
                    arguments: alloc::vec![operand.clone()],
                    method: Some(MethodReference {
                        declaring_type,
                        name: name.into(),
                        parameters: method.parameters,
                        return_type: method.return_type,
                        is_static: true,
                        is_vararg: false,
                        instantiation: None,
                        declaring_instantiation,
                    }),
                },
            });
        }
        None
    }

    /// Applies a type's user-defined `operator true` to `operand`, giving the `bool` a boolean
    /// expression needs (14.11.2). `None` when the type declares no `op_True`, so the caller
    /// falls back to the ordinary conversion-to-`bool` requirement.
    pub(crate) fn bind_operator_true(&mut self, operand: &BoundExpr) -> Option<BoundExpr> {
        let candidates = self.methods_in_chain(&operand.ty, "op_True");
        let argument_types = [operand.ty.clone()];
        if let OverloadResult::Resolved(method) =
            resolve_overload(&self.model, &candidates, &argument_types, &[])
        {
            let declaring_type =
                self.declaring_type_in_chain(&operand.ty, "op_True", &method.parameters);
            let declaring_instantiation =
                self.declaring_instantiation_of(&declaring_type, "op_True", &method.parameters);
            return Some(BoundExpr {
                ty: method.return_type.clone(),
                kind: BoundExprKind::Call {
                    callee: Box::new(error_expr()),
                    arguments: alloc::vec![operand.clone()],
                    method: Some(MethodReference {
                        declaring_type,
                        name: "op_True".into(),
                        parameters: method.parameters,
                        return_type: method.return_type,
                        is_static: true,
                        is_vararg: false,
                        instantiation: None,
                        declaring_instantiation,
                    }),
                },
            });
        }
        None
    }

    fn bind_postfix(
        &mut self,
        operator: PostfixOperator,
        operand_expr: &Expr,
        span: Span,
    ) -> BoundExpr {
        let operand = self.bind_expression(operand_expr);
        let step_name = match operator {
            PostfixOperator::Increment => "op_Increment",
            PostfixOperator::Decrement => "op_Decrement",
        };
        let mut converting: Option<Box<ConvertingStep>> = None;
        let ty = if operand.ty.is_error() {
            TypeSymbol::Error
        } else if as_special(&operand.ty).is_some_and(SpecialType::is_numeric)
            || self.is_enum_type(&operand.ty)
            || matches!(operand.ty, TypeSymbol::Pointer(_))
            || self.has_step_operator(&operand.ty, step_name)
        {
            operand.ty.clone()
        } else if let Some((operator_method, result_conversion)) =
            self.find_converting_step(&operand.ty, step_name)
        {
            converting = Some(Box::new(ConvertingStep {
                operator: operator_method,
                result_conversion,
            }));
            operand.ty.clone()
        } else {
            let symbol = match operator {
                PostfixOperator::Increment => "++",
                PostfixOperator::Decrement => "--",
            };
            self.report_unary(symbol, &operand.ty, span);
            TypeSymbol::Error
        };
        BoundExpr {
            kind: BoundExprKind::Postfix {
                operator,
                operand: Box::new(operand),
                step: converting,
            },
            ty,
        }
    }

    /// Whether `ty` declares a user-defined `op_Increment`/`op_Decrement` (named by
    /// `step`) -- a static method taking and returning `ty`.
    fn has_step_operator(&self, ty: &TypeSymbol, step: &str) -> bool {
        self.methods_in_chain(ty, step).iter().any(|method| {
            method.parameters.len() == 1
                && &method.parameters[0] == ty
                && &method.return_type == ty
        })
    }

    /// A user-defined `op_Increment`/`op_Decrement` applicable to `ty` whose parameter or result
    /// type is NOT `ty` (14.14.2): the operand converts to the parameter type, and the result
    /// converts back to `ty` (identity, a reference conversion -- `None` -- or a user-defined
    /// implicit conversion). Returns the operator and any result conversion. `None` when only the
    /// exact same-type form matches (handled by [`has_step_operator`] + the emit's user_step path)
    /// or none is applicable.
    fn find_converting_step(
        &self,
        ty: &TypeSymbol,
        step: &str,
    ) -> Option<(MethodReference, Option<MethodReference>)> {
        for method in self.methods_in_chain(ty, step) {
            if method.parameters.len() != 1
                || (&method.parameters[0] == ty && &method.return_type == ty)
            {
                continue;
            }
            if !self.converts(ty, &method.parameters[0]) {
                continue;
            }
            let result_conversion = if &method.return_type == ty {
                None
            } else if let Some(conv) = self.user_conversion(&method.return_type, ty, "op_Implicit") {
                Some(conv)
            } else if self.converts(&method.return_type, ty) {
                None
            } else {
                continue;
            };
            let declaring_type = self.declaring_type_in_chain(ty, step, &method.parameters);
            let declaring_instantiation =
                self.declaring_instantiation_of(&declaring_type, step, &method.parameters);
            let operator = MethodReference {
                declaring_type,
                name: step.into(),
                parameters: method.parameters.clone(),
                return_type: method.return_type.clone(),
                is_static: true,
                is_vararg: false,
                instantiation: None,
                declaring_instantiation,
            };
            return Some((operator, result_conversion));
        }
        None
    }

    fn report_unary(&mut self, operator: &str, operand: &TypeSymbol, span: Span) {
        self.diagnostics.push(Diagnostic::new(
            DiagnosticKind::UnaryOperatorNotApplicable {
                operator: operator.into(),
                operand: operand.to_string().into(),
            },
            span,
        ));
    }

    fn bind_conditional(
        &mut self,
        condition: &Expr,
        when_true: &Expr,
        when_false: &Expr,
    ) -> BoundExpr {
        let condition_span = condition.span;
        let condition = self.bind_expression(condition);
        let boolean = TypeSymbol::Special(SpecialType::Boolean);
        let condition = if condition.ty.is_error() || self.converts(&condition.ty, &boolean) {
            condition
        } else if let Some(tested) = self.bind_operator_true(&condition) {
            tested
        } else {
            self.diagnostics.push(Diagnostic::new(
                DiagnosticKind::NoImplicitConversion {
                    from: condition.ty.to_string().into(),
                    to: "bool".into(),
                },
                condition_span,
            ));
            condition
        };
        let span = when_false.span;
        let when_true = self.bind_expression(when_true);
        let when_false = self.bind_expression(when_false);
        let ty = if when_true.ty.is_error() || when_false.ty.is_error() {
            TypeSymbol::Error
        } else if let Some(common) =
            conditional_result_type(&self.model, &when_true.ty, &when_false.ty)
        {
            common
        } else if self.assignable(&when_true, &when_false.ty) {
            when_false.ty.clone()
        } else if self.assignable(&when_false, &when_true.ty) {
            when_true.ty.clone()
        } else {
            self.diagnostics.push(Diagnostic::new(
                DiagnosticKind::ConditionalTypeMismatch {
                    left: when_true.ty.to_string().into(),
                    right: when_false.ty.to_string().into(),
                },
                span,
            ));
            TypeSymbol::Error
        };
        let (when_true, when_false) = if ty.is_error() {
            (when_true, when_false)
        } else {
            (self.convert(when_true, &ty), self.convert(when_false, &ty))
        };
        BoundExpr {
            kind: BoundExprKind::Conditional {
                condition: Box::new(condition),
                when_true: Box::new(when_true),
                when_false: Box::new(when_false),
            },
            ty,
        }
    }

    /// Lowers an event subscription `receiver.E += h` (or `-=`) from outside the declaring
    /// type to a call of the event's `add_E`/`remove_E` accessor (17.7), the handler
    /// converted to the event's delegate type.
    fn bind_event_subscription(
        &mut self,
        receiver: BoundExpr,
        event: &EventSymbol,
        declaring: &TypeSymbol,
        operator: AssignmentOperator,
        value_expr: &Expr,
    ) -> BoundExpr {
        let value = self.bind_expression(value_expr);
        let handler = self.convert(value, &event.ty);
        let prefix = if matches!(operator, AssignmentOperator::Add) {
            "add_"
        } else {
            "remove_"
        };
        let mut accessor = String::from(prefix);
        accessor.push_str(&event.name);
        let void = TypeSymbol::Special(SpecialType::Void);
        let method = MethodReference {
            declaring_instantiation: self.declaring_instantiation_of(
                &declaring,
                &accessor,
                core::slice::from_ref(&event.ty),
            ),
            declaring_type: declaring.clone(),
            name: accessor.clone().into(),
            parameters: alloc::vec![event.ty.clone()],
            return_type: void.clone(),
            is_static: event.is_static,
            is_vararg: false,
            instantiation: None,
        };
        let callee = BoundExpr {
            ty: TypeSymbol::Error,
            kind: BoundExprKind::MethodGroup {
                receiver: Box::new(receiver),
                name: accessor.into(),
            },
        };
        BoundExpr {
            kind: BoundExprKind::Call {
                callee: Box::new(callee),
                arguments: alloc::vec![handler],
                method: Some(method),
            },
            ty: void,
        }
    }

    fn bind_assignment(
        &mut self,
        operator: AssignmentOperator,
        target_expr: &Expr,
        value_expr: &Expr,
        span: Span,
    ) -> BoundExpr {
        let target_span = target_expr.span;
        if let Some(binary_op) = compound_binary_operator(operator) {
            if let ExprKind::ElementAccess {
                receiver,
                arguments,
            } = &target_expr.kind
            {
                if is_repeatable(receiver) && arguments.iter().all(is_repeatable) {
                    let checkpoint = self.diagnostics.len();
                    if let Some(result) =
                        self.bind_indexer_compound(receiver, arguments, binary_op, value_expr, span)
                    {
                        return result;
                    }
                    self.diagnostics.truncate(checkpoint);
                }
            }
        }
        if operator == AssignmentOperator::Assign {
            if let ExprKind::ElementAccess {
                receiver,
                arguments,
            } = &target_expr.kind
            {
                let bound_receiver = self.bind_expression(receiver);
                let setter = if bound_receiver.ty.is_error()
                    || matches!(
                        bound_receiver.ty,
                        TypeSymbol::Array { .. } | TypeSymbol::Special(SpecialType::String)
                    ) {
                    None
                } else {
                    self.indexer_accessor(&bound_receiver.ty, "set_", arguments.len() + 1)
                };
                if let Some(setter) = setter {
                    let mut args: Vec<BoundExpr> = arguments
                        .iter()
                        .map(|argument| self.bind_expression(argument))
                        .collect();
                    args.push(self.bind_expression(value_expr));
                    return self
                        .bind_indexer_store(bound_receiver, &setter, args, span)
                        .unwrap_or_else(error_expr);
                }
            }
        }
        if matches!(
            operator,
            AssignmentOperator::Add | AssignmentOperator::Subtract
        ) {
            if let ExprKind::MemberAccess { receiver, name } = &target_expr.kind {
                let bound_receiver = self.bind_expression(receiver);
                if let Some((event, declaring)) = self.event_declaration(&bound_receiver.ty, name) {
                    if self.outside_event_declarer(&declaring) {
                        return self.bind_event_subscription(
                            bound_receiver,
                            &event,
                            &declaring,
                            operator,
                            value_expr,
                        );
                    }
                }
            }
        }
        let target = self.bind_expression(target_expr);
        if let BoundExprKind::FieldAccess {
            field: Some(field), ..
        } = &target.kind
        {
            let in_constructor = matches!(
                self.current_method.as_ref(),
                Some(context) if &*context.name == ".ctor" || &*context.name == ".cctor"
            );
            if !in_constructor && self.field_is_readonly(&field.declaring_type, &field.name) {
                self.diagnostics.push(Diagnostic::new(
                    DiagnosticKind::ReadonlyAssignment {
                        field: field.name.clone(),
                    },
                    target_span,
                ));
                let (declaring, name) = (field.declaring_type.clone(), field.name.clone());
                self.record_field_write_error(&declaring, &name);
            }
        }
        let mut value = if operator == AssignmentOperator::Assign
            && matches!(&value_expr.kind, ExprKind::ArrayInitializer(_))
            && matches!(target.ty, TypeSymbol::Array { .. })
        {
            let declared = target.ty.clone();
            let (lengths, elements) = match self.bind_rectangular_array(value_expr, &declared, &[]) {
                Some(rectangular) => rectangular,
                None => (Vec::new(), self.bind_array_initializer(value_expr, &declared)),
            };
            BoundExpr {
                kind: BoundExprKind::ArrayCreation { lengths, elements },
                ty: declared,
            }
        } else {
            self.bind_expression(value_expr)
        };
        if matches!(
            operator,
            AssignmentOperator::Add | AssignmentOperator::Subtract
        ) && self
            .type_info_of(&target.ty)
            .is_some_and(|info| info.kind == TypeKind::Delegate)
            && (matches!(value.kind, BoundExprKind::MethodGroup { .. })
                || self
                    .type_info_of(&value.ty)
                    .is_some_and(|info| info.kind == TypeKind::Delegate))
        {
            let delegate_ty = target.ty.clone();
            let delegate_base =
                TypeSymbol::Named([Box::from("System"), Box::from("Delegate")].into());
            let accessor = if matches!(operator, AssignmentOperator::Add) {
                "Combine"
            } else {
                "Remove"
            };
            let operand = self.convert(value, &delegate_ty);
            let method = MethodReference {
                declaring_instantiation: None,
                declaring_type: delegate_base.clone(),
                name: accessor.into(),
                parameters: alloc::vec![delegate_base.clone(), delegate_base.clone()],
                return_type: delegate_base.clone(),
                is_static: true,
                is_vararg: false,
                instantiation: None,
            };
            let callee = BoundExpr {
                ty: TypeSymbol::Error,
                kind: BoundExprKind::MethodGroup {
                    receiver: Box::new(BoundExpr {
                        ty: delegate_base.clone(),
                        kind: BoundExprKind::TypeReference(delegate_base.clone()),
                    }),
                    name: accessor.into(),
                },
            };
            let combine = BoundExpr {
                kind: BoundExprKind::Call {
                    callee: Box::new(callee),
                    arguments: alloc::vec![target.clone(), operand],
                    method: Some(method),
                },
                ty: delegate_base,
            };
            let cast = BoundExpr {
                kind: BoundExprKind::Cast {
                    operand: Box::new(combine),
                    checked: false,
                },
                ty: delegate_ty.clone(),
            };
            return BoundExpr {
                kind: BoundExprKind::Assignment {
                    operator: AssignmentOperator::Assign,
                    target: Box::new(target),
                    value: Box::new(cast),
                    checked: self.checked_context,
                },
                ty: delegate_ty,
            };
        }
        if !target.ty.is_error() && !value.ty.is_error() {
            if let Some(binary_op) = compound_binary_operator(operator) {
                if binary_result_type(binary_op, &target.ty, &value.ty).is_none() {
                    if let Some(result_ty) =
                        pointer_binary_result(binary_op, &target.ty, &value.ty)
                    {
                        let binary = BoundExpr {
                            kind: BoundExprKind::Binary {
                                operator: binary_op,
                                left: Box::new(target.clone()),
                                right: Box::new(value),
                                checked: self.checked_context,
                            },
                            ty: result_ty,
                        };
                        return BoundExpr {
                            ty: target.ty.clone(),
                            kind: BoundExprKind::Assignment {
                                operator: AssignmentOperator::Assign,
                                target: Box::new(target),
                                value: Box::new(binary),
                                checked: self.checked_context,
                            },
                        };
                    }
                    if let Some(result_ty) = self.enum_binary_result(binary_op, &target.ty, &value.ty) {
                        let binary = BoundExpr {
                            kind: BoundExprKind::Binary {
                                operator: binary_op,
                                left: Box::new(target.clone()),
                                right: Box::new(value),
                                checked: self.checked_context,
                            },
                            ty: result_ty,
                        };
                        let assigned = self.convert(binary, &target.ty);
                        return BoundExpr {
                            ty: target.ty.clone(),
                            kind: BoundExprKind::Assignment {
                                operator: AssignmentOperator::Assign,
                                target: Box::new(target),
                                value: Box::new(assigned),
                                checked: self.checked_context,
                            },
                        };
                    }
                    if let Some(call) = self.bind_user_binary_operator(binary_op, &target, &value) {
                        let assigned = self.convert(call, &target.ty);
                        return BoundExpr {
                            ty: target.ty.clone(),
                            kind: BoundExprKind::Assignment {
                                operator: AssignmentOperator::Assign,
                                target: Box::new(target),
                                value: Box::new(assigned),
                                checked: self.checked_context,
                            },
                        };
                    }
                }
            }
        }
        if let BoundExprKind::MethodGroup { name, .. } = &target.kind {
            self.diagnostics.push(Diagnostic::new(
                DiagnosticKind::CannotAssignToMethodGroup { name: name.clone() },
                target_span,
            ));
            return error_expr();
        }
        if let BoundExprKind::Local(name) = &target.kind {
            if let Some(kind) = self.readonly_local_kind(name) {
                self.diagnostics.push(Diagnostic::new(
                    DiagnosticKind::CannotAssignToReadonlyLocal {
                        name: name.clone(),
                        kind,
                    },
                    target_span,
                ));
                return error_expr();
            }
        }
        let this_in_struct =
            matches!(target.kind, BoundExprKind::This) && self.is_value_type(&target.ty);
        if !target.ty.is_error() && !is_lvalue(&target) && !this_in_struct {
            self.diagnostics
                .push(Diagnostic::new(DiagnosticKind::NotAssignable, target_span));
        } else if !target.ty.is_error() && !value.ty.is_error() {
            self.check_assignment(operator, &target.ty, &value, span);
            if matches!(operator, AssignmentOperator::Assign) && !self.assignable(&value, &target.ty)
            {
                if let BoundExprKind::FieldAccess { field: Some(field), .. } = &target.kind {
                    let (declaring, name) = (field.declaring_type.clone(), field.name.clone());
                    self.record_field_write_error(&declaring, &name);
                }
            }
            if matches!(operator, AssignmentOperator::Add)
                && matches!(target.ty, TypeSymbol::Special(SpecialType::String))
            {
                value = self.to_concat_operand(value);
            }
        }
        if !target.ty.is_error() && matches!(operator, AssignmentOperator::Assign) {
            value = self.convert(value, &target.ty);
        }
        let ty = target.ty.clone();
        BoundExpr {
            kind: BoundExprKind::Assignment {
                operator,
                target: Box::new(target),
                value: Box::new(value),
                checked: self.checked_context,
            },
            ty,
        }
    }

    fn check_assignment(
        &mut self,
        operator: AssignmentOperator,
        target: &TypeSymbol,
        value: &BoundExpr,
        span: Span,
    ) {
        match compound_binary_operator(operator) {
            None => {
                self.check_assignable(value, target, span);
            }
            Some(binary) => {
                if binary_result_type(binary, target, &value.ty).is_none() {
                    self.diagnostics.push(Diagnostic::new(
                        DiagnosticKind::OperatorNotApplicable {
                            operator: assignment_symbol(operator).into(),
                            left: target.to_string().into(),
                            right: value.ty.to_string().into(),
                        },
                        span,
                    ));
                }
            }
        }
    }

    fn bind_member_access(&mut self, receiver_expr: &Expr, name: &str, span: Span) -> BoundExpr {
        let mark = self.diagnostics.len();
        let receiver = self.bind_expression(receiver_expr);
        if let Some(type_receiver) = self.color_color_type_receiver(receiver_expr, &receiver, name) {
            self.diagnostics.truncate(mark);
            return self.member_access_of(type_receiver, name, span);
        }
        self.member_access_of(receiver, name, span)
    }

    /// The TYPE-reference receiver for a color-color access (7.6.4.1, "Identical simple names and
    /// type names"): if `receiver_expr` is a single identifier E that bound to a VALUE (field/
    /// property/local/parameter) whose type is a type also named E, and `member` is a static member
    /// or nested type of that type, returns E as a TYPE reference -- so E.member is the legal static
    /// access csc reports, not the CS0176 an instance receiver gives. The value meaning is otherwise
    /// kept (an instance member reaches it). Returns `None` when color-color does not apply.
    fn color_color_type_receiver(
        &self,
        receiver_expr: &Expr,
        receiver: &BoundExpr,
        member: &str,
    ) -> Option<BoundExpr> {
        let ExprKind::Name(id) = &receiver_expr.kind else {
            return None;
        };
        let receiver_is_value = !matches!(
            receiver.kind,
            BoundExprKind::TypeReference(_) | BoundExprKind::NamespaceReference(_)
        ) && !receiver.ty.is_error();
        if receiver_is_value
            && self
                .simple_name_as_type(id)
                .is_some_and(|ty| dotted_type_name(&ty) == dotted_type_name(&receiver.ty))
            && self.member_is_static_or_nested(&receiver.ty, member)
        {
            let ty = receiver.ty.clone();
            Some(BoundExpr {
                kind: BoundExprKind::TypeReference(ty.clone()),
                ty,
            })
        } else {
            None
        }
    }

    /// The type a single identifier denotes as a type-name -- an alias, or a type in the
    /// namespaces in scope (including a nested type of the current type) -- if any. Used for the
    /// color-color rule (7.6.4.1), where a name is both a value and a type of the same type.
    fn simple_name_as_type(&self, id: &str) -> Option<TypeSymbol> {
        if let Some(target) = self.alias_target(id) {
            return Some(target);
        }
        self.type_namespaces_containing(id)
            .first()
            .map(|(namespace, _)| type_symbol_in(namespace, id))
    }

    /// Whether `name` is a static member, or a nested type, of `ty` -- the members color-color
    /// reaches by interpreting the receiver as a type (7.6.4.1). An instance member is left to the
    /// value interpretation.
    fn member_is_static_or_nested(&self, ty: &TypeSymbol, name: &str) -> bool {
        match self.resolve_member(ty, name) {
            MemberResolution::Field(field) => field.is_static,
            MemberResolution::Property { is_static, .. } => is_static,
            MemberResolution::MethodGroup => {
                self.methods_in_chain(ty, name).iter().any(|m| m.is_static)
            }
            MemberResolution::NoSuchMember(_) => matches!(
                ty,
                TypeSymbol::Named(parts) if self.model.get(&parts.join("."), name).is_some()
            ),
            MemberResolution::Unknown => false,
        }
    }

    /// Resolves `receiver.name` from an already-bound receiver (14.5.4). Split from
    /// [`Self::bind_member_access`] so an invocation target can reuse it after peeking the
    /// receiver for a method group (see [`Self::bind_call_target`]).
    fn member_access_of(&mut self, receiver: BoundExpr, name: &str, span: Span) -> BoundExpr {
        if let BoundExprKind::NamespaceReference(namespace) = &receiver.kind {
            let namespace = namespace.clone();
            return self.bind_qualified_name(&namespace, name, span);
        }
        if receiver.ty.is_error() {
            return error_expr();
        }
        if let BoundExprKind::TypeReference(TypeSymbol::Special(special)) = &receiver.kind {
            let special = *special;
            if let Some(constant) = predefined_constant(special, name) {
                let ty = TypeSymbol::Special(special);
                return BoundExpr {
                    kind: BoundExprKind::FieldAccess {
                        receiver: Box::new(receiver),
                        name: name.into(),
                        field: Some(FieldReference {
                            declaring_type: ty.clone(),
                            name: name.into(),
                            ty: ty.clone(),
                            is_static: true,
                            is_readonly: false,
                            is_volatile: false,
                            accessibility: Accessibility::Public,
                            constant: Some(integer_literal(constant)),
                            declaring_instantiation: None,
                        }),
                    },
                    ty,
                };
            }
        }
        if let Some((_, declaring)) = self.event_declaration(&receiver.ty, name) {
            if self.outside_event_declarer(&declaring) {
                self.report(Diagnostic::new(
                    DiagnosticKind::EventOutsideAddRemove { event: name.into() },
                    span,
                ));
                return error_expr();
            }
        }
        let receiver_kind = match receiver_category(&receiver) {
            Receiver::ImplicitThis => Receiver::Instance,
            other => other,
        };
        match self.resolve_member(&receiver.ty, name) {
            MemberResolution::Field(field) => {
                self.check_accessible(&field.declaring_type, field.accessibility, name, span);
                self.check_protected_qualifier(
                    &field.declaring_type,
                    field.accessibility,
                    field.is_static,
                    receiver_category(&receiver),
                    &receiver.ty,
                    name,
                    None,
                    span,
                );
                self.check_static_instance(
                    receiver_kind,
                    field.is_static,
                    &field.declaring_type,
                    name,
                    span,
                );
                BoundExpr {
                    ty: field.ty.clone(),
                    kind: BoundExprKind::FieldAccess {
                        receiver: Box::new(receiver),
                        name: name.into(),
                        field: Some(field),
                    },
                }
            }
            MemberResolution::Property {
                declaring_type,
                ty,
                accessibility,
                is_static,
            } => {
                self.check_accessible(&declaring_type, accessibility, name, span);
                self.check_protected_qualifier(
                    &declaring_type,
                    accessibility,
                    is_static,
                    receiver_category(&receiver),
                    &receiver.ty,
                    name,
                    None,
                    span,
                );
                self.check_static_instance(receiver_kind, is_static, &declaring_type, name, span);
                let (getter_declaring, setter_declaring) =
                    self.property_accessor_declarers(&receiver.ty, name);
                let (getter_instantiation, setter_instantiation) = self
                    .property_accessor_instantiations(
                        &getter_declaring,
                        &setter_declaring,
                        name,
                        &ty,
                    );
                BoundExpr {
                    kind: BoundExprKind::PropertyAccess {
                        receiver: Box::new(receiver),
                        declaring_type: getter_declaring,
                        setter_declaring_type: setter_declaring,
                        getter_instantiation,
                        setter_instantiation,
                        name: name.into(),
                    },
                    ty,
                }
            }
            MemberResolution::MethodGroup => BoundExpr {
                kind: BoundExprKind::MethodGroup {
                    receiver: Box::new(receiver),
                    name: name.into(),
                },
                ty: TypeSymbol::Error,
            },
            MemberResolution::NoSuchMember(type_name) => {
                if let BoundExprKind::TypeReference(TypeSymbol::Named(parts)) = &receiver.kind {
                    let enclosing = parts.join(".");
                    if self.model.get(&enclosing, name).is_some() {
                        let ty = type_symbol_in(&enclosing, name);
                        return BoundExpr {
                            kind: BoundExprKind::TypeReference(ty.clone()),
                            ty,
                        };
                    }
                }
                let unreadable = self
                    .model
                    .get_by_symbol(&self.lookup_type_of(&receiver.ty))
                    .is_some_and(|info| {
                        info.undecodable_members.iter().any(|m| &**m == name)
                    });
                let kind = if unreadable {
                    DiagnosticKind::MemberSignatureNotSupported {
                        type_name: type_name.into(),
                        member: name.into(),
                    }
                } else {
                    DiagnosticKind::MemberNotFound {
                        type_name: type_name.into(),
                        member: name.into(),
                    }
                };
                self.diagnostics.push(Diagnostic::new(kind, span));
                error_expr()
            }
            MemberResolution::Unknown => error_expr(),
        }
    }

    /// Binds the target of an invocation `target(args)`. Like [`Self::bind_expression`], but for
    /// a member access `E.M` it forms the METHOD GROUP when methods of that name are in `E`'s
    /// chain, even if a non-invocable member (a field, or a nested type) of the same name hides
    /// them for ordinary member access (7.4): `x.M()` finds an inherited method M though a
    /// more-derived non-method M hides it. A delegate-typed value still
    /// invokes directly via `Invoke`, so it is NOT overridden.
    fn bind_call_target(&mut self, target: &Expr) -> BoundExpr {
        let ExprKind::MemberAccess { receiver, name } = &target.kind else {
            return self.bind_expression(target);
        };
        let mark = self.diagnostics.len();
        let recv = self.bind_expression(receiver);
        let recv = match self.color_color_type_receiver(receiver, &recv, name) {
            Some(type_receiver) => {
                self.diagnostics.truncate(mark);
                type_receiver
            }
            None => recv,
        };
        if let BoundExprKind::NamespaceReference(namespace) = &recv.kind {
            let namespace = namespace.clone();
            return self.bind_qualified_name(&namespace, name, target.span);
        }
        if recv.ty.is_error() {
            return self.member_access_of(recv, name, target.span);
        }
        let receiver_ty = match &recv.kind {
            BoundExprKind::TypeReference(ty) => ty.clone(),
            _ => recv.ty.clone(),
        };
        let hidden_by_delegate = match self.resolve_member(&receiver_ty, name) {
            MemberResolution::Field(field) => self.is_delegate_type(&field.ty),
            MemberResolution::Property { ty, .. } => self.is_delegate_type(&ty),
            _ => false,
        };
        if !hidden_by_delegate && !self.methods_in_chain(&receiver_ty, name).is_empty() {
            return BoundExpr {
                kind: BoundExprKind::MethodGroup {
                    receiver: Box::new(recv),
                    name: name.clone(),
                },
                ty: TypeSymbol::Error,
            };
        }
        self.member_access_of(recv, name, target.span)
    }

    fn bind_invocation(
        &mut self,
        receiver_expr: &Expr,
        type_argument_refs: &[TypeRef],
        argument_exprs: &[Expr],
        span: Span,
    ) -> BoundExpr {
        let callee = self.bind_call_target(receiver_expr);
        let callee = if self.is_delegate_value(&callee) {
            BoundExpr {
                ty: TypeSymbol::Error,
                kind: BoundExprKind::MethodGroup {
                    receiver: Box::new(callee),
                    name: "Invoke".into(),
                },
            }
        } else {
            callee
        };
        let arguments: Vec<BoundExpr> = argument_exprs
            .iter()
            .map(|argument| self.bind_expression(argument))
            .collect();
        let group = match &callee.kind {
            BoundExprKind::MethodGroup { receiver, name } => {
                Some((receiver.ty.clone(), name.clone()))
            }
            _ => None,
        };
        let explicitly_qualified = matches!(receiver_expr.kind, ExprKind::MemberAccess { .. });
        let receiver_kind = match &callee.kind {
            BoundExprKind::MethodGroup { receiver, .. } => Some(match receiver_category(receiver) {
                Receiver::ImplicitThis if explicitly_qualified => Receiver::Instance,
                other => other,
            }),
            _ => None,
        };
        let written_receiver = match &callee.kind {
            BoundExprKind::MethodGroup { receiver, .. } => Some(receiver_category(receiver)),
            _ => None,
        };
        let mut params_method = false;
        let has_method_group = arguments
            .iter()
            .any(|argument| matches!(argument.kind, BoundExprKind::MethodGroup { .. }));
        let real_error = arguments.iter().any(|argument| {
            argument.ty.is_error() && !matches!(argument.kind, BoundExprKind::MethodGroup { .. })
        });
        let type_arguments: Vec<TypeSymbol> = type_argument_refs
            .iter()
            .map(|argument| {
                let symbol = bind_type(argument);
                self.resolve_named_type(&symbol, argument.span)
            })
            .collect();
        let argument_types: Vec<TypeSymbol> = arguments.iter().map(argument_type).collect();
        let mut resolved = match group {
            Some((receiver_ty, name)) if !real_error => {
                let candidates = self.methods_in_chain(&receiver_ty, &name);
                let set = self.candidates_for_type_arguments(
                    &candidates,
                    &type_arguments,
                    &argument_types,
                    &receiver_ty,
                    span,
                );
                let chosen = if has_method_group {
                    self.resolve_with_method_groups(
                        &name,
                        &receiver_ty,
                        &set.candidates,
                        &arguments,
                        span,
                    )
                } else {
                    let arg_constants: Vec<Option<i64>> =
                        arguments.iter().map(constant_int_value).collect();
                    self.resolve_call_or_uninferable(
                        &name,
                        &receiver_ty,
                        &set,
                        &argument_types,
                        &arg_constants,
                        &arguments,
                        span,
                    )
                };
                chosen.map(|method| {
                    params_method = method.is_params;
                    let instantiation = Self::instantiation_of(&method, &set);
                    self.check_argument_modes(&method, &arguments, span);
                    let declaring_type =
                        self.declaring_type_in_chain(&receiver_ty, &method.name, &method.parameters);
                    let declaring_key = instantiation
                        .as_deref()
                        .map_or(&method.parameters[..], |open| &open.parameters[..]);
                    let declaring_instantiation = self.declaring_instantiation_of(
                        &declaring_type,
                        &method.name,
                        declaring_key,
                    );
                    if let Some(written) = written_receiver {
                        self.check_protected_qualifier(
                            &declaring_type,
                            method.accessibility,
                            method.is_static,
                            written,
                            &receiver_ty,
                            &method.name,
                            Some(&method.parameters),
                            span,
                        );
                    }
                    MethodReference {
                        declaring_type,
                        is_vararg: method.is_vararg,
                        name: method.name,
                        parameters: method.parameters,
                        return_type: method.return_type,
                        is_static: method.is_static,
                        instantiation,
                        declaring_instantiation,
                    }
                })
            }
            _ => None,
        };
        if resolved.is_none() && !arguments.iter().any(|argument| argument.ty.is_error()) {
            if let Some(invoke) = self
                .type_info_of(&callee.ty)
                .filter(|info| info.kind == TypeKind::Delegate)
                .and_then(|info| info.methods.iter().find(|m| &*m.name == "Invoke").cloned())
            {
                resolved = Some(MethodReference {
                    declaring_instantiation: self.declaring_instantiation_of(
                        &callee.ty,
                        "Invoke",
                        &invoke.parameters,
                    ),
                    declaring_type: callee.ty.clone(),
                    name: "Invoke".into(),
                    parameters: invoke.parameters,
                    return_type: invoke.return_type,
                    is_static: false,
                    is_vararg: false,
                    instantiation: None,
                });
            }
        }
        if let (Some(kind), Some(method)) = (receiver_kind, &resolved) {
            if matches!(kind, Receiver::ImplicitThis) && !method.is_static {
                if self.in_static_method() {
                    self.report_no_object_reference(
                        &method.declaring_type,
                        &method.name,
                        true,
                        span,
                    );
                } else if self.in_field_initializer {
                    self.report_field_initializer_reference(
                        &method.declaring_type,
                        &method.name,
                        true,
                        span,
                    );
                }
            }
            self.check_static_instance(
                kind,
                method.is_static,
                &method.declaring_type,
                &method.name,
                span,
            );
        }
        let arguments = match resolved.as_ref() {
            Some(method) if params_method => self.bind_params_arguments(method, arguments),
            Some(method)
                if method.is_vararg && method.parameters.len() + 1 == arguments.len() =>
            {
                let mut remaining = arguments.into_iter();
                let mut bound = Vec::with_capacity(method.parameters.len() + 1);
                for parameter in method.parameters.iter() {
                    if let Some(argument) = remaining.next() {
                        bound.push(self.convert(argument, parameter));
                    }
                }
                bound.extend(remaining);
                bound
            }
            Some(method) if method.parameters.len() == arguments.len() => arguments
                .into_iter()
                .zip(method.parameters.iter())
                .map(|(argument, parameter)| self.convert(argument, parameter))
                .collect(),
            _ => arguments,
        };
        let ty = resolved
            .as_ref()
            .map_or(TypeSymbol::Error, |method| method.return_type.clone());
        BoundExpr {
            kind: BoundExprKind::Call {
                callee: Box::new(callee),
                arguments,
                method: resolved,
            },
            ty,
        }
    }

    /// Binds the arguments of a call to a `params` method: an array supplied directly
    /// (normal form) converts 1:1; otherwise the trailing arguments are wrapped into a
    /// new array of the element type (expanded form).
    fn bind_params_arguments(
        &mut self,
        method: &MethodReference,
        arguments: Vec<BoundExpr>,
    ) -> Vec<BoundExpr> {
        let param_count = method.parameters.len();
        let fixed = param_count.saturating_sub(1);
        let array_ty = method.parameters[fixed].clone();
        if arguments.len() == param_count && self.converts(&arguments[fixed].ty, &array_ty) {
            return arguments
                .into_iter()
                .zip(method.parameters.iter())
                .map(|(argument, parameter)| self.convert(argument, parameter))
                .collect();
        }
        let element_ty = match &array_ty {
            TypeSymbol::Array { element, .. } => (**element).clone(),
            _ => TypeSymbol::Error,
        };
        let mut bound = Vec::with_capacity(param_count);
        let mut remaining = arguments.into_iter();
        for parameter in &method.parameters[..fixed] {
            if let Some(argument) = remaining.next() {
                bound.push(self.convert(argument, parameter));
            }
        }
        let elements: Vec<BoundExpr> = remaining
            .map(|argument| self.convert(argument, &element_ty))
            .collect();
        bound.push(BoundExpr {
            kind: BoundExprKind::ArrayCreation {
                lengths: Vec::new(),
                elements,
            },
            ty: array_ty,
        });
        bound
    }

    /// Reports `CS1620` for an argument that reached a by-reference parameter under the wrong
    /// modifier -- `Take(out v)` against `Take(ref int)`, or the reverse.
    ///
    /// THIS RUNS AFTER RESOLUTION, NOT DURING IT, and that is csc's order too: given both
    /// `T(ref int)` and `T(int)`, `T(out v)` resolves to the BYREF overload and is then faulted
    /// for its modifier, rather than being told no overload matches. Resolution cannot make this
    /// call anyway -- `ref x` and `out x` give the argument the same `ByRef` type, so the two are
    /// indistinguishable until the parameter's recorded mode is consulted.
    ///
    /// The mode is only consulted where BOTH sides are byref. An argument whose byref-ness does
    /// not match the parameter at all never gets here: it is not applicable, so the call fails
    /// resolution and reports `CS1503`. csc says `CS1620`/`CS1615` for those, which is a better
    /// code on an outcome we already agree about -- tracked, and not fixed here, because closing
    /// it means making a byref parameter applicable to an unmodified argument, and that is a
    /// change to overload resolution rather than a diagnostic added beside it.
    fn check_argument_modes(
        &mut self,
        method: &MethodSymbol,
        arguments: &[BoundExpr],
        span: Span,
    ) {
        for (index, argument) in arguments.iter().enumerate() {
            let BoundExprKind::Ref { out, .. } = &argument.kind else {
                continue;
            };
            let Some(mode) = method.parameter_mode(index) else {
                continue;
            };
            let wanted = match mode {
                ParameterMode::Ref => "ref",
                ParameterMode::Out => "out",
                ParameterMode::Value => continue,
            };
            if *out == matches!(mode, ParameterMode::Out) {
                continue;
            }
            self.diagnostics.push(Diagnostic::new(
                DiagnosticKind::ArgumentModeRequired {
                    index: index as u32 + 1,
                    keyword: wanted.into(),
                },
                span,
            ));
        }
    }

    /// The diagnostic for a call that failed ONLY because of by-reference modifiers -- csc's
    /// `CS1620` (the parameter wants a keyword the argument did not write) and `CS1615` (the
    /// argument wrote one the parameter does not take).
    ///
    /// THIS RUNS ONLY AFTER NORMAL RESOLUTION HAS ALREADY FAILED, which is what keeps it from
    /// disturbing overload choice. A modified argument and a byref parameter do not share a type
    /// -- `int` against `ref int` -- so a call like `T(v)` against `T(ref int)` finds nothing
    /// applicable and would report `CS1503`, describing a conversion when the real complaint is a
    /// missing keyword. Rather than making a byref parameter applicable to a bare argument (which
    /// WOULD change resolution, and would make `T(v)` ambiguous wherever `T(int)` also exists),
    /// the candidates are re-examined here with by-reference-ness ignored.
    ///
    /// THE TEST IS BY-REFERENCE SHAPE ALONE, NOT BY TYPE, and I had that wrong at first. csc
    /// reports the modifier whenever NO same-arity candidate accepts the argument's byref-ness at
    /// that position -- even when the type would not convert either. `Take(ref s)` with a `string`
    /// against `Take(int)` is `CS1615`, not the conversion complaint: the keyword is wrong at every
    /// candidate, so it is the first thing to fix. Only when SOME candidate does accept that shape
    /// does the conversion get reported instead, which is why `Take(ref s)` against a group holding
    /// `Take(ref int)` stays `CS1503` -- there the modifier was fine and the type was not.
    ///
    /// Positions are examined left to right, so a good first argument and a bad second reports the
    /// second, as csc does.
    fn modifier_mismatch(
        &self,
        candidates: &[MethodSymbol],
        argument_types: &[TypeSymbol],
        arguments: &[BoundExpr],
    ) -> Option<DiagnosticKind> {
        let is_byref = |ty: &TypeSymbol| matches!(ty, TypeSymbol::ByRef(_));
        let same_arity: Vec<&MethodSymbol> = candidates
            .iter()
            .filter(|candidate| {
                !candidate.is_vararg
                    && !candidate.is_params
                    && candidate.parameters.len() == argument_types.len()
            })
            .collect();
        if same_arity.is_empty() {
            return None;
        }
        for (index, argument) in argument_types.iter().enumerate() {
            let argument_is_byref = is_byref(argument);
            if same_arity
                .iter()
                .any(|candidate| is_byref(&candidate.parameters[index]) == argument_is_byref)
            {
                continue;
            }
            return Some(if argument_is_byref {
                DiagnosticKind::ArgumentModeForbidden {
                    index: index as u32 + 1,
                    keyword: match arguments.get(index).map(|argument| &argument.kind) {
                        Some(BoundExprKind::Ref { out: true, .. }) => "out".into(),
                        _ => "ref".into(),
                    },
                }
            } else {
                DiagnosticKind::ArgumentModeRequired {
                    index: index as u32 + 1,
                    keyword: match same_arity
                        .iter()
                        .find_map(|candidate| candidate.parameter_mode(index))
                    {
                        Some(ParameterMode::Out) => "out".into(),
                        _ => "ref".into(),
                    },
                }
            });
        }
        None
    }

    /// Resolves a call to a method group by overload resolution (14.4.2),
    /// reporting the appropriate diagnostic and returning the chosen method.
    /// A method candidate as csc quotes it in CS0305/CS0308: `C.Id<T>(T)`, and `C.Two<T, U>(T, U)`
    /// for two of each. Measured against csc, including the space after each comma in
    /// BOTH lists -- a renderer that omitted it would differ from csc in a message a user compares
    /// character by character against a search result.
    ///
    /// The declaring type is named as the receiver's static type. That is the type the call was
    /// WRITTEN through, which is what csc quotes; naming the type that actually declares an
    /// inherited method would be more precise and would not match.
    ///
    /// A method with no declared parameter names still renders its angle brackets, from the arity:
    /// same rule as [`crate::resolve::quote_candidate`]'s, and for the same reason -- inventing
    /// `T1, T2` would quote a name that appears in no source.
    fn quote_method_candidate(receiver: &TypeSymbol, method: &MethodSymbol) -> Box<str> {
        let mut text = alloc::format!("{receiver}.{}", method.name);
        if !method.type_parameters.is_empty() {
            text.push('<');
            for (index, parameter) in method.type_parameters.iter().enumerate() {
                if index > 0 {
                    text.push_str(", ");
                }
                text.push_str(parameter);
            }
            text.push('>');
        }
        text.push('(');
        for (index, parameter) in method.parameters.iter().enumerate() {
            if index > 0 {
                text.push_str(", ");
            }
            text.push_str(&alloc::format!("{parameter}"));
        }
        text.push(')');
        text.into()
    }

    /// The candidate set for a call, once the site's explicit type arguments are taken into
    /// account -- ECMA-334 4th ed **14.5.5.1**, whose three bullets this implements directly:
    ///
    /// * *"If F is non-generic, F is a candidate when M has no type argument list"* -- so a
    ///   non-generic method is NOT a candidate for `M<int>(x)`. Measured against csc: CS0308.
    /// * *"If F is generic and M includes a type argument list, F is a candidate when F has the
    ///   same number of method type parameters as were supplied"*, and then the parameter list is
    ///   applicable **after substitution**. Measured against csc: a count mismatch is CS0305.
    /// * *"If F is generic and M has no type argument list"*, F is a candidate when type INFERENCE
    ///   (25.6.4) succeeds for it -- see [`infer_method_type_arguments`].
    ///
    /// **THE SUBSTITUTION IS THE POINT, NOT THE FILTERING.** Returning the matching candidates
    /// unsubstituted would still pick the right method and would still type-check the arguments
    /// against `T`, which accepts everything. `Id<string>(1)` would compile. Every candidate that
    /// survives here is CLOSED, so the ordinary overload resolution and conversion machinery below
    /// sees real types and needs to know nothing about generics.
    ///
    /// **A GENERIC METHOD WHOSE INFERENCE FAILS IS DROPPED RATHER THAN BOUND OPEN**, and that is
    /// the same rule as the standard's: *"If type inference for a particular method fails, that
    /// method does not participate in overload resolution."* Keeping it would bind `Id(1)` to the
    /// open method with `!!0` unsubstituted -- silently the wrong program. The dropped ones are
    /// returned in [`CandidateSet::uninferable`] so the caller can say CS0411 rather than a bare
    /// no-such-overload, which is the difference between naming the real problem and describing a
    /// symptom.
    ///
    /// Reports the constraints a generic METHOD call's explicit type arguments fail (25.7.1).
    ///
    /// **Only when ONE definition survived the arity filter.** With several, the call is an
    /// overload set and the winner is not known here; reporting against each would produce a
    /// message per candidate for a call that resolves fine. csc checks the chosen overload, which
    /// this cannot see -- so the single-candidate case is checked and the ambiguous one is left,
    /// deliberately under-reporting rather than guessing.
    ///
    /// **An imported method's constraints are ABSENT rather than empty** -- `reference.rs` does
    /// not decode `GenericParamConstraint` -- so this is silent for a call into a referenced
    /// assembly. That is a known gap, recorded at the field, not something this function papers
    /// over.
    fn check_method_type_arguments(
        &mut self,
        definitions: &[MethodSymbol],
        type_arguments: &[TypeSymbol],
        receiver_ty: &TypeSymbol,
        span: Span,
    ) {
        let [definition] = definitions else {
            return;
        };
        let declaration = Self::quote_method_candidate(receiver_ty, definition);
        for (index, argument) in type_arguments.iter().enumerate() {
            if argument.is_error() {
                continue;
            }
            let Some(constraints) = definition.constraints_on(index) else {
                continue;
            };
            if constraints.is_empty() {
                continue;
            }
            let parameter: Box<str> = definition.type_parameters[index].clone();
            let constraints = constraints.clone();
            self.check_one_type_argument_as(
                argument,
                &constraints,
                &parameter,
                &declaration,
                GenericMember::Method,
                span,
            );
        }
    }

    fn candidates_for_type_arguments(
        &mut self,
        candidates: &[MethodSymbol],
        type_arguments: &[TypeSymbol],
        argument_types: &[TypeSymbol],
        receiver_ty: &TypeSymbol,
        span: Span,
    ) -> CandidateSet {
        if type_arguments.is_empty() {
            return self.candidates_by_inference(candidates, argument_types, receiver_ty, span);
        }
        if type_arguments.iter().any(TypeSymbol::is_error) {
            return CandidateSet::default();
        }
        let (matching, definitions): (Vec<MethodSymbol>, Vec<MethodSymbol>) = candidates
            .iter()
            .filter(|candidate| candidate.type_parameters.len() == type_arguments.len())
            .filter_map(|candidate| {
                candidate
                    .instantiate(type_arguments)
                    .map(|closed| (closed, candidate.clone()))
            })
            .unzip();
        if !matching.is_empty() {
            self.check_method_type_arguments(&definitions, type_arguments, receiver_ty, span);
            return CandidateSet {
                origins: definitions
                    .into_iter()
                    .map(|definition| {
                        Some(ClosedFrom {
                            definition,
                            arguments: type_arguments.to_vec(),
                        })
                    })
                    .collect(),
                candidates: matching,
                uninferable: Vec::new(),
            };
        }
        if let Some(generic) = candidates
            .iter()
            .find(|candidate| !candidate.type_parameters.is_empty())
        {
            self.report(Diagnostic::new(
                DiagnosticKind::GenericArityMismatch {
                    candidate: Self::quote_method_candidate(receiver_ty, generic),
                    required: generic.type_parameters.len(),
                    member: GenericMember::Method,
                },
                span,
            ));
        } else if let Some(plain) = candidates.first() {
            self.report(Diagnostic::new(
                DiagnosticKind::NonGenericTypeWithTypeArguments {
                    name: Self::quote_method_candidate(receiver_ty, plain),
                    member: GenericMember::Method,
                },
                span,
            ));
        }
        CandidateSet::default()
    }

    /// The candidate set for a call that named NO type arguments -- 14.5.5.1's third bullet, and
    /// where ECMA-334 4th ed 25.6.4 runs.
    ///
    /// A non-generic method is a candidate as it always was. A generic one joins CLOSED over the
    /// arguments inferred for it, and is dropped when inference fails. **A `params` generic method
    /// is inferred in its NORMAL form first and its EXPANDED form only if that fails**, which is
    /// the order 25.6.4's last paragraph gives; taking the expanded form first would infer `T` from
    /// an array argument's ELEMENT type for a call that meant to pass the array whole.
    fn candidates_by_inference(
        &mut self,
        candidates: &[MethodSymbol],
        argument_types: &[TypeSymbol],
        receiver_ty: &TypeSymbol,
        span: Span,
    ) -> CandidateSet {
        let mut set = CandidateSet::default();
        for candidate in candidates {
            if candidate.type_parameters.is_empty() {
                set.candidates.push(candidate.clone());
                set.origins.push(None);
                continue;
            }
            let inferred =
                infer_method_type_arguments(&self.model, candidate, argument_types).or_else(|| {
                    infer_expanded_type_arguments(&self.model, candidate, argument_types)
                });
            let Some(arguments) = inferred else {
                if candidate.parameters.len() == argument_types.len() {
                    set.uninferable.push(candidate.clone());
                }
                continue;
            };
            let Some(closed) = candidate.instantiate(&arguments) else {
                continue;
            };
            self.check_method_type_arguments(
                core::slice::from_ref(candidate),
                &arguments,
                receiver_ty,
                span,
            );
            set.candidates.push(closed);
            set.origins.push(Some(ClosedFrom {
                definition: candidate.clone(),
                arguments,
            }));
        }
        set.apply_identical_signature_tie_break();
        set
    }

    /// The open definition `chosen` was closed from, as a [`MethodInstantiation`] emission can mint
    /// a `MethodSpec` from -- `None` for a call that resolved to an ordinary non-generic method.
    ///
    /// **THE TYPE ARGUMENTS COME FROM THE CANDIDATE, NEVER FROM THE CALL SITE.** The site's list is
    /// EMPTY for an inferred call, so reading them there would mint an instantiation with no
    /// arguments for exactly the calls inference exists to serve. Each candidate carries the
    /// arguments it was closed over, whichever way they arose.
    ///
    /// **A tie yields `None` rather than a guess.** Two generic overloads that close to the SAME
    /// signature (`void M<T>(T)` and `void M<T>(int)` at `M<int>(1)`) are already ambiguous to
    /// overload resolution, and their DEFINITIONS differ -- `(!!0)` against `(int32)` -- so picking
    /// either would name the wrong `MethodDef` in the row. Emission then refuses the call, which is
    /// the outcome an unresolvable call should have anyway. Ties that 14.4.2.2 DOES settle never
    /// reach here: [`CandidateSet::apply_identical_signature_tie_break`] removed the loser.
    fn instantiation_of(
        chosen: &MethodSymbol,
        set: &CandidateSet,
    ) -> Option<Box<MethodInstantiation>> {
        let mut matches = set
            .candidates
            .iter()
            .zip(&set.origins)
            .filter(|(candidate, _)| *candidate == chosen);
        let (_, origin) = matches.next()?;
        if matches.next().is_some() {
            return None;
        }
        let origin = origin.as_ref()?;
        Some(Box::new(MethodInstantiation {
            arguments: origin.arguments.clone(),
            type_parameters: origin.definition.type_parameters.clone(),
            parameters: origin.definition.parameters.clone(),
            return_type: origin.definition.return_type.clone(),
        }))
    }

    /// [`Binder::resolve_call`], except that a failure with nothing better to say about it is
    /// reported as `CS0411` when a generic candidate's type inference is what actually went wrong.
    ///
    /// **THE BOUNDARY IS csc's, MEASURED, AND IT IS NARROWER THAN "INFERENCE FAILED".** Four shapes
    /// pin it:
    ///
    /// ```text
    ///     only `T Id<T>(T)`, called Id(1, 2)            CS1501   wrong ARITY, not inference
    ///     only `T Choose<T>(T, T)`, called (5, "x")     CS0411
    ///     plus `int M(string)`, called M(1, "x")        CS0411   the other candidate takes 1
    ///     plus `int M(string, string)`, called M(1,"x") CS1503   the other candidate takes 2
    /// ```
    ///
    /// So CS0411 is what a call gets when the ordinary machinery would fall all the way through to
    /// a bare count complaint. A same-arity candidate has a real conversion failure to report and
    /// csc reports THAT -- naming the argument that could not convert is more use than naming a
    /// type parameter that could not be inferred for a different overload.
    fn resolve_call_or_uninferable(
        &mut self,
        name: &str,
        declaring: &TypeSymbol,
        set: &CandidateSet,
        argument_types: &[TypeSymbol],
        arg_constants: &[Option<i64>],
        arguments: &[BoundExpr],
        span: Span,
    ) -> Option<MethodSymbol> {
        if !set.uninferable.is_empty()
            && matches!(
                resolve_overload(&self.model, &set.candidates, argument_types, arg_constants),
                OverloadResult::WrongArgumentCount
            )
        {
            let candidate = &set.uninferable[0];
            self.report(Diagnostic::new(
                DiagnosticKind::TypeArgumentsCannotBeInferred {
                    candidate: Self::quote_method_candidate(declaring, candidate),
                },
                span,
            ));
            return None;
        }
        self.resolve_call(
            name,
            declaring,
            &set.candidates,
            argument_types,
            arg_constants,
            arguments,
            span,
        )
    }

    fn resolve_call(
        &mut self,
        name: &str,
        declaring: &TypeSymbol,
        candidates: &[MethodSymbol],
        argument_types: &[TypeSymbol],
        arg_constants: &[Option<i64>],
        arguments: &[BoundExpr],
        span: Span,
    ) -> Option<MethodSymbol> {
        let resolution = resolve_overload(&self.model, candidates, argument_types, arg_constants);
        if matches!(resolution, OverloadResult::BadArgument { .. }) {
            if let Some(kind) = self.modifier_mismatch(candidates, argument_types, arguments) {
                self.diagnostics.push(Diagnostic::new(kind, span));
                return None;
            }
        }
        match resolution {
            OverloadResult::Resolved(method) => {
                let declaring_type = self.declaring_type_in_chain(declaring, name, &method.parameters);
                self.check_accessible(&declaring_type, method.accessibility, name, span);
                Some(method)
            }
            OverloadResult::Ambiguous => {
                self.diagnostics.push(Diagnostic::new(
                    DiagnosticKind::AmbiguousCall {
                        method: name.into(),
                    },
                    span,
                ));
                None
            }
            OverloadResult::WrongArgumentCount => {
                if let Some(vararg) = candidates
                    .iter()
                    .find(|c| c.is_vararg && c.parameters.len() == argument_types.len())
                {
                    self.diagnostics.push(Diagnostic::new(
                        DiagnosticKind::NoArgumentForArglist {
                            method: vararg_member_display(declaring, name, &vararg.parameters),
                        },
                        span,
                    ));
                    return None;
                }
                let group = if candidates.is_empty() {
                    self.methods_in_chain(declaring, name)
                } else {
                    Vec::new()
                };
                let describable: &[MethodSymbol] =
                    if candidates.is_empty() { &group } else { candidates };
                if let [only] = describable {
                    if argument_types.len() < only.parameters.len() {
                        self.diagnostics.push(Diagnostic::new(
                            DiagnosticKind::MissingArgumentForParameter {
                                parameter: only
                                    .parameter_name(argument_types.len())
                                    .unwrap_or("value")
                                    .into(),
                                method: qualified_method_with_modes(declaring, name, only),
                            },
                            span,
                        ));
                        return None;
                    }
                }
                self.diagnostics.push(Diagnostic::new(
                    DiagnosticKind::NoOverloadForArgumentCount {
                        method: name.into(),
                        count: argument_types.len() as u32,
                    },
                    span,
                ));
                None
            }
            OverloadResult::BadArgument { index, from, to } => {
                self.diagnostics.push(Diagnostic::new(
                    DiagnosticKind::ArgumentConversion {
                        index: index as u32 + 1,
                        from: from.to_string().into(),
                        to: to.to_string().into(),
                    },
                    span,
                ));
                None
            }
        }
    }

    /// Resolves a call/constructor whose arguments include a method group (which has no
    /// type on its own, so it is excluded from the type-based [`resolve_call`]). A candidate
    /// applies when its parameter count matches, each ordinary argument is assignable to its
    /// parameter, and each method-group argument's parameter is a DELEGATE type -- the group
    /// converts to it (15.4), and `convert` builds the delegate at the call site. Returns the
    /// unique applicable method (reporting CS0122 if inaccessible, CS0121 if two apply),
    /// else `None`.
    /// Whether a method-group argument is applicable to a parameter (15.4): the parameter must be a
    /// delegate type and the group must contain a candidate whose signature exactly matches the
    /// delegate's `Invoke` (parameters + return), the same test `bind_delegate_creation` applies.
    /// Without it a `void()` group looks applicable to EVERY delegate parameter, so overloads like
    /// `F(D0)`/`F(D1)` both match and the call is falsely reported ambiguous (CS0121).
    fn method_group_matches_delegate(&self, argument: &BoundExpr, parameter: &TypeSymbol) -> bool {
        let BoundExprKind::MethodGroup { receiver, name } = &argument.kind else {
            return false;
        };
        let Some(invoke) = self.type_info_of(parameter).and_then(|info| {
            if info.kind == TypeKind::Delegate {
                info.methods.iter().find(|m| &*m.name == "Invoke").cloned()
            } else {
                None
            }
        }) else {
            return false;
        };
        self.methods_in_chain(&receiver.ty, name)
            .iter()
            .any(|candidate| {
                candidate.parameters == invoke.parameters
                    && candidate.return_type == invoke.return_type
            })
    }

    fn resolve_with_method_groups(
        &mut self,
        name: &str,
        declaring: &TypeSymbol,
        candidates: &[MethodSymbol],
        arguments: &[BoundExpr],
        span: Span,
    ) -> Option<MethodSymbol> {
        let applicable: Vec<MethodSymbol> = candidates
            .iter()
            .filter(|candidate| {
                candidate.parameters.len() == arguments.len()
                    && arguments.iter().zip(&candidate.parameters).all(
                        |(argument, parameter)| {
                            if matches!(argument.kind, BoundExprKind::Ref { .. })
                                || matches!(parameter, TypeSymbol::ByRef(_))
                            {
                                argument_type(argument) == *parameter
                            } else if matches!(argument.kind, BoundExprKind::MethodGroup { .. }) {
                                self.method_group_matches_delegate(argument, parameter)
                            } else {
                                self.assignable(argument, parameter)
                            }
                        },
                    )
            })
            .cloned()
            .collect();
        match applicable.as_slice() {
            [method] => {
                self.check_accessible(declaring, method.accessibility, name, span);
                Some(method.clone())
            }
            [] => None,
            _ => {
                self.diagnostics.push(Diagnostic::new(
                    DiagnosticKind::AmbiguousCall { method: name.into() },
                    span,
                ));
                None
            }
        }
    }

    fn bind_element_access(
        &mut self,
        receiver_expr: &Expr,
        argument_exprs: &[Expr],
        span: Span,
    ) -> BoundExpr {
        let receiver = self.bind_expression(receiver_expr);
        let indices: Vec<BoundExpr> = argument_exprs
            .iter()
            .map(|argument| self.bind_expression(argument))
            .collect();
        let element = match &receiver.ty {
            TypeSymbol::Array { element, .. } => Some((**element).clone()),
            TypeSymbol::Special(SpecialType::String) if indices.len() == 1 => {
                Some(TypeSymbol::Special(SpecialType::Char))
            }
            TypeSymbol::Pointer(element) if indices.len() == 1 => Some((**element).clone()),
            _ => None,
        };
        if let Some(ty) = element {
            for (index, argument) in indices.iter().zip(argument_exprs) {
                self.check_index_or_length(index, argument.span);
            }
            return BoundExpr {
                kind: BoundExprKind::ElementAccess {
                    receiver: Box::new(receiver),
                    indices,
                },
                ty,
            };
        }
        if receiver.ty.is_error() {
            return error_expr();
        }
        let Some(getter) = self.indexer_accessor(&receiver.ty, "get_", indices.len()) else {
            self.diagnostics.push(Diagnostic::new(
                DiagnosticKind::CannotIndex {
                    type_name: receiver.ty.to_string().into(),
                },
                span,
            ));
            return error_expr();
        };
        self.bind_indexer_call(receiver, &getter, indices, span)
            .unwrap_or_else(error_expr)
    }

    /// Resolves an indexer accessor overload (`get_`/`set_`) on `receiver_ty` and converts the
    /// arguments to its parameter types (boxing/upcasting). Returns the resolved accessor and the
    /// converted arguments, or `None` when no overload matches (the resolver reports the error).
    /// Shared by the indexer read ([`Self::bind_indexer_call`]) and write
    /// ([`Self::bind_indexer_store`]) paths.
    fn resolve_indexer_accessor(
        &mut self,
        receiver_ty: &TypeSymbol,
        accessor: &str,
        arguments: Vec<BoundExpr>,
        span: Span,
    ) -> Option<(MethodReference, Vec<BoundExpr>)> {
        if receiver_ty.is_error() || arguments.iter().any(|argument| argument.ty.is_error()) {
            return None;
        }
        let candidates = self.methods_in_chain(receiver_ty, accessor);
        let argument_types: Vec<TypeSymbol> = arguments.iter().map(argument_type).collect();
        let arg_constants: Vec<Option<i64>> = arguments.iter().map(constant_int_value).collect();
        let method = self.resolve_call(
            accessor,
            receiver_ty,
            &candidates,
            &argument_types,
            &arg_constants,
            &arguments,
            span,
        )?;
        let declaring_type =
            self.declaring_type_in_chain(receiver_ty, &method.name, &method.parameters);
        let declaring_instantiation =
            self.declaring_instantiation_of(&declaring_type, &method.name, &method.parameters);
        let method_ref = MethodReference {
            declaring_type,
            is_vararg: method.is_vararg,
            name: method.name,
            parameters: method.parameters,
            return_type: method.return_type,
            is_static: false,
            instantiation: None,
            declaring_instantiation,
        };
        let arguments = if method_ref.parameters.len() == arguments.len() {
            arguments
                .into_iter()
                .zip(method_ref.parameters.iter())
                .map(|(argument, parameter)| self.convert(argument, parameter))
                .collect()
        } else {
            arguments
        };
        Some((method_ref, arguments))
    }

    /// Binds an indexer READ `obj[args]` as a call to its `get_` accessor (14.5.6.2).
    fn bind_indexer_call(
        &mut self,
        receiver: BoundExpr,
        accessor: &str,
        arguments: Vec<BoundExpr>,
        span: Span,
    ) -> Option<BoundExpr> {
        let (method_ref, arguments) =
            self.resolve_indexer_accessor(&receiver.ty, accessor, arguments, span)?;
        let ty = method_ref.return_type.clone();
        let callee = BoundExpr {
            ty: TypeSymbol::Error,
            kind: BoundExprKind::MethodGroup {
                receiver: Box::new(receiver),
                name: accessor.into(),
            },
        };
        Some(BoundExpr {
            kind: BoundExprKind::Call {
                callee: Box::new(callee),
                arguments,
                method: Some(method_ref),
            },
            ty,
        })
    }

    /// Binds a COMPOUND indexer assignment `receiver[indices] op= value` as the read-modify-write
    /// it is: `receiver[indices] = receiver[indices] op value`.
    ///
    /// The receiver and indices are bound ONCE and the bound trees CLONED for the second use --
    /// the same shape the pointer, enum and user-operator compound lowerings take. Binding the
    /// syntax twice instead would report any error in it twice: `nope[0] += 1` gave CS0103 twice
    /// where csc gives it once. The two accessor RESOLUTIONS are still independent, since each is
    /// resolved against its own argument list, so a `[IndexerName]`-renamed indexer still gets its
    /// matching pair.
    ///
    /// `None` when either accessor does not resolve, leaving the caller's existing paths to report.
    fn bind_indexer_compound(
        &mut self,
        receiver: &lamella_syntax::ast::Expr,
        arguments: &[lamella_syntax::ast::Expr],
        binary_op: BinaryOperator,
        value_expr: &lamella_syntax::ast::Expr,
        span: Span,
    ) -> Option<BoundExpr> {
        let read_receiver = self.bind_expression(receiver);
        if read_receiver.ty.is_error()
            || matches!(
                read_receiver.ty,
                TypeSymbol::Array { .. } | TypeSymbol::Special(SpecialType::String)
            )
        {
            return None;
        }
        let setter = self.indexer_accessor(&read_receiver.ty, "set_", arguments.len() + 1)?;
        let getter = self.indexer_accessor(&read_receiver.ty, "get_", arguments.len())?;
        let indices: Vec<BoundExpr> = arguments
            .iter()
            .map(|argument| self.bind_expression(argument))
            .collect();
        let store_receiver = read_receiver.clone();
        let store_indices = indices.clone();
        let current = self.bind_indexer_call(read_receiver, &getter, indices, span)?;

        let operand = self.bind_expression(value_expr);
        let result_ty = binary_result_type(binary_op, &current.ty, &operand.ty)?;
        let combined = BoundExpr {
            kind: BoundExprKind::Binary {
                operator: binary_op,
                left: Box::new(current),
                right: Box::new(operand),
                checked: self.checked_context,
            },
            ty: result_ty,
        };

        let mut store_args = store_indices;
        store_args.push(combined);
        self.bind_indexer_store(store_receiver, &setter, store_args, span)
    }

    /// Binds an indexer WRITE `obj[indices] = value` as an [`BoundExprKind::IndexerAccess`] store
    /// (14.14.1): it resolves the `set_` accessor over `[indices..., value]`, then splits the
    /// converted value (the setter's last argument) from the indices. The result is an
    /// `Assignment` whose type is the indexer's element type, so -- unlike a bare void `set_` call
    /// -- the write is usable as a value (`int y = (a[i] = v)`). `None` when no setter matches.
    fn bind_indexer_store(
        &mut self,
        receiver: BoundExpr,
        accessor: &str,
        arguments: Vec<BoundExpr>,
        span: Span,
    ) -> Option<BoundExpr> {
        let (setter, mut arguments) =
            self.resolve_indexer_accessor(&receiver.ty, accessor, arguments, span)?;
        let value = arguments.pop()?;
        let element_ty = value.ty.clone();
        let target = BoundExpr {
            ty: element_ty.clone(),
            kind: BoundExprKind::IndexerAccess {
                receiver: Box::new(receiver),
                indices: arguments,
                setter,
            },
        };
        Some(BoundExpr {
            ty: element_ty,
            kind: BoundExprKind::Assignment {
                operator: AssignmentOperator::Assign,
                target: Box::new(target),
                value: Box::new(value),
                checked: self.checked_context,
            },
        })
    }

    /// Binds an object or collection initializer against the type being created.
    ///
    /// **MEASURED, and it decides the whole shape: csc reports these member diagnostics EVEN WHEN
    /// the language-version gate has already refused the initializer.** `new C { Nope = 1 }` under
    /// `/langversion:ISO-1` draws CS8022 *and* CS0117. So binding is unconditional and the gate is
    /// additive -- an initializer is never left unbound merely because the dialect forbids it.
    ///
    /// | mistake | csc |
    /// |---|---|
    /// | no such member | `CS0117` |
    /// | a `readonly` field | `CS0191` |
    /// | a property with no setter | `CS0200` |
    /// | a `static` field or property | `CS1914` |
    /// | a value of the wrong type | `CS0029`, from binding the value against the member's type |
    ///
    /// Only `CS1914` is specific to initializers; the rest are the ordinary assignment rules
    /// reached through a different syntax, which is why four of the five needed no new diagnostic.
    fn bind_initializer(
        &mut self,
        target_ty: &TypeSymbol,
        initializer: &Initializer,
        span: Span,
    ) -> BoundInitializer {
        match initializer {
            Initializer::Collection(elements) => {
                if !self.implements_enumerable(target_ty) {
                    self.report(Diagnostic::new(
                        DiagnosticKind::NotACollectionInitializerTarget {
                            type_name: target_ty.to_string().into(),
                        },
                        span,
                    ));
                } else if self.methods_in_chain(target_ty, "Add").is_empty() {
                    self.report(Diagnostic::new(
                        DiagnosticKind::MemberNotFoundOnExpression {
                            type_name: target_ty.to_string().into(),
                            member: "Add".into(),
                        },
                        span,
                    ));
                }
                BoundInitializer::Collection(
                    elements.iter().map(|element| self.bind_expression(element)).collect(),
                )
            }
            Initializer::Object(members) => BoundInitializer::Object(
                members
                    .iter()
                    .map(|member| self.bind_member_initializer(target_ty, member))
                    .collect(),
            ),
        }
    }

    /// Binds one `name = value`, resolving `name` against `target_ty` and reporting the measured
    /// rule it breaks.
    fn bind_member_initializer(
        &mut self,
        target_ty: &TypeSymbol,
        member: &MemberInitializer,
    ) -> BoundMemberInitializer {
        let resolution = self.resolve_member(target_ty, &member.name);
        let qualified = format!("{target_ty}.{}", member.name);
        if matches!(member.value, MemberInitializerValue::Nested(_))
            && self.is_required_member(target_ty, &member.name)
        {
            self.report(Diagnostic::new(
                DiagnosticKind::RequiredMemberNeedsValue {
                    member: qualified.clone().into(),
                },
                member.span,
            ));
        }
        let mut target = BoundInitializerTarget::Unresolved;
        let mut expected: Option<TypeSymbol> = None;
        match &resolution {
            MemberResolution::Field(reference) => {
                if reference.is_static {
                    self.report(Diagnostic::new(
                        DiagnosticKind::StaticMemberInObjectInitializer {
                            member: qualified.clone().into(),
                        },
                        member.span,
                    ));
                } else if reference.is_readonly {
                    self.report(Diagnostic::new(
                        DiagnosticKind::ReadonlyAssignment {
                            field: reference.name.clone(),
                        },
                        member.span,
                    ));
                }
                expected = Some(reference.ty.clone());
                target = BoundInitializerTarget::Field(reference.clone());
            }
            MemberResolution::Property {
                declaring_type,
                ty,
                is_static,
                ..
            } => {
                if *is_static {
                    self.report(Diagnostic::new(
                        DiagnosticKind::StaticMemberInObjectInitializer {
                            member: qualified.clone().into(),
                        },
                        member.span,
                    ));
                } else if !self.property_has_setter(declaring_type, &member.name) {
                    self.report(Diagnostic::new(
                        DiagnosticKind::PropertyCannotBeAssigned {
                            property: qualified.clone().into(),
                        },
                        member.span,
                    ));
                }
                expected = Some(ty.clone());
                target = BoundInitializerTarget::Property {
                    setter_declaring_type: declaring_type.clone(),
                    ty: ty.clone(),
                };
            }
            MemberResolution::MethodGroup | MemberResolution::NoSuchMember(_) => {
                self.report(Diagnostic::new(
                    DiagnosticKind::MemberNotFound {
                        type_name: target_ty.to_string().into(),
                        member: member.name.clone(),
                    },
                    member.span,
                ));
            }
            MemberResolution::Unknown => {}
        }
        let value = match &member.value {
            MemberInitializerValue::Expression(expression) => {
                let bound = self.bind_expression(expression);
                if let Some(ty) = &expected {
                    self.check_assignable(&bound, ty, member.span);
                }
                BoundMemberInitializerValue::Expression(bound)
            }
            MemberInitializerValue::Nested(nested) => {
                let nested_ty = expected.unwrap_or_else(|| target_ty.clone());
                BoundMemberInitializerValue::Nested(self.bind_initializer(
                    &nested_ty,
                    nested,
                    member.span,
                ))
            }
        };
        BoundMemberInitializer {
            name: member.name.clone(),
            target,
            value,
        }
    }

    /// Whether `ty` implements `System.Collections.IEnumerable`, the condition a collection
    /// initializer requires (csc CS1922).
    ///
    /// **The interface is required but never CALLED** -- a collection initializer emits `Add`, not
    /// an enumeration. csc demands it anyway as the marker that the type is a collection at all,
    /// which is why this is a separate check from having an `Add` and gets its own diagnostic.
    ///
    /// **THE INTERFACE, NOT A `GetEnumerator` METHOD, AND I MEASURED THAT AFTER ASSUMING
    /// OTHERWISE.** The first version of this asked whether a `GetEnumerator` was reachable,
    /// reasoning that the interface's only content is that method. csc disagrees: a class with a
    /// public `IEnumerator GetEnumerator()` and an `Add`, but no `: IEnumerable`, is still CS1922.
    /// The check is nominal, so duck-typing it would ACCEPT a program csc rejects.
    ///
    /// Answers `true` for a type outside the model: refusing something we cannot inspect would
    /// invent a diagnostic, and the permissive direction here cannot.
    fn implements_enumerable(&self, ty: &TypeSymbol) -> bool {
        const ENUMERABLE: &str = "IEnumerable";
        let root = self.lookup_type_of(ty);
        if self.type_info_of(&root).is_none() {
            return true;
        }
        let mut pending = alloc::vec![root];
        let mut visited: Vec<TypeSymbol> = Vec::new();
        while let Some(current) = pending.pop() {
            if visited.contains(&current) {
                continue;
            }
            if current
                .to_string()
                .rsplit('.')
                .next()
                .is_some_and(|last| last == ENUMERABLE)
            {
                return true;
            }
            visited.push(current.clone());
            if let Some(info) = self.type_info_of(&current) {
                pending.extend(info.bases.iter().cloned());
            }
        }
        false
    }

    /// Whether the property `name` declared on `declaring_type` has a `set` accessor.
    ///
    /// Looked up rather than carried on [`MemberResolution::Property`], which records what a READ
    /// needs; assignability is a question only an assignment asks.
    /// Every `required` member of `ty` and of its base classes, as
    /// `(declaring type's simple name, member name)` -- the pair csc's `CS9035` renders as `B.P`,
    /// naming the type that DECLARED the member rather than the one being constructed.
    ///
    /// The walk is bounded against a malformed cyclic base, exactly as
    /// [`Self::base_property_match`]'s is, and stops at an unresolvable base: a type we cannot see
    /// might declare a required member, and inventing one would be a false diagnostic.
    fn required_members_in_chain(&self, ty: &TypeSymbol) -> Vec<(Box<str>, Box<str>)> {
        let mut found: Vec<(Box<str>, Box<str>)> = Vec::new();
        let mut visited: Vec<TypeSymbol> = Vec::new();
        let mut current = Some(ty.clone());
        while let Some(at) = current.take() {
            if visited.contains(&at) {
                break;
            }
            visited.push(at.clone());
            let Some(info) = self.model.get_by_symbol(&at) else {
                break;
            };
            for field in &info.fields {
                if field.is_required {
                    found.push((info.name.clone(), field.name.clone()));
                }
            }
            for property in &info.properties {
                if property.is_required && !property.is_override {
                    found.push((info.name.clone(), property.name.clone()));
                }
            }
            current = info.base.clone();
        }
        found
    }

    /// Whether `ty` or any base class declares a `required` member -- the condition for guarding a
    /// constructor with `[Obsolete(.., error: true)]` + `[CompilerFeatureRequired("RequiredMembers")]`.
    ///
    /// **THIS IS A DIFFERENT QUESTION FROM THE ONE THE TYPE-LEVEL `RequiredMemberAttribute`
    /// ANSWERS, and measuring csc is what separated them.** That marker goes on a type that
    /// DECLARES a required member; this guard goes on every constructor of a type that has one
    /// ANYWHERE IN ITS CHAIN. A derived type declaring nothing required carries no type-level
    /// marker and still carries the guard on its constructors -- symmetry gets that backwards, and
    /// the cost of getting it backwards is a down-level consumer constructing an object with unset
    /// required members.
    ///
    /// Shares [`Self::required_members_in_chain`] rather than re-walking, so the emitter and
    /// `CS9035` cannot disagree about which types have required members.
    #[must_use]
    pub fn has_required_members_in_chain(&self, ty: &TypeSymbol) -> bool {
        !self.required_members_in_chain(ty).is_empty()
    }

    /// `CS9035`: reports every `required` member of the type being created that the object
    /// initializer does not name -- one diagnostic per member, at the TYPE NAME of the `new`.
    ///
    /// **MEASURED, and it is the rule this feature exists for: assigning the member in the
    /// CONSTRUCTOR BODY does not satisfy it.** `class C { public required int P; public C() { P = 1; } }`
    /// still draws CS9035 at `new C()`. The only two ways to satisfy one are an object initializer
    /// and a constructor carrying `[SetsRequiredMembers]`, so this is a metadata rule rather than
    /// definite assignment. A struct's parameterless constructor is not an exception -- `new S()`
    /// draws it too.
    ///
    /// **THE `[SetsRequiredMembers]` EXEMPTION IS IMPLEMENTED AND IS NOT APPLIED HERE.** It is a
    /// property of the CHOSEN constructor, so the caller applies it after overload resolution and
    /// simply does not call this; see [`MethodSymbol::sets_required_members`].
    ///
    /// The other condition the caller applies is that an ABSTRACT target reports `CS0144` alone,
    /// measured: a creation that cannot happen does not also report what it failed to initialize.
    fn check_required_members_are_set(
        &mut self,
        target_ty: &TypeSymbol,
        initializer: Option<&Initializer>,
        span: Span,
    ) {
        let required = self.required_members_in_chain(target_ty);
        if required.is_empty() {
            return;
        }
        let named: Vec<&str> = match initializer {
            Some(Initializer::Object(members)) => {
                members.iter().map(|member| &*member.name).collect()
            }
            Some(Initializer::Collection(_)) | None => Vec::new(),
        };
        for (declaring, member) in required {
            if named.contains(&&*member) {
                continue;
            }
            self.diagnostics.push(Diagnostic::new(
                DiagnosticKind::RequiredMemberMustBeSet {
                    member: format!("{declaring}.{member}").into(),
                },
                span,
            ));
        }
    }

    /// Whether `name` names a `required` member of `ty` or of one of its bases.
    fn is_required_member(&self, ty: &TypeSymbol, name: &str) -> bool {
        self.required_members_in_chain(ty)
            .iter()
            .any(|(_, member)| &**member == name)
    }

    fn property_has_setter(&self, declaring_type: &TypeSymbol, name: &str) -> bool {
        self.type_info_of(declaring_type)
            .and_then(|info| info.find_property(name).map(|property| property.has_setter))
            .unwrap_or(true)
    }

    fn bind_object_creation(
        &mut self,
        target: &TypeRef,
        argument_exprs: &[Expr],
        initializer: Option<&Initializer>,
        span: Span,
    ) -> BoundExpr {
        let target_ty = self.resolve_named_type(&bind_type(target), target.span);
        let bound_initializer =
            initializer.map(|initializer| self.bind_initializer(&target_ty, initializer, span));
        let arguments: Vec<BoundExpr> = argument_exprs
            .iter()
            .map(|argument| self.bind_expression(argument))
            .collect();
        if self
            .type_info_of(&target_ty)
            .is_some_and(|info| info.kind == TypeKind::Delegate)
        {
            return self.bind_delegate_creation_new(&target_ty, arguments, span);
        }
        if let TypeSymbol::Named(parts) = &target_ty
            && let [name] = &parts[..]
            && let Some(constraints) = self.type_parameter_in_scope(name)
        {
            if !constraints.requires_default_constructor() {
                self.diagnostics.push(Diagnostic::new(
                    DiagnosticKind::CannotCreateVariableTypeInstance {
                        parameter: name.clone(),
                    },
                    span,
                ));
            }
        }
        if self
            .type_info_of(&target_ty)
            .is_some_and(|info| info.is_abstract)
        {
            self.diagnostics.push(Diagnostic::new(
                DiagnosticKind::CannotCreateAbstractInstance {
                    type_name: target_ty.to_string().into(),
                },
                span,
            ));
        }
        let has_method_group = arguments
            .iter()
            .any(|argument| matches!(argument.kind, BoundExprKind::MethodGroup { .. }));
        let real_error = arguments.iter().any(|argument| {
            argument.ty.is_error() && !matches!(argument.kind, BoundExprKind::MethodGroup { .. })
        });
        let mut constructor = None;
        let mut ctor_sets_required_members = false;
        let mut arguments = arguments;
        let ty = if target_ty.is_error() {
            TypeSymbol::Error
        } else {
            if !real_error {
                if arguments.is_empty() && self.is_value_type(&target_ty) {
                    constructor = Some(MethodReference {
                        declaring_instantiation: self
                            .declaring_instantiation_of(&target_ty, ".ctor", &[]),
                        declaring_type: target_ty.clone(),
                        name: ".ctor".into(),
                        parameters: Vec::new(),
                        return_type: TypeSymbol::Special(SpecialType::Void),
                        is_static: false,
                        is_vararg: false,
                        instantiation: None,
                    });
                } else if let Some(constructors) = self
                    .type_info_of(&target_ty)
                    .map(|info| info.constructors.clone())
                {
                    let constructors = self.accessible_overloads(&target_ty, &constructors);
                    let chosen = if has_method_group {
                        self.resolve_with_method_groups(
                            ".ctor",
                            &target_ty,
                            &constructors,
                            &arguments,
                            span,
                        )
                    } else {
                        let argument_types: Vec<TypeSymbol> =
                            arguments.iter().map(argument_type).collect();
                        let arg_constants: Vec<Option<i64>> =
                            arguments.iter().map(constant_int_value).collect();
                        self.check_constructor(
                            &target_ty,
                            &constructors,
                            &argument_types,
                            &arg_constants,
                            span,
                        )
                    };
                    if let Some(chosen) = chosen {
                        ctor_sets_required_members = chosen.sets_required_members;
                        let ctor_ref = MethodReference {
                            declaring_instantiation: self.declaring_instantiation_of(
                                &target_ty,
                                ".ctor",
                                &chosen.parameters,
                            ),
                            declaring_type: target_ty.clone(),
                            name: ".ctor".into(),
                            parameters: chosen.parameters.clone(),
                            return_type: TypeSymbol::Special(SpecialType::Void),
                            is_static: false,
                            is_vararg: chosen.is_vararg,
                            instantiation: None,
                        };
                        if chosen.is_params {
                            arguments =
                                self.bind_params_arguments(&ctor_ref, core::mem::take(&mut arguments));
                        } else if chosen.is_vararg
                            && chosen.parameters.len() + 1 == arguments.len()
                        {
                            let mut remaining = core::mem::take(&mut arguments).into_iter();
                            let mut bound = Vec::with_capacity(chosen.parameters.len() + 1);
                            for parameter in chosen.parameters.iter() {
                                if let Some(argument) = remaining.next() {
                                    bound.push(self.convert(argument, parameter));
                                }
                            }
                            bound.extend(remaining);
                            arguments = bound;
                        } else if chosen.parameters.len() == arguments.len() {
                            arguments = core::mem::take(&mut arguments)
                                .into_iter()
                                .zip(chosen.parameters.iter())
                                .map(|(argument, parameter)| self.convert(argument, parameter))
                                .collect();
                        }
                        constructor = Some(ctor_ref);
                    }
                }
            }
            target_ty
        };
        if !ctor_sets_required_members
            && !self
                .type_info_of(&ty)
                .is_some_and(|info| info.is_abstract)
        {
            self.check_required_members_are_set(&ty, initializer, target.span);
        }
        BoundExpr {
            kind: BoundExprKind::ObjectCreation {
                arguments,
                constructor,
                initializer: bound_initializer,
            },
            ty,
        }
    }

    /// Binds a `new D(...)` whose target is a delegate type (14.5.10.3): a method-group argument
    /// converts as in [`Self::bind_delegate_creation`]; a VALUE of a delegate type whose `Invoke`
    /// matches `D`'s creates a delegate to that value's invocation list -- the operand becomes the
    /// receiver and its own `Invoke` the target, which the emitter already lowers as
    /// `dup; ldvirtftn Invoke; newobj`. The delegate-value form lives HERE and not in the pure
    /// binder because it exists only in a delegate-creation-expression -- there is no implicit
    /// conversion between distinct delegate types, so the conversion probes must not see it.
    ///
    /// When neither form applies, this reports the csc-matching diagnostic (all probed):
    /// `CS1729` for no arguments, `CS0149` for an argument that is neither form (or extras after
    /// it), `CS0123` for a method group or delegate value with no signature match.
    fn bind_delegate_creation_new(
        &mut self,
        delegate_ty: &TypeSymbol,
        arguments: Vec<BoundExpr>,
        span: Span,
    ) -> BoundExpr {
        let bound = self.bind_delegate_creation(delegate_ty, &arguments, span);
        if matches!(bound.kind, BoundExprKind::DelegateCreation { .. }) {
            return bound;
        }
        if let [argument] = &arguments[..] {
            if matches!(argument.kind, BoundExprKind::Ref { .. }) {
                self.report(Diagnostic::new(DiagnosticKind::MethodNameExpected, span));
                return bound;
            }
        }
        let invoke_of = |binder: &Self, ty: &TypeSymbol| {
            binder
                .type_info_of(ty)
                .filter(|info| info.kind == TypeKind::Delegate)
                .and_then(|info| info.methods.iter().find(|m| &*m.name == "Invoke").cloned())
        };
        if let ([argument], Some(target_invoke)) = (&arguments[..], invoke_of(self, delegate_ty)) {
            if let Some(operand_invoke) = invoke_of(self, &argument.ty) {
                if operand_invoke.parameters == target_invoke.parameters
                    && operand_invoke.return_type == target_invoke.return_type
                {
                    let operand_ty = self.resolve_type(&argument.ty);
                    return BoundExpr {
                        kind: BoundExprKind::DelegateCreation {
                            delegate_type: delegate_ty.clone(),
                            target: MethodReference {
                                declaring_instantiation: self.declaring_instantiation_of(
                                    &operand_ty,
                                    &operand_invoke.name,
                                    &operand_invoke.parameters,
                                ),
                                declaring_type: operand_ty,
                                name: operand_invoke.name.clone(),
                                parameters: operand_invoke.parameters.clone(),
                                return_type: operand_invoke.return_type,
                                is_static: false,
                                is_vararg: false,
                                instantiation: None,
                            },
                            receiver: Some(Box::new(argument.clone())),
                        },
                        ty: delegate_ty.clone(),
                    };
                }
                self.report(Diagnostic::new(
                    DiagnosticKind::NoOverloadMatchesDelegate {
                        method: alloc::format!("{}.Invoke", argument.ty).into(),
                        delegate: delegate_ty.to_string().into(),
                    },
                    span,
                ));
                return bound;
            }
        }
        let kind = match &arguments[..] {
            [] => DiagnosticKind::NoConstructor {
                type_name: delegate_ty.to_string().into(),
                count: 0,
            },
            [argument] => match &argument.kind {
                BoundExprKind::MethodGroup { name, .. } => {
                    DiagnosticKind::NoOverloadMatchesDelegate {
                        method: name.clone(),
                        delegate: delegate_ty.to_string().into(),
                    }
                }
                _ if argument.ty.is_error() => return bound,
                _ => DiagnosticKind::MethodNameExpected,
            },
            _ => DiagnosticKind::MethodNameExpected,
        };
        self.report(Diagnostic::new(kind, span));
        bound
    }

    /// Binds `new D(methodGroup)`: the method group converts to delegate `D` when a
    /// method named in it matches `D`'s `Invoke` signature (same parameters and return).
    /// A static target carries no receiver; an instance target keeps its receiver.
    fn bind_delegate_creation(
        &self,
        delegate_ty: &TypeSymbol,
        arguments: &[BoundExpr],
        _span: Span,
    ) -> BoundExpr {
        let recover = BoundExpr {
            kind: BoundExprKind::ObjectCreation {
                arguments: Vec::new(),
                constructor: None,
                initializer: None,
            },
            ty: delegate_ty.clone(),
        };
        let Some(invoke) = self
            .type_info_of(delegate_ty)
            .and_then(|info| info.methods.iter().find(|m| &*m.name == "Invoke").cloned())
        else {
            return recover;
        };
        let [argument] = arguments else {
            return recover;
        };
        let BoundExprKind::MethodGroup { receiver, name } = &argument.kind else {
            return recover;
        };
        let receiver_ty = receiver.ty.clone();
        let Some(target) = self
            .methods_in_chain(&receiver_ty, name)
            .into_iter()
            .find(|m| m.parameters == invoke.parameters && m.return_type == invoke.return_type)
        else {
            return recover;
        };
        let declaring = self.declaring_type_in_chain(&receiver_ty, name, &target.parameters);
        let bound_receiver = if target.is_static {
            None
        } else {
            Some(receiver.clone())
        };
        BoundExpr {
            kind: BoundExprKind::DelegateCreation {
                delegate_type: delegate_ty.clone(),
                target: MethodReference {
                    declaring_instantiation: self.declaring_instantiation_of(
                        &declaring,
                        &target.name,
                        &target.parameters,
                    ),
                    declaring_type: declaring,
                    is_vararg: target.is_vararg,
                    name: target.name.clone(),
                    parameters: target.parameters.clone(),
                    return_type: target.return_type,
                    is_static: target.is_static,
                    instantiation: None,
                },
                receiver: bound_receiver,
            },
            ty: delegate_ty.clone(),
        }
    }

    /// Filters overload candidates to those accessible from the current context (7.4): an
    /// inaccessible member does not participate in overload resolution, so it cannot shadow an
    /// accessible one. If EVERY candidate is inaccessible, all are returned so the caller still
    /// reports its normal failure rather than silently finding nothing.
    fn accessible_overloads(
        &self,
        declaring: &TypeSymbol,
        candidates: &[MethodSymbol],
    ) -> Vec<MethodSymbol> {
        let accessible: Vec<MethodSymbol> = candidates
            .iter()
            .filter(|candidate| self.is_accessible(declaring, candidate.accessibility))
            .cloned()
            .collect();
        if accessible.is_empty() {
            candidates.to_vec()
        } else {
            accessible
        }
    }

    /// Resolves `new T(args)` against `T`'s constructors, reporting the diagnostic
    /// for a failed resolution. The created type is the result regardless.
    fn check_constructor(
        &mut self,
        target: &TypeSymbol,
        constructors: &[MethodSymbol],
        argument_types: &[TypeSymbol],
        arg_constants: &[Option<i64>],
        span: Span,
    ) -> Option<MethodSymbol> {
        match resolve_overload(&self.model, constructors, argument_types, arg_constants) {
            OverloadResult::Resolved(constructor) => return Some(constructor),
            OverloadResult::WrongArgumentCount => {
                if let Some(vararg) = constructors
                    .iter()
                    .find(|c| c.is_vararg && c.parameters.len() == argument_types.len())
                {
                    self.diagnostics.push(Diagnostic::new(
                        DiagnosticKind::NoArgumentForArglist {
                            method: vararg_member_display(target, ".ctor", &vararg.parameters),
                        },
                        span,
                    ));
                    return None;
                }
                if let [only] = constructors {
                    if argument_types.len() < only.parameters.len() {
                        self.diagnostics.push(Diagnostic::new(
                            DiagnosticKind::MissingArgumentForParameter {
                                parameter: only
                                    .parameter_name(argument_types.len())
                                    .unwrap_or("value")
                                    .into(),
                                method: qualified_method_with_modes(
                                    target,
                                    &simple_type_name(target),
                                    only,
                                ),
                            },
                            span,
                        ));
                        return None;
                    }
                }
                self.diagnostics.push(Diagnostic::new(
                    DiagnosticKind::NoConstructor {
                        type_name: target.to_string().into(),
                        count: argument_types.len() as u32,
                    },
                    span,
                ))
            }
            OverloadResult::BadArgument { index, from, to } => {
                self.diagnostics.push(Diagnostic::new(
                    DiagnosticKind::ArgumentConversion {
                        index: index as u32 + 1,
                        from: from.to_string().into(),
                        to: to.to_string().into(),
                    },
                    span,
                ));
            }
            OverloadResult::Ambiguous => self.diagnostics.push(Diagnostic::new(
                DiagnosticKind::AmbiguousCall {
                    method: target.to_string().into(),
                },
                span,
            )),
        }
        None
    }

    /// Whether a member of `declaring` with this accessibility is reachable from the current
    /// context (10.5.1). `private` is the declaring type and any type nested in it;
    /// `protected` adds the types derived from the declaring type. `internal` and
    /// `protected internal` are treated as accessible: a same-assembly access IS allowed,
    /// and lcsc does not distinguish a reference assembly's internal members (rarely named)
    /// -- cross-assembly internal enforcement is a known gap.
    fn is_accessible(&self, declaring: &TypeSymbol, accessibility: Accessibility) -> bool {
        let Some(current) = self.current_type.clone() else {
            return match accessibility {
                Accessibility::Public => true,
                Accessibility::Internal | Accessibility::ProtectedInternal => {
                    !self.type_is_external(declaring)
                }
                Accessibility::Private | Accessibility::Protected => false,
            };
        };
        self.accessible_from(&current, declaring, accessibility)
    }

    /// Whether a member of `declaring` with the given accessibility is accessible from the
    /// program text of `from` -- `is_accessible` generalized past the current bind context, for
    /// checks that name their vantage type explicitly (an override target is judged from the
    /// DERIVING class, whatever is being bound at the time).
    fn accessible_from(
        &self,
        from: &TypeSymbol,
        declaring: &TypeSymbol,
        accessibility: Accessibility,
    ) -> bool {
        let declaring = &definition_of(declaring);
        match accessibility {
            Accessibility::Public => true,
            Accessibility::Internal => !self.type_is_external(declaring),
            Accessibility::ProtectedInternal => {
                !self.type_is_external(declaring)
                    || self.protected_vantage(from, declaring).is_some()
            }
            Accessibility::Private => self.within_private_scope(from, declaring),
            Accessibility::Protected => {
                self.within_private_scope(from, declaring)
                    || self.protected_vantage(from, declaring).is_some()
            }
        }
    }

    /// The innermost type at or enclosing `from` that derives from `declaring` -- the class whose
    /// derivation grants access to `declaring`'s `protected` members, and the class a qualifier
    /// must therefore be an instance of (10.5.3). `None` when no enclosing type derives, so the
    /// member is simply out of reach.
    ///
    /// THE ENCLOSING WALK IS THE POINT, not a refinement. A nested type is written inside its
    /// enclosing class's program text, so it reaches that class's inherited protected members
    /// exactly as the class itself does. Asking only whether `from` derives refused correct code
    /// the moment an access was written in a nested helper -- `class D : B { class N { ... } }`
    /// drew CS0122 where csc compiles it -- and the walk is transitive because the nesting can be.
    fn protected_vantage(&self, from: &TypeSymbol, declaring: &TypeSymbol) -> Option<TypeSymbol> {
        let mut current = Some(from.clone());
        while let Some(ty) = current {
            if self.derives_from(&ty, declaring) {
                return Some(ty);
            }
            let enclosing = self
                .type_info_of(&ty)
                .and_then(|info| info.enclosing.clone());
            current = enclosing.map(|name| named_symbol_from_dotted(&name));
        }
        None
    }

    /// Whether `ty` comes from a referenced assembly (so its `internal` members are not
    /// accessible from the unit being compiled).
    fn type_is_external(&self, ty: &TypeSymbol) -> bool {
        self.type_info_of(ty).is_some_and(|info| info.is_external)
    }

    /// Whether the current type is `declaring` or a type nested (at any depth) within it --
    /// the scope a `private` member is accessible from (10.5.1).
    fn in_private_scope_of(&self, declaring: &TypeSymbol) -> bool {
        self.current_type
            .as_ref()
            .is_some_and(|current| self.within_private_scope(current, declaring))
    }

    /// Whether `from` is `declaring` or a type nested (at any depth) within it.
    fn within_private_scope(&self, from: &TypeSymbol, declaring: &TypeSymbol) -> bool {
        if from == declaring || fold_primitive_name(from) == fold_primitive_name(declaring) {
            return true;
        }
        let declaring_name = declaring.to_string();
        let mut info = self.type_info_of(from);
        while let Some(type_info) = info {
            match type_info.enclosing.as_deref() {
                None => return false,
                Some(enclosing) if enclosing == declaring_name => return true,
                Some(enclosing) => info = self.type_info_of(&named_symbol_from_dotted(enclosing)),
            }
        }
        false
    }

    /// Whether `from` derives (directly or transitively) from `declaring`.
    fn derives_from(&self, from: &TypeSymbol, declaring: &TypeSymbol) -> bool {
        let mut info = self.type_info_of(from);
        while let Some(base) = info.and_then(|type_info| type_info.base.clone()) {
            if &base == declaring {
                return true;
            }
            info = self.type_info_of(&base);
        }
        false
    }

    /// Reports `CS0122` when a member is not accessible from the current context.
    fn check_accessible(
        &mut self,
        declaring: &TypeSymbol,
        accessibility: Accessibility,
        member: &str,
        span: Span,
    ) {
        if !self.is_accessible(declaring, accessibility) {
            let mut qualified = declaring.to_string();
            qualified.push('.');
            qualified.push_str(member);
            self.diagnostics.push(Diagnostic::new(
                DiagnosticKind::Inaccessible {
                    member: qualified.into(),
                },
                span,
            ));
        }
    }

    /// Reports `CS1540` when a `protected` INSTANCE member is reached from a derived class through
    /// a qualifier whose type is not that class or one derived from it (10.5.3). Deriving from `B`
    /// lets `D` touch `B`'s protected members *on a `D`* -- not on an arbitrary `B`, which may be
    /// some unrelated subclass's instance. Runs only where [`Self::check_accessible`] found the
    /// member reachable, so the two never both fire.
    ///
    /// EVERY SILENCE HERE IS MEASURED AGAINST csc, and each one is load-bearing -- this rule makes
    /// lcsc stricter, so a missing exemption refuses correct code:
    /// - `this.m` / `base.m` and an unqualified `m` are not qualified accesses at all;
    /// - a STATIC protected member named through its type is a different rule, not this one;
    /// - inside the declaring type's own program text the member is in private scope and the
    ///   qualifier is unconstrained (`B` itself may reach `d.m` through any `B`);
    /// - `protected internal` reached from the SAME assembly is accessible AS INTERNAL, so the
    ///   protected clause never governs it. That is the exemption most likely to bite: the two
    ///   spellings are mutually exclusive across an assembly boundary, and same-assembly code
    ///   spells it `protected internal` precisely to be reachable without derivation.
    ///
    /// The message names `current_type` -- for an access written in a nested class, the NESTED
    /// type, though a qualifier of the ENCLOSING class is what actually satisfies the rule. That
    /// is csc's own wording and it was measured rather than inferred: `class D : B { class N { ...
    /// } }` reports "must be of type 'D.N'" while accepting a `D` qualifier.
    ///
    /// TWO LIMITS, both measured and neither papered over. An OVERLOADED protected method is named
    /// differently: csc reports during member LOOKUP and so names the FIRST-DECLARED overload,
    /// while this reports after resolution and names the one the call actually binds to. Matching
    /// would mean quoting a signature the call does not use, so the divergence is deliberate.
    /// And a protected INDEXER is not covered at all -- `resolve_indexer_accessor` returns a
    /// `MethodReference`, which carries no accessibility, so the check has nothing to test. That
    /// is an UNDER-report: it accepts what csc rejects, which blocks nobody and reads as a gap
    /// rather than as a compiler that refuses correct code.
    fn check_protected_qualifier(
        &mut self,
        declaring: &TypeSymbol,
        accessibility: Accessibility,
        is_static: bool,
        receiver: Receiver,
        qualifier_ty: &TypeSymbol,
        member: &str,
        parameters: Option<&[TypeSymbol]>,
        span: Span,
    ) {
        if is_static || !matches!(receiver, Receiver::Instance) {
            return;
        }
        match accessibility {
            Accessibility::Protected => {}
            Accessibility::ProtectedInternal if self.type_is_external(declaring) => {}
            _ => return,
        }
        let Some(current) = self.current_type.clone() else {
            return;
        };
        if self.within_private_scope(&current, declaring) {
            return;
        }
        let Some(vantage) = self.protected_vantage(&current, declaring) else {
            return;
        };
        if qualifier_ty == &vantage || self.derives_from(qualifier_ty, &vantage) {
            return;
        }
        self.report(Diagnostic::new(
            DiagnosticKind::ProtectedQualifier {
                member: match parameters {
                    Some(parameters) => qualified_method(declaring, member, parameters),
                    None => qualified_member(declaring, member),
                },
                qualifier: qualifier_ty.to_string().into(),
                accessing: current.to_string().into(),
            },
            span,
        ));
    }

    /// How a named attribute argument's name resolves against the attribute class -- which decides
    /// which of three diagnostics it draws. csc separates them because the repairs differ: rename
    /// it, widen it, or pick a different member.
    ///
    /// THE SPLIT IS MEASURED, and it does not follow the CS0617 message. That message says named
    /// arguments must be "fields which are not readonly, static, or const, or read-write properties
    /// which are public and not static" -- it mentions `public` only for properties. csc requires
    /// it of fields too: an `internal` field draws CS0617, while a `private` or `protected` one
    /// draws CS0122 instead, because that one is not reachable at all. Reachable-but-unusable and
    /// unreachable are different answers, so they are different variants here.
    pub(crate) fn named_attribute_argument_target(
        &self,
        attribute_type: &TypeSymbol,
        name: &str,
    ) -> NamedArgumentTarget {
        let mut current = Some(attribute_type.clone());
        while let Some(ty) = current {
            let Some(info) = self.type_info_of(&ty) else {
                return NamedArgumentTarget::Missing;
            };
            if let Some(field) = info.fields.iter().find(|field| &*field.name == name) {
                return if !self.is_accessible(&ty, field.accessibility) {
                    NamedArgumentTarget::Inaccessible(ty.clone())
                } else if field.accessibility == Accessibility::Public
                    && !field.is_static
                    && !field.is_readonly
                    && field.constant.is_none()
                {
                    NamedArgumentTarget::Valid(ty.clone())
                } else {
                    NamedArgumentTarget::NotAValidTarget(ty.clone())
                };
            }
            if let Some(property) = info.properties.iter().find(|p| &*p.name == name) {
                return if !self.is_accessible(&ty, property.accessibility) {
                    NamedArgumentTarget::Inaccessible(ty.clone())
                } else if property.accessibility == Accessibility::Public
                    && !property.is_static
                    && property.has_getter
                    && property.has_setter
                {
                    NamedArgumentTarget::Valid(ty.clone())
                } else {
                    NamedArgumentTarget::NotAValidTarget(ty.clone())
                };
            }
            if info.methods.iter().any(|method| &*method.name == name) {
                return NamedArgumentTarget::NotAValidTarget(ty.clone());
            }
            if self.model.get(&ty.to_string(), name).is_some() {
                return NamedArgumentTarget::NotAValidTarget(ty.clone());
            }
            current = self.type_info_of(&ty).and_then(|info| info.base.clone());
        }
        NamedArgumentTarget::Missing
    }

    /// Whether the method currently being bound is a `static` method with no `this` --
    /// so an unqualified instance member (read through an implicit `this`) is `CS0120`,
    /// and the `this` keyword itself is `CS0026`. A REPL submission is exempt: its
    /// `Submit$N` is static but reaches session members through the `s` parameter (not a
    /// `this`), so `session_receiver` standing in for `this` makes those accesses legal.
    fn in_static_method(&self) -> bool {
        self.session_receiver.is_none()
            && self.current_method.as_ref().is_some_and(|method| method.is_static)
    }

    /// Reports `CS0120` for an instance member reached with no object -- an implicit `this`
    /// in a static method, where there is none. The member is rendered qualified by its
    /// declaring type (`C.x`), with `()` appended for a method (`C.Foo()`), matching csc.
    fn report_no_object_reference(
        &mut self,
        declaring: &TypeSymbol,
        member: &str,
        is_method: bool,
        span: Span,
    ) {
        let mut qualified = declaring.to_string();
        qualified.push('.');
        qualified.push_str(member);
        if is_method {
            qualified.push_str("()");
        }
        self.report(Diagnostic::new(
            DiagnosticKind::ObjectReferenceRequired {
                member: qualified.into(),
            },
            span,
        ));
    }

    /// Reports `CS0236` for an instance member reached by a field initializer through an implicit
    /// `this` -- a field initializer runs with no instance, so a non-static field, method, or
    /// property of the containing type is unreachable (17.4.5). The member is rendered qualified by
    /// its declaring type (`C.first`), with `()` appended for a method (`C.M()`), matching csc.
    fn report_field_initializer_reference(
        &mut self,
        declaring: &TypeSymbol,
        member: &str,
        is_method: bool,
        span: Span,
    ) {
        let mut qualified = declaring.to_string();
        qualified.push('.');
        qualified.push_str(member);
        if is_method {
            qualified.push_str("()");
        }
        self.report(Diagnostic::new(
            DiagnosticKind::FieldInitializerReference {
                member: qualified.into(),
            },
            span,
        ));
    }

    /// Reports the static/instance mismatch of accessing a member through `receiver`
    /// (`CS0120` for an instance member named through a type, `CS0176` for a static
    /// member through an instance). An access through `this`/`base` is exempt.
    fn check_static_instance(
        &mut self,
        receiver: Receiver,
        is_static: bool,
        declaring: &TypeSymbol,
        member: &str,
        span: Span,
    ) {
        let kind = match receiver {
            Receiver::ViaType if !is_static => DiagnosticKind::ObjectReferenceRequired {
                member: qualified_member(declaring, member),
            },
            Receiver::Instance if is_static => DiagnosticKind::StaticMemberViaInstance {
                member: qualified_member(declaring, member),
            },
            _ => return,
        };
        self.diagnostics.push(Diagnostic::new(kind, span));
    }

    /// Looks a member up on a type, walking the base-class chain (14.3, 14.5.4).
    /// If `name` is a field-like event reachable on `ty` (itself or a base), returns the
    /// event and the symbol of the type that declares it. `+=`/`-=` route through its
    /// accessors from outside that type (17.7), and any other use there is CS0070.
    fn event_declaration(&self, ty: &TypeSymbol, name: &str) -> Option<(EventSymbol, TypeSymbol)> {
        let lookup = self.lookup_type_of(ty);
        let mut current = self.type_info_of(&lookup);
        while let Some(info) = current {
            if let Some(event) = info.find_event(name) {
                return Some((event.clone(), type_symbol_in(&info.namespace, &info.name)));
            }
            current = info.base.as_ref().and_then(|base| self.type_info_of(base));
        }
        None
    }

    /// Whether code currently being bound is outside the type that declares `event_owner`
    /// (so `+=`/`-=` must route through accessors and other uses are CS0070).
    fn outside_event_declarer(&self, declaring: &TypeSymbol) -> bool {
        !self.in_private_scope_of(declaring)
    }

    pub(crate) fn resolve_member(&self, ty: &TypeSymbol, name: &str) -> MemberResolution {
        let lookup = self.lookup_type_of(ty);
        let Some(is_interface) = self
            .type_info_of(&lookup)
            .map(|info| info.kind == TypeKind::Interface)
        else {
            return MemberResolution::Unknown;
        };
        let mut visited: Vec<TypeSymbol> = Vec::new();
        let mut pending = alloc::vec![lookup.clone()];
        if is_interface {
            pending.insert(0, type_symbol_in("System", "Object"));
        }
        let mut inaccessible: Option<MemberResolution> = None;
        while let Some(current_ty) = pending.pop() {
            if visited.contains(&current_ty) {
                continue;
            }
            visited.push(current_ty.clone());
            let Some(info) = self.type_info_of(&current_ty) else {
                continue;
            };
            let declaring = type_symbol_in(&info.namespace, &info.name);
            let named = member_declaring_type(&current_ty, &info);
            let (resolution, accessible) = if let Some(field) = info.find_field(name) {
                (
                    MemberResolution::Field(FieldReference {
                        declaring_instantiation: self
                            .field_instantiation_of(&named, &field.name),
                        declaring_type: named.clone(),
                        name: field.name.clone(),
                        ty: self.resolve_type(&field.ty),
                        is_static: field.is_static,
                        is_readonly: field.is_readonly,
                        is_volatile: field.is_volatile,
                        accessibility: field.accessibility,
                        constant: field.constant.clone(),
                    }),
                    self.is_accessible(&declaring, field.accessibility),
                )
            } else if let Some(property) = info.find_property(name) {
                (
                    MemberResolution::Property {
                        declaring_type: named.clone(),
                        ty: self.resolve_type(&property.ty),
                        accessibility: property.accessibility,
                        is_static: property.is_static,
                    },
                    self.is_accessible(&declaring, property.accessibility),
                )
            } else if info.methods_named(name).next().is_some() {
                let any_accessible = info
                    .methods_named(name)
                    .any(|method| self.is_accessible(&declaring, method.accessibility));
                (MemberResolution::MethodGroup, any_accessible)
            } else {
                for base in member_lookup_bases(&info) {
                    pending.push(base);
                }
                continue;
            };
            if accessible {
                return resolution;
            }
            inaccessible.get_or_insert(resolution);
            for base in member_lookup_bases(&info) {
                pending.push(base);
            }
        }
        inaccessible.unwrap_or(MemberResolution::NoSuchMember(ty.to_string()))
    }

    /// Resolves a simple name against the STATIC members of the current type's enclosing types
    /// (14.5.2): a nested type sees its enclosing types' static fields, constants, and properties
    /// by simple name, at any depth. Instance members of an enclosing type are not in scope (there
    /// is no enclosing-instance `this`). `None` when no enclosing type provides the name.
    fn resolve_enclosing_static(&self, name: &str) -> Option<BoundExpr> {
        let enclosing_of = |ty: &TypeSymbol| {
            self.type_info_of(ty)
                .and_then(|info| info.enclosing.clone())
                .map(|enclosing| named_symbol_from_dotted(&enclosing))
        };
        let mut enclosing = self.current_type.as_ref().and_then(enclosing_of);
        while let Some(ty) = enclosing {
            let type_reference = || {
                Box::new(BoundExpr {
                    kind: BoundExprKind::TypeReference(ty.clone()),
                    ty: ty.clone(),
                })
            };
            match self.resolve_member(&ty, name) {
                MemberResolution::Field(field) if field.is_static => {
                    return Some(BoundExpr {
                        ty: field.ty.clone(),
                        kind: BoundExprKind::FieldAccess {
                            receiver: type_reference(),
                            name: name.into(),
                            field: Some(field),
                        },
                    });
                }
                MemberResolution::Property {
                    ty: property_ty,
                    is_static: true,
                    ..
                } => {
                    let (getter_declaring, setter_declaring) =
                        self.property_accessor_declarers(&ty, name);
                    let (getter_instantiation, setter_instantiation) = self
                        .property_accessor_instantiations(
                            &getter_declaring,
                            &setter_declaring,
                            name,
                            &property_ty,
                        );
                    return Some(BoundExpr {
                        kind: BoundExprKind::PropertyAccess {
                            receiver: type_reference(),
                            declaring_type: getter_declaring,
                            setter_declaring_type: setter_declaring,
                            getter_instantiation,
                            setter_instantiation,
                            name: name.into(),
                        },
                        ty: property_ty,
                    });
                }
                _ => {}
            }
            enclosing = enclosing_of(&ty);
        }
        None
    }

    /// The model entry for a named type, if any. Owned for an instantiation, borrowed otherwise --
    /// see [`crate::symbols::Model::get_by_symbol`].
    fn type_info_of(&self, ty: &TypeSymbol) -> Option<alloc::borrow::Cow<'_, TypeInfo>> {
        self.model.get_by_symbol(ty)
    }

    /// Whether `ty` is a delegate type (its values are invocable via `Invoke`).
    fn is_delegate_type(&self, ty: &TypeSymbol) -> bool {
        self.type_info_of(ty)
            .is_some_and(|info| info.kind == TypeKind::Delegate)
    }

    /// Whether `expr` is a delegate-typed VALUE (so `expr(args)` means `expr.Invoke(args)`)
    /// rather than a method group, type, or namespace.
    fn is_delegate_value(&self, expr: &BoundExpr) -> bool {
        !matches!(
            expr.kind,
            BoundExprKind::MethodGroup { .. }
                | BoundExprKind::TypeReference(_)
                | BoundExprKind::NamespaceReference(_)
        ) && self
            .type_info_of(&expr.ty)
            .is_some_and(|info| info.kind == TypeKind::Delegate)
    }

    /// Whether the field `name` declared by `declaring` is `readonly` (CS0191).
    fn field_is_readonly(&self, declaring: &TypeSymbol, name: &str) -> bool {
        self.type_info_of(declaring)
            .and_then(|info| {
                info.fields
                    .iter()
                    .find(|field| &*field.name == name)
                    .map(|field| field.is_readonly)
            })
            .unwrap_or(false)
    }

    /// Reports the IMPLICIT `: base()` that no base constructor can accept -- `CS7036` naming the
    /// first unsupplied parameter when the base declares exactly one constructor, `CS1729` when it
    /// declares several. Both measured against csc.
    ///
    /// This is the quietest shape in the family, because nothing in the source mentions the call:
    /// `class D : B { public D() { } }` where `B` only has `B(int)` names no base call, and neither
    /// does a `D` declaring NO constructor at all, whose synthesized default carries the same
    /// implicit one. The explicit `: base(...)` spelling was silent for a
    /// DIFFERENT reason -- `bind_constructor_chain` resolves it from the emitter, where a
    /// diagnostic reaches no one -- so its arity is checked here too, and the two spellings answer
    /// with one rule instead of two.
    ///
    /// A constructor chaining to `: this(...)` does not call the base and is skipped. Only ARITY is
    /// checked here: an inaccessible base constructor is `CS0122`, a separate rule, and reporting
    /// this one in its place would be a wrong code on a real error.
    pub(crate) fn check_base_constructor_call(
        &mut self,
        class_ty: &TypeSymbol,
        declaration: &lamella_syntax::ast::TypeDecl,
    ) {
        if !matches!(declaration.kind, lamella_syntax::ast::TypeKind::Class) {
            return;
        }
        let Some(base) = self.type_info_of(class_ty).and_then(|info| info.base.clone()) else {
            return;
        };
        let Some(base_info) = self.type_info_of(&base) else {
            return;
        };
        let constructors = base_info.constructors.clone();
        if constructors.is_empty() {
            return;
        }
        let mut sites: Vec<(Span, usize)> = Vec::new();
        let mut declared_any = false;
        for member in &declaration.members {
            if let lamella_syntax::ast::Member::Constructor {
                modifiers,
                initializer,
                span,
                ..
            } = member
            {
                if modifiers
                    .iter()
                    .any(|m| matches!(m, lamella_syntax::ast::Modifier::Static))
                {
                    continue;
                }
                declared_any = true;
                match initializer {
                    Some(init)
                        if matches!(
                            init.kind,
                            lamella_syntax::ast::ConstructorInitializerKind::This
                        ) => {}
                    Some(init) => sites.push((*span, init.arguments.len())),
                    None => sites.push((*span, 0)),
                }
            }
        }
        if !declared_any {
            sites.push((declaration.span, 0));
        }
        for (span, argc) in sites {
            if constructors.iter().any(|constructor| {
                constructor.parameters.len() == argc
                    || (constructor.is_params && argc + 1 >= constructor.parameters.len())
            }) {
                continue;
            }
            let diagnostic = match constructors.as_slice() {
                [only] if argc < only.parameters.len() => {
                    DiagnosticKind::MissingArgumentForParameter {
                        parameter: only.parameter_name(argc).unwrap_or("value").into(),
                        method: qualified_method_with_modes(
                            &base,
                            &simple_type_name(&base),
                            only,
                        ),
                    }
                }
                _ => DiagnosticKind::NoConstructor {
                    type_name: base.to_string().into(),
                    count: argc as u32,
                },
            };
            self.diagnostics.push(Diagnostic::new(diagnostic, span));
        }
    }

    /// Reports `CS0535` for each interface member a concrete class/struct does not
    /// implement. An abstract class (or an interface/enum) is exempt.
    pub(crate) fn check_interface_implementations(
        &mut self,
        class_ty: &TypeSymbol,
        declaration: &lamella_syntax::ast::TypeDecl,
    ) {
        if declaration
            .modifiers
            .iter()
            .any(|modifier| matches!(modifier, lamella_syntax::ast::Modifier::Abstract))
        {
            return;
        }
        let concrete = self
            .model
            .get_by_symbol(class_ty)
            .is_some_and(|info| matches!(info.kind, TypeKind::Class | TypeKind::Struct));
        if !concrete {
            return;
        }
        for interface in self.transitive_interfaces(class_ty) {
            let (members, properties) = match self.model.get_by_symbol(&interface) {
                Some(info) => (info.methods.clone(), info.properties.clone()),
                None => continue,
            };
            let interface_name = dotted_type_name(&interface);
            for property in &properties {
                let declared_all: Vec<PropertySymbol> = self
                    .model
                    .get_by_symbol(class_ty)
                    .map(|info| {
                        info.properties
                            .iter()
                            .filter(|candidate| candidate.name == property.name)
                            .cloned()
                            .collect()
                    })
                    .unwrap_or_default();
                if declared_all.iter().any(|candidate| {
                    self.normalize_for_signature(&candidate.ty)
                        == self.normalize_for_signature(&property.ty)
                }) {
                    continue;
                }
                let Some(declared) = declared_all.into_iter().next() else {
                    continue;
                };
                let member = alloc::format!("{}.{}", declaration.name, property.name).into();
                let interface_member =
                    alloc::format!("{interface_name}.{}", property.name).into();
                if declared.accessibility != Accessibility::Public {
                    continue;
                }
                if self.normalize_for_signature(&declared.ty)
                    != self.normalize_for_signature(&property.ty)
                {
                    self.diagnostics.push(Diagnostic::new(
                        DiagnosticKind::InterfaceImplementationReturnType {
                            type_name: declaration.name.clone(),
                            interface_member,
                            member,
                            return_type: self
                                .normalize_for_signature(&property.ty)
                                .to_string()
                                .into(),
                        },
                        declaration.span,
                    ));
                }
            }
            for member in &members {
                let status = self.interface_member_status(class_ty, &interface, member);
                if status == InterfaceMemberStatus::Implemented {
                    continue;
                }
                let interface_member =
                    abstract_member_signature(&interface_name, &member.name, &member.parameters);
                let kind = match status {
                    InterfaceMemberStatus::Implemented => continue,
                    InterfaceMemberStatus::Missing => {
                        let mut member_name = interface_name.clone();
                        member_name.push('.');
                        member_name.push_str(&member.name);
                        DiagnosticKind::InterfaceMemberNotImplemented {
                            type_name: declaration.name.clone(),
                            member: member_name.into(),
                        }
                    }
                    InterfaceMemberStatus::NotPublic => {
                        DiagnosticKind::InterfaceImplementationNotPublic {
                            type_name: declaration.name.clone(),
                            interface_member,
                            member: abstract_member_signature(
                                &declaration.name,
                                &member.name,
                                &member.parameters,
                            ),
                        }
                    }
                    InterfaceMemberStatus::WrongReturnType => {
                        DiagnosticKind::InterfaceImplementationReturnType {
                            type_name: declaration.name.clone(),
                            interface_member,
                            member: abstract_member_signature(
                                &declaration.name,
                                &member.name,
                                &member.parameters,
                            ),
                            return_type: self
                                .normalize_for_signature(&member.return_type)
                                .to_string()
                                .into(),
                        }
                    }
                };
                self.diagnostics
                    .push(Diagnostic::new(kind, declaration.span));
            }
        }
    }

    /// Reports `CS0115` for each source `override` method in `declaration` whose name and
    /// parameter types match no method up the base *class* chain (including the implicit
    /// `System.Object` root every type derives from). The check keys on the EXISTENCE of a
    /// same-signature base member, not on its `virtual`/`abstract`/`override`-ness: a base
    /// member that exists but is not overridable is `CS0506`, and one that matches but for the
    /// return type is `CS0508` -- both are left to those rules, so this stays a strict subset of
    /// csc. Interfaces are not override targets (implementing one is `CS0535`), so only the
    /// `base` chain is walked. Conservative: if the chain cannot be fully resolved (an unknown
    /// base type, or no model `System.Object`), nothing is reported -- the target might be there.
    pub(crate) fn check_overrides_have_base(
        &mut self,
        class_ty: &TypeSymbol,
        declaration: &lamella_syntax::ast::TypeDecl,
    ) {
        for member in &declaration.members {
            let lamella_syntax::ast::Member::Method {
                modifiers,
                name,
                parameters,
                return_type,
                explicit_interface: None,
                span,
                ..
            } = member
            else {
                continue;
            };
            if !modifiers
                .iter()
                .any(|modifier| matches!(modifier, lamella_syntax::ast::Modifier::Override))
            {
                continue;
            }
            let query: Vec<TypeSymbol> = parameters
                .iter()
                .map(|parameter| {
                    self.normalize_for_signature(&crate::bind::parameter_symbol(parameter))
                })
                .collect();
            let method_sig =
                || crate::program::method_signature(&declaration.name, name, parameters);
            match self.base_method_match(class_ty, name, &query) {
                (None, true) => self.diagnostics.push(Diagnostic::new(
                    DiagnosticKind::NoMethodToOverride {
                        method: method_sig(),
                    },
                    *span,
                )),
                (None, false) => {}
                (Some((base, base_type, base_is_external)), _) => {
                    let base_sig =
                        crate::program::method_signature(&base_type, name, parameters);
                    let declared_access = crate::declaration::accessibility_of(modifiers);
                    if declared_access
                        != required_override_accessibility(base.accessibility, base_is_external)
                    {
                        self.diagnostics.push(Diagnostic::new(
                            DiagnosticKind::OverrideChangesAccess {
                                method: method_sig(),
                                access: base.accessibility.keyword().into(),
                                base: base_sig.clone(),
                            },
                            *span,
                        ));
                    }
                    if !base.is_virtual && !base.is_abstract && !base.is_override {
                        self.diagnostics.push(Diagnostic::new(
                            DiagnosticKind::CannotOverrideNonVirtual {
                                method: method_sig(),
                                base: base_sig,
                            },
                            *span,
                        ));
                    } else if base.is_sealed {
                        self.diagnostics.push(Diagnostic::new(
                            DiagnosticKind::CannotOverrideSealed {
                                method: method_sig(),
                                base: base_sig,
                            },
                            *span,
                        ));
                    } else {
                        let overriding = self
                            .normalize_for_signature(&crate::bind::bind_type(return_type));
                        let overridden = self.normalize_for_signature(&base.return_type);
                        if overriding != overridden {
                            self.diagnostics.push(Diagnostic::new(
                                DiagnosticKind::OverrideReturnTypeMismatch {
                                    method: method_sig(),
                                    return_type: overridden.to_string().into(),
                                    base: base_sig,
                                },
                                *span,
                            ));
                        }
                    }
                }
            }
        }
    }

    /// Reports the override-legality family for each source `override` PROPERTY and INDEXER in
    /// `declaration` -- what [`Self::check_overrides_have_base`] does for methods, one member kind
    /// over. The rules are the same (overriding nothing is `CS0115`, a base slot that is not
    /// overridable is `CS0506`, one that `sealed` closed is `CS0239`, a changed accessibility is
    /// `CS0507`), except that a differing TYPE is `CS1715` rather than `CS0508` -- a property has
    /// a type where a method has a return type, and csc gives that its own code.
    ///
    /// Only the base LOOKUP differs between the two member kinds. A property matches a base
    /// PROPERTY by name. An indexer has no name to match on, so it matches the base's `get_Item`
    /// ACCESSOR by index types -- which is where an indexer's overridability is recorded for a
    /// source and a referenced indexer alike.
    pub(crate) fn check_property_overrides_have_base(
        &mut self,
        class_ty: &TypeSymbol,
        declaration: &lamella_syntax::ast::TypeDecl,
    ) {
        for member in &declaration.members {
            let declares_override = |modifiers: &[lamella_syntax::ast::Modifier]| {
                modifiers
                    .iter()
                    .any(|modifier| matches!(modifier, lamella_syntax::ast::Modifier::Override))
            };
            match member {
                lamella_syntax::ast::Member::Property {
                    modifiers,
                    ty,
                    name,
                    explicit_interface: None,
                    span,
                    ..
                } if declares_override(modifiers) => {
                    let (found, resolved) = self.base_property_match(class_ty, name);
                    if let Some((property, declaring, _)) = &found {
                        let overrides_required = property.is_required;
                        let declares_required = modifiers.iter().any(|modifier| {
                            matches!(modifier, lamella_syntax::ast::Modifier::Required)
                        });
                        if overrides_required && !declares_required {
                            self.diagnostics.push(Diagnostic::new(
                                DiagnosticKind::OverrideMustBeRequired {
                                    member: format!("{}.{}", declaration.name, name).into(),
                                    base_member: format!("{declaring}.{name}").into(),
                                },
                                *span,
                            ));
                        }
                    }
                    let slot = found.map(|(property, declaring, base_is_external)| {
                        (
                            BaseSlot {
                                is_virtual: property.is_virtual,
                                is_abstract: property.is_abstract,
                                is_override: property.is_override,
                                is_sealed: property.is_sealed,
                                accessibility: property.accessibility,
                                ty: property.ty,
                                accessibility_is_known: !base_is_external,
                                base_is_external,
                            },
                            format!("{declaring}.{name}").into(),
                        )
                    });
                    self.report_override_slot(
                        format!("{}.{}", declaration.name, name).into(),
                        crate::declaration::accessibility_of(modifiers),
                        &bind_type(ty),
                        slot,
                        resolved,
                        true,
                        *span,
                    );
                }
                lamella_syntax::ast::Member::Indexer {
                    modifiers,
                    ty,
                    parameters,
                    span,
                    ..
                } if declares_override(modifiers) => {
                    let indices: Vec<TypeSymbol> = parameters
                        .iter()
                        .map(|parameter| {
                            self.normalize_for_signature(&crate::bind::parameter_symbol(parameter))
                        })
                        .collect();
                    let (found, _) = self.base_method_match(class_ty, "get_Item", &indices);
                    let indexer = |type_name: &str| {
                        format!(
                            "{type_name}.this[{}]",
                            crate::program::parameter_type_list(parameters)
                        )
                        .into()
                    };
                    let slot = found.map(|(accessor, declaring, base_is_external)| {
                        (
                            BaseSlot {
                                is_virtual: accessor.is_virtual,
                                is_abstract: accessor.is_abstract,
                                is_override: accessor.is_override,
                                is_sealed: accessor.is_sealed,
                                accessibility: accessor.accessibility,
                                ty: accessor.return_type,
                                accessibility_is_known: true,
                                base_is_external,
                            },
                            indexer(&declaring),
                        )
                    });
                    self.report_override_slot(
                        indexer(&declaration.name),
                        crate::declaration::accessibility_of(modifiers),
                        &bind_type(ty),
                        slot,
                        false,
                        false,
                        *span,
                    );
                }
                _ => {}
            }
        }
    }

    /// Checks an array/pointer INDEX or an array LENGTH. The permitted type is not `int` alone:
    /// `int`, `uint`, `long` and `ulong` all serve (12.4, 14.5.10.2), which matters for a pointer
    /// indexed by a `ulong` offset. Only when none of the four accepts the value is it reported --
    /// against `int`, which is the type csc names.
    fn check_index_or_length(&mut self, value: &BoundExpr, span: Span) {
        if value.ty.is_error() {
            return;
        }
        let int_ty = TypeSymbol::Special(SpecialType::Int32);
        let accepted = [
            SpecialType::Int32,
            SpecialType::UInt32,
            SpecialType::Int64,
            SpecialType::UInt64,
        ]
        .iter()
        .any(|special| self.assignable(value, &TypeSymbol::Special(*special)));
        if !accepted {
            self.check_assignable(value, &int_ty, span);
        }
    }

    /// The shared tail of the property and indexer override rules: given the base slot the member
    /// resolved to (or its absence), report the same family [`Self::check_overrides_have_base`]
    /// reports for a method. `chain_resolved` false means the base chain could not be walked to
    /// its end, so nothing is concluded from a miss; `report_missing` false suppresses `CS0115`
    /// for a member kind whose lookup can miss for reasons other than absence.
    fn report_override_slot(
        &mut self,
        signature: Box<str>,
        declared_access: Accessibility,
        declared_ty: &TypeSymbol,
        slot: Option<(BaseSlot, Box<str>)>,
        chain_resolved: bool,
        report_missing: bool,
        span: Span,
    ) {
        let Some((base, base_signature)) = slot else {
            if chain_resolved && report_missing {
                self.diagnostics.push(Diagnostic::new(
                    DiagnosticKind::NoMethodToOverride { method: signature },
                    span,
                ));
            }
            return;
        };
        if base.accessibility_is_known
            && declared_access
                != required_override_accessibility(base.accessibility, base.base_is_external)
        {
            self.diagnostics.push(Diagnostic::new(
                DiagnosticKind::OverrideChangesAccess {
                    method: signature.clone(),
                    access: base.accessibility.keyword().into(),
                    base: base_signature.clone(),
                },
                span,
            ));
        }
        if !base.is_virtual && !base.is_abstract && !base.is_override {
            self.diagnostics.push(Diagnostic::new(
                DiagnosticKind::CannotOverrideNonVirtual {
                    method: signature,
                    base: base_signature,
                },
                span,
            ));
        } else if base.is_sealed {
            self.diagnostics.push(Diagnostic::new(
                DiagnosticKind::CannotOverrideSealed {
                    method: signature,
                    base: base_signature,
                },
                span,
            ));
        } else {
            let overriding = self.normalize_for_signature(declared_ty);
            let overridden = self.normalize_for_signature(&base.ty);
            if overriding != overridden {
                self.diagnostics.push(Diagnostic::new(
                    DiagnosticKind::OverridePropertyTypeMismatch {
                        property: signature,
                        ty: overridden.to_string().into(),
                        base: base_signature,
                    },
                    span,
                ));
            }
        }
    }

    /// The property named `name` declared by the nearest base type up `class_ty`'s class chain,
    /// with that type's simple name and whether its accessibility is known (false for a
    /// referenced type, whose properties are synthesized from their accessors and carry a
    /// placeholder accessibility rather than one read from metadata). The second element is
    /// false when the chain could not be walked to its end, exactly as in
    /// [`Self::base_method_match`]: an unresolved base might declare the target, so a miss
    /// concludes nothing.
    fn base_property_match(
        &self,
        class_ty: &TypeSymbol,
        name: &str,
    ) -> (Option<(PropertySymbol, Box<str>, bool)>, bool) {
        let mut visited: Vec<TypeSymbol> = Vec::new();
        let mut current = self
            .model
            .get_by_symbol(class_ty)
            .and_then(|info| info.base.clone());
        while let Some(ty) = current.take() {
            if visited.contains(&ty) {
                break;
            }
            visited.push(ty.clone());
            let Some(info) = self.model.get_by_symbol(&ty) else {
                return (None, false);
            };
            if let Some(property) = info.find_property(name) {
                if self.accessible_from(class_ty, &ty, property.accessibility) {
                    return (
                        Some((property.clone(), info.name.clone(), info.is_external)),
                        true,
                    );
                }
            }
            current = info.base.clone();
        }
        if self.model.get_by_symbol(&type_symbol_in("System", "Object")).is_none() {
            return (None, false);
        }
        (None, true)
    }

    /// Reports `CS0534` for each inherited abstract member a non-abstract class does not
    /// implement -- one diagnostic per unimplemented member, naming its declaring type
    /// (`'D' does not implement inherited abstract member 'B.M()'`). Walks the base *class*
    /// chain most-derived first, keeping the most-derived declaration of each instance-method
    /// slot; a slot whose winning declaration is still `abstract` is unimplemented. An abstract
    /// class (or a struct/interface/enum) is exempt. Conservative: an unresolved base truncates
    /// the walk, and property/indexer/event accessors are not modeled here, so this under-reports
    /// rather than risk a false diagnostic. (Interface members are `CS0535`, checked separately.)
    pub(crate) fn check_abstract_implementations(
        &mut self,
        class_ty: &TypeSymbol,
        declaration: &lamella_syntax::ast::TypeDecl,
    ) {
        if declaration
            .modifiers
            .iter()
            .any(|modifier| matches!(modifier, lamella_syntax::ast::Modifier::Abstract))
        {
            return;
        }
        if !self
            .model
            .get_by_symbol(class_ty)
            .is_some_and(|info| info.kind == TypeKind::Class)
        {
            return;
        }
        let mut slots: Vec<(Box<str>, Vec<TypeSymbol>, Box<str>, bool, bool)> = Vec::new();
        let mut accessor_slots: Vec<(Box<str>, Vec<TypeSymbol>, bool, bool, Box<str>)> = Vec::new();
        let mut visited: Vec<TypeSymbol> = Vec::new();
        let mut current = Some(class_ty.clone());
        while let Some(ty) = current.take() {
            if visited.contains(&ty) {
                break;
            }
            let declared_here = ty == *class_ty;
            visited.push(ty.clone());
            let Some(info) = self.model.get_by_symbol(&ty) else {
                break;
            };
            let declaring = info.name.clone();
            for method in &info.methods {
                if method.is_static || is_special_member_name(&method.name) {
                    continue;
                }
                let key: Vec<TypeSymbol> = method
                    .parameters
                    .iter()
                    .map(|parameter| self.normalize_for_signature(parameter))
                    .collect();
                let already = slots
                    .iter()
                    .any(|(name, params, ..)| **name == *method.name && *params == key);
                if !already {
                    slots.push((
                        method.name.clone(),
                        key,
                        declaring.clone(),
                        method.is_abstract,
                        declared_here,
                    ));
                }
            }
            for method in &info.methods {
                let accessor = match &*method.name {
                    "get_Item" => "get",
                    "set_Item" => "set",
                    _ => continue,
                };
                if method.is_static {
                    continue;
                }
                let indices = if accessor == "set" {
                    method.parameters.split_last().map_or(&[][..], |(_, rest)| rest)
                } else {
                    &method.parameters[..]
                };
                let key: Vec<TypeSymbol> = indices
                    .iter()
                    .map(|parameter| self.normalize_for_signature(parameter))
                    .collect();
                push_accessor_slot(
                    &mut accessor_slots,
                    &alloc::format!("this.{accessor}"),
                    &key,
                    method.is_abstract,
                    declared_here,
                    accessor_member(&declaring, &indexer_display(&key), accessor),
                );
            }
            for property in &info.properties {
                if property.is_static {
                    continue;
                }
                for (present, accessor) in
                    [(property.has_getter, "get"), (property.has_setter, "set")]
                {
                    if !present {
                        continue;
                    }
                    push_accessor_slot(
                        &mut accessor_slots,
                        &alloc::format!("{}.{accessor}", property.name),
                        &[],
                        property.is_abstract,
                        declared_here,
                        accessor_member(&declaring, &property.name, accessor),
                    );
                }
            }
            for event in &info.events {
                if event.is_static {
                    continue;
                }
                for accessor in ["add", "remove"] {
                    push_accessor_slot(
                        &mut accessor_slots,
                        &alloc::format!("{}.{accessor}", event.name),
                        &[],
                        event.is_abstract,
                        declared_here,
                        accessor_member(&declaring, &event.name, accessor),
                    );
                }
            }
            current = info.base.clone();
        }
        for (name, params, declaring, is_abstract, declared_here) in &slots {
            if *is_abstract && !declared_here {
                self.diagnostics.push(Diagnostic::new(
                    DiagnosticKind::AbstractMemberNotImplemented {
                        type_name: declaration.name.clone(),
                        member: abstract_member_signature(declaring, name, params),
                    },
                    declaration.span,
                ));
            }
        }
        for (_, _, is_abstract, declared_here, member) in &accessor_slots {
            if *is_abstract && !declared_here {
                self.diagnostics.push(Diagnostic::new(
                    DiagnosticKind::AbstractMemberNotImplemented {
                        type_name: declaration.name.clone(),
                        member: member.clone(),
                    },
                    declaration.span,
                ));
            }
        }
    }

    /// Whether the base *class* chain of `class_ty` -- plus the implicit `System.Object` (and,
    /// for a struct, `System.ValueType`) root every type derives from -- declares a method named
    /// `name` whose parameters match `query` (both sides normalized so a metadata `System.String`
    /// and the `string` keyword compare equal). Returns `(found, resolved)`; `resolved` is false
    /// when an unknown base type truncated the walk or `System.Object` is absent from the model,
    /// so the caller can decline to report.
    /// Finds the base-CLASS method an `override` of `name(query)` targets: the first matching
    /// (name + normalized params) method up the base chain, paired with its declaring type's
    /// simple name (for the diagnostic message). The bool is whether the chain fully resolved
    /// (false = an unknown base, so the target cannot be confirmed present or absent -- report
    /// nothing). Interfaces are not override targets (implementing one is CS0535), so only the
    /// base *class* chain is walked.
    fn base_method_match(
        &self,
        class_ty: &TypeSymbol,
        name: &str,
        query: &[TypeSymbol],
    ) -> (Option<(MethodSymbol, Box<str>, bool)>, bool) {
        let mut visited: Vec<TypeSymbol> = Vec::new();
        let mut current = self
            .model
            .get_by_symbol(class_ty)
            .and_then(|info| info.base.clone());
        while let Some(ty) = current.take() {
            if visited.contains(&ty) {
                break;
            }
            visited.push(ty.clone());
            let Some(info) = self.model.get_by_symbol(&ty) else {
                return (None, false);
            };
            if let Some(method) = self.matching_method(&info, name, query) {
                if self.accessible_from(class_ty, &ty, method.accessibility) {
                    return (
                        Some((method.clone(), info.name.clone(), info.is_external)),
                        true,
                    );
                }
            }
            current = info.base.clone();
        }
        for root in ["Object", "ValueType"] {
            let root_ty = type_symbol_in("System", root);
            if *class_ty == root_ty || visited.contains(&root_ty) {
                continue;
            }
            match self.model.get_by_symbol(&root_ty) {
                Some(info) => {
                    if let Some(method) = self.matching_method(&info, name, query) {
                        return (
                            Some((method.clone(), info.name.clone(), info.is_external)),
                            true,
                        );
                    }
                }
                None if root == "Object" => return (None, false),
                None => {}
            }
        }
        (None, true)
    }

    /// The method in `info` named `name` whose parameter types match `query` (already normalized);
    /// each candidate parameter is normalized so metadata- and source-derived framework types
    /// compare equal.
    fn matching_method<'info>(
        &self,
        info: &'info TypeInfo,
        name: &str,
        query: &[TypeSymbol],
    ) -> Option<&'info MethodSymbol> {
        info.methods.iter().find(|method| {
            &*method.name == name
                && method.parameters.len() == query.len()
                && method
                    .parameters
                    .iter()
                    .zip(query)
                    .all(|(candidate, wanted)| self.normalize_for_signature(candidate) == *wanted)
        })
    }

    /// Normalizes a type for cross-source signature comparison: a framework-named primitive/
    /// `object`/`string` folds to its special form, at every level of an array/pointer/byref, so
    /// a metadata `System.Object` parameter and the `object` keyword are the same type.
    fn normalize_for_signature(&self, ty: &TypeSymbol) -> TypeSymbol {
        match ty {
            TypeSymbol::Array { element, rank } => TypeSymbol::Array {
                element: Box::new(self.normalize_for_signature(element)),
                rank: *rank,
            },
            TypeSymbol::Pointer(inner) => {
                TypeSymbol::Pointer(Box::new(self.normalize_for_signature(inner)))
            }
            TypeSymbol::ByRef(inner) => {
                TypeSymbol::ByRef(Box::new(self.normalize_for_signature(inner)))
            }
            TypeSymbol::Named(parts) => {
                let qualified = if parts.len() == 1 {
                    self.model
                        .type_with_simple_name(&parts[0])
                        .unwrap_or_else(|| ty.clone())
                } else {
                    ty.clone()
                };
                qualified.fold_builtin()
            }
            _ => ty.clone(),
        }
    }

    /// Whether an instance member emitted under `method_name` with `params` (a method, or a
    /// `get_`/`set_`/`add_`/`remove_` accessor) implements a member of an interface `class_ty`
    /// implements -- the test for the sealed-virtual (`virtual final newslot`) flags an implicit
    /// interface implementation carries (20.4.1). A member matching no interface member stays as
    /// declared (non-virtual unless `virtual`/`abstract`/`override`), matching csc; merely
    /// implementing SOME interface does not virtualize a type's unrelated members. `params` is
    /// canonicalized so a body-bound signature matches the interface's model signature.
    #[must_use]
    pub fn member_implements_interface(
        &self,
        class_ty: &TypeSymbol,
        method_name: &str,
        params: &[TypeSymbol],
    ) -> bool {
        let canonical: Vec<TypeSymbol> = params.iter().map(|p| self.canonicalize(p)).collect();
        let property = method_name
            .strip_prefix("get_")
            .or_else(|| method_name.strip_prefix("set_"));
        let event = method_name
            .strip_prefix("add_")
            .or_else(|| method_name.strip_prefix("remove_"));
        self.transitive_interfaces(class_ty).iter().any(|interface| {
            self.model.get_by_symbol(interface).is_some_and(|info| {
                info.methods.iter().any(|candidate| {
                    &*candidate.name == method_name
                        && candidate.parameters.len() == canonical.len()
                        && candidate
                            .parameters
                            .iter()
                            .zip(&canonical)
                            .all(|(a, b)| a == b)
                }) || property.is_some_and(|name| info.properties.iter().any(|p| &*p.name == name))
                    || event.is_some_and(|name| info.events.iter().any(|e| &*e.name == name))
            })
        })
    }

    /// The interfaces a type transitively implements: its own interface bases, plus those
    /// interfaces' base interfaces.
    pub(crate) fn transitive_interfaces(&self, ty: &TypeSymbol) -> Vec<TypeSymbol> {
        let mut result: Vec<TypeSymbol> = Vec::new();
        let mut stack: Vec<TypeSymbol> = alloc::vec![ty.clone()];
        while let Some(current) = stack.pop() {
            let Some(info) = self.model.get_by_symbol(&current) else {
                continue;
            };
            for base in &info.bases {
                let is_interface = self
                    .model
                    .get_by_symbol(base)
                    .is_some_and(|base_info| base_info.kind == TypeKind::Interface);
                if is_interface && !result.contains(base) {
                    result.push(base.clone());
                    stack.push(base.clone());
                }
            }
        }
        result
    }

    /// Whether `class_ty` implements interface `member` -- implicitly (a matching method in the
    /// class's own type or a base CLASS, or a property/event whose accessor the member names) or
    /// explicitly (a method registered under a mangled
    /// `<interface>.<member>` name). The implicit search walks only the base-class chain, NOT the
    /// interfaces: the interface's own abstract declaration must not be mistaken for its
    /// implementation (that is the CS0535 the caller reports). An explicit implementation is
    /// answered ONCE, by [`Self::explicitly_implements`], which names the interface and matches the
    /// parameter list and runs before the implicit rules.
    ///
    /// TEST-ONLY, and safely so: it collapses [`Self::interface_member_status`] to a boolean, so a
    /// test asking the yes/no question still exercises the four-state routine the diagnostics use.
    /// The production callers all want to know WHICH failure it was, because each names a different
    /// repair (`CS0535` / `CS0737` / `CS0738`) -- so none of them can use this shape.
    #[cfg(test)]
    fn implements_interface_member(
        &self,
        class_ty: &TypeSymbol,
        interface: &TypeSymbol,
        member: &MethodSymbol,
    ) -> bool {
        matches!(
            self.interface_member_status(class_ty, interface, member),
            InterfaceMemberStatus::Implemented
        )
    }

    /// Why a class does or does not implement one interface method. A member matching by name and
    /// parameters is not automatically an implementation: it must ALSO be public and return the
    /// interface's type, and csc gives each failure its own code (`CS0737`, `CS0738`) because each
    /// names a different repair. Only a member matching by signature can reach those two -- a
    /// class with nothing of that name is plainly `CS0535`.
    ///
    /// **An EXPLICIT implementation is matched before any of that (20.4.1)**, so a member that
    /// fails those rules cannot stand in for one the class also declares explicitly.
    fn interface_member_status(
        &self,
        class_ty: &TypeSymbol,
        interface: &TypeSymbol,
        member: &MethodSymbol,
    ) -> InterfaceMemberStatus {
        if self.explicitly_implements(class_ty, interface, member) {
            return InterfaceMemberStatus::Implemented;
        }
        let candidate = self
            .methods_in_class_chain(class_ty, &member.name)
            .into_iter()
            .find(|candidate| candidate.parameters == member.parameters);
        if let Some(candidate) = candidate {
            if candidate.accessibility != Accessibility::Public {
                return InterfaceMemberStatus::NotPublic;
            }
            if self.normalize_for_signature(&candidate.return_type)
                != self.normalize_for_signature(&member.return_type)
            {
                return InterfaceMemberStatus::WrongReturnType;
            }
            return InterfaceMemberStatus::Implemented;
        }
        if self.implemented_by_accessor_or_explicit(class_ty, member) {
            return InterfaceMemberStatus::Implemented;
        }
        InterfaceMemberStatus::Missing
    }

    /// Whether the class declares an explicit implementation of `member` FOR THIS INTERFACE.
    ///
    /// **THE QUALIFIER IS RESOLVED, NOT COMPARED AS TEXT.** The collector stores an explicit
    /// implementation under the qualifier the source wrote plus `.<member>`, and both
    /// `IEnumerable.GetEnumerator` and `System.Collections.IEnumerable.GetEnumerator` are legal
    /// spellings of one member -- so the stored name is split at its last `.` and the qualifier
    /// put through the same scope search a written type name gets. Naming the interface and
    /// matching the parameter list is what lets it run BEFORE the implicit rules without crediting
    /// a member of some other interface.
    fn explicitly_implements(
        &self,
        class_ty: &TypeSymbol,
        interface: &TypeSymbol,
        member: &MethodSymbol,
    ) -> bool {
        let mut suffix = String::from(".");
        suffix.push_str(&member.name);
        let wanted = dotted_type_name(interface);
        if wanted.is_empty() {
            return false;
        }
        let Some(info) = self.model.get_by_symbol(class_ty) else {
            return false;
        };
        info.methods.iter().any(|declared| {
            let Some(qualifier) = declared.name.strip_suffix(&suffix) else {
                return false;
            };
            if qualifier.is_empty() || declared.parameters != member.parameters {
                return false;
            }
            let parts: Vec<Box<str>> = qualifier.split('.').map(Box::from).collect();
            let written = TypeSymbol::Named(parts.into_boxed_slice());
            dotted_type_name(&self.resolve_type(&written)) == wanted
        })
    }

    /// The one implementation form the signature walk cannot see: a `get_`/`set_`/`add_`/
    /// `remove_` member supplied by declaring the property or field-like event itself. A source
    /// type keeps those accessors in `properties`/`events` rather than as synthesized methods, so
    /// the method walk above does not find them.
    ///
    /// **AN EXPLICIT IMPLEMENTATION IS NOT ANSWERED HERE.** It is answered once, and strictly, by
    /// [`Self::explicitly_implements`], which names the interface and matches the parameter list
    /// and runs before the implicit rules. An explicit PROPERTY is reached by the property walk
    /// below rather than as a method: the collector stores one under its PLAIN name.
    fn implemented_by_accessor_or_explicit(
        &self,
        class_ty: &TypeSymbol,
        member: &MethodSymbol,
    ) -> bool {
        self.accessor_provided_by_property_or_event(class_ty, member)
    }

    /// The methods named `name` in `ty`'s own type and its base CLASS chain (walking `info.base`,
    /// which `link_bases` resolves to the base *class*, never an interface). Used to decide
    /// implicit interface implementation: a candidate here is a concrete provider of the member,
    /// whereas `methods_in_chain` would also surface the interface's own abstract method.
    fn methods_in_class_chain(&self, ty: &TypeSymbol, name: &str) -> Vec<MethodSymbol> {
        let mut methods: Vec<MethodSymbol> = Vec::new();
        let mut visited: Vec<TypeSymbol> = Vec::new();
        let mut current = Some(self.lookup_type_of(ty));
        while let Some(cur) = current {
            if visited.contains(&cur) {
                break;
            }
            visited.push(cur.clone());
            let Some(info) = self.type_info_of(&cur) else {
                break;
            };
            for method in info.methods_named(name) {
                methods.push(method.clone());
            }
            current = info.base.clone();
        }
        methods
    }

    /// Whether interface accessor method `member` is satisfied by a property or field-like event
    /// of the same name in `class_ty`'s own type or a base CLASS. A source type keeps a property's
    /// `get_`/`set_` (and an event's `add_`/`remove_`) accessors in `properties`/`events` rather
    /// than as synthesized methods -- only indexers and metadata-loaded types carry the accessor
    /// methods themselves -- so an interface accessor a source type implements via a property or
    /// event is invisible to the method walk and would be a false CS0535. Lenient by design
    /// (matched on accessor name and kind, not the full signature): a strict subset never risks a
    /// false positive, so a subtler type mismatch is left for csc (CS0738) rather than reported.
    fn accessor_provided_by_property_or_event(
        &self,
        class_ty: &TypeSymbol,
        member: &MethodSymbol,
    ) -> bool {
        let property = member
            .name
            .strip_prefix("get_")
            .filter(|_| member.parameters.is_empty())
            .map(|name| (name, true))
            .or_else(|| {
                member
                    .name
                    .strip_prefix("set_")
                    .filter(|_| member.parameters.len() == 1)
                    .map(|name| (name, false))
            });
        let event = member
            .name
            .strip_prefix("add_")
            .or_else(|| member.name.strip_prefix("remove_"))
            .filter(|_| member.parameters.len() == 1);
        if property.is_none() && event.is_none() {
            return false;
        }
        let mut visited: Vec<TypeSymbol> = Vec::new();
        let mut current = Some(self.lookup_type_of(class_ty));
        while let Some(cur) = current {
            if visited.contains(&cur) {
                break;
            }
            visited.push(cur.clone());
            let Some(info) = self.type_info_of(&cur) else {
                break;
            };
            if let Some((name, need_getter)) = property {
                if info.properties.iter().any(|candidate| {
                    &*candidate.name == name
                        && if need_getter {
                            candidate.has_getter
                        } else {
                            candidate.has_setter
                        }
                }) {
                    return true;
                }
            }
            if let Some(name) = event {
                if info.events.iter().any(|candidate| &*candidate.name == name) {
                    return true;
                }
            }
            current = info.base.clone();
        }
        false
    }

    /// Reports `CS0146` if `class_ty`'s base-class chain is circular (A : B, B : A). The
    /// chain walk is bounded by a visited set, so a cycle is detected, not looped on.
    pub(crate) fn check_base_cycle(
        &mut self,
        class_ty: &TypeSymbol,
        declaration: &lamella_syntax::ast::TypeDecl,
    ) {
        let mut visited: Vec<TypeSymbol> = Vec::new();
        let mut current = Some(class_ty.clone());
        while let Some(ty) = current.take() {
            if visited.contains(&ty) {
                self.diagnostics.push(Diagnostic::new(
                    DiagnosticKind::CircularBase {
                        type_name: declaration.name.clone(),
                    },
                    declaration.span,
                ));
                return;
            }
            visited.push(ty.clone());
            current = self
                .model
                .get_by_symbol(&ty)
                .and_then(|info| info.base.clone());
        }
    }

    /// Reports `CS0529` if `interface_ty`'s base-interface hierarchy is circular (interface
    /// I : J, J : I). The graph walk is bounded by a visited set, and emitting the error skips
    /// emission -- where the interface flattening for the metadata would otherwise loop forever.
    pub(crate) fn check_interface_cycle(
        &mut self,
        interface_ty: &TypeSymbol,
        declaration: &lamella_syntax::ast::TypeDecl,
    ) {
        let Some(bases) = self
            .model
            .get_by_symbol(interface_ty)
            .map(|info| info.bases.clone())
        else {
            return;
        };
        for base in &bases {
            if self.type_reaches(base, interface_ty, false) {
                self.diagnostics.push(Diagnostic::new(
                    DiagnosticKind::CircularInterface {
                        type_name: declaration.name.clone(),
                        base: base.to_string().into(),
                    },
                    declaration.span,
                ));
                return;
            }
        }
    }

    /// Reports `CS0523` if a value-type field of struct `struct_ty` cycles back through
    /// value-type fields to the struct itself (`struct S { S f; }`) -- an infinitely-sized
    /// layout. The walk is bounded by a visited set; emitting the error skips emission, where
    /// the struct-layout computation would otherwise loop forever.
    pub(crate) fn check_struct_layout_cycle(
        &mut self,
        struct_ty: &TypeSymbol,
        declaration: &lamella_syntax::ast::TypeDecl,
    ) {
        let Some(fields) = self.model.get_by_symbol(struct_ty).map(|info| {
            info.fields
                .iter()
                .filter(|field| !field.is_static)
                .map(|field| (field.name.clone(), field.ty.clone()))
                .collect::<Vec<_>>()
        }) else {
            return;
        };
        for (name, ty) in &fields {
            if self.type_reaches(ty, struct_ty, true) {
                self.diagnostics.push(Diagnostic::new(
                    DiagnosticKind::StructLayoutCycle {
                        member: alloc::format!("{}.{}", declaration.name, name).into(),
                        type_name: ty.to_string().into(),
                    },
                    declaration.span,
                ));
                return;
            }
        }
    }

    /// Whether following `start`'s bases -- or, when `by_value`, its non-static value-type fields
    /// -- transitively reaches `target`. A bounded graph walk (visited set), used to detect an
    /// interface-hierarchy or struct-layout cycle without looping on it.
    fn type_reaches(&self, start: &TypeSymbol, target: &TypeSymbol, by_value: bool) -> bool {
        let mut visited: Vec<TypeSymbol> = Vec::new();
        let mut stack: Vec<TypeSymbol> = alloc::vec![start.clone()];
        while let Some(current) = stack.pop() {
            if &current == target {
                return true;
            }
            if visited.contains(&current) {
                continue;
            }
            visited.push(current.clone());
            let Some(info) = self.model.get_by_symbol(&current) else {
                continue;
            };
            if by_value {
                if info.kind == TypeKind::Struct {
                    for field in &info.fields {
                        if !field.is_static {
                            stack.push(field.ty.clone());
                        }
                    }
                }
            } else {
                for base in &info.bases {
                    stack.push(base.clone());
                }
            }
        }
        false
    }

    /// Every method named `name` on `ty` or any of its base classes -- the method
    /// group an invocation resolves over (most-derived first).
    /// Resolves a no-argument instance method `name` on `receiver_ty` to a reference -- for
    /// the compiler-synthesized calls of a `foreach` enumerator pattern (GetEnumerator on the
    /// collection, MoveNext/get_Current on the enumerator). `None` when the type has no such
    /// method (so the collection is not enumerable), without reporting a diagnostic.
    pub(crate) fn resolve_instance_method(
        &mut self,
        receiver_ty: &TypeSymbol,
        name: &str,
        span: Span,
    ) -> Option<MethodReference> {
        let candidates = self.methods_in_chain(receiver_ty, name);
        if candidates.is_empty() {
            return None;
        }
        let chosen = self.resolve_call(name, receiver_ty, &candidates, &[], &[], &[], span)?;
        let declaring_type =
            self.declaring_type_in_chain(receiver_ty, &chosen.name, &chosen.parameters);
        let declaring_instantiation =
            self.declaring_instantiation_of(&declaring_type, &chosen.name, &chosen.parameters);
        Some(MethodReference {
            declaring_type,
            is_vararg: chosen.is_vararg,
            name: chosen.name,
            parameters: chosen.parameters,
            return_type: chosen.return_type,
            is_static: chosen.is_static,
            instantiation: None,
            declaring_instantiation,
        })
    }

    /// Resolves a readable instance property's `get_<name>` getter on `ty` (14.5.4): the
    /// accessor method when the model records one directly, else one synthesized from the
    /// property symbol -- a source-declared property is modeled as a `PropertySymbol` without
    /// a separate accessor method, yet its getter is emitted under `get_<name>`. `None` when
    /// `ty` has no such readable instance property. Used to bind the `Current` of the
    /// enumerator pattern (15.8.4) whether the enumerator is source- or reference-declared.
    pub(crate) fn resolve_property_getter(
        &mut self,
        ty: &TypeSymbol,
        name: &str,
        span: Span,
    ) -> Option<MethodReference> {
        let getter = format!("get_{name}");
        if let Some(method) = self.resolve_instance_method(ty, &getter, span) {
            return Some(method);
        }
        match self.resolve_member(ty, name) {
            MemberResolution::Property {
                declaring_type,
                ty: property_ty,
                is_static: false,
                ..
            } => Some(MethodReference {
                declaring_instantiation: self
                    .declaring_instantiation_of(&declaring_type, &getter, &[]),
                declaring_type,
                name: getter.into(),
                parameters: Vec::new(),
                return_type: property_ty,
                is_static: false,
                is_vararg: false,
                instantiation: None,
            }),
            _ => None,
        }
    }

    /// Whether a bound call is to a `[Conditional("X")]` method none of whose symbols are
    /// defined here -- so the call statement is omitted whole (24.4.2), arguments and all. The
    /// method's `conditional` is recovered from the model by the resolved overload.
    pub(crate) fn conditional_call_omitted(&self, expr: &BoundExpr) -> bool {
        let BoundExprKind::Call {
            method: Some(method),
            ..
        } = &expr.kind
        else {
            return false;
        };
        let conditional = self
            .methods_in_chain(&method.declaring_type, &method.name)
            .into_iter()
            .find(|candidate| candidate.parameters == method.parameters)
            .map(|candidate| candidate.conditional)
            .unwrap_or_default();
        !conditional.is_empty()
            && !conditional
                .iter()
                .any(|symbol| self.defined_symbols.contains(symbol))
    }

    /// The name of an indexer accessor on `ty` (or a base): a `prefix`-named method (`get_`/`set_`)
    /// taking `arity` parameters. A C# indexer defaults to `Item` (`get_Item`/`set_Item`) but a
    /// `[IndexerName]` renames it -- String and StringBuilder use `Chars`. A regular property
    /// accessor never has parameters (a getter takes 0, a setter 1), so matching the index arity
    /// (read = indices, write = indices + the value) finds the indexer, not a plain property. The
    /// first match walking the chain (most-derived first) wins.
    fn indexer_accessor(&self, ty: &TypeSymbol, prefix: &str, arity: usize) -> Option<Box<str>> {
        let mut visited: Vec<TypeSymbol> = Vec::new();
        let mut pending = alloc::vec![self.lookup_type_of(ty)];
        while let Some(current_ty) = pending.pop() {
            if visited.contains(&current_ty) {
                continue;
            }
            visited.push(current_ty.clone());
            let Some(info) = self.type_info_of(&current_ty) else {
                continue;
            };
            for method in &info.methods {
                if method.parameters.len() == arity && method.name.starts_with(prefix) {
                    return Some(method.name.clone());
                }
            }
            for base in member_lookup_bases(&info) {
                pending.push(base);
            }
        }
        None
    }

    fn methods_in_chain(&self, ty: &TypeSymbol, name: &str) -> Vec<MethodSymbol> {
        let mut methods: Vec<MethodSymbol> = Vec::new();
        let mut inaccessible: Vec<MethodSymbol> = Vec::new();
        let mut visited: Vec<TypeSymbol> = Vec::new();
        let lookup = self.lookup_type_of(ty);
        let mut pending = alloc::vec![lookup.clone()];
        if self
            .type_info_of(&lookup)
            .is_some_and(|info| info.kind == TypeKind::Interface)
        {
            pending.insert(0, type_symbol_in("System", "Object"));
        }
        while let Some(current_ty) = pending.pop() {
            if visited.contains(&current_ty) {
                continue;
            }
            visited.push(current_ty.clone());
            let Some(info) = self.type_info_of(&current_ty) else {
                continue;
            };
            let declaring = type_symbol_in(&info.namespace, &info.name);
            for method in info.methods_named(name) {
                let conversion_operator = matches!(name, "op_Implicit" | "op_Explicit");
                let bucket = if self.is_accessible(&declaring, method.accessibility) {
                    &mut methods
                } else {
                    &mut inaccessible
                };
                if !bucket.iter().any(|kept| {
                    kept.parameters == method.parameters
                        && (!conversion_operator || kept.return_type == method.return_type)
                }) {
                    bucket.push(method.clone());
                }
            }
            for base in member_lookup_bases(&info) {
                pending.push(base);
            }
        }
        if methods.is_empty() {
            inaccessible
        } else {
            methods
        }
    }

    /// The types that declare a property's `get_`/`set_` accessors, reached from `receiver_ty`.
    /// They differ for a partially-overridden property -- a `sealed override { set; }` inherits its
    /// getter -- so each accessor is named on the most-derived type whose declaration of the
    /// property provides it (14.5.4). For a whole property both are the property's own declaring
    /// type. Walks the property up the base chain because accessors are not model `methods`.
    ///
    /// An INACCESSIBLE declaration provides no accessors, exactly as it does not resolve in
    /// `resolve_member` (7.3 looks up accessible members only): a `protected new` property in a
    /// nested class must not capture the getter of an access that RESOLVED to the accessible base
    /// property -- the two walks disagreeing is how `z.P = z.P` bound base's float property but
    /// called the derived double getter. Overrides are unaffected: an override carries its base
    /// declaration's accessibility, so the accessors of any property that resolved stay eligible.
    /// The OPEN signatures of a property's two accessors, when it was reached through an
    /// instantiated generic type. `(None, None)` for a property of an ordinary type.
    ///
    /// **ONE IMPLEMENTATION FOR ALL THREE PLACES A `PropertyAccess` IS BUILT.** The declaring-type
    /// walk this pairs with had four copies and was repaired in two of them; writing the
    /// instantiation lookup three times here would set the same trap one layer up. `property_ty` is
    /// the SUBSTITUTED property type, which is what the setter takes as its value parameter.
    fn property_accessor_instantiations(
        &self,
        getter_declaring: &TypeSymbol,
        setter_declaring: &TypeSymbol,
        name: &str,
        property_ty: &TypeSymbol,
    ) -> (Option<Box<TypeInstantiation>>, Option<Box<TypeInstantiation>>) {
        let _ = property_ty;
        let accessor = |declaring: &TypeSymbol, want_setter: bool| {
            let (type_parameters, parameters, return_type) =
                self.model.open_property_accessor(declaring, name, want_setter)?;
            Some(Box::new(TypeInstantiation {
                type_parameters,
                parameters,
                return_type,
            }))
        };
        (
            accessor(getter_declaring, false),
            accessor(setter_declaring, true),
        )
    }

    fn property_accessor_declarers(
        &self,
        receiver_ty: &TypeSymbol,
        name: &str,
    ) -> (TypeSymbol, TypeSymbol) {
        let fallback = self.lookup_type_of(receiver_ty);
        let mut getter: Option<TypeSymbol> = None;
        let mut setter: Option<TypeSymbol> = None;
        let mut visited: Vec<TypeSymbol> = Vec::new();
        let mut pending = alloc::vec![fallback.clone()];
        while let Some(ty) = pending.pop() {
            if visited.contains(&ty) {
                continue;
            }
            visited.push(ty.clone());
            let Some(info) = self.type_info_of(&ty) else {
                continue;
            };
            if let Some(property) = info.find_property(name) {
                let declaring = type_symbol_in(&info.namespace, &info.name);
                let named = member_declaring_type(&ty, &info);
                if self.is_accessible(&declaring, property.accessibility) {
                    if getter.is_none() && property.has_getter {
                        getter = Some(named.clone());
                    }
                    if setter.is_none() && property.has_setter {
                        setter = Some(named);
                    }
                }
            }
            if getter.is_some() && setter.is_some() {
                break;
            }
            for base in member_lookup_bases(&info) {
                pending.push(base);
            }
        }
        (
            getter.unwrap_or_else(|| fallback.clone()),
            setter.unwrap_or(fallback),
        )
    }

    /// The type a resolved method reference should name: the most-derived type from
    /// `ty` up its base chain that declares the method `name(parameters)`. An override
    /// names the deriving type; a method only inherited names the base that declares
    /// it (so the emitted token resolves there, not on the receiver's type).
    ///
    /// **AN INSTANTIATION IS NAMED AS ITSELF, NOT AS ITS DEFINITION.** A member found through
    /// `Box<int>` is a member OF `Box<int>`; naming it `` Box`1 `` loses the argument list that
    /// decides what its signature means. [`TypeInfo::instantiate`] deliberately keeps the
    /// definition's own `name` (its doc says why), so rebuilding the symbol from `namespace`/`name`
    /// -- which is what every non-generic case wants -- silently drops the `<int>` here.
    ///
    /// **THE LOSS IS SILENT AND IT REACHES METADATA.** Measured before this returned the
    /// instantiation: `Box<int>.Echo(41)` emitted a `MemberRef` whose parent was a `TypeRef` to
    /// this module's OWN `` Box`1 ``, carrying the SUBSTITUTED signature rather than the
    /// definition's `!0`; `b.Get()` emitted `callvirt` straight at the open `MethodDef`, whose `!0`
    /// return is never substituted. Both compiled clean and exited 0, and the IL was
    /// instruction-for-instruction identical to the non-generic control's -- so nothing at the call
    /// site could tell the right answer from the wrong one.
    ///
    /// The base chain is already substituted (`Box<int> : Base<int>` walks to `Base<int>`, not
    /// `` Base`1 ``), so the walk's own `current_ty` is the symbol wanted; only the rebuild threw
    /// it away.
    fn declaring_type_in_chain(
        &self,
        ty: &TypeSymbol,
        name: &str,
        parameters: &[TypeSymbol],
    ) -> TypeSymbol {
        let lookup = self.lookup_type_of(ty);
        let mut visited: Vec<TypeSymbol> = Vec::new();
        let mut pending = alloc::vec![lookup.clone()];
        if self
            .type_info_of(&lookup)
            .is_some_and(|info| info.kind == TypeKind::Interface)
        {
            pending.insert(0, type_symbol_in("System", "Object"));
        }
        let mut inaccessible_match: Option<TypeSymbol> = None;
        while let Some(current_ty) = pending.pop() {
            if visited.contains(&current_ty) {
                continue;
            }
            visited.push(current_ty.clone());
            let Some(info) = self.type_info_of(&current_ty) else {
                continue;
            };
            let declaring = type_symbol_in(&info.namespace, &info.name);
            let mut declares = false;
            let mut accessible = false;
            for method in info.methods_named(name) {
                if method.parameters.as_slice() == parameters {
                    declares = true;
                    if self.is_accessible(&declaring, method.accessibility) {
                        accessible = true;
                    }
                }
            }
            let named = member_declaring_type(&current_ty, &info);
            if accessible {
                return named;
            }
            if declares && inaccessible_match.is_none() {
                inaccessible_match = Some(named);
            }
            for base in member_lookup_bases(&info) {
                pending.push(base);
            }
        }
        inaccessible_match.unwrap_or(lookup)
    }

    /// The [`TypeInstantiation`] for a member named through an instantiated generic type --
    /// `None` for a member of an ordinary type, and for a member whose open declaration cannot be
    /// identified (emission refuses such a call rather than writing a `!n` it guessed).
    ///
    /// Takes the SUBSTITUTED parameters, because those are what overload resolution chose and what
    /// the caller has in hand; the model substitutes each candidate back to find the match.
    fn declaring_instantiation_of(
        &self,
        declaring: &TypeSymbol,
        name: &str,
        parameters: &[TypeSymbol],
    ) -> Option<Box<TypeInstantiation>> {
        let (type_parameters, open) = self.model.open_member(declaring, name, parameters)?;
        Some(Box::new(TypeInstantiation {
            type_parameters,
            parameters: open.parameters,
            return_type: open.return_type,
        }))
    }

    /// The [`FieldInstantiation`] for a field named through an instantiated generic type --
    /// `None` for a field of an ordinary type, and for one whose open declaration is not in reach
    /// (emission then refuses rather than naming the definition's shared cell).
    fn field_instantiation_of(
        &self,
        declaring: &TypeSymbol,
        name: &str,
    ) -> Option<Box<FieldInstantiation>> {
        let (type_parameters, ty) = self.model.open_field(declaring, name)?;
        Some(Box::new(FieldInstantiation {
            type_parameters,
            ty,
        }))
    }

    /// Binds a simple name (14.5.2). For now a name resolves only to a local
    /// variable or parameter; anything else is `CS0103` (field, type, and
    /// namespace lookup arrive with the declaration model).
    fn bind_name(&mut self, name: &str, span: Span) -> BoundExpr {
        if let Some((value, ty, _)) = self.const_locals.get(name) {
            return BoundExpr {
                kind: BoundExprKind::Literal(value.clone()),
                ty: ty.clone(),
            };
        }
        if let Some(ty) = self.lookup_local(name) {
            return BoundExpr {
                kind: BoundExprKind::Local(name.into()),
                ty: ty.clone(),
            };
        }
        if let Some(receiver) = self.session_receiver.clone() {
            if let Some((stable, ty)) = self.session_fields.get(name).cloned() {
                let repl_type = self.current_type.clone().unwrap_or(TypeSymbol::Error);
                return self.session_field_access(&receiver, &repl_type, &stable, &ty);
            }
        }
        if let Some(TypeSymbol::Named(parts)) = &self.current_type {
            let mut enclosing = String::new();
            for part in parts.iter() {
                if !enclosing.is_empty() {
                    enclosing.push('.');
                }
                enclosing.push_str(part);
            }
            if self.model.get(&enclosing, name).is_some() {
                let ty = type_symbol_in(&enclosing, name);
                return BoundExpr {
                    kind: BoundExprKind::TypeReference(ty.clone()),
                    ty,
                };
            }
        }
        if let Some(current) = self.current_type.clone() {
            match self.resolve_member(&current, name) {
                MemberResolution::Field(field) => {
                    if !field.is_static && self.in_static_method() {
                        self.report_no_object_reference(
                            &field.declaring_type,
                            name,
                            false,
                            span,
                        );
                    } else if !field.is_static && self.in_field_initializer {
                        self.report_field_initializer_reference(
                            &field.declaring_type,
                            name,
                            false,
                            span,
                        );
                    }
                    return BoundExpr {
                        ty: field.ty.clone(),
                        kind: BoundExprKind::FieldAccess {
                            receiver: Box::new(self.implicit_receiver()),
                            name: name.into(),
                            field: Some(field),
                        },
                    };
                }
                MemberResolution::Property {
                    declaring_type,
                    ty,
                    is_static,
                    ..
                } => {
                    if !is_static && self.in_static_method() {
                        self.report_no_object_reference(&declaring_type, name, false, span);
                    } else if !is_static && self.in_field_initializer {
                        self.report_field_initializer_reference(&declaring_type, name, false, span);
                    }
                    let receiver = if is_static {
                        BoundExpr {
                            kind: BoundExprKind::TypeReference(current.clone()),
                            ty: current.clone(),
                        }
                    } else {
                        self.implicit_receiver()
                    };
                    let (getter_declaring, setter_declaring) =
                        self.property_accessor_declarers(&current, name);
                    let (getter_instantiation, setter_instantiation) = self
                        .property_accessor_instantiations(
                            &getter_declaring,
                            &setter_declaring,
                            name,
                            &ty,
                        );
                    return BoundExpr {
                        kind: BoundExprKind::PropertyAccess {
                            receiver: Box::new(receiver),
                            declaring_type: getter_declaring,
                            setter_declaring_type: setter_declaring,
                            getter_instantiation,
                            setter_instantiation,
                            name: name.into(),
                        },
                        ty,
                    };
                }
                MemberResolution::MethodGroup => {
                    return BoundExpr {
                        kind: BoundExprKind::MethodGroup {
                            receiver: Box::new(self.implicit_receiver()),
                            name: name.into(),
                        },
                        ty: TypeSymbol::Error,
                    };
                }
                MemberResolution::NoSuchMember(_) | MemberResolution::Unknown => {}
            }
            if let Some(bound) = self.resolve_enclosing_static(name) {
                return bound;
            }
        }
        if let Some(target) = self.alias_target(name) {
            return BoundExpr {
                kind: BoundExprKind::TypeReference(target.clone()),
                ty: target,
            };
        }
        let hits = self.type_namespaces_containing(name);
        if let Some((namespace, imported)) = hits.first() {
            let ambiguous_import = *imported && hits.get(1).is_some_and(|(_, other)| *other);
            if !ambiguous_import {
                let ty = type_symbol_in(namespace, name);
                return BoundExpr {
                    kind: BoundExprKind::TypeReference(ty.clone()),
                    ty,
                };
            }
            self.diagnostics.push(Diagnostic::new(
                DiagnosticKind::AmbiguousReference {
                    name: name.into(),
                    first: full_type_name(&hits[0].0, name),
                    second: full_type_name(&hits[1].0, name),
                },
                span,
            ));
            return error_expr();
        }
        if self.model.is_namespace(name) {
            return BoundExpr {
                kind: BoundExprKind::NamespaceReference(name.into()),
                ty: TypeSymbol::Error,
            };
        }
        if let Some(full) = self.resolve_partial_namespace(name) {
            return BoundExpr {
                kind: BoundExprKind::NamespaceReference(full),
                ty: TypeSymbol::Error,
            };
        }
        self.diagnostics.push(Diagnostic::new(
            DiagnosticKind::NameNotFound { name: name.into() },
            span,
        ));
        error_expr()
    }

    /// Resolves `namespace.name`: a nested namespace, a type, or `CS0234`.
    fn bind_qualified_name(&mut self, namespace: &str, name: &str, span: Span) -> BoundExpr {
        if self.model.get(namespace, name).is_some() {
            let ty = qualified_type_symbol(namespace, name);
            return BoundExpr {
                kind: BoundExprKind::TypeReference(ty.clone()),
                ty,
            };
        }
        let mut nested = String::from(namespace);
        nested.push('.');
        nested.push_str(name);
        if self.model.is_namespace(&nested) {
            return BoundExpr {
                kind: BoundExprKind::NamespaceReference(nested.into()),
                ty: TypeSymbol::Error,
            };
        }
        self.diagnostics.push(Diagnostic::new(
            DiagnosticKind::NamespaceMemberNotFound {
                namespace: namespace.into(),
                name: name.into(),
            },
            span,
        ));
        error_expr()
    }

    /// The `this` access, typed as the enclosing type (the error type when there
    /// is none, for recovery).
    fn this_expr(&self) -> BoundExpr {
        BoundExpr {
            kind: BoundExprKind::This,
            ty: self.current_type.clone().unwrap_or(TypeSymbol::Error),
        }
    }

    /// The receiver an implicit member access reads through: `this` normally, or the
    /// `s: __Repl` parameter in REPL session mode. A submission's `Submit$N` is a
    /// static method, so its session locals -- modeled as fields of the enclosing
    /// `__Repl` -- are reached through the parameter `s` (`ldarg.0; ldfld`), not a
    /// `this` it does not have. Both carry the enclosing type, so member lookup is
    /// identical; only the emitted receiver differs.
    fn implicit_receiver(&self) -> BoundExpr {
        match &self.session_receiver {
            Some(name) => BoundExpr {
                kind: BoundExprKind::Local(name.clone()),
                ty: self.current_type.clone().unwrap_or(TypeSymbol::Error),
            },
            None => self.this_expr(),
        }
    }

    /// The `base` access, typed as the enclosing type's base class (the error type
    /// when there is no enclosing type or it has no base, for recovery).
    fn base_expr(&self) -> BoundExpr {
        let base = self
            .current_type
            .as_ref()
            .and_then(|ty| self.type_info_of(ty))
            .and_then(|info| info.base.clone());
        BoundExpr {
            kind: BoundExprKind::Base,
            ty: base.unwrap_or(TypeSymbol::Error),
        }
    }
}

/// Binds a single expression and discards the diagnostics, for callers that only
/// want the typed tree.
#[must_use]
pub fn bind_expression(expr: &Expr) -> BoundExpr {
    let mut binder = Binder::new();
    binder.bind_expression(expr)
}

/// The result type of pointer arithmetic (18.5.6, unsafe): `p + n` / `n + p` / `p - n`
/// (a `T*`, the integer scaled by `sizeof(T)`) and `p - q` (a `long`, the element-count
/// difference). `None` when neither operand is a pointer (a plain numeric op handles it).
fn pointer_binary_result(
    operator: BinaryOperator,
    left: &TypeSymbol,
    right: &TypeSymbol,
) -> Option<TypeSymbol> {
    use BinaryOperator::{
        Add, Equal, GreaterThan, GreaterThanOrEqual, LessThan, LessThanOrEqual, NotEqual, Subtract,
    };
    let integral = |ty: &TypeSymbol| matches!(ty, TypeSymbol::Special(special) if special.is_integral());
    match (operator, left, right) {
        (Add, TypeSymbol::Pointer(_), other) | (Add, other, TypeSymbol::Pointer(_))
            if integral(other) =>
        {
            Some(if matches!(left, TypeSymbol::Pointer(_)) {
                left.clone()
            } else {
                right.clone()
            })
        }
        (Subtract, TypeSymbol::Pointer(_), other) if integral(other) => Some(left.clone()),
        (Subtract, TypeSymbol::Pointer(a), TypeSymbol::Pointer(b)) if a == b => {
            Some(TypeSymbol::Special(SpecialType::Int64))
        }
        (
            Equal | NotEqual | LessThan | LessThanOrEqual | GreaterThan | GreaterThanOrEqual,
            TypeSymbol::Pointer(_),
            TypeSymbol::Pointer(_),
        ) => Some(TypeSymbol::Special(SpecialType::Boolean)),
        _ => None,
    }
}

/// The result type of a binary operator on operand types, or `None` if the
/// operator does not apply (14.7-14.12).
fn binary_result_type(
    operator: BinaryOperator,
    left: &TypeSymbol,
    right: &TypeSymbol,
) -> Option<TypeSymbol> {
    use BinaryOperator as Op;
    let bool_type = TypeSymbol::Special(SpecialType::Boolean);
    let left_special = as_special(left);
    let right_special = as_special(right);
    match operator {
        Op::Add
            if (left_special == Some(SpecialType::String)
                || right_special == Some(SpecialType::String)
                || (left_special == Some(SpecialType::Object)
                    && right_special == Some(SpecialType::Null))
                || (right_special == Some(SpecialType::Object)
                    && left_special == Some(SpecialType::Null)))
                && !left.is_void()
                && !right.is_void() =>
        {
            Some(TypeSymbol::Special(SpecialType::String))
        }
        Op::Multiply | Op::Divide | Op::Modulo | Op::Add | Op::Subtract => {
            binary_numeric_promotion(left_special?, right_special?).map(TypeSymbol::Special)
        }
        Op::LessThan | Op::GreaterThan | Op::LessThanOrEqual | Op::GreaterThanOrEqual => {
            binary_numeric_promotion(left_special?, right_special?).map(|_| bool_type)
        }
        Op::Equal | Op::NotEqual => equality_comparable(left, right).then_some(bool_type),
        Op::LogicalAnd | Op::LogicalOr => {
            let boolean = Some(SpecialType::Boolean);
            (left_special == boolean && right_special == boolean).then_some(bool_type)
        }
        Op::BitwiseAnd | Op::BitwiseOr | Op::BitwiseXor => {
            let boolean = Some(SpecialType::Boolean);
            if left_special == boolean && right_special == boolean {
                Some(bool_type)
            } else {
                let (left, right) = (left_special?, right_special?);
                (is_integral(left) && is_integral(right))
                    .then_some(binary_numeric_promotion(left, right).map(TypeSymbol::Special))
                    .flatten()
            }
        }
        Op::LeftShift | Op::RightShift => {
            let (left, right) = (left_special?, right_special?);
            (is_integral(left) && is_integral(right))
                .then_some(TypeSymbol::Special(shift_result(left)))
        }
    }
}

/// The common numeric type both operands are converted to under binary numeric promotion
/// (14.2.6.2), for the operators where it applies -- so [`Binder::bind_binary`] can insert the
/// widening conversions the emitter lowers (ECMA-335 requires both operands of `add`/`ceq` to
/// share a type, so `r8` + `i4` cannot be mixed). `None` for string concatenation, the shift operators, the logical
/// operators, and any non-numeric operand pair -- which promote differently or not at all and are
/// handled in `bind_binary` directly.
fn binary_operand_promotion(
    operator: BinaryOperator,
    left: &TypeSymbol,
    right: &TypeSymbol,
) -> Option<SpecialType> {
    use BinaryOperator as Op;
    let (left, right) = (as_special(left)?, as_special(right)?);
    match operator {
        Op::Add if left == SpecialType::String || right == SpecialType::String => None,
        Op::Add
        | Op::Subtract
        | Op::Multiply
        | Op::Divide
        | Op::Modulo
        | Op::LessThan
        | Op::GreaterThan
        | Op::LessThanOrEqual
        | Op::GreaterThanOrEqual
        | Op::Equal
        | Op::NotEqual => binary_numeric_promotion(left, right),
        Op::BitwiseAnd | Op::BitwiseOr | Op::BitwiseXor => (is_integral(left) && is_integral(right))
            .then(|| binary_numeric_promotion(left, right))
            .flatten(),
        Op::LeftShift | Op::RightShift | Op::LogicalAnd | Op::LogicalOr => None,
    }
}

/// The outcome of looking a member up on a type.
/// How a member-access receiver was written, for the static/instance check.
#[derive(Clone, Copy)]
enum Receiver {
    /// Through a type name, e.g. `Type.Member`.
    ViaType,
    /// Through `this`/`base` (implicit or explicit): no static/instance error.
    ImplicitThis,
    /// Through an instance value, e.g. `obj.Member`.
    Instance,
}

/// Categorizes a bound receiver for the static/instance check (CS0120/CS0176).
fn receiver_category(receiver: &BoundExpr) -> Receiver {
    match &receiver.kind {
        BoundExprKind::TypeReference(_) => Receiver::ViaType,
        BoundExprKind::This | BoundExprKind::Base => Receiver::ImplicitThis,
        _ => Receiver::Instance,
    }
}

/// `Type.member`, for a diagnostic message.
fn qualified_member(declaring: &TypeSymbol, member: &str) -> Box<str> {
    let mut qualified = declaring.to_string();
    qualified.push('.');
    qualified.push_str(member);
    qualified.into()
}

/// `Type.method(p1, p2)`, for a diagnostic message that names a METHOD -- csc spells a method
/// member with its parameter type list, so `B.F` alone is not the same string as `B.F(int, string)`
/// and a message quoting the bare name is not byte-identical.
///
/// A `ref`/`out` parameter renders as `ref T` either way: `MethodSymbol.parameters` records
/// by-reference-ness but not WHICH of the two spellings the source wrote, so an `out` parameter
/// prints `ref`. That is the same missing fact that blocks CS1620 and CS7036, asserted here rather
/// than papered over, and it moves the day parameter modes join the symbol table.
/// A type's own name without its namespace or enclosing types -- `Inner` for `C.Inner`. A
/// constructor's display name is the type's simple name, so `C.Inner`'s reads `C.Inner.Inner(...)`.
fn simple_type_name(ty: &TypeSymbol) -> String {
    match ty {
        TypeSymbol::Named(parts) => parts.last().map(|part| part.to_string()).unwrap_or_default(),
        other => other.to_string(),
    }
}

/// A member's display signature with each parameter's MODIFIER, e.g. `C.M(int, out int)` or
/// `B.B(params int[])` -- what csc puts in `CS7036`.
///
/// This is [`qualified_method`] with the modes filled in. The two are kept apart on purpose: the
/// older one renders from a bare `&[TypeSymbol]`, which is all a `MethodReference` carries, and
/// its callers are at parity today. Once every one of them holds a `MethodSymbol` they should
/// collapse into this.
///
/// KNOWN GAP, measured: csc qualifies the type MINIMALLY FOR THE ERROR SITE, so a method in
/// `namespace N { class C }` reported from inside `C` reads `C.M(int)` where this renders
/// `N.C.M(int)`. Enclosing TYPES are kept by both (`C.Inner.Inner(int, int)`) -- it is only the
/// namespace that csc drops, and only when the site can see the type without it. Closing this
/// needs the using-scope at the report site, not a different string builder, so it is written
/// down rather than approximated: dropping the namespace unconditionally would be wrong for a
/// type the site could NOT have named unqualified.
fn qualified_method_with_modes(
    declaring: &TypeSymbol,
    member: &str,
    method: &MethodSymbol,
) -> Box<str> {
    let mut qualified = declaring.to_string();
    qualified.push('.');
    qualified.push_str(member);
    if !method.type_parameters.is_empty() {
        qualified.push('<');
        for (index, parameter) in method.type_parameters.iter().enumerate() {
            if index > 0 {
                qualified.push_str(", ");
            }
            qualified.push_str(parameter);
        }
        qualified.push('>');
    }
    qualified.push('(');
    for (index, parameter) in method.parameters.iter().enumerate() {
        if index > 0 {
            qualified.push_str(", ");
        }
        if method.is_params && index + 1 == method.parameters.len() {
            qualified.push_str("params ");
        }
        match method.parameter_mode(index) {
            Some(ParameterMode::Out) => {
                qualified.push_str("out ");
                qualified.push_str(&strip_byref(parameter));
            }
            _ => qualified.push_str(&parameter.to_string()),
        }
    }
    qualified.push(')');
    qualified.into()
}

/// A by-reference parameter's type rendered WITHOUT its `ref` prefix, so an `out` parameter can
/// be spelled `out int` rather than `out ref int`.
fn strip_byref(parameter: &TypeSymbol) -> alloc::string::String {
    match parameter {
        TypeSymbol::ByRef(inner) => inner.to_string(),
        other => other.to_string(),
    }
}

fn qualified_method(declaring: &TypeSymbol, member: &str, parameters: &[TypeSymbol]) -> Box<str> {
    let mut qualified = declaring.to_string();
    qualified.push('.');
    qualified.push_str(member);
    qualified.push('(');
    for (index, parameter) in parameters.iter().enumerate() {
        if index > 0 {
            qualified.push_str(", ");
        }
        qualified.push_str(&parameter.to_string());
    }
    qualified.push(')');
    qualified.into()
}

/// What a named attribute argument's name resolves to, from
/// [`Binder::named_attribute_argument_target`].
/// Each "found" variant carries the type that DECLARES the member, which is not always the
/// attribute class -- an inherited field is declared by a base. The unused-field pass keys on the
/// declaring type, so recording the attribute's own type instead would leave an inherited field
/// looking unassigned.
pub(crate) enum NamedArgumentTarget {
    /// A public, instance, writable field or read-write property: legal, report nothing.
    Valid(TypeSymbol),
    /// Reachable in principle but not from here -- `CS0122`.
    Inaccessible(TypeSymbol),
    /// Reachable and unusable: a non-public, static, readonly or const field; a property that is
    /// not public, is static, or lacks an accessor; a method; a nested type -- `CS0617`.
    NotAValidTarget(TypeSymbol),
    /// No member of that name anywhere in the attribute class's chain -- `CS0246`.
    Missing,
}

pub(crate) enum MemberResolution {
    /// A field, with its resolved reference.
    Field(FieldReference),
    /// A property, with its declaring type, type, accessibility, and staticness.
    Property {
        /// The type that declares the property.
        declaring_type: TypeSymbol,
        /// The property's type.
        ty: TypeSymbol,
        /// The property's accessibility.
        accessibility: Accessibility,
        /// Whether the property is `static`.
        is_static: bool,
    },
    /// One or more methods of that name (a method group).
    MethodGroup,
    /// The type is known but has no such member; carries the type's display name.
    NoSuchMember(String),
    /// The type is not in the model, so members cannot be resolved.
    Unknown,
}

/// A named-type symbol from a (non-empty, dotted) namespace and a simple name.
fn qualified_type_symbol(namespace: &str, name: &str) -> TypeSymbol {
    let mut parts: Vec<Box<str>> = namespace.split('.').map(Box::from).collect();
    parts.push(Box::from(name));
    TypeSymbol::Named(parts.into_boxed_slice())
}

/// A named-type symbol from a full dotted name (e.g. `Outer` or `Ns.Outer`), as a type's
/// `enclosing` is stored -- the inverse of [`TypeSymbol`]'s `Display`.
fn named_symbol_from_dotted(dotted: &str) -> TypeSymbol {
    TypeSymbol::Named(dotted.split('.').map(Box::from).collect())
}

/// A named-type symbol from a namespace (possibly empty) and a simple name.
fn type_symbol_in(namespace: &str, name: &str) -> TypeSymbol {
    if namespace.is_empty() {
        TypeSymbol::Named([Box::from(name)].into())
    } else {
        qualified_type_symbol(namespace, name)
    }
}

/// The full dotted name of a type in a namespace (the bare name when global).
fn full_type_name(namespace: &str, name: &str) -> Box<str> {
    if namespace.is_empty() {
        Box::from(name)
    } else {
        let mut full = String::from(namespace);
        full.push('.');
        full.push_str(name);
        full.into()
    }
}

/// An error placeholder expression, used for recovery.
fn error_expr() -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Error,
        ty: TypeSymbol::Error,
    }
}

/// The compile-time constant value of a predefined integral type's `MaxValue` or
/// `MinValue` member (4.1.5), as a two's-complement `i64` so an `ldc.i4`/`ldc.i8`
/// reproduces the right bits (`uint.MaxValue` -> -1 as `i32`, `ulong.MaxValue` -> -1
/// as `i64`). `None` for any other type or member name.
fn predefined_constant(special: SpecialType, member: &str) -> Option<i64> {
    use SpecialType as S;
    let (min, max): (i64, i64) = match special {
        S::SByte => (i8::MIN as i64, i8::MAX as i64),
        S::Byte => (0, u8::MAX as i64),
        S::Int16 => (i16::MIN as i64, i16::MAX as i64),
        S::UInt16 => (0, u16::MAX as i64),
        S::Char => (0, u16::MAX as i64),
        S::Int32 => (i32::MIN as i64, i32::MAX as i64),
        S::UInt32 => (0, u32::MAX as i64),
        S::Int64 => (i64::MIN, i64::MAX),
        S::UInt64 => (0, u64::MAX as i64),
        _ => return None,
    };
    match member {
        "MaxValue" => Some(max),
        "MinValue" => Some(min),
        _ => None,
    }
}

/// Whether `expr` is a constant of type `int`/`long` whose value fits the integral
/// `target` -- the implicit constant expression conversion (13.1.7), which lets
/// `byte b = 10` and `b[0] = 10` compile without a cast.
fn implicit_constant_conversion(expr: &BoundExpr, target: &TypeSymbol) -> bool {
    let (TypeSymbol::Special(source), TypeSymbol::Special(target)) = (&expr.ty, target) else {
        return false;
    };
    if !matches!(source, SpecialType::Int32 | SpecialType::Int64) {
        return false;
    }
    match constant_int_value(expr) {
        Some(value) => constant_fits(value, *target),
        None => false,
    }
}

/// The rendered value of an int/long CONSTANT that is out of range of the integral type it is
/// narrowing to under the constant-expression conversion (13.1.7) -- so an out-of-range one is
/// CS0031, not the generic narrowing CS0266. An `int` constant converts to
/// sbyte/byte/short/ushort/uint/ulong; a `long` constant only to ulong; every other target (char,
/// a floating type, an identity or widening target, or a `uint`/other non-int/long source) is
/// outside this rule, so those keep the CS0266/CS0029 path. `None` when the rule does not apply or
/// the value is in range.
fn constant_out_of_range(value: &BoundExpr, target: &TypeSymbol) -> Option<Box<str>> {
    use SpecialType as S;
    let (TypeSymbol::Special(source), TypeSymbol::Special(target)) = (&value.ty, target) else {
        return None;
    };
    let eligible = match source {
        S::Int32 => matches!(
            target,
            S::SByte | S::Byte | S::Int16 | S::UInt16 | S::UInt32 | S::UInt64
        ),
        S::Int64 => *target == S::UInt64,
        _ => false,
    };
    if !eligible {
        return None;
    }
    let folded = constant_int_value(value)?;
    if constant_fits(folded, *target) {
        None
    } else {
        Some(alloc::string::ToString::to_string(&folded).into())
    }
}

/// Whether `ty` is `uint` -- the only unsigned type whose binary numeric promotion with a
/// signed operand widens to `long`, so a fitting `int` constant is instead retyped `uint` (13.1.7).
fn is_uint(ty: &TypeSymbol) -> bool {
    matches!(ty, TypeSymbol::Special(SpecialType::UInt32))
}

/// Whether `expr` is an `int` constant whose value fits the unsigned integral `target`
/// (13.1.7 constant expression conversion, as applied by a binary operator's overload resolution).
fn int_constant_fits(expr: &BoundExpr, target: SpecialType) -> bool {
    matches!(expr.ty, TypeSymbol::Special(SpecialType::Int32))
        && constant_int_value(expr).is_some_and(|value| constant_fits(value, target))
}

/// The compile-time value of a constant integer expression (14.15): an integer or
/// character literal, a member access that folded to a constant (a `const` field, an
/// enum member, a predefined `MaxValue`/`MinValue`), a `+`/`-` on one, or -- through the
/// full constant evaluator -- an arithmetic/bitwise operation, a cast, or a conditional.
/// `None` for a non-constant expression (or a `bool`/`string` constant, which is not an
/// integer). Case labels, the constant-expression conversions, and overload resolution
/// all fold through this.
pub(crate) fn constant_int_value(expr: &BoundExpr) -> Option<i64> {
    match &expr.kind {
        BoundExprKind::Literal(Literal::Integer { value, .. }) => i64::try_from(*value).ok(),
        BoundExprKind::Literal(Literal::Character(unit)) => Some(i64::from(*unit)),
        BoundExprKind::Literal(Literal::Boolean(value)) => Some(i64::from(*value)),
        BoundExprKind::FieldAccess {
            field: Some(field), ..
        } => field.constant.as_ref().and_then(literal_int_value),
        BoundExprKind::Unary { operator, operand } => match operator {
            UnaryOperator::Plus => constant_int_value(operand),
            UnaryOperator::Minus => constant_int_value(operand)?.checked_neg(),
            _ => None,
        },
        BoundExprKind::Binary { .. }
        | BoundExprKind::Cast { .. }
        | BoundExprKind::Conditional { .. } => match constant_literal_value(expr)? {
            Literal::Integer { value, .. } => Some(value as i64),
            Literal::Character(unit) => Some(i64::from(unit)),
            Literal::Boolean(value) => Some(i64::from(value)),
            _ => None,
        },
        _ => None,
    }
}

/// Coerces a constant integer `value` to the integral or `char` type `target`, wrapping to
/// the target's width (14.15 over a numeric/char cast, unchecked). `None` for a non-integral
/// target (a floating or decimal cast is not folded).
/// Casts a constant literal to a `Special` target type at compile time (6.2.1): a real operand
/// rounds to a floating-point target's precision or truncates toward zero to an integral target; an
/// integer operand goes through [`coerce_constant`]. Used to fold `(T)constant`.
pub(crate) fn cast_constant(operand: &Literal, target: SpecialType) -> Option<Literal> {
    use SpecialType as S;
    match operand {
        Literal::Real { bits, .. } => {
            let value = f64::from_bits(*bits);
            match target {
                S::Single => Some(Literal::Real {
                    bits: f64::from(value as f32).to_bits(),
                    suffix: RealSuffix::Float,
                }),
                S::Double => Some(Literal::Real {
                    bits: value.to_bits(),
                    suffix: RealSuffix::Double,
                }),
                _ => coerce_constant(value as i64, target),
            }
        }
        _ => coerce_constant(literal_int_value(operand)?, target),
    }
}

pub(crate) fn coerce_constant(value: i64, target: SpecialType) -> Option<Literal> {
    use SpecialType as S;
    Some(match target {
        S::SByte => integer_literal(i64::from(value as i8)),
        S::Byte => integer_literal(i64::from(value as u8)),
        S::Int16 => integer_literal(i64::from(value as i16)),
        S::UInt16 => integer_literal(i64::from(value as u16)),
        S::Int32 => integer_literal(i64::from(value as i32)),
        S::UInt32 => integer_literal(i64::from(value as u32)),
        S::Int64 | S::UInt64 => integer_literal(value),
        S::Char => Literal::Character(value as u16),
        S::Single => Literal::Real {
            bits: f64::from(value as f32).to_bits(),
            suffix: RealSuffix::Float,
        },
        S::Double => Literal::Real {
            bits: (value as f64).to_bits(),
            suffix: RealSuffix::Double,
        },
        _ => return None,
    })
}

/// The compile-time constant value of a bound expression as a [`Literal`] (14.15), the full
/// evaluator: a literal, a folded `const`/enum member access, an implicit conversion of a
/// constant, a unary or binary operation, a numeric/char cast, or a conditional whose
/// condition folds to a `bool`. `None` for anything not a constant expression. Unlike
/// [`constant_int_value`] this keeps the value's kind (so a `bool`, `char`, or `string`
/// constant round-trips), for a local constant's stored value.
pub(crate) fn constant_literal_value(expr: &BoundExpr) -> Option<Literal> {
    use crate::declaration::{fold_const_binary, fold_const_unary};
    match &expr.kind {
        BoundExprKind::Literal(literal) => Some(literal.clone()),
        BoundExprKind::FieldAccess {
            field: Some(field), ..
        } => field.constant.clone(),
        BoundExprKind::Conversion { operand, .. } => {
            let inner = constant_literal_value(operand)?;
            match (&expr.ty, &inner) {
                (
                    TypeSymbol::Special(target @ (SpecialType::Single | SpecialType::Double)),
                    Literal::Integer { .. },
                ) => coerce_constant(literal_int_value(&inner)?, *target),
                _ => Some(inner),
            }
        }
        BoundExprKind::Unary { operator, operand } => {
            fold_const_unary(*operator, &constant_literal_value(operand)?)
        }
        BoundExprKind::Binary {
            operator,
            left,
            right,
            ..
        } => fold_const_binary(
            *operator,
            &constant_literal_value(left)?,
            &constant_literal_value(right)?,
        ),
        BoundExprKind::Cast { operand, .. } => match &expr.ty {
            TypeSymbol::Special(target) => {
                cast_constant(&constant_literal_value(operand)?, *target)
            }
            _ => None,
        },
        BoundExprKind::Conditional {
            condition,
            when_true,
            when_false,
        } => match constant_literal_value(condition)? {
            Literal::Boolean(true) => constant_literal_value(when_true),
            Literal::Boolean(false) => constant_literal_value(when_false),
            _ => None,
        },
        _ => None,
    }
}

/// An integer constant literal of the given value (the form a folded enum member /
/// `const` field / predefined `MaxValue` takes); its `i64` round-trips via `value as u64`.
pub(crate) fn integer_literal(value: i64) -> Literal {
    Literal::Integer {
        value: value as u64,
        suffix: lamella_syntax::token::IntegerSuffix::None,
    }
}

/// The `i64` value of an integral constant literal -- an integer (its two's-complement
/// bits), a `char`, or a `bool` -- the form case labels, constant-conversion checks, and the
/// enum-member value table compare. `None` for a real, string, or null literal.
pub fn literal_int_value(literal: &Literal) -> Option<i64> {
    match literal {
        Literal::Integer { value, .. } => Some(*value as i64),
        Literal::Character(unit) => Some(i64::from(*unit)),
        Literal::Boolean(b) => Some(i64::from(*b)),
        Literal::Real { .. } | Literal::Decimal { .. } | Literal::String(_) | Literal::Null => None,
    }
}

/// Whether the constant `value` is in range of the integral `target` (13.1.7). `char`
/// is excluded: an int constant needs an explicit cast to `char`.
fn constant_fits(value: i64, target: SpecialType) -> bool {
    use SpecialType as S;
    match target {
        S::SByte => i8::try_from(value).is_ok(),
        S::Byte => u8::try_from(value).is_ok(),
        S::Int16 => i16::try_from(value).is_ok(),
        S::UInt16 => u16::try_from(value).is_ok(),
        S::UInt32 => u32::try_from(value).is_ok(),
        S::UInt64 => value >= 0,
        _ => false,
    }
}

/// The outcome of overload resolution over a method group (14.4.2).
enum OverloadResult {
    /// A unique best overload.
    Resolved(MethodSymbol),
    /// Two or more applicable overloads with no unique best.
    Ambiguous,
    /// No overload accepts this number of arguments.
    WrongArgumentCount,
    /// A count matches but an argument does not convert to the parameter.
    BadArgument {
        /// The 0-based argument position.
        index: usize,
        /// The argument type.
        from: TypeSymbol,
        /// The parameter type.
        to: TypeSymbol,
    },
}

/// The type an argument carries into overload resolution (14.4.2.1): a `ref`/`out`
/// argument is a byref (`T&`), so it matches only a parameter of the identical
/// passing mode and type; any other argument contributes its expression type.
fn argument_type(argument: &BoundExpr) -> TypeSymbol {
    if matches!(argument.kind, BoundExprKind::Ref { .. }) {
        TypeSymbol::ByRef(Box::new(argument.ty.clone()))
    } else {
        argument.ty.clone()
    }
}

/// Whether an argument -- its type `arg_ty`, plus `arg_const` (its compile-time integer value
/// when it is a constant) -- is applicable to parameter `param`: a standard implicit conversion,
/// or the implicit constant-expression conversion (13.1.7) for an `int`/`long` constant whose
/// value fits an integral parameter (so `Set(0x518, m)` binds when `Set` takes `uint`).
/// A byref (`ref`/`out`) argument or parameter applies only with the identical passing
/// mode and type on the other side (14.4.2.1) -- no conversion crosses a byref boundary.
fn arg_applicable(
    model: &Model,
    arg_ty: &TypeSymbol,
    arg_const: Option<i64>,
    param: &TypeSymbol,
) -> bool {
    if is_arglist_marker(arg_ty) {
        return false;
    }
    if matches!(arg_ty, TypeSymbol::ByRef(_)) || matches!(param, TypeSymbol::ByRef(_)) {
        return arg_ty == param;
    }
    if converts(model, arg_ty, param) || user_implicit_converts(model, arg_ty, param) {
        return true;
    }
    matches!(
        arg_ty,
        TypeSymbol::Special(SpecialType::Int32 | SpecialType::Int64)
    ) && match param {
        TypeSymbol::Special(target) => arg_const.is_some_and(|value| constant_fits(value, *target)),
        _ => false,
    }
}

/// Whether a user-defined implicit conversion (`op_Implicit`) takes `from` to `to` (17.9.3): a
/// one-parameter operator declared on either type. An argument with such a conversion to its
/// parameter is applicable (14.4.2.1). Conversion operators are not inherited, so the direct
/// members of each type suffice.
fn user_implicit_converts(model: &Model, from: &TypeSymbol, to: &TypeSymbol) -> bool {
    [from, to].into_iter().any(|owner| {
        model.get_by_symbol(owner).is_some_and(|info| {
            info.methods.iter().any(|method| {
                &*method.name == "op_Implicit"
                    && method.parameters.len() == 1
                    && &method.parameters[0] == from
                    && &method.return_type == to
            })
        })
    })
}

/// Chooses the overload for a call (14.4.2): the unique best among the applicable candidates,
/// or the diagnostic-bearing outcome otherwise. Conversions resolve against `model` so a
/// derived argument matches a base parameter; `arg_constants` carries each argument's
/// compile-time integer value (or `None`) to enable the constant conversion (13.1.7) in
/// applicability. An empty `arg_constants` means "no constants" (e.g. the operator paths).
fn resolve_overload(
    model: &Model,
    candidates: &[MethodSymbol],
    arguments: &[TypeSymbol],
    arg_constants: &[Option<i64>],
) -> OverloadResult {
    let applicable: Vec<&MethodSymbol> = candidates
        .iter()
        .filter(|candidate| is_applicable(model, candidate, arguments, arg_constants))
        .collect();
    if let Some(best) = best_candidate(model, &applicable, arguments, arg_constants) {
        return OverloadResult::Resolved(best.clone());
    }
    if !applicable.is_empty() {
        return OverloadResult::Ambiguous;
    }
    let mut expanded = None;
    for candidate in candidates {
        if candidate.is_vararg
            && candidate.parameters.len() + 1 == arguments.len()
            && arguments.last().is_some_and(is_arglist_marker)
        {
            if let Some(bad) = first_bad_normal(
                model,
                candidate,
                &arguments[..arguments.len() - 1],
                arg_constants,
            ) {
                return bad;
            }
        }
        if !candidate.is_vararg && candidate.parameters.len() == arguments.len() {
            if let Some(bad) = first_bad_normal(model, candidate, arguments, arg_constants) {
                return bad;
            }
        }
        if candidate.is_params && expanded.is_none() {
            expanded = first_bad_expanded(model, candidate, arguments, arg_constants);
        }
    }
    expanded.unwrap_or(OverloadResult::WrongArgumentCount)
}

/// The first argument that does not convert to `method`'s same-position parameter
/// (the arities already match), as a [`OverloadResult::BadArgument`]; `None` when
/// every argument converts -- in which case the method would have been applicable.
fn first_bad_normal(
    model: &Model,
    method: &MethodSymbol,
    arguments: &[TypeSymbol],
    arg_constants: &[Option<i64>],
) -> Option<OverloadResult> {
    arguments
        .iter()
        .zip(&method.parameters)
        .enumerate()
        .find_map(|(index, (argument, parameter))| {
            (!arg_applicable(
                model,
                argument,
                arg_constants.get(index).copied().flatten(),
                parameter,
            ))
            .then(|| OverloadResult::BadArgument {
                index,
                from: argument.clone(),
                to: parameter.clone(),
            })
        })
}

/// The first argument that does not convert to a `params` `method` in EXPANDED form
/// -- each leading argument against its fixed parameter, each trailing argument
/// against the array element type (14.4.2.1) -- as a [`OverloadResult::BadArgument`].
/// `None` when the method cannot take this many arguments expanded, its last
/// parameter is not an array, or every argument converts.
fn first_bad_expanded(
    model: &Model,
    method: &MethodSymbol,
    arguments: &[TypeSymbol],
    arg_constants: &[Option<i64>],
) -> Option<OverloadResult> {
    let fixed = method.parameters.len().saturating_sub(1);
    if arguments.len() < fixed {
        return None;
    }
    let TypeSymbol::Array { element, .. } = &method.parameters[fixed] else {
        return None;
    };
    arguments.iter().enumerate().find_map(|(index, argument)| {
        let parameter = if index < fixed {
            &method.parameters[index]
        } else {
            &**element
        };
        (!arg_applicable(
            model,
            argument,
            arg_constants.get(index).copied().flatten(),
            parameter,
        ))
        .then(|| OverloadResult::BadArgument {
            index,
            from: argument.clone(),
            to: parameter.clone(),
        })
    })
}

/// Whether a method is applicable to the arguments: in normal form the counts match
/// and every argument converts to its parameter (14.4.2.1); a `params` method is also
/// applicable in expanded form, where the trailing arguments fill its array.
fn is_applicable(
    model: &Model,
    method: &MethodSymbol,
    arguments: &[TypeSymbol],
    arg_constants: &[Option<i64>],
) -> bool {
    is_normal_applicable(model, method, arguments, arg_constants)
        || (method.is_params && is_applicable_expanded(model, method, arguments, arg_constants))
}

/// Whether a method applies in NORMAL form: the counts match and every argument converts
/// to its parameter (14.4.2.1). (Expanded `params` form is [`is_applicable_expanded`].)
/// A vararg member's sentinel behaves as one required trailing parameter that only an
/// `__arglist(...)` argument matches (csc: CS7036 when missing), so its effective arity
/// is `parameters + 1` and the marker argument must close the list.
fn is_normal_applicable(
    model: &Model,
    method: &MethodSymbol,
    arguments: &[TypeSymbol],
    arg_constants: &[Option<i64>],
) -> bool {
    if method.is_vararg {
        let Some((last, fixed)) = arguments.split_last() else {
            return false;
        };
        return is_arglist_marker(last)
            && method.parameters.len() == fixed.len()
            && fixed
                .iter()
                .zip(&method.parameters)
                .enumerate()
                .all(|(i, (argument, parameter))| {
                    arg_applicable(
                        model,
                        argument,
                        arg_constants.get(i).copied().flatten(),
                        parameter,
                    )
                });
    }
    method.parameters.len() == arguments.len()
        && arguments
            .iter()
            .zip(&method.parameters)
            .enumerate()
            .all(|(i, (argument, parameter))| {
                arg_applicable(model, argument, arg_constants.get(i).copied().flatten(), parameter)
            })
}

/// Whether a `params` method applies in expanded form (14.4.2.1): the leading fixed
/// parameters convert, and every trailing argument converts to the array's element type.
fn is_applicable_expanded(
    model: &Model,
    method: &MethodSymbol,
    arguments: &[TypeSymbol],
    arg_constants: &[Option<i64>],
) -> bool {
    let fixed = method.parameters.len().saturating_sub(1);
    if arguments.len() < fixed {
        return false;
    }
    if !arguments[..fixed]
        .iter()
        .zip(&method.parameters[..fixed])
        .enumerate()
        .all(|(i, (argument, parameter))| {
            arg_applicable(model, argument, arg_constants.get(i).copied().flatten(), parameter)
        })
    {
        return false;
    }
    let TypeSymbol::Array { element, .. } = &method.parameters[fixed] else {
        return false;
    };
    arguments[fixed..].iter().enumerate().all(|(offset, argument)| {
        arg_applicable(
            model,
            argument,
            arg_constants.get(fixed + offset).copied().flatten(),
            element,
        )
    })
}

/// The single applicable candidate better than every other, or `None` when none
/// is uniquely best.
fn best_candidate<'a>(
    model: &Model,
    applicable: &[&'a MethodSymbol],
    arguments: &[TypeSymbol],
    arg_constants: &[Option<i64>],
) -> Option<&'a MethodSymbol> {
    applicable.iter().copied().find(|&candidate| {
        applicable.iter().all(|&other| {
            core::ptr::eq(candidate, other)
                || is_better(model, candidate, other, arguments, arg_constants)
        })
    })
}

/// Whether `c1` is a better function member than `c2` for the arguments: no worse
/// a parameter for every argument and strictly better for at least one, using the
/// better-conversion-target rule (14.4.2.2, 14.4.2.3 simplified).
fn is_better(
    model: &Model,
    c1: &MethodSymbol,
    c2: &MethodSymbol,
    arguments: &[TypeSymbol],
    arg_constants: &[Option<i64>],
) -> bool {
    let c1_normal = is_normal_applicable(model, c1, arguments, arg_constants);
    let c2_normal = is_normal_applicable(model, c2, arguments, arg_constants);
    if c1_normal != c2_normal {
        return c1_normal;
    }
    let mut strictly_better_somewhere = false;
    let compared = arguments
        .len()
        .min(c1.parameters.len())
        .min(c2.parameters.len());
    for index in 0..compared {
        let (p1, p2) = (&c1.parameters[index], &c2.parameters[index]);
        if p1 == p2 {
            continue;
        }
        let arg = &arguments[index];
        let (std1, std2) = (converts(model, arg, p1), converts(model, arg, p2));
        if std1 != std2 {
            if std1 {
                strictly_better_somewhere = true;
                continue;
            }
            return false;
        }
        if converts(model, p1, p2) || signed_preferred(p1, p2) {
            strictly_better_somewhere = true;
        } else {
            return false;
        }
    }
    strictly_better_somewhere
}

/// The signed/unsigned better-conversion special cases (14.4.2.3): a signed integral
/// target is better than a wider-or-equal unsigned one when neither converts to the
/// other (`sbyte` over byte/ushort/uint/ulong; `short` over ushort/uint/ulong; `int`
/// over uint/ulong; `long` over ulong). This is what makes `Console.WriteLine(byte)`
/// resolve to the `int` overload rather than report a spurious CS0121.
fn signed_preferred(p1: &TypeSymbol, p2: &TypeSymbol) -> bool {
    use SpecialType as S;
    let (Some(a), Some(b)) = (as_special(p1), as_special(p2)) else {
        return false;
    };
    matches!(
        (a, b),
        (S::SByte, S::Byte | S::UInt16 | S::UInt32 | S::UInt64)
            | (S::Int16, S::UInt16 | S::UInt32 | S::UInt64)
            | (S::Int32, S::UInt32 | S::UInt64)
            | (S::Int64, S::UInt64)
    )
}

/// The special type of `ty`, if it is one.
fn as_special(ty: &TypeSymbol) -> Option<SpecialType> {
    match ty {
        TypeSymbol::Special(special) => Some(*special),
        _ => None,
    }
}

/// The dotted name an expression spells, if it spells one: a simple name, or a member-access chain
/// of them (`N.Box`). `None` for anything else, because a constructed type's name is a NAME and an
/// expression in that position that is not one -- `f().Box<int>` -- names no type.
fn dotted_name_parts(expr: &Expr) -> Option<Vec<Box<str>>> {
    match &expr.kind {
        ExprKind::Name(name) => Some(alloc::vec![name.clone()]),
        ExprKind::MemberAccess { receiver, name } => {
            let mut parts = dotted_name_parts(receiver)?;
            parts.push(name.clone());
            Some(parts)
        }
        _ => None,
    }
}

/// The type a member declared by `info` is NAMED THROUGH, reached from a receiver of type
/// `receiver_ty`: the receiver itself when that is already an instantiation, and otherwise the
/// declaration's own [`self_instantiation`] -- so `` H`1 `` is never the answer for a generic type.
///
/// One walk resolves fields and properties, a second resolves methods, a third finds a property's
/// two accessor declarers, and each spelled this closure out again. The first two gained the
/// self-instantiation case and the third did not, so a generic type's own PROPERTY stayed erased
/// while its fields and methods were repaired -- the same defect surviving in the one position
/// nobody edited. Call this; do not write the match again.
fn member_declaring_type(receiver_ty: &TypeSymbol, info: &TypeInfo) -> TypeSymbol {
    let declaring = type_symbol_in(&info.namespace, &info.name);
    match receiver_ty {
        TypeSymbol::Instantiation { .. } => receiver_ty.clone(),
        _ => self_instantiation(&declaring, &info.type_parameters).unwrap_or(declaring),
    }
}

/// The DEFINITION a type symbol was built from -- `` H`1 `` for `H<int>` and for `` H<!0> `` alike
/// -- and the symbol itself for every non-instantiated form.
///
/// The inverse of [`self_instantiation`], for the questions whose answer a type ARGUMENT cannot
/// change: accessibility is the whole of that class today.
fn definition_of(ty: &TypeSymbol) -> TypeSymbol {
    match ty {
        TypeSymbol::Instantiation {
            definition,
            arguments,
        } => crate::symbols::definition_symbol(definition, arguments.len()),
        _ => ty.clone(),
    }
}

/// The instantiation a generic DEFINITION names its own members through -- `H<!0, !1>` for
/// `class H<T, U>` -- or `None` when `declaring` is not a plain name or the type is not generic.
///
/// **A DEFINITION REACHED IN ITS OWN BODY IS STILL AN INSTANTIATION, AND NAMING IT AS THE BARE
/// DEFINITION LOSES DATA RATHER THAN METADATA.** A member named on the definition reaches its own
/// field through a `FieldDef`, where csc names it through a `TypeSpec` over the type's own
/// parameters and emits a `MemberRef` parented by that -- which is why csc's images for a generic
/// type carry a second `TypeSpec` that ours did not.
///
/// **THE FIRST FIELD SURVIVES THE WRONG FORM AND EVERY LATER ONE DOES NOT.** A one-field type
/// passes, and so does a write from outside the type, so the shapes that would show it are exactly
/// the ones a small probe uses. A lookup that SUCCEEDS on the wrong row is worse than one that
/// misses, because nothing reports it.
///
/// **`declaring` ARRIVES MANGLED AND AN INSTANTIATION'S `definition` IS NOT**, so the last part is
/// put back through [`unmangled_type_name`]: `` Holder`1 `` here would have every consumer mangle
/// the arity in a second time and ask the model for `` Holder`1`1 ``. That misses, the field falls
/// back to a path with no `TypeSpec` to parent on, and the write is refused with a message about a
/// field outside this module -- which is the shape this whole function exists to stop.
fn self_instantiation(declaring: &TypeSymbol, type_parameters: &[Box<str>]) -> Option<TypeSymbol> {
    let TypeSymbol::Named(parts) = declaring else {
        return None;
    };
    if type_parameters.is_empty() {
        return None;
    }
    let (last, enclosing) = parts.split_last()?;
    let mut definition: Vec<Box<str>> = enclosing.to_vec();
    definition.push(unmangled_type_name(last));
    Some(TypeSymbol::Instantiation {
        definition: definition.into_boxed_slice(),
        arguments: type_parameters
            .iter()
            .map(|parameter| TypeSymbol::Named(alloc::vec![parameter.clone()].into_boxed_slice()))
            .collect(),
    })
}

/// A named type's dotted name (`["NS", "I"]` -> "NS.I"); empty for a non-named type.
fn dotted_type_name(ty: &TypeSymbol) -> String {
    match ty {
        TypeSymbol::Named(parts) => {
            let mut name = String::new();
            for part in parts.iter() {
                if !name.is_empty() {
                    name.push('.');
                }
                name.push_str(part);
            }
            name
        }
        _ => String::new(),
    }
}

/// Whether `name` is a compiler-emitted special member -- a property/event/indexer accessor,
/// an operator, or a constructor -- which the CS0534 abstract-member check skips (its message
/// formats an ordinary method signature, and source properties/events are modeled separately).
fn is_special_member_name(name: &str) -> bool {
    name.starts_with("get_")
        || name.starts_with("set_")
        || name.starts_with("add_")
        || name.starts_with("remove_")
        || name.starts_with("op_")
        || name == ".ctor"
        || name == ".cctor"
}

/// Records the most-derived declaration of one accessor slot, keyed by name and index types. The
/// walk runs derived-first, so the first entry for a key wins and a base's abstract accessor is
/// forgotten once a derived type declares it -- which is exactly what "implemented" means.
fn push_accessor_slot(
    slots: &mut Vec<(Box<str>, Vec<TypeSymbol>, bool, bool, Box<str>)>,
    key: &str,
    params: &[TypeSymbol],
    is_abstract: bool,
    declared_here: bool,
    member: Box<str>,
) {
    if slots
        .iter()
        .any(|(seen, seen_params, ..)| **seen == *key && *seen_params == *params)
    {
        return;
    }
    slots.push((
        key.into(),
        params.to_vec(),
        is_abstract,
        declared_here,
        member,
    ));
}

/// How csc names an unimplemented accessor in a `CS0534` message: `B.P.get`, `B.this[int].set`,
/// `B.E.add`.
fn accessor_member(declaring: &str, name: &str, accessor: &str) -> Box<str> {
    alloc::format!("{declaring}.{name}.{accessor}").into()
}

/// An indexer as it appears inside such a message: `this[int]`.
fn indexer_display(indices: &[TypeSymbol]) -> String {
    let mut rendered = String::from("this[");
    for (index, ty) in indices.iter().enumerate() {
        if index > 0 {
            rendered.push_str(", ");
        }
        rendered.push_str(&alloc::format!("{ty}"));
    }
    rendered.push(']');
    rendered
}

/// The qualified signature of an abstract member for a `CS0534` message (`B.M(int)`): the
/// declaring type's simple name, the method name, and the parameter types.
fn abstract_member_signature(declaring: &str, name: &str, params: &[TypeSymbol]) -> Box<str> {
    let mut signature = String::from(declaring);
    signature.push('.');
    signature.push_str(name);
    signature.push('(');
    for (index, parameter) in params.iter().enumerate() {
        if index > 0 {
            signature.push_str(", ");
        }
        signature.push_str(&alloc::format!("{parameter}"));
    }
    signature.push(')');
    signature.into()
}

/// The `System.Type` named type, the result of a `typeof` expression (14.5.11).
fn system_type() -> TypeSymbol {
    TypeSymbol::Named([Box::from("System"), Box::from("Type")].into())
}

/// `System.TypedReference` -- the type of a `__makeref` result and the operand of `__reftype`
/// and `__refvalue`. A special byref-like value type; its signature element is `TYPEDBYREF`,
/// so emission encodes it specially rather than as a value type named by a token.
fn typed_reference() -> TypeSymbol {
    TypeSymbol::Named([Box::from("System"), Box::from("TypedReference")].into())
}

/// `System.RuntimeArgumentHandle` -- the type of a bare `__arglist` (what
/// `ArgIterator(RuntimeArgumentHandle)` consumes). An ordinary corlib value type.
fn runtime_argument_handle() -> TypeSymbol {
    TypeSymbol::Named([Box::from("System"), Box::from("RuntimeArgumentHandle")].into())
}

/// The pseudo-type of an `__arglist(...)` argument pack. It is not a real type: it exists
/// so overload resolution can match the pack against a vararg member's sentinel (and
/// nothing else), and so CS1503 renders `__arglist` exactly as csc does. The single-part
/// name never resolves (user code cannot spell it -- `__arglist` lexes as the keyword).
fn arglist_marker() -> TypeSymbol {
    TypeSymbol::Named([Box::from("__arglist")].into())
}

/// Whether `ty` is the [`arglist_marker`] pseudo-type (an `__arglist(...)` argument pack).
fn is_arglist_marker(ty: &TypeSymbol) -> bool {
    matches!(ty, TypeSymbol::Named(parts) if parts.len() == 1 && &*parts[0] == "__arglist")
}

/// csc's member display in CS7036 for a vararg member: `P.M(int, __arglist)`, with a
/// constructor shown by its type name (`T.T(__arglist)`).
fn vararg_member_display(
    declaring: &TypeSymbol,
    name: &str,
    parameters: &[TypeSymbol],
) -> Box<str> {
    use core::fmt::Write;
    let declaring = declaring.to_string();
    let shown = if name == ".ctor" {
        declaring.rsplit('.').next().unwrap_or(&declaring).to_string()
    } else {
        String::from(name)
    };
    let mut display = String::new();
    let _ = write!(display, "{declaring}.{shown}(");
    for parameter in parameters {
        let _ = write!(display, "{parameter}, ");
    }
    display.push_str("__arglist)");
    display.into()
}

/// `System.Array`, whose members (Length, GetLength, ...) an array's member access
/// resolves against.
fn system_array() -> TypeSymbol {
    TypeSymbol::Named([Box::from("System"), Box::from("Array")].into())
}

/// The types member lookup continues into when a type's own members do not answer a name --
/// ECMA-334 **7.4**.
///
/// **A CLASS OR STRUCT CONTINUES INTO ITS BASE CLASS AND NOT INTO ITS INTERFACES.** An interface's
/// members are in scope for a receiver of the INTERFACE type; they are not members of a class that
/// implements it. A class's own implementations are its own members and are found before this
/// function is reached, which is why the difference is invisible until a member exists ONLY on the
/// interface -- an EXPLICIT implementation (13.4.1), whose name the class deliberately records
/// QUALIFIED (`I.Get`) so that it is unreachable unqualified.
///
/// **FOLLOWING INTERFACES FROM EVERY TYPE ACCEPTS PROGRAMS csc REJECTS**, and it does so silently
/// -- there is no refusal to notice, only a call that binds. Measured, with no generics in it:
///
/// ```text
///     class Impl : I { void I.Take(object o) { } public void Take(int i) { } }
///     new Impl().Take("a string")     accepted, and ran      csc: CS1503
///
///     class Impl : I { int I.Get() { return 42; } }
///     new Impl().Get()                answered 42            csc: CS1061
/// ```
///
/// The second is the sharper one: the class has no `Get` at all, and the call resolved to the
/// INTERFACE's declaration. On an imported type the same hole let `List<int>.Add("not an int")`
/// bind to `IList.Add(object)` and throw `ArgumentException` at run time.
///
/// **AN INTERFACE CONTINUES INTO ITS BASE INTERFACES**, which is what the interface walk was
/// written for (`IList : ICollection : IEnumerable`) and is unchanged.
///
/// **THIS IS MEMBER LOOKUP AND NOTHING ELSE.** `transitive_interfaces`, `type_reaches` and
/// `implements_enumerable` ask what a value CONVERTS to -- a class does convert to its interfaces
/// -- so they walk the whole base graph and must keep doing so. The two questions share a field
/// (`bases`) and not a rule, which is why they read alike and must not be unified.
fn member_lookup_bases(info: &TypeInfo) -> Vec<TypeSymbol> {
    if info.kind == TypeKind::Interface {
        return info.bases.clone();
    }
    info.base.iter().cloned().collect()
}

/// The type whose members a receiver of type `ty` resolves against: `System.Array`
/// for an array (its members live there), otherwise `ty` itself.
fn member_lookup_type(ty: &TypeSymbol) -> TypeSymbol {
    if matches!(ty, TypeSymbol::Array { .. }) {
        system_array()
    } else {
        ty.clone()
    }
}

/// The type of a conditional expression from its branch types (14.13): the branch
/// type the other implicitly converts to, or `None` (`CS0173`) when there is no
/// one-way conversion between them.
fn conditional_result_type(
    model: &Model,
    when_true: &TypeSymbol,
    when_false: &TypeSymbol,
) -> Option<TypeSymbol> {
    if when_true == when_false {
        return Some(when_true.clone());
    }
    match (
        converts(model, when_true, when_false),
        converts(model, when_false, when_true),
    ) {
        (true, false) => Some(when_false.clone()),
        (false, true) => Some(when_true.clone()),
        _ => None,
    }
}

/// Whether a bound expression denotes something assignable: a local or parameter,
/// a field or (writable) property, or an array element. A read-only property's
/// missing setter is a finer check left for later.
/// Whether an expression can be evaluated TWICE with the same result and no extra effect.
///
/// A compound indexer assignment (`c[i] += v`) names its receiver and indices once in the source
/// and twice in the lowering -- once for the `get_`, once for the `set_`. C# evaluates them once
/// (14.14.2), so the lowering is only sound when re-evaluating is unobservable. This is the
/// conservative test for that: a name, `this`, a literal, and reads THROUGH those. Anything that
/// can run user code or write state -- a call, an assignment, `++`/`--`, `new` -- is refused, so
/// `Next()[i] += 1` keeps its existing behavior rather than calling `Next()` twice.
///
/// Nothing writes between the get and the set in this lowering, so a repeated field or element
/// READ observes the same value by construction; that is why those are permitted.
fn is_repeatable(expr: &lamella_syntax::ast::Expr) -> bool {
    use lamella_syntax::ast::ExprKind as K;
    match &expr.kind {
        K::Name(_) | K::Literal(_) | K::This | K::Base => true,
        K::Parenthesized(inner) | K::Checked(inner) | K::Unchecked(inner) => is_repeatable(inner),
        K::Cast { operand, .. } => is_repeatable(operand),
        K::Binary { left, right, .. } => is_repeatable(left) && is_repeatable(right),
        K::Unary { operator, operand } => {
            !matches!(
                operator,
                UnaryOperator::PreIncrement | UnaryOperator::PreDecrement
            ) && is_repeatable(operand)
        }
        _ => false,
    }
}

fn is_lvalue(expr: &BoundExpr) -> bool {
    matches!(
        expr.kind,
        BoundExprKind::Local(_)
            | BoundExprKind::FieldAccess { .. }
            | BoundExprKind::PropertyAccess { .. }
            | BoundExprKind::ElementAccess { .. }
            | BoundExprKind::Dereference { .. }
            | BoundExprKind::RefValue { .. }
    )
}

/// The binary operator underlying a compound assignment, or `None` for simple `=`.
fn compound_binary_operator(operator: AssignmentOperator) -> Option<BinaryOperator> {
    use AssignmentOperator as A;
    Some(match operator {
        A::Assign => return None,
        A::Add => BinaryOperator::Add,
        A::Subtract => BinaryOperator::Subtract,
        A::Multiply => BinaryOperator::Multiply,
        A::Divide => BinaryOperator::Divide,
        A::Modulo => BinaryOperator::Modulo,
        A::And => BinaryOperator::BitwiseAnd,
        A::Or => BinaryOperator::BitwiseOr,
        A::Xor => BinaryOperator::BitwiseXor,
        A::LeftShift => BinaryOperator::LeftShift,
        A::RightShift => BinaryOperator::RightShift,
    })
}

/// The source symbol of an assignment operator, for diagnostics.
fn assignment_symbol(operator: AssignmentOperator) -> &'static str {
    use AssignmentOperator as A;
    match operator {
        A::Assign => "=",
        A::Add => "+=",
        A::Subtract => "-=",
        A::Multiply => "*=",
        A::Divide => "/=",
        A::Modulo => "%=",
        A::And => "&=",
        A::Or => "|=",
        A::Xor => "^=",
        A::LeftShift => "<<=",
        A::RightShift => ">>=",
    }
}

/// Whether two types may be compared with `==`/`!=`. Numeric pairs that promote,
/// `bool` pairs, identical types, and anything against `object` qualify; the
/// stricter reference-equality rules arrive with the type hierarchy.
fn equality_comparable(left: &TypeSymbol, right: &TypeSymbol) -> bool {
    if let (Some(left), Some(right)) = (as_special(left), as_special(right)) {
        if left.is_numeric() && right.is_numeric() {
            return binary_numeric_promotion(left, right).is_some();
        }
        if left == SpecialType::Boolean && right == SpecialType::Boolean {
            return true;
        }
    }
    left == right
        || matches!(left, TypeSymbol::Special(SpecialType::Object))
        || matches!(right, TypeSymbol::Special(SpecialType::Object))
}

/// Binary numeric promotion (14.2.6.2): the common type of two numeric operands,
/// or `None` if either is not numeric (or `decimal` is mixed with floating point).
fn binary_numeric_promotion(left: SpecialType, right: SpecialType) -> Option<SpecialType> {
    use SpecialType::{Decimal, Double, Int16, Int32, Int64, SByte, Single, UInt32, UInt64};
    if !left.is_numeric() || !right.is_numeric() {
        return None;
    }
    let has = |special: SpecialType| left == special || right == special;
    Some(if has(Decimal) {
        return None;
    } else if has(Double) {
        Double
    } else if has(Single) {
        Single
    } else if has(UInt64) {
        UInt64
    } else if has(Int64) {
        Int64
    } else if has(UInt32) {
        if matches!(left, SByte | Int16 | Int32) || matches!(right, SByte | Int16 | Int32) {
            Int64
        } else {
            UInt32
        }
    } else {
        Int32
    })
}

/// Whether a special type is one of the integral types (14.8 shift, bitwise).
/// Folds a qualified framework-primitive name (`System.Decimal`, `System.Int32`) to its keyword
/// `Special` form -- matching what SignatureCanon does at the token layer -- so a type compiling
/// its own members compares equal whether spelled as the framework name or the folded primitive.
/// System.Enum and System.ValueType extend System.ValueType in metadata but are themselves
/// REFERENCE types (a value of that static type is a boxed object), so a concrete value type boxes
/// when converted to one -- they are never value types despite the model marking their kind.
fn is_reference_base_class(ty: &TypeSymbol) -> bool {
    matches!(ty, TypeSymbol::Named(parts)
        if matches!(&**parts, [ns, name] if &**ns == "System" && (&**name == "Enum" || &**name == "ValueType")))
}

fn fold_primitive_name(ty: &TypeSymbol) -> TypeSymbol {
    if let TypeSymbol::Named(parts) = ty {
        if parts.len() == 2 {
            if let Some(special) = crate::reference::special_for_named(&parts[0], &parts[1]) {
                return TypeSymbol::Special(special);
            }
        }
    }
    ty.clone()
}

fn is_integral(special: SpecialType) -> bool {
    use SpecialType::{Byte, Char, Int16, Int32, Int64, SByte, UInt16, UInt32, UInt64};
    matches!(
        special,
        SByte | Byte | Int16 | UInt16 | Int32 | UInt32 | Int64 | UInt64 | Char
    )
}

/// The result type of a shift, i.e. the unary-numeric-promoted left operand:
/// `int`, `uint`, `long`, or `ulong` (14.8).
fn shift_result(left: SpecialType) -> SpecialType {
    match left {
        SpecialType::Int32 | SpecialType::UInt32 | SpecialType::Int64 | SpecialType::UInt64 => left,
        _ => SpecialType::Int32,
    }
}

/// The result type of a prefix unary operator, or `None` if it does not apply
/// (14.6). The `++`/`--` cases keep the operand type; their lvalue requirement is
/// checked once name resolution lands.
fn unary_result_type(operator: UnaryOperator, operand: &TypeSymbol) -> Option<TypeSymbol> {
    use SpecialType::{Boolean, Int64, UInt32, UInt64};
    let special = as_special(operand)?;
    match operator {
        UnaryOperator::Plus => special
            .is_numeric()
            .then_some(TypeSymbol::Special(unary_numeric_promote(special))),
        UnaryOperator::Minus => match special {
            UInt64 => None,
            SpecialType::Decimal => None,
            UInt32 => Some(TypeSymbol::Special(Int64)),
            other if other.is_numeric() => Some(TypeSymbol::Special(unary_numeric_promote(other))),
            _ => None,
        },
        UnaryOperator::Not => (special == Boolean).then_some(TypeSymbol::Special(Boolean)),
        UnaryOperator::Complement => {
            is_integral(special).then_some(TypeSymbol::Special(unary_numeric_promote(special)))
        }
        UnaryOperator::PreIncrement | UnaryOperator::PreDecrement => {
            special.is_numeric().then_some(operand.clone())
        }
    }
}

/// Unary numeric promotion (14.2.6.1): the smaller integral types and `char`
/// promote to `int`; every other numeric type is unchanged.
fn unary_numeric_promote(special: SpecialType) -> SpecialType {
    use SpecialType::{Byte, Char, Int16, Int32, SByte, UInt16};
    match special {
        SByte | Byte | Int16 | UInt16 | Char => Int32,
        other => other,
    }
}

/// The source symbol of a prefix unary operator, for diagnostics.
fn unary_operator_symbol(operator: UnaryOperator) -> &'static str {
    match operator {
        UnaryOperator::Plus => "+",
        UnaryOperator::Minus => "-",
        UnaryOperator::Not => "!",
        UnaryOperator::Complement => "~",
        UnaryOperator::PreIncrement => "++",
        UnaryOperator::PreDecrement => "--",
    }
}

/// The type of a literal (9.4.4).
fn literal_type(literal: &Literal) -> TypeSymbol {
    let special = match literal {
        Literal::Integer { value, suffix } => integer_literal_type(*value, *suffix),
        Literal::Real { suffix, .. } => match suffix {
            RealSuffix::Float => SpecialType::Single,
            RealSuffix::Decimal => SpecialType::Decimal,
            RealSuffix::Double | RealSuffix::None => SpecialType::Double,
        },
        Literal::Decimal { .. } => SpecialType::Decimal,
        Literal::Character(_) => SpecialType::Char,
        Literal::String(_) => SpecialType::String,
        Literal::Boolean(_) => SpecialType::Boolean,
        Literal::Null => SpecialType::Null,
    };
    TypeSymbol::Special(special)
}

/// The type of an integer literal (9.4.4.2): the first type in the
/// suffix-determined list whose range holds the value.
fn integer_literal_type(value: u64, suffix: IntegerSuffix) -> SpecialType {
    let i32_max = i32::MAX as u64;
    let u32_max = u32::MAX as u64;
    let i64_max = i64::MAX as u64;
    match suffix {
        IntegerSuffix::None => {
            if value <= i32_max {
                SpecialType::Int32
            } else if value <= u32_max {
                SpecialType::UInt32
            } else if value <= i64_max {
                SpecialType::Int64
            } else {
                SpecialType::UInt64
            }
        }
        IntegerSuffix::Unsigned => {
            if value <= u32_max {
                SpecialType::UInt32
            } else {
                SpecialType::UInt64
            }
        }
        IntegerSuffix::Long => {
            if value <= i64_max {
                SpecialType::Int64
            } else {
                SpecialType::UInt64
            }
        }
        IntegerSuffix::UnsignedLong => SpecialType::UInt64,
    }
}

/// The source symbol of a binary operator, for diagnostics.
fn operator_symbol(operator: BinaryOperator) -> &'static str {
    use BinaryOperator as Op;
    match operator {
        Op::Multiply => "*",
        Op::Divide => "/",
        Op::Modulo => "%",
        Op::Add => "+",
        Op::Subtract => "-",
        Op::LeftShift => "<<",
        Op::RightShift => ">>",
        Op::LessThan => "<",
        Op::GreaterThan => ">",
        Op::LessThanOrEqual => "<=",
        Op::GreaterThanOrEqual => ">=",
        Op::Equal => "==",
        Op::NotEqual => "!=",
        Op::BitwiseAnd => "&",
        Op::BitwiseXor => "^",
        Op::BitwiseOr => "|",
        Op::LogicalAnd => "&&",
        Op::LogicalOr => "||",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lamella_syntax::parser::parse_expression;

    fn bound_type(source: &str) -> TypeSymbol {
        bind_expression(&parse_expression(source).expr).ty
    }

    fn codes(source: &str) -> Vec<u16> {
        let mut binder = Binder::new();
        binder.bind_expression(&parse_expression(source).expr);
        binder
            .into_diagnostics()
            .iter()
            .map(Diagnostic::code)
            .collect()
    }

    fn special(source: &str) -> SpecialType {
        match bound_type(source) {
            TypeSymbol::Special(special) => special,
            other => panic!("expected a special type, got {other:?}"),
        }
    }

    #[test]
    fn integer_literal_types_follow_the_value_and_suffix() {
        assert_eq!(special("42"), SpecialType::Int32);
        assert_eq!(special("2147483648"), SpecialType::UInt32);
        assert_eq!(special("10000000000000000000"), SpecialType::UInt64);
        assert_eq!(special("1u"), SpecialType::UInt32);
        assert_eq!(special("1L"), SpecialType::Int64);
    }

    #[test]
    fn arithmetic_uses_binary_numeric_promotion() {
        assert_eq!(special("1 + 2"), SpecialType::Int32);
        assert_eq!(special("1 + 2L"), SpecialType::Int64);
        assert_eq!(special("1 + 2.0"), SpecialType::Double);
        assert_eq!(special("1 * 2.0f"), SpecialType::Single);
        assert_eq!(special("'a' + 1"), SpecialType::Int32);
    }

    #[test]
    fn relational_equality_and_logical_yield_bool() {
        assert_eq!(special("1 < 2"), SpecialType::Boolean);
        assert_eq!(special("1 == 2"), SpecialType::Boolean);
        assert_eq!(special("true != false"), SpecialType::Boolean);
        assert_eq!(special("true && false"), SpecialType::Boolean);
    }

    #[test]
    fn bitwise_and_shift_typing() {
        assert_eq!(special("1 & 2"), SpecialType::Int32);
        assert_eq!(special("true | false"), SpecialType::Boolean);
        assert_eq!(special("1 << 2"), SpecialType::Int32);
        assert_eq!(special("1L << 2"), SpecialType::Int64);
    }

    #[test]
    fn inapplicable_operators_are_cs0019() {
        assert_eq!(codes("true + 1"), [19]);
        assert_eq!(codes("1 && 2"), [19]);
        assert_eq!(codes("\"x\" - \"y\""), [19]);
        assert_eq!(codes("(true + 1) + 2"), [19]);
    }

    #[test]
    fn unary_operator_typing() {
        assert_eq!(special("-1"), SpecialType::Int32);
        assert_eq!(special("-1L"), SpecialType::Int64);
        assert_eq!(special("-1u"), SpecialType::Int64);
        assert_eq!(special("+1"), SpecialType::Int32);
        assert_eq!(special("!true"), SpecialType::Boolean);
        assert_eq!(special("~1"), SpecialType::Int32);
        assert_eq!(special("1++"), SpecialType::Int32);
        assert_eq!(special("++1L"), SpecialType::Int64);
    }

    #[test]
    fn inapplicable_unary_operators_are_cs0023() {
        assert_eq!(codes("-true"), [23]);
        assert_eq!(codes("!1"), [23]);
        assert_eq!(codes("~true"), [23]);
        assert_eq!(codes("true++"), [23]);
    }

    fn bound_in_scope(binder: &mut Binder, source: &str) -> TypeSymbol {
        binder.bind_expression(&parse_expression(source).expr).ty
    }

    #[test]
    fn simple_names_resolve_to_declared_locals() {
        let mut binder = Binder::new();
        binder.enter_scope();
        binder.declare_local("x", TypeSymbol::Special(SpecialType::Int32));
        binder.declare_local("name", TypeSymbol::Special(SpecialType::String));
        assert_eq!(
            bound_in_scope(&mut binder, "x"),
            TypeSymbol::Special(SpecialType::Int32)
        );
        assert_eq!(
            bound_in_scope(&mut binder, "x + 1"),
            TypeSymbol::Special(SpecialType::Int32)
        );
        assert_eq!(
            bound_in_scope(&mut binder, "name"),
            TypeSymbol::Special(SpecialType::String)
        );
        assert!(binder.diagnostics().is_empty());
    }

    #[test]
    fn unknown_names_are_cs0103() {
        let mut binder = Binder::new();
        binder.enter_scope();
        binder.bind_expression(&parse_expression("missing").expr);
        let codes: Vec<u16> = binder.diagnostics().iter().map(Diagnostic::code).collect();
        assert_eq!(codes, [103]);
    }

    #[test]
    fn cast_typetest_typeof_and_checked() {
        assert_eq!(special("(long)1"), SpecialType::Int64);
        assert_eq!(special("1 is int"), SpecialType::Boolean);
        assert_eq!(special("1 as object"), SpecialType::Object);
        assert_eq!(bound_type("typeof(int)").to_string(), "System.Type");
        assert_eq!(special("checked(1 + 2)"), SpecialType::Int32);
        assert_eq!(special("unchecked(1)"), SpecialType::Int32);
    }

    #[test]
    fn casts_require_an_explicit_conversion() {
        assert_eq!(codes("(byte)1"), []);
        assert_eq!(codes("(int)1u"), []);
        assert_eq!(codes("(long)1"), []);
        assert_eq!(codes("(string)1"), [30]);
        assert_eq!(codes("(bool)1"), [30]);
    }

    #[test]
    fn conditional_result_type_and_condition_check() {
        assert_eq!(special("true ? 1 : 2"), SpecialType::Int32);
        assert_eq!(special("true ? 1 : 2L"), SpecialType::Int64);
        assert_eq!(special("false ? 2L : 1"), SpecialType::Int64);
        assert_eq!(codes("1 ? 1 : 2"), [29]);
        assert_eq!(codes("true ? 1 : \"x\""), [173]);
    }

    #[test]
    fn conditional_arms_are_converted_to_the_result_type() {
        let mut binder = Binder::new();
        let bound = binder.bind_expression(&parse_expression("true ? 1 : 2L").expr);
        let long = TypeSymbol::Special(SpecialType::Int64);
        assert_eq!(bound.ty, long);
        let BoundExprKind::Conditional {
            when_true,
            when_false,
            ..
        } = &bound.kind
        else {
            panic!("expected a conditional, got {:?}", bound.kind);
        };
        assert_eq!(when_true.ty, long, "the `1` arm must widen to long");
        assert_eq!(when_false.ty, long, "the `2L` arm is already long");
    }

    #[test]
    fn assignment_typing_and_checks() {
        let mut binder = Binder::new();
        binder.enter_scope();
        binder.declare_local("x", TypeSymbol::Special(SpecialType::Int32));
        assert_eq!(
            bound_in_scope(&mut binder, "x = 1"),
            TypeSymbol::Special(SpecialType::Int32)
        );
        bound_in_scope(&mut binder, "x += 2");
        assert!(binder.diagnostics().is_empty());
        let before = binder.diagnostics().len();
        bound_in_scope(&mut binder, "x = true");
        assert_eq!(binder.diagnostics()[before].code(), 29);
    }

    #[test]
    fn assigning_to_a_non_variable_is_cs0131() {
        assert_eq!(codes("1 = 2"), [131]);
    }

    #[test]
    fn member_access_resolves_fields_method_groups_and_missing_members() {
        use crate::symbols::{FieldSymbol, MethodSymbol, TypeInfo, TypeKind};
        let mut model = Model::new();
        let mut widget = TypeInfo::new("", "Widget", TypeKind::Class);
        widget.fields.push(FieldSymbol {
            name: "count".into(),
            ty: TypeSymbol::Special(SpecialType::Int32),
            is_static: false,
            is_readonly: false,
            is_volatile: false,
            accessibility: crate::symbols::Accessibility::Public,
            constant: None,
            is_required: false,
        });
        widget.methods.push(MethodSymbol {
            name: "Area".into(),
            return_type: TypeSymbol::Special(SpecialType::Double),
            parameters: Vec::new(),
            parameter_info: Vec::new(),
            is_static: false,
            is_params: false,
            is_vararg: false,
            is_virtual: false,
            is_abstract: false,
            is_override: false,
            is_sealed: false,
            accessibility: crate::symbols::Accessibility::Public,
            conditional: Vec::new(),
            sets_required_members: false,
            type_parameters: Vec::new(),
            type_parameter_constraints: Vec::new(),
        });
        model.insert(widget);

        let mut binder = Binder::with_model(model);
        binder.enter_scope();
        binder.declare_local("w", TypeSymbol::Named(["Widget".into()].into()));

        let count = binder.bind_expression(&parse_expression("w.count").expr);
        assert_eq!(count.ty, TypeSymbol::Special(SpecialType::Int32));
        let area = binder.bind_expression(&parse_expression("w.Area").expr);
        assert!(matches!(area.kind, BoundExprKind::MethodGroup { .. }));
        assert!(binder.diagnostics().is_empty());
        binder.bind_expression(&parse_expression("w.missing").expr);
        assert_eq!(binder.diagnostics().last().map(Diagnostic::code), Some(117));
    }

    #[test]
    fn a_member_we_could_not_decode_is_told_apart_from_one_that_is_absent() {
        use crate::diagnostic::CodeNamespace;
        use crate::symbols::{TypeInfo, TypeKind};

        let mut model = Model::new();
        let mut widget = TypeInfo::new("", "Widget", TypeKind::Class);
        widget.is_external = true;
        widget.undecodable_members.push("Where".into());
        model.insert(widget);

        let mut binder = Binder::with_model(model);
        binder.enter_scope();
        binder.declare_local("w", TypeSymbol::Named(["Widget".into()].into()));

        binder.bind_expression(&parse_expression("w.Where").expr);
        let reported = binder.diagnostics().last().expect("a diagnostic");
        assert_eq!(reported.code(), 2);
        assert_eq!(reported.namespace(), CodeNamespace::Lam);

        binder.bind_expression(&parse_expression("w.Nonesuch").expr);
        let reported = binder.diagnostics().last().expect("a diagnostic");
        assert_eq!(reported.code(), 117);
        assert_eq!(reported.namespace(), CodeNamespace::Cs);
    }

    #[test]
    fn internal_member_of_a_referenced_assembly_is_cs0122() {
        use crate::symbols::{Accessibility, FieldSymbol, TypeInfo, TypeKind};
        let internal_field = |name: &str| FieldSymbol {
            name: name.into(),
            ty: TypeSymbol::Special(SpecialType::Int32),
            is_static: false,
            is_readonly: false,
            is_volatile: false,
            accessibility: Accessibility::Internal,
            constant: None,
            is_required: false,
        };
        let mut model = Model::new();
        let mut external = TypeInfo::new("", "Lib", TypeKind::Class);
        external.is_external = true;
        external.fields.push(internal_field("Secret"));
        model.insert(external);
        let mut here = TypeInfo::new("", "Here", TypeKind::Class);
        here.fields.push(internal_field("Shared"));
        model.insert(here);

        let mut binder = Binder::with_model(model);
        binder.enter_scope();
        binder.declare_local("lib", TypeSymbol::Named(["Lib".into()].into()));
        binder.declare_local("here", TypeSymbol::Named(["Here".into()].into()));

        binder.bind_expression(&parse_expression("lib.Secret").expr);
        assert_eq!(binder.diagnostics().last().map(Diagnostic::code), Some(122));

        let before = binder.diagnostics().len();
        binder.bind_expression(&parse_expression("here.Shared").expr);
        assert_eq!(binder.diagnostics().len(), before);
    }

    #[test]
    fn array_creation_and_element_access() {
        assert_eq!(bound_type("new int[5]").to_string(), "int[]");
        assert_eq!(bound_type("new int[5, 6]").to_string(), "int[,]");
        assert_eq!(bound_type("new int[3][]").to_string(), "int[][]");

        let mut binder = Binder::new();
        binder.enter_scope();
        binder.declare_local("a", TypeSymbol::Special(SpecialType::Int32).into_array(1));
        assert_eq!(
            bound_in_scope(&mut binder, "a[0]"),
            TypeSymbol::Special(SpecialType::Int32)
        );
        assert!(binder.diagnostics().is_empty());
        binder.declare_local("n", TypeSymbol::Special(SpecialType::Int32));
        bound_in_scope(&mut binder, "n[0]");
        assert!(binder.diagnostics().iter().any(|d| d.code() == 21));
    }

    #[test]
    fn object_creation_resolves_constructors() {
        use crate::declaration::collect_model;
        use lamella_syntax::parser::parse_compilation_unit;

        let unit = parse_compilation_unit(
            "class Point { Point(int x, int y) { } Point(int x) { } } class Empty { }",
        )
        .unit;
        let model = collect_model(&unit);
        let bound = |source: &str| {
            Binder::with_model(model.clone()).bind_expression(&parse_expression(source).expr)
        };
        let codes = |source: &str| {
            let mut binder = Binder::with_model(model.clone());
            binder.bind_expression(&parse_expression(source).expr);
            binder
                .into_diagnostics()
                .iter()
                .map(Diagnostic::code)
                .collect::<Vec<_>>()
        };

        assert_eq!(bound("new Point(1, 2)").ty.to_string(), "Point");
        assert!(codes("new Point(1, 2)").is_empty());
        assert_eq!(bound("new Empty()").ty.to_string(), "Empty");
        assert!(codes("new Empty()").is_empty());
        assert_eq!(codes("new Point(1, 2, 3)"), [1729]);
        assert_eq!(codes("new Point(true, 2)"), [1503]);
        assert_eq!(codes("new Gadget()"), [246]);
    }

    #[test]
    fn this_and_bare_names_resolve_against_the_enclosing_type() {
        use crate::symbols::{FieldSymbol, MethodSymbol, TypeInfo, TypeKind};
        let mut widget = TypeInfo::new("", "Widget", TypeKind::Class);
        widget.fields.push(FieldSymbol {
            name: "count".into(),
            ty: TypeSymbol::Special(SpecialType::Int32),
            is_static: false,
            is_readonly: false,
            is_volatile: false,
            accessibility: crate::symbols::Accessibility::Public,
            constant: None,
            is_required: false,
        });
        widget.methods.push(MethodSymbol {
            name: "Area".into(),
            return_type: TypeSymbol::Special(SpecialType::Double),
            parameters: Vec::new(),
            parameter_info: Vec::new(),
            is_static: false,
            is_params: false,
            is_vararg: false,
            is_virtual: false,
            is_abstract: false,
            is_override: false,
            is_sealed: false,
            accessibility: crate::symbols::Accessibility::Public,
            conditional: Vec::new(),
            sets_required_members: false,
            type_parameters: Vec::new(),
            type_parameter_constraints: Vec::new(),
        });
        let mut model = Model::new();
        model.insert(widget);

        let mut binder = Binder::with_model(model);
        binder.enter_type(TypeSymbol::Named(["Widget".into()].into()));
        binder.enter_scope();

        assert_eq!(bound_in_scope(&mut binder, "this").to_string(), "Widget");
        assert_eq!(
            bound_in_scope(&mut binder, "count"),
            TypeSymbol::Special(SpecialType::Int32)
        );
        assert_eq!(
            bound_in_scope(&mut binder, "this.count"),
            TypeSymbol::Special(SpecialType::Int32)
        );
        assert_eq!(
            bound_in_scope(&mut binder, "Area()"),
            TypeSymbol::Special(SpecialType::Double)
        );
        assert_eq!(
            bound_in_scope(&mut binder, "this.Area()"),
            TypeSymbol::Special(SpecialType::Double)
        );
        assert!(binder.diagnostics().is_empty());
        bound_in_scope(&mut binder, "missing");
        assert!(binder.diagnostics().iter().any(|d| d.code() == 103));
    }

    #[test]
    fn member_lookup_walks_the_base_chain() {
        use crate::declaration::collect_model;
        use lamella_syntax::parser::parse_compilation_unit;

        let unit = parse_compilation_unit(
            "class Animal { public int legs; public int Speed() { } } \
             class Dog : Animal { public string breed; }",
        )
        .unit;
        let model = collect_model(&unit);
        let mut binder = Binder::with_model(model);
        binder.enter_scope();
        binder.declare_local("d", TypeSymbol::Named(["Dog".into()].into()));

        assert_eq!(
            bound_in_scope(&mut binder, "d.breed"),
            TypeSymbol::Special(SpecialType::String)
        );
        assert_eq!(
            bound_in_scope(&mut binder, "d.legs"),
            TypeSymbol::Special(SpecialType::Int32)
        );
        assert_eq!(
            bound_in_scope(&mut binder, "d.Speed()"),
            TypeSymbol::Special(SpecialType::Int32)
        );
        assert!(binder.diagnostics().is_empty());
    }

    #[test]
    fn base_access_resolves_against_the_base_class() {
        use crate::declaration::collect_model;
        use lamella_syntax::parser::parse_compilation_unit;

        let unit = parse_compilation_unit(
            "class Animal { public int Speed() { return 0; } } class Dog : Animal { }",
        )
        .unit;
        let model = collect_model(&unit);
        let mut binder = Binder::with_model(model);
        binder.enter_type(TypeSymbol::Named(["Dog".into()].into()));
        binder.enter_scope();
        assert_eq!(
            bound_in_scope(&mut binder, "base.Speed()"),
            TypeSymbol::Special(SpecialType::Int32)
        );
        assert!(binder.diagnostics().is_empty());
    }

    #[test]
    fn static_access_through_a_type_name() {
        use crate::declaration::collect_model;
        use lamella_syntax::parser::parse_compilation_unit;

        let unit = parse_compilation_unit(
            "class Calc { public static int Zero; public static int Pi() { return 3; } }",
        )
        .unit;
        let model = collect_model(&unit);
        let mut binder = Binder::with_model(model);
        binder.enter_scope();

        assert_eq!(
            bound_in_scope(&mut binder, "Calc.Zero"),
            TypeSymbol::Special(SpecialType::Int32)
        );
        assert_eq!(
            bound_in_scope(&mut binder, "Calc.Pi()"),
            TypeSymbol::Special(SpecialType::Int32)
        );
        assert!(binder.diagnostics().is_empty());
        bound_in_scope(&mut binder, "Nope");
        assert!(binder.diagnostics().iter().any(|d| d.code() == 103));
    }

    #[test]
    fn enum_members_and_enum_casts() {
        use crate::declaration::collect_model;
        use lamella_syntax::parser::parse_compilation_unit;

        let unit = parse_compilation_unit("enum Color { Red, Green, Blue }").unit;
        let model = collect_model(&unit);
        let mut binder = Binder::with_model(model);
        binder.enter_scope();

        assert_eq!(
            bound_in_scope(&mut binder, "Color.Red").to_string(),
            "Color"
        );
        assert_eq!(
            bound_in_scope(&mut binder, "(int)Color.Red"),
            TypeSymbol::Special(SpecialType::Int32)
        );
        assert_eq!(bound_in_scope(&mut binder, "(Color)1").to_string(), "Color");
        assert!(binder.diagnostics().is_empty());
    }

    #[test]
    fn qualified_namespace_names_resolve_to_types() {
        use crate::declaration::collect_model;
        use lamella_syntax::parser::parse_compilation_unit;

        let unit = parse_compilation_unit(
            "namespace A.B { class Widget { } } namespace A { class Top { } }",
        )
        .unit;
        let model = collect_model(&unit);
        let mut binder = Binder::with_model(model);
        binder.enter_scope();

        assert_eq!(
            bound_in_scope(&mut binder, "A.B.Widget").to_string(),
            "A.B.Widget"
        );
        assert_eq!(bound_in_scope(&mut binder, "A.Top").to_string(), "A.Top");
        assert!(binder.diagnostics().is_empty());
        bound_in_scope(&mut binder, "A.Nope");
        assert!(binder.diagnostics().iter().any(|d| d.code() == 234));
    }

    #[test]
    fn sibling_namespace_resolves_through_the_enclosing_namespace() {
        use lamella_syntax::parser::parse_compilation_unit;
        let codes = |source: &str| {
            let unit = parse_compilation_unit(source).unit;
            let mut codes: Vec<u16> = crate::bind_compilation_unit(&unit)
                .iter()
                .map(Diagnostic::code)
                .collect();
            codes.sort_unstable();
            codes
        };

        assert_eq!(
            codes(
                "namespace N.A { public enum E { X, Y } } \
                 namespace N.B { class C { static int Run() { A.E e = A.E.Y; return (int)e; } } }"
            ),
            []
        );
        assert!(codes(
            "namespace N.A { public enum E { X } } \
             namespace N.B { class C { Z.E f; static int Run() { return 0; } } }"
        )
        .contains(&246));
    }

    #[test]
    fn using_directives_resolve_unqualified_type_names() {
        use crate::declaration::collect_model;
        use lamella_syntax::parser::parse_compilation_unit;

        let unit = parse_compilation_unit(
            "namespace System { class Console { } } \
             namespace Drawing { class Pen { } } \
             namespace Text { class Pen { } }",
        )
        .unit;
        let model = collect_model(&unit);

        let mut bare = Binder::with_model(model.clone());
        bare.enter_scope();
        bare.bind_expression(&parse_expression("Console").expr);
        assert!(bare.diagnostics().iter().any(|d| d.code() == 103));

        let mut binder = Binder::with_model(model.clone());
        binder.import_namespace("System");
        binder.enter_scope();
        assert_eq!(
            bound_in_scope(&mut binder, "Console").to_string(),
            "System.Console"
        );
        assert!(binder.diagnostics().is_empty());

        let mut ambiguous = Binder::with_model(model);
        ambiguous.import_namespace("Drawing");
        ambiguous.import_namespace("Text");
        ambiguous.enter_scope();
        ambiguous.bind_expression(&parse_expression("Pen").expr);
        assert!(ambiguous.diagnostics().iter().any(|d| d.code() == 104));
    }

    #[test]
    fn property_access_and_member_assignment() {
        use crate::declaration::collect_model;
        use lamella_syntax::parser::parse_compilation_unit;

        let unit = parse_compilation_unit(
            "class Box { public int Width { get { return 0; } set { } } public int height; }",
        )
        .unit;
        let model = collect_model(&unit);
        let mut binder = Binder::with_model(model);
        binder.enter_scope();
        binder.declare_local("b", TypeSymbol::Named(["Box".into()].into()));

        assert_eq!(
            bound_in_scope(&mut binder, "b.Width"),
            TypeSymbol::Special(SpecialType::Int32)
        );
        bound_in_scope(&mut binder, "b.height = 5");
        bound_in_scope(&mut binder, "b.Width = 5");
        assert!(binder.diagnostics().is_empty());
    }

    #[test]
    fn reference_conversions_follow_the_base_chain() {
        use crate::declaration::collect_model;
        use lamella_syntax::parser::parse_compilation_unit;

        let unit = parse_compilation_unit(
            "class Animal { } class Dog : Animal { } class Pen { public void Hold(Animal a) { } }",
        )
        .unit;
        let model = collect_model(&unit);
        let mut binder = Binder::with_model(model);
        binder.enter_scope();
        binder.declare_local("a", TypeSymbol::Named(["Animal".into()].into()));
        binder.declare_local("d", TypeSymbol::Named(["Dog".into()].into()));
        binder.declare_local("p", TypeSymbol::Named(["Pen".into()].into()));

        bound_in_scope(&mut binder, "a = d");
        bound_in_scope(&mut binder, "p.Hold(d)");
        assert!(binder.diagnostics().is_empty());
        assert!(binder.converts(
            &TypeSymbol::Named(["Dog".into()].into()),
            &TypeSymbol::Special(SpecialType::Object)
        ));
        bound_in_scope(&mut binder, "d = a");
        assert!(binder.diagnostics().iter().any(|d| d.code() == 266));
    }

    #[test]
    fn method_binding_checks_return_and_scopes_parameters() {
        use lamella_syntax::parser::parse_statement;
        let int = TypeSymbol::Special(SpecialType::Int32);
        let void = TypeSymbol::Special(SpecialType::Void);

        let codes = |return_type: TypeSymbol, source: &str| {
            let mut binder = Binder::new();
            let body = parse_statement(source).statement;
            binder.bind_method(None, "M", return_type, &[], &[], false, &body);
            binder
                .into_diagnostics()
                .iter()
                .map(Diagnostic::code)
                .collect::<Vec<_>>()
        };

        assert_eq!(codes(int.clone(), "{ return 1; }"), []);
        assert_eq!(codes(int.clone(), "{ return; }"), [126]);
        assert_eq!(codes(int.clone(), "{ return \"x\"; }"), [29]);
        assert_eq!(codes(void.clone(), "{ return 1; }"), [127]);
        assert_eq!(codes(void, "{ return; }"), []);

        let mut binder = Binder::new();
        let body = parse_statement("{ return n; }").statement;
        binder.bind_method(None, "M", int.clone(), &[("n".into(), int)], &[], false, &body);
        assert!(binder.diagnostics().is_empty());
    }

    #[test]
    fn definite_assignment_reports_cs0165() {
        use lamella_syntax::parser::parse_statement;
        let int = TypeSymbol::Special(SpecialType::Int32);
        let void = TypeSymbol::Special(SpecialType::Void);
        let codes = |source: &str| {
            let mut binder = Binder::new();
            let body = parse_statement(source).statement;
            binder.bind_method(None, "M", void.clone(), &[], &[], false, &body);
            binder
                .into_diagnostics()
                .iter()
                .filter(|diagnostic| {
                    diagnostic.severity() == lamella_syntax::diagnostic::Severity::Error
                })
                .map(Diagnostic::code)
                .collect::<Vec<_>>()
        };

        assert_eq!(codes("{ int x; int y = x; }"), [165]);
        assert_eq!(codes("{ int x; x = 1; int y = x; }"), []);
        assert_eq!(
            codes("{ bool c = true; int x; if (c) x = 1; int y = x; }"),
            [165]
        );
        assert_eq!(
            codes("{ bool c = true; int x; if (c) x = 1; else x = 2; int y = x; }"),
            []
        );
        assert_eq!(
            codes("{ bool c = true; int x; if (c) return; else x = 1; int y = x; }"),
            []
        );
        assert_eq!(codes("{ int x; if (true) x = 1; int y = x; }"), []);

        let mut binder = Binder::new();
        let body = parse_statement("{ int y = p; }").statement;
        binder.bind_method(None, "M", void, &[("p".into(), int)], &[], false, &body);
        assert!(!binder.diagnostics().iter().any(|diagnostic| {
            diagnostic.severity() == lamella_syntax::diagnostic::Severity::Error
        }));
    }

    #[test]
    fn definite_assignment_ignores_unreachable_switch_sections() {
        use lamella_syntax::parser::parse_statement;
        let int = TypeSymbol::Special(SpecialType::Int32);
        let void = TypeSymbol::Special(SpecialType::Void);
        let codes = |source: &str, params: &[(Box<str>, TypeSymbol)]| {
            let mut binder = Binder::new();
            let body = parse_statement(source).statement;
            binder.bind_method(None, "M", void.clone(), params, &[], false, &body);
            binder
                .into_diagnostics()
                .iter()
                .filter(|diagnostic| {
                    diagnostic.severity() == lamella_syntax::diagnostic::Severity::Error
                })
                .map(Diagnostic::code)
                .collect::<Vec<_>>()
        };

        assert_eq!(
            codes(
                "{ int x; switch (2) { case 1: break; case 2: x = 1; break; } int y = x; }",
                &[],
            ),
            []
        );
        assert_eq!(
            codes(
                "{ int x; switch (2) { case 1: x = 1; break; case 2: goto case 1; case 3: break; } \
                 int y = x; }",
                &[],
            ),
            []
        );
        assert_eq!(
            codes(
                "{ int x; switch (n) { case 1: break; case 2: x = 1; break; } int y = x; }",
                &[("n".into(), int.clone())],
            ),
            [165]
        );
    }

    #[test]
    fn not_all_paths_return_is_cs0161() {
        use lamella_syntax::parser::parse_statement;
        let int = TypeSymbol::Special(SpecialType::Int32);
        let void = TypeSymbol::Special(SpecialType::Void);

        let codes = |return_type: TypeSymbol, source: &str| {
            let mut binder = Binder::new();
            let body = parse_statement(source).statement;
            binder.bind_method(None, "M", return_type, &[], &[], false, &body);
            binder
                .into_diagnostics()
                .iter()
                .filter(|diagnostic| {
                    diagnostic.severity() == lamella_syntax::diagnostic::Severity::Error
                })
                .map(Diagnostic::code)
                .collect::<Vec<_>>()
        };

        assert_eq!(codes(int.clone(), "{ int x = 1; }"), [161]);
        assert_eq!(
            codes(int.clone(), "{ if (true) return 1; else return 2; }"),
            []
        );
        assert_eq!(codes(int.clone(), "{ while (true) { } }"), []);
        assert_eq!(codes(int.clone(), "{ goto Nowhere; }"), [161, 159]);
        assert_eq!(codes(int.clone(), "{ L: goto L; }"), []);
        assert_eq!(codes(int, "{ throw; }"), [156]);
        assert_eq!(codes(void, "{ int x = 1; }"), []);
    }

    #[test]
    fn invocation_does_overload_resolution() {
        use crate::symbols::{MethodSymbol, TypeInfo, TypeKind};

        fn method(
            name: &str,
            return_type: TypeSymbol,
            parameters: Vec<TypeSymbol>,
        ) -> MethodSymbol {
            MethodSymbol {
                name: name.into(),
                return_type,
                parameters,
                parameter_info: Vec::new(),
                is_static: false,
                is_params: false,
                is_vararg: false,
                is_virtual: false,
                is_abstract: false,
                is_override: false,
                is_sealed: false,
                accessibility: crate::symbols::Accessibility::Public,
                conditional: Vec::new(),
                sets_required_members: false,
                type_parameters: Vec::new(),
                type_parameter_constraints: Vec::new(),
            }
        }
        let int = TypeSymbol::Special(SpecialType::Int32);
        let long = TypeSymbol::Special(SpecialType::Int64);
        let double = TypeSymbol::Special(SpecialType::Double);
        let void = TypeSymbol::Special(SpecialType::Void);

        let mut calc = TypeInfo::new("", "Calc", TypeKind::Class);
        calc.methods
            .push(method("F", int.clone(), alloc::vec![int.clone()]));
        calc.methods
            .push(method("F", double.clone(), alloc::vec![double.clone()]));
        calc.methods
            .push(method("Take", void.clone(), alloc::vec![int.clone()]));
        calc.methods.push(method(
            "G",
            void.clone(),
            alloc::vec![int.clone(), long.clone()],
        ));
        calc.methods
            .push(method("G", void, alloc::vec![long, int.clone()]));
        let mut take_all = method(
            "P",
            TypeSymbol::Special(SpecialType::Void),
            alloc::vec![
                TypeSymbol::Special(SpecialType::String),
                int.clone().into_array(1),
            ],
        );
        take_all.is_params = true;
        calc.methods.push(take_all);
        let mut model = Model::new();
        model.insert(calc);

        let call_codes = |source: &str| {
            let mut binder = Binder::with_model(model.clone());
            binder.enter_scope();
            binder.declare_local("c", TypeSymbol::Named(["Calc".into()].into()));
            binder.bind_expression(&parse_expression(source).expr);
            binder
                .into_diagnostics()
                .iter()
                .map(Diagnostic::code)
                .collect::<Vec<_>>()
        };
        let call_type = |source: &str| {
            let mut binder = Binder::with_model(model.clone());
            binder.enter_scope();
            binder.declare_local("c", TypeSymbol::Named(["Calc".into()].into()));
            binder.bind_expression(&parse_expression(source).expr).ty
        };

        assert_eq!(call_type("c.F(1)"), int);
        assert_eq!(call_type("c.F(1.0)"), double);
        assert_eq!(call_type("c.F(1L)"), double);
        assert!(call_codes("c.F(1)").is_empty());
        assert_eq!(call_codes("c.Take(1, 2)"), [1501]);
        assert_eq!(call_codes("c.Take(\"x\")"), [1503]);
        assert_eq!(call_codes("c.G(1, 1)"), [121]);
        assert!(call_codes("c.P(\"s\", 1, 2)").is_empty());
        assert_eq!(call_codes("c.P(1, 2, 3)"), [1503]);
        assert_eq!(call_codes("c.P(\"s\", 1, \"x\")"), [1503]);
        assert_eq!(call_codes("c.P()"), [7036]);
    }

    #[test]
    fn private_member_access_from_outside_is_cs0122() {
        use lamella_syntax::parser::parse_compilation_unit;
        let codes = |source: &str| {
            let unit = parse_compilation_unit(source).unit;
            let mut codes: Vec<u16> = crate::bind_compilation_unit(&unit)
                .iter()
                .map(Diagnostic::code)
                .collect();
            codes.sort_unstable();
            codes
        };

        assert_eq!(
            codes(
                "class Counter { int Bump() { return 0; } } \
                 class Program { static int Run() { Counter c = new Counter(); return c.Bump(); } }"
            ),
            [122]
        );
        assert_eq!(
            codes(
                "class Counter { public int Bump() { return 0; } } \
                 class Program { static int Run() { Counter c = new Counter(); return c.Bump(); } }"
            ),
            []
        );
    }

    #[test]
    fn static_instance_mismatch_is_cs0120_and_cs0176() {
        use lamella_syntax::parser::parse_compilation_unit;
        let codes = |source: &str| {
            let unit = parse_compilation_unit(source).unit;
            let mut codes: Vec<u16> = crate::bind_compilation_unit(&unit)
                .iter()
                .map(Diagnostic::code)
                .collect();
            codes.sort_unstable();
            codes
        };

        assert_eq!(
            codes(
                "class A { public int I() { return 1; } } \
                 class C { static int Run() { return A.I(); } }"
            ),
            [120]
        );
        assert_eq!(
            codes(
                "class A { public static int S() { return 1; } } \
                 class C { static int Run() { A a = new A(); return a.S(); } }"
            ),
            [176]
        );
        assert_eq!(
            codes(
                "class A { public static int S() { return 1; } public int I() { return 1; } } \
                 class C { static int Run() { A a = new A(); return A.S() + a.I(); } }"
            ),
            []
        );
    }

    #[test]
    fn color_color_resolves_the_type_not_cs0176() {
        use lamella_syntax::parser::parse_compilation_unit;
        let codes = |source: &str| {
            let unit = parse_compilation_unit(source).unit;
            let mut codes: Vec<u16> = crate::bind_compilation_unit(&unit)
                .iter()
                .map(Diagnostic::code)
                .collect();
            codes.sort_unstable();
            codes
        };

        assert_eq!(
            codes(
                "enum E { A } \
                 class C { E e_field; E E { get { return e_field; } } void M() { E x = E.A; } }"
            ),
            [219, 649]
        );
        assert_eq!(
            codes(
                "class Palette { public static int Default() { return 7; } } \
                 class C { Palette Palette; int M() { return Palette.Default(); } }"
            ),
            [169]
        );
        assert!(codes(
            "class A { public static int S; } \
             class C { int M() { A a = new A(); return a.S; } }"
        )
        .contains(&176));
    }

    #[test]
    fn unimplemented_interface_member_is_cs0535() {
        use lamella_syntax::parser::parse_compilation_unit;
        let codes = |source: &str| {
            let unit = parse_compilation_unit(source).unit;
            let mut codes: Vec<u16> = crate::bind_compilation_unit(&unit)
                .iter()
                .map(Diagnostic::code)
                .collect();
            codes.sort_unstable();
            codes
        };

        assert_eq!(
            codes("interface I { void M(); } class C : I { }"),
            [535]
        );
        assert_eq!(
            codes("interface I { void M(); } class C : I { public void M() { } }"),
            []
        );
        assert_eq!(
            codes(
                "interface I { void M(); } class B { public void M() { } } class C : B, I { }"
            ),
            []
        );
        assert_eq!(
            codes("interface I { void M(); } abstract class C : I { }"),
            []
        );
    }

    /// An explicit implementation must be credited to ITS OWN interface, and to a member with the
    /// same parameter list.
    ///
    /// The first row is the program that showed it was not: a class implementing only `IA.M()`,
    /// credited with `IB`'s unrelated `M(int)` by a check that compared name SUFFIXES. csc reports
    /// CS0535 for `IB.M(int)`.
    ///
    /// **THE OTHER FOUR ROWS ARE THE FALSE-POSITIVE CONTROLS, AND THEY ARE WHY THE LENIENT CLAUSE
    /// EXISTED.** One member has two legal qualifier spellings -- written through a `using`, and
    /// fully qualified -- and a check that compared the stored name as TEXT would refuse one of
    /// them. A false CS0535 is worse than accepting a bad program, so the strict path has to be
    /// shown accepting both before the lenient one can be believed dead.
    #[test]
    fn an_explicit_implementation_is_credited_only_to_its_own_interface_member() {
        use lamella_syntax::parser::parse_compilation_unit;
        let codes = |source: &str| {
            let unit = parse_compilation_unit(source).unit;
            let mut codes: Vec<u16> = crate::bind_compilation_unit(&unit)
                .iter()
                .map(Diagnostic::code)
                .collect();
            codes.sort_unstable();
            codes
        };

        assert_eq!(
            codes(
                "interface IA { int M(); } interface IB { string M(int x); } \
                 class C : IA, IB { int IA.M() { return 1; } }"
            ),
            [535]
        );
        assert_eq!(
            codes(
                "interface IA { int M(); } interface IB { string M(int x); } \
                 class C : IA, IB { int IA.M() { return 1; } string IB.M(int x) { return null; } }"
            ),
            []
        );
        assert_eq!(
            codes(
                "namespace N { interface I { void M(); } } \
                 class C : N.I { void N.I.M() { } }"
            ),
            []
        );
        assert_eq!(
            codes(
                "using N; namespace N { interface I { void M(); } } \
                 class C : N.I { void I.M() { } }"
            ),
            []
        );
        assert_eq!(
            codes(
                "interface I { int P { get; } } \
                 class C : I { int I.P { get { return 1; } } }"
            ),
            []
        );
    }

    #[test]
    fn interface_accessor_implemented_by_a_property_or_event_is_not_cs0535() {
        use crate::symbols::{
            Accessibility, EventSymbol, MethodSymbol, PropertySymbol, TypeInfo, TypeKind,
        };
        let int = || TypeSymbol::Special(SpecialType::Int32);
        let accessor = |name: &str, parameters: Vec<TypeSymbol>| MethodSymbol {
            name: name.into(),
            return_type: int(),
            parameters,
            parameter_info: Vec::new(),
            is_static: false,
            is_params: false,
            is_vararg: false,
            is_virtual: true,
            is_abstract: true,
            is_override: false,
            is_sealed: false,
            accessibility: Accessibility::Public,
            conditional: Vec::new(),
            sets_required_members: false,
            type_parameters: Vec::new(),
            type_parameter_constraints: Vec::new(),
        };
        let get_p = accessor("get_P", Vec::new());
        let set_p = accessor("set_P", vec![int()]);
        let add_e = accessor("add_E", vec![int()]);
        let remove_e = accessor("remove_E", vec![int()]);
        let property = |has_getter, has_setter| PropertySymbol {
            name: "P".into(),
            ty: int(),
            is_static: false,
            accessibility: Accessibility::Public,
            is_virtual: false,
            is_abstract: false,
            is_override: false,
            is_sealed: false,
            has_getter,
            has_setter,
            is_required: false,
        };
        let event = || EventSymbol {
            name: "E".into(),
            ty: int(),
            is_static: false,
            accessibility: Accessibility::Public,
            is_abstract: false,
        };
        let implements =
            |properties: Vec<PropertySymbol>, events: Vec<EventSymbol>, member: &MethodSymbol| {
                let mut class = TypeInfo::new("", "C", TypeKind::Class);
                class.properties = properties;
                class.events = events;
                let mut model = Model::new();
                model.insert(class);
                Binder::with_model(model).implements_interface_member(
                    &TypeSymbol::Named(["C".into()].into()),
                    &TypeSymbol::Named(["I".into()].into()),
                    member,
                )
            };

        assert!(implements(vec![property(true, true)], Vec::new(), &get_p));
        assert!(implements(vec![property(true, true)], Vec::new(), &set_p));
        assert!(implements(Vec::new(), vec![event()], &add_e));
        assert!(implements(Vec::new(), vec![event()], &remove_e));
        assert!(!implements(Vec::new(), Vec::new(), &get_p));
        assert!(!implements(Vec::new(), Vec::new(), &add_e));
        assert!(implements(vec![property(true, false)], Vec::new(), &get_p));
        assert!(!implements(vec![property(true, false)], Vec::new(), &set_p));
    }

    #[test]
    fn override_of_a_non_overridable_or_return_mismatched_base_is_cs0506_cs0508() {
        use lamella_syntax::parser::parse_compilation_unit;
        let codes = |source: &str| {
            let unit = parse_compilation_unit(source).unit;
            let mut codes: Vec<u16> = crate::bind_compilation_unit(&unit)
                .iter()
                .map(Diagnostic::code)
                .collect();
            codes.sort_unstable();
            codes
        };

        assert_eq!(
            codes("class B { public void M() { } } class D : B { public override void M() { } }"),
            [506]
        );
        assert_eq!(
            codes(
                "class B { public virtual int M() { return 0; } } \
                 class D : B { public override string M() { return null; } }"
            ),
            [508]
        );
        assert_eq!(
            codes(
                "class B { public virtual int M() { return 0; } } \
                 class D : B { public override int M() { return 1; } }"
            ),
            []
        );
        assert_eq!(
            codes("class C { public override string ToString() { return \"c\"; } }"),
            []
        );
        assert_eq!(
            codes(
                "class B { public virtual int M() { return 0; } } \
                 class D : B { public override int M() { return 1; } } \
                 class E : D { public override int M() { return 2; } }"
            ),
            []
        );
    }

    /// The four FALSE POSITIVES this cluster closed: valid C# 1.0 that lcsc refused. These cannot
    /// live in `corpus-invalid` -- csc accepts every one -- so the guard is here, asserting the
    /// absence of a diagnostic rather than its presence.
    #[test]
    fn valid_conversions_and_casts_are_not_refused() {
        use lamella_syntax::parser::parse_compilation_unit;
        let codes = |source: &str| {
            let unit = parse_compilation_unit(source).unit;
            let mut codes: Vec<u16> = crate::bind_compilation_unit(&unit)
                .iter()
                .map(Diagnostic::code)
                .collect();
            codes.sort_unstable();
            codes.dedup();
            codes
        };

        for valid in [
            "interface I { } interface J { } \
             class C { static J M(I v) { return (J)v; } }",
            "interface I { } class B { } \
             class C { static I M(B v) { return (I)v; } }",
            "class S { public static implicit operator short(S v) { return 1; } } \
             class C { static int M() { int v = new S(); return v; } }",
            "struct S { public static bool operator true(S v) { return true; } \
             public static bool operator false(S v) { return false; } } \
             class C { static int M(S c) { return c ? 1 : 2; } }",
        ] {
            assert_eq!(codes(valid), [], "expected no diagnostic for: {valid}");
        }

        assert_eq!(
            codes("interface I { } sealed class B { } class C { static I M(B v) { return (I)v; } }"),
            [30]
        );
        assert_eq!(
            codes("interface I { } struct S { } class C { static I M(S v) { return (I)v; } }"),
            [30]
        );
    }

    #[test]
    fn control_transfers_and_operands_that_have_no_meaning_in_context() {
        use lamella_syntax::parser::parse_compilation_unit;
        let codes = |source: &str| {
            let unit = parse_compilation_unit(source).unit;
            let mut codes: Vec<u16> = crate::bind_compilation_unit(&unit)
                .iter()
                .map(Diagnostic::code)
                .collect();
            codes.sort_unstable();
            codes.dedup();
            codes
        };

        assert_eq!(codes("class C { static void M() { throw; } }"), [156]);
        assert_eq!(
            codes("class C { static int M() { try { return 1; } finally { return 2; } } }"),
            [157]
        );
        assert_eq!(
            codes(
                "class C { static void M() { while (true) { try { } finally { break; } } } }"
            ),
            [157]
        );
        assert_eq!(codes("class C { static void M() { goto case 1; } }"), [153]);
        assert_eq!(codes("class C { static void M() { lock (1) { } } }"), [185]);
        assert_eq!(
            codes("abstract class A { } class C { static void M() { A a = new A(); } }"),
            [144]
        );
        assert_eq!(codes("class C { const int V = 1 / 0; }"), [20]);
        assert_eq!(codes("class C { const int V = 1 % 0; }"), [20]);

        for clean in [
            "class C { static void M() { try { } finally { while (true) { break; } } } }",
            "class C { static void M() { try { } catch { throw; } } }",
            "class C { static void M() { object o = new object(); lock (o) { } } }",
            "class C { static int M(int d) { return 1 / d; } }",
            "class C { static int M(int x) { switch (x) { case 1: goto case 2; \
             case 2: return 2; default: return 0; } } }",
            "class C { static int M() { try { return 1; } finally { } } }",
        ] {
            assert_eq!(codes(clean), [], "expected no diagnostic for: {clean}");
        }
    }

    #[test]
    fn a_member_that_matches_an_interface_signature_must_also_be_public_and_return_its_type() {
        use lamella_syntax::parser::parse_compilation_unit;
        let codes = |source: &str| {
            let unit = parse_compilation_unit(source).unit;
            let mut codes: Vec<u16> = crate::bind_compilation_unit(&unit)
                .iter()
                .map(Diagnostic::code)
                .collect();
            codes.sort_unstable();
            codes.dedup();
            codes
        };

        assert_eq!(
            codes(
                "interface I { int M(); } \
                 class C : I { public string M() { return null; } }"
            ),
            [738]
        );
        assert_eq!(
            codes(
                "interface I { int P { get; } } \
                 class C : I { public string P { get { return null; } } }"
            ),
            [738]
        );
        assert_eq!(
            codes("interface I { int M(); } class C : I { int M() { return 1; } }"),
            [737]
        );
        assert_eq!(codes("interface I { int M(); } class C : I { }"), [535]);

        for clean in [
            "interface I { int M(); } class C : I { public int M() { return 1; } }",
            "interface I { int P { get; } } class C : I { public int P { get { return 1; } } }",
            "interface I { int M(); } abstract class C : I { }",
        ] {
            assert_eq!(codes(clean), [], "expected no diagnostic for: {clean}");
        }
    }

    /// An explicit implementation is matched BEFORE the implicit rules (20.4.1), so a same-named
    /// member that is not itself a valid implementation does not shadow it.
    ///
    /// **THE TABLE IS OVER THE WAYS THE PAIR CAN BE WRITTEN, NOT OVER ONE EXAMPLE.** The shape
    /// arrives in real source as the canonical collection -- `public IEnumerator<int>
    /// GetEnumerator()` for `IEnumerable<T>` beside `IEnumerator IEnumerable.GetEnumerator()` for
    /// the inherited non-generic one -- but nothing about it needs generics or an import, and
    /// framing it that way is what kept it off the queue. The last two rows are the controls that
    /// make the accept column non-vacuous: the same class shape with the explicit implementation
    /// naming a DIFFERENT interface must still report, or the check is crediting anything dotted.
    #[test]
    fn an_explicit_implementation_is_not_shadowed_by_a_failing_implicit_candidate() {
        use lamella_syntax::parser::parse_compilation_unit;
        let codes = |source: &str| {
            let unit = parse_compilation_unit(source).unit;
            let mut codes: Vec<u16> = crate::bind_compilation_unit(&unit)
                .iter()
                .map(Diagnostic::code)
                .collect();
            codes.sort_unstable();
            codes.dedup();
            codes
        };

        for clean in [
            "interface IA { int M(); } interface IB { string M(); } \
             class C : IA, IB { public int M() { return 1; } string IB.M() { return null; } }",
            "interface IA { int M(); } interface IB { string M(); } \
             class C : IA, IB { public string M() { return null; } int IA.M() { return 1; } }",
            "interface IA { int M(); } class C : IA { int M() { return 1; } int IA.M() { return 2; } }",
            "interface IBase { int M(); } interface IDerived : IBase { string M(string s); } \
             class C : IDerived { public string M(string s) { return s; } int IBase.M() { return 1; } }",
            "interface IBase { int M(); } interface IDerived : IBase { string N(); } \
             class C : IDerived { int IBase.M() { return 1; } string IDerived.N() { return null; } }",
        ] {
            assert_eq!(codes(clean), [], "expected no diagnostic for: {clean}");
        }

        assert_eq!(
            codes(
                "interface IA { int M(); } interface IB { string M(); } \
                 class C : IA, IB { int IA.M() { return 1; } public int M() { return 1; } }"
            ),
            [738],
            "IB.M is not implemented: the explicit member names IA"
        );
        assert_eq!(
            codes(
                "interface IA { int M(); } interface IB { string M(); } \
                 class C : IA, IB { int IA.M() { return 1; } private string M() { return null; } }"
            ),
            [737],
            "IB.M is not implemented: the explicit member names IA"
        );
    }

    #[test]
    fn an_unimplemented_abstract_accessor_member_is_cs0534() {
        use lamella_syntax::parser::parse_compilation_unit;
        let codes = |source: &str| {
            let unit = parse_compilation_unit(source).unit;
            let mut codes: Vec<u16> = crate::bind_compilation_unit(&unit)
                .iter()
                .map(Diagnostic::code)
                .collect();
            codes.sort_unstable();
            codes.dedup();
            codes
        };

        assert_eq!(
            codes("abstract class B { public abstract int P { get; } } class D : B { }"),
            [534]
        );
        assert_eq!(
            codes("abstract class B { public abstract int this[int i] { get; } } class D : B { }"),
            [534]
        );
        assert_eq!(
            codes(
                "delegate void H(); \
                 abstract class B { public abstract event H E; } class D : B { }"
            ),
            [534]
        );

        for clean in [
            "abstract class B { public abstract int P { get; } } \
             class D : B { public override int P { get { return 1; } } }",
            "abstract class B { public abstract int this[int i] { get; } } \
             class D : B { public override int this[int i] { get { return i; } } }",
            "delegate void H(); \
             abstract class B { public abstract event H E; } \
             abstract class D : B { }",
            "class B { public virtual int P { get { return 0; } } } class D : B { }",
        ] {
            assert_eq!(codes(clean), [], "expected no diagnostic for: {clean}");
        }
    }

    #[test]
    fn override_of_a_property_or_indexer_follows_the_same_rules_as_a_method() {
        use lamella_syntax::parser::parse_compilation_unit;
        let codes = |source: &str| {
            let unit = parse_compilation_unit(source).unit;
            let mut codes: Vec<u16> = crate::bind_compilation_unit(&unit)
                .iter()
                .map(Diagnostic::code)
                .collect();
            codes.sort_unstable();
            codes.dedup();
            codes
        };

        assert_eq!(
            codes(
                "class B { public int P { get { return 0; } } } \
                 class D : B { public override int P { get { return 1; } } }"
            ),
            [506]
        );
        assert_eq!(
            codes(
                "class B { public int this[int i] { get { return 0; } } } \
                 class D : B { public override int this[int i] { get { return 1; } } }"
            ),
            [506]
        );
        assert_eq!(
            codes(
                "class B { public virtual int P { get { return 0; } } } \
                 class D : B { public sealed override int P { get { return 1; } } } \
                 class E : D { public override int P { get { return 2; } } }"
            ),
            [239]
        );
        assert_eq!(
            codes(
                "class B { public virtual int this[int i] { get { return 0; } } } \
                 class D : B { public sealed override int this[int i] { get { return 1; } } } \
                 class E : D { public override int this[int i] { get { return 2; } } }"
            ),
            [239]
        );
        assert_eq!(
            codes(
                "class B { public virtual int P { get { return 0; } } } \
                 class D : B { protected override int P { get { return 1; } } }"
            ),
            [507]
        );
        assert_eq!(
            codes(
                "class B { protected virtual int P { get { return 0; } } } \
                 class D : B { public override int P { get { return 1; } } }"
            ),
            [507]
        );
        assert_eq!(
            codes(
                "class B { public virtual int this[int i] { get { return 0; } } } \
                 class D : B { protected override int this[int i] { get { return 1; } } }"
            ),
            [507]
        );
        assert_eq!(
            codes(
                "class B { public virtual int P { get { return 0; } } } \
                 class D : B { public override string P { get { return null; } } }"
            ),
            [1715]
        );
        assert_eq!(
            codes(
                "class B { public virtual int this[int i] { get { return 0; } } } \
                 class D : B { public override string this[int i] { get { return null; } } }"
            ),
            [1715]
        );

        for clean in [
            "class B { public virtual int P { get { return 0; } } } \
             class D : B { public override int P { get { return 1; } } }",
            "class B { protected virtual int P { get { return 0; } } } \
             class D : B { protected override int P { get { return 1; } } }",
            "class B { public virtual int P { get { return 0; } } } \
             class D : B { public sealed override int P { get { return 1; } } }",
            "class B { public virtual int P { get { return 0; } } } \
             class D : B { public sealed override int P { get { return 1; } } } \
             class E : D { public new int P { get { return 2; } } }",
            "abstract class B { public abstract int P { get; } } \
             class D : B { public override int P { get { return 1; } } }",
            "class B { public virtual int P { get { return 0; } } } \
             class D : B { public override int P { get { return 1; } } } \
             class E : D { public override int P { get { return 2; } } }",
            "class B { public virtual int P { get { return 0; } set { } } } \
             class D : B { public override int P { get { return 1; } } }",
            "class B { public virtual int this[int i] { get { return 0; } set { } } } \
             class D : B { public override int this[int i] { get { return 1; } set { } } }",
        ] {
            assert_eq!(codes(clean), [], "expected no diagnostic for: {clean}");
        }
    }

    #[test]
    fn switch_binds_constant_cases_and_flags_a_non_constant_label() {
        use lamella_syntax::parser::parse_compilation_unit;
        let codes = |source: &str| {
            let unit = parse_compilation_unit(source).unit;
            let mut codes: Vec<u16> = crate::bind_compilation_unit(&unit)
                .iter()
                .map(Diagnostic::code)
                .collect();
            codes.sort_unstable();
            codes
        };

        assert_eq!(
            codes(
                "class C { static int Run() { int x = 1; int y = 2; \
                 switch (x) { case y: return 1; default: return 0; } } }"
            ),
            [150]
        );
        assert_eq!(
            codes(
                "class C { static int Run() { int x = 1; \
                 switch (x) { case 1: return 1; default: return 0; } } }"
            ),
            []
        );
    }

    #[test]
    fn switch_duplicate_label_is_cs0152_and_fall_through_is_cs0163() {
        use lamella_syntax::parser::parse_compilation_unit;
        let codes = |source: &str| {
            let unit = parse_compilation_unit(source).unit;
            let mut codes: Vec<u16> = crate::bind_compilation_unit(&unit)
                .iter()
                .map(Diagnostic::code)
                .collect();
            codes.sort_unstable();
            codes
        };

        assert_eq!(
            codes(
                "class C { static int Run(int x) { \
                 switch (x) { case 1: return 1; case 1: return 2; default: return 0; } } }"
            ),
            [152]
        );
        assert_eq!(
            codes(
                "class C { static int Run(int x) { int y = 0; \
                 switch (x) { case 1: y = 1; default: y = 2; break; } return y; } }"
            ),
            [163]
        );
        assert_eq!(
            codes(
                "class C { static int Run(int x) { int y = 0; \
                 switch (x) { case 1: y = 1; break; default: break; } return y; } }"
            ),
            []
        );
    }

    #[test]
    fn a_pairable_operator_declared_alone_is_cs0216() {
        use lamella_syntax::parser::parse_compilation_unit;
        let codes = |source: &str| {
            let unit = parse_compilation_unit(source).unit;
            let mut codes: Vec<u16> = crate::bind_compilation_unit(&unit)
                .iter()
                .map(Diagnostic::code)
                .collect();
            codes.sort_unstable();
            codes
        };

        for lone in [
            "public static bool operator <(C a, C b) { return false; }",
            "public static bool operator >(C a, C b) { return false; }",
            "public static bool operator <=(C a, C b) { return false; }",
            "public static bool operator >=(C a, C b) { return false; }",
            "public static bool operator true(C a) { return false; }",
            "public static bool operator false(C a) { return false; }",
        ] {
            assert_eq!(
                codes(&alloc::format!("class C {{ {lone} }}")),
                [216],
                "expected CS0216 for: {lone}"
            );
        }

        for source in [
            "class C { public static bool operator <(C a, C b) { return false; } \
             public static bool operator >(C a, C b) { return false; } }",
            "class C { public static bool operator >=(C a, C b) { return false; } \
             public static bool operator <=(C a, C b) { return false; } }",
            "class C { public static bool operator true(C a) { return false; } \
             public static bool operator false(C a) { return false; } }",
            "struct S { public static bool operator <(S a, S b) { return false; } \
             public static bool operator >(S a, S b) { return false; } }",
            "class C { public static C operator +(C a, C b) { return a; } }",
            "class C { public static C operator ~(C a) { return a; } }",
        ] {
            assert_eq!(codes(source), [], "false positive on: {source}");
        }
    }

    #[test]
    fn catching_or_throwing_a_non_exception_type_is_cs0155() {
        use lamella_syntax::parser::parse_compilation_unit;
        let codes = |source: &str| {
            let unit = parse_compilation_unit(source).unit;
            let mut codes: Vec<u16> = crate::bind_compilation_unit(&unit)
                .iter()
                .map(Diagnostic::code)
                .collect();
            codes.sort_unstable();
            codes
        };

        for source in [
            "class C { static void M() { try { } catch (int e) { int y = e; } } }",
            "class C { static void M() { try { } catch (string e) { string y = e; } } }",
            "class C { static void M() { try { } catch (object e) { object y = e; } } }",
            "class K { } class C { static void M() { try { } catch (K e) { K y = e; } } }",
            "struct S { } class C { static void M() { try { } catch (S e) { S y = e; } } }",
            "interface I { } class C { static void M() { try { } catch (I e) { I y = e; } } }",
            "enum E { A } class C { static void M() { try { } catch (E e) { E y = e; } } }",
            "class C { static void M() { throw 5; } }",
            "class C { static void M() { throw \"x\"; } }",
            "class K { } class C { static void M() { throw new K(); } }",
        ] {
            assert_eq!(codes(source), [155], "expected CS0155 for: {source}");
        }

        assert_eq!(codes("class C { static void M() { throw null; } }"), []);
        assert_eq!(
            codes("class C { static void M() { try { } catch { throw; } } }"),
            []
        );
        assert_eq!(
            codes("class C { static void M() { try { } catch (Whatever e) { object y = e; } } }"),
            [246]
        );
    }

    #[test]
    fn overriding_a_sealed_member_is_cs0239_and_changing_access_is_cs0507() {
        use lamella_syntax::parser::parse_compilation_unit;
        let codes = |source: &str| {
            let unit = parse_compilation_unit(source).unit;
            let mut codes: Vec<u16> = crate::bind_compilation_unit(&unit)
                .iter()
                .map(Diagnostic::code)
                .collect();
            codes.sort_unstable();
            codes
        };

        assert_eq!(
            codes(
                "class B { public virtual int M() { return 1; } } \
                 class D : B { public sealed override int M() { return 2; } } \
                 class E : D { public override int M() { return 3; } }"
            ),
            [239]
        );
        for derived in [
            "protected override int M() { return 2; }",
            "internal override int M() { return 2; }",
        ] {
            assert_eq!(
                codes(&alloc::format!(
                    "class B {{ public virtual int M() {{ return 1; }} }} \
                     class D : B {{ {derived} }}"
                )),
                [507]
            );
        }
        assert_eq!(
            codes(
                "class B { protected virtual int M() { return 1; } } \
                 class D : B { public override int M() { return 2; } }"
            ),
            [507]
        );

        for source in [
            "class B { public virtual int M() { return 1; } } \
             class D : B { public override int M() { return 2; } }",
            "class B { protected virtual int M() { return 1; } } \
             class D : B { protected override int M() { return 2; } }",
            "class B { public virtual int M() { return 1; } } \
             class D : B { public sealed override int M() { return 2; } }",
            "class B { public virtual int M() { return 1; } } \
             class D : B { public sealed override int M() { return 2; } } \
             class E : D { public new int M() { return 3; } }",
        ] {
            assert_eq!(codes(source), [], "false positive on: {source}");
        }
    }

    #[test]
    fn a_final_switch_section_that_falls_out_is_cs8070_naming_its_last_label() {
        use lamella_syntax::parser::parse_compilation_unit;
        let bind = |source: &str| {
            let unit = parse_compilation_unit(source).unit;
            crate::bind_compilation_unit(&unit)
        };
        let codes = |source: &str| {
            let mut codes: Vec<u16> = bind(source).iter().map(Diagnostic::code).collect();
            codes.sort_unstable();
            codes
        };
        let label_of = |source: &str| {
            bind(source)
                .iter()
                .find_map(|d| match &d.kind {
                    DiagnosticKind::SwitchFallOutFinal { label } => Some(label.to_string()),
                    DiagnosticKind::SwitchFallThrough { label } => Some(label.to_string()),
                    _ => None,
                })
                .expect("a fall-through diagnostic")
        };

        assert_eq!(
            codes(
                "class C { static int Run(int x) { int y = 0; \
                 switch (x) { case 1: y = 1; break; case 2: y = 2; } return y; } }"
            ),
            [8070]
        );
        assert_eq!(
            codes(
                "class C { static int Run(int x) { int y = 0; \
                 switch (x) { case 1: y = 1; default: y = 2; break; } return y; } }"
            ),
            [163]
        );

        assert_eq!(
            label_of(
                "class C { static int Run(int x) { int y = 0; \
                 switch (x) { case 1: case 2: y = 2; } return y; } }"
            ),
            "case 2:"
        );
        assert_eq!(
            label_of(
                "class C { static int Run(int x) { int y = 0; \
                 switch (x) { case 1: break; default: y = 2; } return y; } }"
            ),
            "default:"
        );
        assert_eq!(
            label_of(
                "class C { static int Run(string x) { int y = 0; \
                 switch (x) { case \"hi\": y = 2; } return y; } }"
            ),
            "case \"hi\":"
        );
    }

    #[test]
    fn duplicate_local_is_cs0128_and_shadowing_is_cs0136() {
        use lamella_syntax::parser::parse_compilation_unit;
        let codes = |source: &str| {
            let unit = parse_compilation_unit(source).unit;
            let mut codes: Vec<u16> = crate::bind_compilation_unit(&unit)
                .iter()
                .map(Diagnostic::code)
                .collect();
            codes.sort_unstable();
            codes
        };

        assert_eq!(
            codes("class C { static int Run() { int x = 1; int x = 2; return x; } }"),
            [128, 219]
        );
        assert_eq!(
            codes("class C { static int Run() { int x = 1; { int x = 2; return x; } } }"),
            [136, 219]
        );
        assert_eq!(
            codes("class C { static int Run() { int x = 1; { int y = 2; return x + y; } } }"),
            []
        );
    }

    #[test]
    fn non_statement_expression_is_cs0201() {
        use lamella_syntax::parser::parse_compilation_unit;
        let codes = |source: &str| {
            let unit = parse_compilation_unit(source).unit;
            let mut codes: Vec<u16> = crate::bind_compilation_unit(&unit)
                .iter()
                .map(Diagnostic::code)
                .collect();
            codes.sort_unstable();
            codes
        };

        assert_eq!(
            codes("class C { static void Run() { int x = 1; x + 1; } }"),
            [201]
        );
        assert_eq!(
            codes(
                "class C { static int Get() { return 1; } \
                 static void Run() { int x = 0; x = x + 1; Get(); } }"
            ),
            []
        );
    }

    #[test]
    fn narrowing_is_cs0266_but_unrelated_types_are_cs0029() {
        use lamella_syntax::parser::parse_compilation_unit;
        let codes = |source: &str| {
            let unit = parse_compilation_unit(source).unit;
            let mut codes: Vec<u16> = crate::bind_compilation_unit(&unit)
                .iter()
                .map(Diagnostic::code)
                .collect();
            codes.sort_unstable();
            codes
        };

        assert_eq!(
            codes("class C { static int Run() { int x = 3.14; return x; } }"),
            [266]
        );
        assert_eq!(
            codes("class C { static int Run() { int x = \"s\"; return x; } }"),
            [29]
        );
    }

    #[test]
    fn unused_locals_warn_cs0219_and_cs0168() {
        use lamella_syntax::parser::parse_compilation_unit;
        let codes = |source: &str| {
            let unit = parse_compilation_unit(source).unit;
            let mut codes: Vec<u16> = crate::bind_compilation_unit(&unit)
                .iter()
                .map(Diagnostic::code)
                .collect();
            codes.sort_unstable();
            codes
        };

        assert_eq!(
            codes("class C { static int Run() { int spare = 5; return 0; } }"),
            [219]
        );
        assert_eq!(
            codes("class C { static int Run() { bool b = 1; return 0; } }"),
            [29]
        );
        assert_eq!(
            codes("class C { static int Run() { int spare; return 0; } }"),
            [168]
        );
        assert_eq!(
            codes("class C { static int Run() { int x = 5; return x; } }"),
            []
        );
        assert_eq!(
            codes("class C { static int Run() { Bogus b = null; return 0; } }"),
            [246]
        );
        assert_eq!(
            codes("class C { static int Run() { { int x = 1; } { int x = 2; return x; } } }"),
            [219]
        );
        assert_eq!(
            codes("class C { static int Run() { int x = 1; if (x > 0) { int x = 2; return x; } return x; } }"),
            [136]
        );
        assert_eq!(
            codes("class C { static int Run() { int x = 5; { return x; } } }"),
            []
        );
    }

    #[test]
    fn unused_private_field_warns_cs0414() {
        use lamella_syntax::parser::parse_compilation_unit;
        let codes = |source: &str| {
            let unit = parse_compilation_unit(source).unit;
            let mut codes: Vec<u16> = crate::bind_compilation_unit(&unit)
                .iter()
                .map(Diagnostic::code)
                .collect();
            codes.sort_unstable();
            codes
        };

        assert_eq!(
            codes("class C { int f = 5; static int Run() { return 0; } }"),
            [414]
        );
        assert_eq!(
            codes("class C { int f; C() { f = 5; } static int Run() { return 0; } }"),
            [414]
        );
        assert_eq!(
            codes(
                "class C { int f; C() { f = 5; } int Get() { return f; } \
                 static int Run() { return 0; } }"
            ),
            []
        );
        assert_eq!(
            codes("class C { int f = 0; void Bump() { f += 1; } static int Run() { return 0; } }"),
            []
        );
        assert_eq!(
            codes("class C { public int f = 5; static int Run() { return 0; } }"),
            []
        );
        assert_eq!(
            codes("class C { const int f = 5; static int Run() { return 0; } }"),
            []
        );
        assert_eq!(
            codes("class C { int f; static int Run() { return 0; } }"),
            [169]
        );
        assert_eq!(
            codes("class C { readonly int f; void M() { f = 3; } static int Run() { return 0; } }"),
            [191]
        );
        assert_eq!(
            codes(
                "class C { int f; void M() { f = \"x\"; } static int Run() { return 0; } }"
            ),
            [29]
        );
    }

    #[test]
    fn unreferenced_label_is_cs0164() {
        use lamella_syntax::parser::parse_compilation_unit;
        let codes = |source: &str| {
            let unit = parse_compilation_unit(source).unit;
            let mut codes: Vec<u16> = crate::bind_compilation_unit(&unit)
                .iter()
                .map(Diagnostic::code)
                .collect();
            codes.sort_unstable();
            codes
        };

        assert_eq!(codes("class C { static int Run() { Foo: return 0; } }"), [164]);
        assert_eq!(
            codes("class C { static int Run() { goto Foo; Foo: return 0; } }"),
            []
        );
        assert_eq!(
            codes("class C { static int Run() { Again: ; Again: ; return 0; } }"),
            [140, 164]
        );
    }

    #[test]
    fn unreachable_code_is_cs0162() {
        use lamella_syntax::parser::parse_compilation_unit;
        let codes = |source: &str| {
            let unit = parse_compilation_unit(source).unit;
            let mut codes: Vec<u16> = crate::bind_compilation_unit(&unit)
                .iter()
                .map(Diagnostic::code)
                .collect();
            codes.sort_unstable();
            codes
        };

        assert_eq!(
            codes("class C { static int M() { return 1; int x = 5; return x; } }"),
            [162]
        );
        assert_eq!(
            codes("class C { static int M() { while (true) { } int x = 5; return x; } }"),
            [162]
        );
        assert_eq!(
            codes(
                "class C { static int M(int p) { \
                 if (p > 0) return 1; else return 2; int x = 5; return x; } }"
            ),
            [162]
        );
        assert_eq!(
            codes("class C { static int M() { while (true) { break; } return 42; } }"),
            []
        );
    }

    #[test]
    fn a_call_records_its_resolved_method() {
        use crate::symbols::{MethodSymbol, TypeInfo, TypeKind};

        let int = TypeSymbol::Special(SpecialType::Int32);
        let mut calc = TypeInfo::new("", "Calc", TypeKind::Class);
        calc.methods.push(MethodSymbol {
            name: "F".into(),
            return_type: int.clone(),
            parameters: alloc::vec![int.clone()],
            parameter_info: Vec::new(),
            is_static: false,
            is_params: false,
            is_vararg: false,
            is_virtual: false,
            is_abstract: false,
            is_override: false,
            is_sealed: false,
            accessibility: crate::symbols::Accessibility::Public,
            conditional: Vec::new(),
            sets_required_members: false,
            type_parameters: Vec::new(),
            type_parameter_constraints: Vec::new(),
        });
        let mut model = Model::new();
        model.insert(calc);

        let mut binder = Binder::with_model(model);
        binder.enter_scope();
        let calc_type = TypeSymbol::Named(["Calc".into()].into());
        binder.declare_local("c", calc_type.clone());
        let call = binder.bind_expression(&parse_expression("c.F(1)").expr);

        let BoundExprKind::Call {
            method: Some(method),
            ..
        } = call.kind
        else {
            panic!("the call should record its resolved method");
        };
        assert_eq!(&*method.name, "F");
        assert_eq!(method.parameters, alloc::vec![int.clone()]);
        assert_eq!(method.return_type, int);
        assert!(!method.is_static);
        assert_eq!(method.declaring_type, calc_type);
    }

    #[test]
    fn scopes_nest_and_unwind() {
        let mut binder = Binder::new();
        binder.enter_scope();
        binder.declare_local("outer", TypeSymbol::Special(SpecialType::Int32));
        binder.enter_scope();
        binder.declare_local("inner", TypeSymbol::Special(SpecialType::Boolean));
        assert!(!bound_in_scope(&mut binder, "outer").is_error());
        assert!(!bound_in_scope(&mut binder, "inner").is_error());
        binder.exit_scope();
        assert!(!bound_in_scope(&mut binder, "outer").is_error());
        let before = binder.diagnostics().len();
        assert!(bound_in_scope(&mut binder, "inner").is_error());
        assert_eq!(binder.diagnostics().len(), before + 1);
    }
}
