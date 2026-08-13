//! The type and member symbol model (ECMA-334 1st ed, clauses 17-18).

use crate::resolve::TypeTable;
use crate::special::SpecialType;
use crate::types::TypeSymbol;
use alloc::borrow::Cow;
use alloc::boxed::Box;
use lamella_syntax::ast::Literal;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

/// The flavour of a declared type (17.1, 18, 21, 22).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeKind {
    /// A `class`.
    Class,
    /// A `struct`.
    Struct,
    /// An `interface`.
    Interface,
    /// An `enum`.
    Enum,
    /// A `delegate`.
    Delegate,
}

/// A member's declared accessibility (10.5.1). The default for a class member is
/// [`Accessibility::Private`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Accessibility {
    /// `public` -- accessible everywhere.
    Public,
    /// `protected` -- the declaring type and its derived types.
    Protected,
    /// `internal` -- the declaring assembly.
    Internal,
    /// `protected internal` -- protected or internal.
    ProtectedInternal,
    /// `private` -- the declaring type only.
    Private,
}

impl Accessibility {
    /// The C# keyword, spelled as a diagnostic message quotes it.
    #[must_use]
    pub(crate) fn keyword(self) -> &'static str {
        match self {
            Accessibility::Public => "public",
            Accessibility::Protected => "protected",
            Accessibility::Internal => "internal",
            Accessibility::ProtectedInternal => "protected internal",
            Accessibility::Private => "private",
        }
    }
}

/// A field of a type (17.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldSymbol {
    /// The field's name.
    pub name: Box<str>,
    /// The field's type.
    pub ty: TypeSymbol,
    /// Whether the field is `static`.
    pub is_static: bool,
    /// Whether the field is `readonly` (assignable only in a constructor or initializer).
    pub is_readonly: bool,
    /// Whether the field is `volatile` (17.4.3): its reads and writes carry the `volatile.`
    /// prefix so the runtime does not reorder them.
    pub is_volatile: bool,
    /// The field's accessibility.
    pub accessibility: Accessibility,
    /// The compile-time constant value of a `const` field or enum member (folded at the use
    /// site instead of an `ldsfld`); `None` for an ordinary field.
    pub constant: Option<Literal>,
    /// Whether the field is `required` (C# 11): every object creation must assign it in an
    /// object initializer, or through a constructor carrying `[SetsRequiredMembers]`.
    ///
    /// Carried across an assembly boundary by a `RequiredMemberAttribute` (II.23.2), which the
    /// reference reader decodes: there is no FieldAttributes bit for `required`, so the attribute
    /// IS the encoding. Both halves closed together, as the earlier note here predicted they would
    /// -- this compiler emits the attribute and reads it.
    pub is_required: bool,
}

/// A property of a type (17.6), reduced to its name and type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PropertySymbol {
    /// The property's name.
    pub name: Box<str>,
    /// The property's type.
    pub ty: TypeSymbol,
    /// Whether the property is `static`.
    pub is_static: bool,
    /// The property's accessibility.
    pub accessibility: Accessibility,
    /// Whether the property is declared `virtual` -- its accessors are overridable slots.
    pub is_virtual: bool,
    /// Whether the property is `abstract` (its accessors have no body and a concrete derived
    /// class must override them).
    pub is_abstract: bool,
    /// Whether the property is declared `override` -- it replaces a base slot rather than
    /// introducing a new one.
    pub is_override: bool,
    /// Whether the property is declared `sealed` -- an `override` that CLOSES its slot, so no
    /// further derived class may override it.
    pub is_sealed: bool,
    /// Whether this declaration provides a `get` accessor. A partially-overridden property may
    /// declare only one accessor and inherit the other, so each accessor is named on the type
    /// that declares it (14.5.4).
    pub has_getter: bool,
    /// Whether this declaration provides a `set` accessor.
    pub has_setter: bool,
    /// Whether the property is `required` (C# 11); see [`FieldSymbol::is_required`].
    ///
    /// **An imported property's answer comes from the Property TABLE, not from its accessors.**
    /// The reference reader synthesizes a property from its `get_`/`set_` methods, and
    /// `RequiredMemberAttribute` sits on the property row -- so a walk over accessors can never
    /// find it, and would answer `false` for every imported required property without looking like
    /// it had missed anything.
    pub is_required: bool,
}

/// A field-like event of a type (17.7): its `add`/`remove` accessors combine/remove a
/// handler on a backing delegate field. Outside the declaring type, `+=`/`-=` route through
/// the accessors and any other use is `CS0070`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventSymbol {
    /// The event's name.
    pub name: Box<str>,
    /// The event's (delegate) type.
    pub ty: TypeSymbol,
    /// Whether the event is `static`.
    pub is_static: bool,
    /// The event's accessibility (the visibility of its `add`/`remove` accessors).
    pub accessibility: Accessibility,
    /// Whether the event is `abstract`: its `add`/`remove` accessors are unimplemented slots a
    /// concrete derived class must supply.
    pub is_abstract: bool,
}

/// How a parameter is passed (17.5.1). The signature TYPE records by-reference-ness
/// (`TypeSymbol::ByRef`) but cannot tell `ref` from `out`: both are `T&` in metadata, separated
/// only by the `Out` flag on the parameter row. Diagnostics need the distinction -- CS1620 is
/// literally "this argument needs `ref`, not `out`" -- so it is recorded here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ParameterMode {
    /// Passed by value.
    #[default]
    Value,
    /// `ref`: passed by reference, assigned by the caller before the call.
    Ref,
    /// `out`: passed by reference, assigned by the callee before it returns.
    Out,
}

/// What a parameter DECLARES beyond its type: the name a diagnostic quotes, and whether it is
/// `ref` or `out`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParameterInfo {
    /// The declared name. Empty when the source of this method could not supply one.
    pub name: Box<str>,
    /// `ref` / `out` / by value.
    pub mode: ParameterMode,
}

/// A method of a type (17.5), reduced to what overload resolution needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodSymbol {
    /// The method's name.
    pub name: Box<str>,
    /// The return type (`void` is `SpecialType::Void`).
    pub return_type: TypeSymbol,
    /// The parameter types, in order. This is what overload resolution and signature identity are
    /// computed from, and it is deliberately unchanged: widening it to a struct would rewrite ~230
    /// call sites for facts almost none of them want.
    pub parameters: Vec<TypeSymbol>,
    /// The declaration facts for those parameters -- names and `ref`/`out` modes.
    ///
    /// INVARIANT: EMPTY, or exactly as long as `parameters`. Empty means "not known", which is the
    /// honest state for a synthesized method or one whose source could not supply names -- and it
    /// is why this is a separate vector rather than fields on the type list. A consumer must treat
    /// empty as absence of information, never as "no parameters": see [`MethodSymbol::parameter_name`].
    pub parameter_info: Vec<ParameterInfo>,
    /// Whether the method is `static`.
    pub is_static: bool,
    /// Whether the last parameter is a `params` array (a variable-length trailing
    /// argument list at the call site).
    pub is_params: bool,
    /// Whether the parameter list ends with csc's `__arglist` marker (the typedref knob):
    /// the member takes CLI varargs beyond `parameters`, and a call must supply a trailing
    /// `__arglist(...)` argument at the sentinel position.
    pub is_vararg: bool,
    /// Whether the method is declared `virtual` -- an overridable vtable slot. An
    /// interface's members are implicitly virtual+abstract.
    pub is_virtual: bool,
    /// Whether the method is `abstract` (has no body and must be overridden by a concrete
    /// derived class). An interface's members are implicitly abstract.
    pub is_abstract: bool,
    /// Whether the method is declared `override` -- it replaces a base `virtual`/`abstract`/
    /// `override` slot rather than introducing a new one.
    pub is_override: bool,
    /// Whether the method is declared `sealed` -- an `override` that CLOSES its slot, so no
    /// further derived class may override it. False for a referenced or synthetic method
    /// (an under-report: a missed sealed slot is a gap, never a false positive).
    pub is_sealed: bool,
    /// The method's accessibility.
    pub accessibility: Accessibility,
    /// The `[Conditional("SYMBOL")]` symbols (24.4.2): a call to this method is omitted unless
    /// one of these is defined at the call site. Empty for an unconditional method.
    pub conditional: Vec<Box<str>>,
    /// Whether this constructor carries `[System.Diagnostics.CodeAnalysis.SetsRequiredMembers]`:
    /// it promises to set every `required` member of its type, so an object creation through it
    /// needs no object initializer and draws no `CS9035`.
    ///
    /// **MEASURED: this is the ONLY thing that grants the exemption.** Assigning the member in
    /// an ordinary constructor body does not -- `class C { public required int P; public C() { P = 1; } }`
    /// still draws CS9035 at `new C()`. The rule is metadata rather than definite assignment,
    /// which is exactly what lets it hold across an assembly boundary where no body is available
    /// to inspect. Meaningless on a non-constructor and always `false` there.
    pub sets_required_members: bool,
    /// The names of the method's OWN type parameters, in declaration order -- `["T"]` for
    /// `T Id<T>(T x)`. Empty for every ordinary method, which is every C# 1.0 method.
    ///
    /// **THESE ARE THE METHOD'S, NEVER THE DECLARING TYPE'S.** The distinction is the one
    /// metadata spells `!!0` against `!0`: inside `class Box<T> { U Map<U>(U u) }` the type
    /// contributes `T` and this list holds only `U`. Merging them would make `Map` look
    /// two-parameter and let `Map<int, string>(x)` resolve.
    ///
    /// **Unlike [`TypeInfo::type_parameters`], this IS load-bearing for resolution rather than
    /// only for diagnostics.** A type's arity is already mangled into its name
    /// ([`metadata_type_name`]); a method's is not part of its name at all -- `M()` and `M<T>()`
    /// are separate overloads distinguished only by this count -- so ECMA-334 14.5.5.1 selects
    /// candidates by comparing it against the call site's type-argument count.
    pub type_parameters: Vec<Box<str>>,
    /// The constraints on each of this method's OWN type parameters (25.7), in the same order.
    ///
    /// **May be SHORTER than `type_parameters`, unlike the type-level pair**, and
    /// [`MethodSymbol::constraints_on`] is what makes that safe. A method reaches this model from
    /// two directions -- collected from source, where the clauses are known, and rebuilt in a
    /// dozen synthetic sites that have no syntax to read -- so requiring the lengths to match would
    /// make every synthetic site state a fact it does not have. An absent entry means "nothing
    /// known", which is the same under-report `is_sealed` takes, and it errs toward accepting.
    pub type_parameter_constraints: Vec<TypeParameterConstraints>,
}

impl MethodSymbol {
    /// The constraints on this method's type parameter at `index`, or `None` when there is no such
    /// parameter or nothing was recorded for it. See the field for why absence is legal here and
    /// is not on [`TypeInfo`].
    #[must_use]
    pub fn constraints_on(&self, index: usize) -> Option<&TypeParameterConstraints> {
        if index >= self.type_parameters.len() {
            return None;
        }
        self.type_parameter_constraints.get(index)
    }

    /// The declared name of parameter `index`, or `None` when this method carries no parameter
    /// facts.
    ///
    /// EVERY CALLER MUST HANDLE `None`, and that is the point of returning an option rather than
    /// an empty string: a diagnostic whose message quotes a parameter name cannot be emitted at
    /// all for a method whose names we never learned. Emitting it with a blank or invented name
    /// would be worse than not emitting it -- the message would look authoritative and name the
    /// wrong thing.
    #[must_use]
    pub fn parameter_name(&self, index: usize) -> Option<&str> {
        self.parameter_info
            .get(index)
            .map(|info| &*info.name)
            .filter(|name| !name.is_empty())
    }

    /// How parameter `index` is passed, or `None` when this method carries no parameter facts.
    /// `None` is NOT `Value`: an unknown mode must not be reported as by-value, or CS1620 would
    /// fire on a method we simply could not read.
    #[must_use]
    pub fn parameter_mode(&self, index: usize) -> Option<ParameterMode> {
        self.parameter_info.get(index).map(|info| info.mode)
    }

    /// This generic method DEFINITION closed over `arguments`: the same method with every mention
    /// of one of its own type parameters replaced, so `Id<int>` returns `int` and takes `int` where
    /// `Id<T>` returns `T` and takes `T`.
    ///
    /// `None` when the argument count does not match the declared parameter count -- the same
    /// refusal-never-partial-substitution rule [`TypeInfo::instantiate`] follows, and for the same
    /// reason: a signature left half-substituted reads as ordinary and means something else.
    ///
    /// **THIS IS WHAT MAKES A GENERIC CALL TYPE-CHECK AT ALL, AND ITS ABSENCE IS SILENT.**
    /// ECMA-334 14.5.5.1: *"the parameters of a generic method are considered AFTER substituting
    /// the type arguments"*. Bind `Id<string>(1)` against the OPEN method and the argument is
    /// checked against `T`, which accepts anything -- the call compiles, resolves to the open
    /// method with `!!0` never substituted, and is wrong. Substituted, the same call is the CS1503
    /// csc reports. **The difference between correct and catastrophically wrong here produces no
    /// diagnostic on the failing side, so it can only be caught by a test that asserts the
    /// REJECTION.**
    ///
    /// Substitution is BY NAME, the binder's model throughout -- see [`substitute`], whose doc
    /// explains why that is the same rule metadata's numbering expresses from the other side.
    #[must_use]
    pub fn instantiate(&self, arguments: &[TypeSymbol]) -> Option<MethodSymbol> {
        if arguments.len() != self.type_parameters.len() {
            return None;
        }
        let bindings: BTreeMap<&str, &TypeSymbol> = self
            .type_parameters
            .iter()
            .map(|parameter| &**parameter)
            .zip(arguments)
            .collect();
        let mut closed = self.clone();
        closed.type_parameters = Vec::new();
        closed.return_type = substitute(&self.return_type, &bindings);
        for parameter in &mut closed.parameters {
            *parameter = substitute(parameter, &bindings);
        }
        Some(closed)
    }
}

/// A named type with its members.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeInfo {
    /// The namespace, empty for the global namespace.
    pub namespace: Box<str>,
    /// The unqualified type name.
    pub name: Box<str>,
    /// The kind of type.
    pub kind: TypeKind,
    /// The direct base class, resolved from `bases` by [`Model::link_bases`].
    pub base: Option<TypeSymbol>,
    /// Every type listed after `:` (the base class and/or interfaces), as written.
    pub bases: Vec<TypeSymbol>,
    /// The type's fields.
    pub fields: Vec<FieldSymbol>,
    /// The type's properties.
    pub properties: Vec<PropertySymbol>,
    /// The type's methods.
    pub methods: Vec<MethodSymbol>,
    /// Names of members this build read the type's metadata for but could NOT decode, so they are
    /// absent from `methods` above.
    ///
    /// **THE POINT IS TO STOP A REFUSAL ARRIVING AS AN ABSENCE.** A signature the decoder declines
    /// -- today, any generic one -- yields `None`, and a consumer that skipped it would report the
    /// member as not existing. That is a false statement about someone else's assembly: the member
    /// is there and we cannot read it. Recording the name lets the failing lookup say which of the
    /// two it is.
    ///
    /// **IT DELIBERATELY DOES NOT MAKE THE TYPE UNUSABLE.** The
    /// refusal has to fire on USE of a specific member and stay silent otherwise.
    pub undecodable_members: Vec<Box<str>>,
    /// The type's field-like events (17.7), in addition to their backing delegate field in
    /// `fields`. Drives `+=`/`-=` routing through the accessors and `CS0070`.
    pub events: Vec<EventSymbol>,
    /// The type's instance constructors (each modeled as a method whose
    /// parameters drive `new T(...)` overload resolution).
    pub constructors: Vec<MethodSymbol>,
    /// For a nested type, the full name of the type it is nested in (e.g. `"Outer"`);
    /// `None` for a top-level type. Drives the `NestedClass` row and the empty namespace
    /// on emission.
    pub enclosing: Option<Box<str>>,
    /// Whether this type comes from a referenced assembly (not the unit being compiled), so
    /// an `internal` member of it is `CS0122` from here (cross-assembly internal).
    pub is_external: bool,
    /// For an external type, the simple name of the assembly that defines it (so its `TypeRef`
    /// is scoped to the right `AssemblyRef`, not just mscorlib). `None` for a this-module type.
    pub assembly: Option<Box<str>>,
    /// The type's declared accessibility (10.2.3), for the accessibility-consistency checks
    /// (CS0050-CS0053). Defaults to `public`; a source type sets it from its modifiers, so a
    /// reference or synthetic type is treated as public (a safe under-report of a non-public one).
    pub accessibility: Accessibility,
    /// Whether the type is `sealed` (10.1.1.2), for the CS0509 derive-from-sealed check. Defaults to
    /// `false`; a source type sets it from its modifiers, so a reference or synthetic type is treated
    /// as non-sealed (a safe under-report -- deriving from an unflagged sealed type is a gap, never a
    /// false positive).
    pub is_sealed: bool,
    /// Whether the type is `abstract` -- it cannot be instantiated. Defaults to `false` for a
    /// referenced or synthetic type, the same safe under-report as `is_sealed`.
    pub is_abstract: bool,
    /// The names of the type's declared type parameters, in declaration order -- `["T"]` for
    /// `Box<T>`. Empty for every non-generic type, which is every C# 1.0 type.
    ///
    /// This is for DIAGNOSTICS, not for identity: arity is already part of the type's `name`
    /// ([`metadata_type_name`]), so nothing resolves through this list. It exists because csc names
    /// the candidate by its parameters -- *Using the generic type 'Box<T>' requires 1 type
    /// arguments* -- and a message that printed the arity instead would be a different message.
    pub type_parameters: Vec<Box<str>>,
    /// The constraints on each declared type parameter (25.7), in the same order and ALWAYS the
    /// same length as `type_parameters` -- a parameter with no `where` clause gets an empty entry
    /// rather than a missing one.
    ///
    /// **Length equality is an invariant, not a coincidence, and [`TypeInfo::constraints_on`] is
    /// how it is read** so that a desynchronized pair cannot silently index the wrong parameter.
    /// Two parallel vectors are a shape that drifts when a new case lands in one of them; keeping
    /// the read behind an accessor is what makes a drift a `None` rather than a wrong answer.
    ///
    /// Unlike `type_parameters` this IS load-bearing: it decides whether `Box<int>` is legal, and
    /// it is what the `GenericParam` flag word and the `GenericParamConstraint` rows are built from.
    pub type_parameter_constraints: Vec<TypeParameterConstraints>,
}

/// The resolved constraints on ONE type parameter (ECMA-334 4th ed 25.7).
///
/// **The three flag constraints and the named ones are separate fields because metadata separates
/// them**: `class`/`struct`/`new()` are bits in the `GenericParam` flag word (II.23.1.7) while a
/// named class, interface or type parameter is a `GenericParamConstraint` ROW (II.22.21). Modeling
/// all four as a list of "constraints" would put the encoding decision at every read site.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TypeParameterConstraints {
    /// `where T : class` -- the argument must be a reference type. Metadata `0x0004`.
    pub reference_type: bool,
    /// `where T : struct` -- the argument must be a non-nullable value type. Metadata `0x0008`.
    ///
    /// **This IMPLIES the default-constructor bit in metadata but NOT in this model.** II.10.1.7
    /// requires an emitter that sets `0x0008` to set `0x0010` with it, because every value type has
    /// a parameterless constructor; the source, however, may not write both (CS0451). Keeping the
    /// source fact here and applying the implication at emission is what lets the check and the
    /// encoding disagree honestly rather than one of them being wrong.
    pub value_type: bool,
    /// `where T : new()` -- the argument must have a public parameterless constructor. Metadata
    /// `0x0010`.
    pub default_constructor: bool,
    /// The named class, interface and type-parameter constraints, in source order. Each becomes one
    /// `GenericParamConstraint` row.
    pub types: Vec<TypeSymbol>,
}

impl TypeParameterConstraints {
    /// Whether nothing was written -- the state of every parameter with no `where` clause, and of
    /// every type parameter in a C# 1.0 compilation.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        !self.reference_type
            && !self.value_type
            && !self.default_constructor
            && self.types.is_empty()
    }

    /// Whether the argument is required to have an accessible parameterless constructor -- written
    /// `new()`, or implied by `struct`. This is the test `new T()` (CS0304) asks.
    #[must_use]
    pub fn requires_default_constructor(&self) -> bool {
        self.default_constructor || self.value_type
    }
}

/// A type's METADATA name: the declared name with its generic arity mangled in, per ECMA-335
/// II.10.7.2 (`Box<T>` is `` Box`1 ``). An arity of 0 is the name unchanged, so every ordinary type
/// is untouched.
///
/// **THIS IS AN IDENTITY RULE, NOT A DISPLAY ONE.** Arity is PART of a generic type's name --
/// `Box`, `Box<T>` and `Box<T,U>` are three unrelated types that may all be declared in one
/// namespace, and only the mangled spelling tells them apart. A model keyed by the bare name
/// collapses them, which is why a wrong arity used to resolve to whichever one was collected.
///
/// It is the ONE spelling: a definition read from a reference assembly already arrives mangled, and
/// this function is what makes a definition read from SOURCE arrive the same way, so the two sources
/// meet in one key space. It is NOT the instantiation spelling -- naming `Box<int>` is
/// `lamella_aot::generics::TypeArg::spell`'s job and must never be re-implemented here.
#[must_use]
pub fn metadata_type_name(name: &str, arity: usize) -> String {
    if arity == 0 {
        String::from(name)
    } else {
        alloc::format!("{name}`{arity}")
    }
}

/// The generic arity mangled into a metadata type name: 1 for `` List`1 ``, 0 for `List`.
///
/// Only a NUMERIC tail is an arity -- `` Box`Extra `` is an ordinary type whose name happens to
/// contain a backtick, and reading it as one would silently renumber it. Same rule
/// [`crate::resolve::TypeTable::candidates`] applies searching the other way.
#[must_use]
pub fn mangled_arity(name: &str) -> usize {
    match name.rsplit_once('`') {
        Some((_, tail)) => tail.parse().unwrap_or(0),
        None => 0,
    }
}

/// [`metadata_type_name`] undone: `` Box`1 `` -> `Box`, and any other name unchanged.
///
/// **THE TWO SPELLINGS ARE NOT INTERCHANGEABLE AND THE COMPILER CANNOT TELL THEM APART.**
/// [`TypeSymbol::Instantiation`]'s `definition` holds the last part UNMANGLED -- every consumer
/// (`open_field`, `open_method`, [`definition_symbol`]) mangles the arity back in from the argument
/// count -- while a [`TypeInfo`]'s `name` holds it MANGLED, because that is the model's key. Both
/// are `Box<str>`, so handing one where the other is wanted type-checks and asks the model for
/// `` Box`1`1 ``, a name nothing declares: the lookup misses, the caller falls through to a path
/// that erases the instantiation, and no diagnostic is produced.
///
/// Only a NUMERIC tail is an arity, for [`mangled_arity`]'s reason. Applies to ONE part: a nested
/// definition's ENCLOSING parts stay mangled, which is what lets [`definition_metadata_name`]
/// recover how many parameters the last part introduces on its own.
#[must_use]
pub fn unmangled_type_name(name: &str) -> Box<str> {
    match name.rsplit_once('`') {
        Some((head, tail)) if !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) => {
            Box::from(head)
        }
        _ => Box::from(name),
    }
}

/// A generic DEFINITION's metadata name: its last part with the arity **it itself declares**
/// mangled in, given the whole instantiation's argument count.
///
/// **A NESTED TYPE'S ARITY IS NOT ITS INSTANTIATION'S ARGUMENT COUNT, AND THAT IS THE ONE CASE
/// [`metadata_type_name`] CANNOT BE ASKED DIRECTLY.** `List<T>.Enumerator` is spelled
/// `` List`1/Enumerator `` in metadata -- **`Enumerator`, with no suffix**, because a nested type's
/// mangled name carries only the parameters it introduces (II.10.7.2) while the enclosing type's
/// are supplied by the `GenericInst` wrapped around it. So `List<int>.Enumerator` is one type
/// ARGUMENT over a definition of arity ZERO, and mangling that argument count into the last part
/// asks for `` Enumerator`1 `` -- a name no assembly declares. Measured: it then resolved by simple
/// name to `` System.Diagnostics.Activity.Enumerator`1 `` out of a diagnostics package, and the
/// image threw `TypeLoadException` on load.
///
/// The arity the enclosing types already account for is read back off the definition's own
/// preceding parts, which carry it: a nested type is keyed with its enclosing type's FULL NAME
/// standing in for the namespace ([`crate::reference`] and `declaration.rs` both do this), so
/// `` ["System","Collections","Generic","List`1","Enumerator"] `` says 1 of the 1 argument belongs
/// to `` List`1 ``. A namespace segment never carries a backtick, so summing over every preceding
/// part is the same answer as summing over the enclosing types alone.
///
/// **THE RULE HAD TEN IMPLEMENTATIONS AND THE NESTED CASE WOULD HAVE LANDED IN ONE.** Every
/// consumer of an instantiation re-mangles its definition from the argument count -- the model's
/// four lookups, the emitter's token key, the arity refusal, the constraint check. They call this
/// instead, because a rule with several homes gains a new case in a subset of them and the subset
/// is usually empty.
#[must_use]
pub fn definition_metadata_name(definition: &[Box<str>], arguments: usize) -> String {
    let Some((name, enclosing)) = definition.split_last() else {
        return String::new();
    };
    let inherited: usize = enclosing.iter().map(|part| mangled_arity(part)).sum();
    metadata_type_name(name, arguments.saturating_sub(inherited))
}

/// An instantiation's generic DEFINITION as the plain named symbol every token table and model
/// holds it under -- the definition's parts with [`definition_metadata_name`] applied to the last.
///
/// This is the spelling both a source-collected and a reference-read definition arrive with, so it
/// is the one key space the two meet in.
#[must_use]
pub fn definition_symbol(definition: &[Box<str>], arguments: usize) -> TypeSymbol {
    let mut parts: Vec<Box<str>> = definition.to_vec();
    if let Some(last) = parts.last_mut() {
        *last = definition_metadata_name(definition, arguments).into();
    }
    TypeSymbol::Named(parts.into())
}

/// `ty` with every type parameter named in `bindings` replaced by its argument, recursing through
/// every position a parameter can hide in.
///
/// **SUBSTITUTION IS BY NAME HERE, AND THAT IS THE BINDER'S MODEL RATHER THAN A SHORTCUT.** A
/// metadata signature numbers its parameters (`!0`, and `lamella_aot::generics::TypeArg` matches
/// that model because it decodes signatures), but the binder never sees a number: `T` inside
/// `class Box<T>` is an ordinary [`TypeSymbol::Named`] that resolves because
/// `Binder::enter_type_parameters` put `T` in scope for the length of the declaration. Substituting
/// by name is therefore the SAME rule from the other side, and it lands on C#'s own scoping rule --
/// a type parameter hides any type of that name (ECMA-334 1st ed 10.8) -- so a member written
/// against an outer type called `T` is one the language already says means the parameter.
///
/// **A name NOT in `bindings` is left alone**, which is what keeps an ordinary type whose name
/// happens to be short from being rewritten.
fn substitute(ty: &TypeSymbol, bindings: &BTreeMap<&str, &TypeSymbol>) -> TypeSymbol {
    match ty {
        TypeSymbol::Named(parts) => match parts.split_first() {
            Some((name, [])) => match bindings.get(&**name) {
                Some(&argument) => argument.clone(),
                None => ty.clone(),
            },
            _ => ty.clone(),
        },
        TypeSymbol::Instantiation {
            definition,
            arguments,
        } => TypeSymbol::Instantiation {
            definition: definition.clone(),
            arguments: arguments
                .iter()
                .map(|argument| substitute(argument, bindings))
                .collect(),
        },
        TypeSymbol::Array { element, rank } => substitute(element, bindings).into_array(*rank),
        TypeSymbol::Pointer(element) => {
            TypeSymbol::Pointer(Box::new(substitute(element, bindings)))
        }
        TypeSymbol::ByRef(element) => TypeSymbol::ByRef(Box::new(substitute(element, bindings))),
        TypeSymbol::Special(_) | TypeSymbol::Error => ty.clone(),
    }
}

impl TypeInfo {
    /// A type with no members yet, ready for fields and methods to be added.
    #[must_use]
    pub fn new(namespace: &str, name: &str, kind: TypeKind) -> TypeInfo {
        TypeInfo {
            namespace: namespace.into(),
            name: name.into(),
            kind,
            base: None,
            bases: Vec::new(),
            fields: Vec::new(),
            properties: Vec::new(),
            methods: Vec::new(),
            undecodable_members: Vec::new(),
            events: Vec::new(),
            constructors: Vec::new(),
            enclosing: None,
            is_external: false,
            assembly: None,
            accessibility: Accessibility::Public,
            is_sealed: false,
            is_abstract: false,
            type_parameters: Vec::new(),
            type_parameter_constraints: Vec::new(),
        }
    }

    /// The constraints on the type parameter at `index`, or `None` if there is no such parameter.
    ///
    /// **Reading through this rather than indexing `type_parameter_constraints` is what keeps a
    /// length drift honest.** The two vectors are built together and are meant to stay the same
    /// length; a build that added a parameter without its entry would, on a raw index, either panic
    /// or silently read the NEXT parameter's constraints and enforce the wrong rule. Here it
    /// answers `None`, which every caller already has to handle for a non-generic type.
    #[must_use]
    pub fn constraints_on(&self, index: usize) -> Option<&TypeParameterConstraints> {
        if index >= self.type_parameters.len() {
            return None;
        }
        self.type_parameter_constraints.get(index)
    }

    /// This generic DEFINITION closed over `arguments`: the same members, with every mention of a
    /// type parameter replaced by the corresponding argument, so `Box<int>.Get()` returns `int`
    /// where `Box<T>.Get()` returns `T`.
    ///
    /// `None` when the argument count does not match the declared parameter count. **That is a
    /// refusal, never a partial substitution**, and it is the same rule
    /// `lamella_aot::generics::TypeArg::substitute` follows for a parameter number it cannot
    /// close: a member left half-substituted is a signature that reads as ordinary and means
    /// something else. `resolve_type` reports CS0305 before it gets here, so this is the structural
    /// guard behind that diagnostic rather than a second one.
    ///
    /// **THE TYPE'S OWN `name` IS THE DEFINITION'S, DELIBERATELY.** Naming an instantiation is
    /// the canonical spelling's job, `lamella_aot::generics::TypeArg::spell` is the only
    /// implementation of it, and it is a frozen wire value -- a second spelling that agreed
    /// today and diverged on the first nested argument would give a baker-lowered and a
    /// device-instantiated `List<int>` different names, which is the cast hole the generics
    /// identity rule exists to close. This carries the members and leaves the name alone.
    #[must_use]
    pub fn instantiate(&self, arguments: &[TypeSymbol]) -> Option<TypeInfo> {
        if arguments.len() != self.type_parameters.len() {
            return None;
        }
        let bindings: BTreeMap<&str, &TypeSymbol> = self
            .type_parameters
            .iter()
            .map(|parameter| &**parameter)
            .zip(arguments)
            .collect();
        let mut closed = self.clone();
        closed.type_parameters = Vec::new();
        for field in &mut closed.fields {
            field.ty = substitute(&field.ty, &bindings);
        }
        for property in &mut closed.properties {
            property.ty = substitute(&property.ty, &bindings);
        }
        for event in &mut closed.events {
            event.ty = substitute(&event.ty, &bindings);
        }
        for method in closed
            .methods
            .iter_mut()
            .chain(closed.constructors.iter_mut())
        {
            method.return_type = substitute(&method.return_type, &bindings);
            for parameter in &mut method.parameters {
                *parameter = substitute(parameter, &bindings);
            }
        }
        for base in &mut closed.bases {
            *base = substitute(base, &bindings);
        }
        closed.base = closed.base.as_ref().map(|base| substitute(base, &bindings));
        Some(closed)
    }

    /// The field with the given name declared directly on this type (no
    /// inheritance walk yet).
    #[must_use]
    pub fn find_field(&self, name: &str) -> Option<&FieldSymbol> {
        self.fields.iter().find(|field| &*field.name == name)
    }

    /// The field-like event with the given name declared directly on this type.
    #[must_use]
    pub fn find_event(&self, name: &str) -> Option<&EventSymbol> {
        self.events.iter().find(|event| &*event.name == name)
    }

    /// The property with the given name declared directly on this type.
    #[must_use]
    pub fn find_property(&self, name: &str) -> Option<&PropertySymbol> {
        self.properties
            .iter()
            .find(|property| &*property.name == name)
    }

    /// The methods with the given name -- the method group overload resolution
    /// chooses from (no inheritance walk yet).
    pub fn methods_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a MethodSymbol> {
        self.methods
            .iter()
            .filter(move |method| &*method.name == name)
    }
}

/// Every type in scope, keyed by namespace and name. The binder's reference world
/// for member lookup.
#[derive(Debug, Default, Clone)]
pub struct Model {
    types: BTreeMap<(String, String), TypeInfo>,
}

impl Model {
    /// An empty model.
    #[must_use]
    pub fn new() -> Model {
        Model::default()
    }

    /// Adds a type, replacing any earlier one with the same namespace and name.
    pub fn insert(&mut self, info: TypeInfo) {
        let key = (String::from(&*info.namespace), String::from(&*info.name));
        self.types.insert(key, info);
    }

    /// The type with the given namespace and name, if present.
    #[must_use]
    pub fn get(&self, namespace: &str, name: &str) -> Option<&TypeInfo> {
        self.types
            .get(&(String::from(namespace), String::from(name)))
    }

    /// The type with the given namespace and name, mutably, if present. Used by the constant
    /// resolution pass to fill a field's folded value once the whole model is collected.
    pub fn get_mut(&mut self, namespace: &str, name: &str) -> Option<&mut TypeInfo> {
        self.types
            .get_mut(&(String::from(namespace), String::from(name)))
    }

    /// The number of program entry points declared in THIS compilation (10.1): a `static Main`
    /// returning `void` or `int` and taking no parameters or a single `string[]`. Types loaded
    /// from a reference assembly are excluded. Two or more is a CS0017 (multiple entry points).
    #[must_use]
    pub fn entry_point_count(&self) -> usize {
        self.types
            .values()
            .filter(|info| !info.is_external)
            .flat_map(|info| info.methods.iter())
            .filter(|method| {
                &*method.name == "Main" && method.is_static && is_entry_point_method(method)
            })
            .count()
    }

    /// The type a [`TypeSymbol`] refers to, if present. A predefined type resolves
    /// to its `System.<Name>` reference type; array and error types have none.
    ///
    /// **A [`Cow`] BECAUSE ONE ARM CANNOT BORROW.** Every other type is a row in this model and
    /// is handed back by reference; an INSTANTIATION is not a row and its members are computed, so
    /// it is the one answer this model has to own. Returning `&TypeInfo` was what forced the
    /// instantiation arm to answer `None`, and `None` there is not "no members" -- it is a type
    /// whose every member lookup silently fails.
    #[must_use]
    pub fn get_by_symbol(&self, ty: &TypeSymbol) -> Option<Cow<'_, TypeInfo>> {
        match ty {
            TypeSymbol::Named(parts) => {
                let (namespace, name) = split_named(parts);
                self.get(&namespace, name).map(Cow::Borrowed)
            }
            TypeSymbol::Special(SpecialType::Null) => None,
            TypeSymbol::Special(special) => {
                let (namespace, name) = special.full_name();
                self.get(namespace, name).map(Cow::Borrowed)
            }
            TypeSymbol::Instantiation {
                definition,
                arguments,
            } => {
                let (namespace, _) = split_named(definition);
                let definition = self.get(
                    &namespace,
                    &definition_metadata_name(definition, arguments.len()),
                )?;
                Some(Cow::Owned(definition.instantiate(arguments)?))
            }
            TypeSymbol::Array { .. }
            | TypeSymbol::Pointer(_)
            | TypeSymbol::ByRef(_)
            | TypeSymbol::Error => None,
        }
    }

    /// The OPEN declaration behind a member reached through an INSTANTIATED generic type, with the
    /// definition's type-parameter NAMES beside it: `(["T"], Box<T>.Get() -> T)` for a `Get()`
    /// resolved on `Box<int>`.
    ///
    /// **THE OPEN SIGNATURE CANNOT BE RECOVERED FROM THE CLOSED ONE**, which is the same reason
    /// `MethodInstantiation` exists one axis over: after substitution `Box<int>.Get()` reads
    /// `int Get()`, indistinguishable from an ordinary `int Get()` on a non-generic type.
    /// ECMA-335 4th ed II.23.2.1 wants the DEFINITION's signature on the `MemberRef`, so emission
    /// has to be handed what substitution consumed.
    ///
    /// **THE MEMBER IS IDENTIFIED BY SUBSTITUTING, NOT BY POSITION.** `TypeInfo::instantiate`
    /// clones member-for-member, so an index would work today and would break silently the first
    /// time anything filters or reorders that list -- and a wrong-but-plausible open signature is
    /// exactly the failure this whole area cannot see. Substituting each candidate and comparing
    /// against the parameters overload resolution actually chose asks the question in the same
    /// terms the answer was produced in.
    ///
    /// `None` for a non-instantiated type, for a definition not in the model, and for a member
    /// that does not match -- emission then refuses rather than writing a `!n` it guessed.
    #[must_use]
    pub fn open_member(
        &self,
        declaring: &TypeSymbol,
        name: &str,
        parameters: &[TypeSymbol],
    ) -> Option<(Vec<Box<str>>, MethodSymbol)> {
        let TypeSymbol::Instantiation {
            definition,
            arguments,
        } = declaring
        else {
            return None;
        };
        let (namespace, _) = split_named(definition);
        let open = self.get(
            &namespace,
            &definition_metadata_name(definition, arguments.len()),
        )?;
        if open.type_parameters.len() != arguments.len() {
            return None;
        }
        let bindings: BTreeMap<&str, &TypeSymbol> = open
            .type_parameters
            .iter()
            .map(|parameter| &**parameter)
            .zip(arguments.iter())
            .collect();
        let candidates = if name == ".ctor" {
            &open.constructors
        } else {
            &open.methods
        };
        let member = candidates.iter().find(|candidate| {
            &*candidate.name == name
                && candidate.parameters.len() == parameters.len()
                && candidate
                    .parameters
                    .iter()
                    .zip(parameters)
                    .all(|(open_ty, closed)| substitute(open_ty, &bindings) == *closed)
        })?;
        Some((open.type_parameters.clone(), member.clone()))
    }

    /// [`Model::open_member`] for a PROPERTY ACCESSOR, which is not in `methods` to be found.
    ///
    /// **A SOURCE-DECLARED PROPERTY CONTRIBUTES A `PropertySymbol` AND NO ACCESSOR METHODS**, so
    /// `open_member` searching `methods` for `get_Item` finds nothing and answers `None` -- which
    /// the caller reads as "not reached through an instantiation" and which silently drops back to
    /// naming the definition's open accessor. The accessor signature is DERIVED here instead: a
    /// getter takes nothing and returns the property's open type, a setter takes that type and
    /// returns void.
    ///
    /// `want_setter` picks which. Returns the definition's type-parameter names beside the open
    /// signature, exactly as [`Model::open_member`] does, so both feed one `TypeInstantiation`.
    #[must_use]
    pub fn open_property_accessor(
        &self,
        declaring: &TypeSymbol,
        property: &str,
        want_setter: bool,
    ) -> Option<(Vec<Box<str>>, Vec<TypeSymbol>, TypeSymbol)> {
        let TypeSymbol::Instantiation {
            definition,
            arguments,
        } = declaring
        else {
            return None;
        };
        let (namespace, _) = split_named(definition);
        let open = self.get(
            &namespace,
            &definition_metadata_name(definition, arguments.len()),
        )?;
        if open.type_parameters.len() != arguments.len() {
            return None;
        }
        let declared = open.find_property(property)?;
        let open_ty = declared.ty.clone();
        Some(if want_setter {
            (
                open.type_parameters.clone(),
                alloc::vec![open_ty],
                TypeSymbol::Special(SpecialType::Void),
            )
        } else {
            (open.type_parameters.clone(), Vec::new(), open_ty)
        })
    }

    /// [`Model::open_member`] for a FIELD: the definition's type-parameter names and the field's
    /// type BEFORE substitution, for a field reached through an instantiated generic type.
    ///
    /// A field has no overloads, so it is found by NAME alone -- there is nothing to disambiguate
    /// and no substitute-and-compare step. `None` for a non-instantiated type, and for a
    /// definition or field not in the model.
    #[must_use]
    pub fn open_field(
        &self,
        declaring: &TypeSymbol,
        name: &str,
    ) -> Option<(Vec<Box<str>>, TypeSymbol)> {
        let TypeSymbol::Instantiation {
            definition,
            arguments,
        } = declaring
        else {
            return None;
        };
        let (namespace, _) = split_named(definition);
        let open = self.get(
            &namespace,
            &definition_metadata_name(definition, arguments.len()),
        )?;
        if open.type_parameters.len() != arguments.len() {
            return None;
        }
        let field = open.find_field(name)?;
        Some((open.type_parameters.clone(), field.ty.clone()))
    }

    /// Resolves each type's base *class* -- the first of its declared bases that is
    /// a class -- so member lookup can walk the inheritance chain. Run once after
    /// every type is inserted.
    pub fn link_bases(&mut self) {
        let links: Vec<((String, String), TypeSymbol)> = self
            .types
            .iter()
            .filter_map(|(key, info)| {
                info.bases
                    .iter()
                    .find_map(|base| self.resolve_class_base(base))
                    .map(|base| (key.clone(), base))
            })
            .collect();
        for (key, base) in links {
            if let Some(info) = self.types.get_mut(&key) {
                info.base = Some(base);
            }
        }
    }

    /// Promotes every non-public, non-nested, this-assembly type to `internal` -- the default
    /// accessibility of a top-level type (10.2.3). Collection derives accessibility from modifiers,
    /// which defaults to `private` (right for a nested member, wrong for a top-level type), so this
    /// corrects the default once every type's `enclosing` is known. Reference (external) types keep
    /// their metadata accessibility, and an explicitly `public` type is left alone.
    pub fn default_toplevel_types_to_internal(&mut self) {
        for info in self.types.values_mut() {
            if !info.is_external
                && info.enclosing.is_none()
                && info.accessibility != Accessibility::Public
            {
                info.accessibility = Accessibility::Internal;
            }
        }
    }

    /// Resolves a written base to the symbol of a model type of `kind`: by exact match,
    /// else (for an unqualified base such as a `using`-imported `Exception` or
    /// `IEnumerator`) by a unique simple-name match across namespaces. `None` if no such
    /// type exists, or the simple name is ambiguous -- base names are not yet resolved
    /// through `using` directives, so this stands in for that for a BCL base.
    fn resolve_base_of_kind(&self, base: &TypeSymbol, kind: TypeKind) -> Option<TypeSymbol> {
        if self.get_by_symbol(base).is_some_and(|info| info.kind == kind) {
            return Some(base.clone());
        }
        let TypeSymbol::Named(parts) = base else {
            return None;
        };
        if parts.len() != 1 {
            return None;
        }
        let simple = &*parts[0];
        let mut found: Option<TypeSymbol> = None;
        for ((namespace, name), info) in &self.types {
            if &**name == simple && info.kind == kind {
                if found.is_some() {
                    return None;
                }
                found = Some(symbol_from_key(namespace, name));
            }
        }
        found
    }

    /// Resolves a written base to a class in the model -- the inheritance-chain base.
    pub fn resolve_class_base(&self, base: &TypeSymbol) -> Option<TypeSymbol> {
        self.resolve_base_of_kind(base, TypeKind::Class)
    }

    /// Resolves a written base to a STRUCT in the model. A struct is never a legal base -- it is
    /// implicitly sealed -- so this exists to NAME one in a diagnostic, which the class-only
    /// lookup cannot do.
    pub fn resolve_struct_base(&self, base: &TypeSymbol) -> Option<TypeSymbol> {
        self.resolve_base_of_kind(base, TypeKind::Struct)
    }

    /// Resolves a written base to an interface in the model -- the `InterfaceImpl` source
    /// for a class that implements an interface, named qualified or (via `using`) not.
    pub fn resolve_interface_base(&self, base: &TypeSymbol) -> Option<TypeSymbol> {
        self.resolve_base_of_kind(base, TypeKind::Interface)
    }

    /// Whether `namespace` is a declared namespace -- some type lives in it or in a
    /// namespace nested under it.
    #[must_use]
    pub fn is_namespace(&self, namespace: &str) -> bool {
        self.types.keys().any(|(type_namespace, _)| {
            type_namespace == namespace
                || type_namespace
                    .strip_prefix(namespace)
                    .is_some_and(|rest| rest.starts_with('.'))
        })
    }

    /// The existence-only [`TypeTable`] for plain type-name resolution.
    #[must_use]
    pub fn type_table(&self) -> TypeTable {
        let mut table = TypeTable::new();
        for ((namespace, name), info) in &self.types {
            table.insert_generic(namespace, name, info.type_parameters.clone());
        }
        table
    }

    /// Every declared type's simple name (with duplicates across namespaces), for
    /// type-name completion. The caller filters/dedups.
    pub fn type_names(&self) -> impl Iterator<Item = &str> + '_ {
        self.types.keys().map(|(_, name)| name.as_ref())
    }

    /// Every declared type's `(namespace, simple name)`, for namespace-aware completion
    /// (`System.` -> the types and child namespaces under `System`). The caller filters
    /// and dedups.
    pub fn type_keys(&self) -> impl Iterator<Item = (&str, &str)> + '_ {
        self.types
            .keys()
            .map(|(namespace, name)| (namespace.as_ref(), name.as_ref()))
    }

    /// Marks the type `(namespace, name)` as nested in `enclosing` (its full name).
    pub fn set_enclosing(&mut self, namespace: &str, name: &str, enclosing: &str) {
        if let Some(info) = self
            .types
            .get_mut(&(String::from(namespace), String::from(name)))
        {
            info.enclosing = Some(enclosing.into());
        }
    }

    /// The symbol of the model type with the given simple name, when exactly one matches
    /// (a stand-in for `using`-directive resolution -- used by completion to resolve a
    /// bare type name like `Console`). `None` if absent or ambiguous.
    #[must_use]
    pub fn type_with_simple_name(&self, name: &str) -> Option<TypeSymbol> {
        let mut found: Option<TypeSymbol> = None;
        for (namespace, type_name) in self.types.keys() {
            if &**type_name == name {
                if found.is_some() {
                    return None;
                }
                found = Some(symbol_from_key(namespace, type_name));
            }
        }
        found
    }

    /// Rewrites every member signature's single-part named types to their canonical
    /// fully-qualified form, so a type written `WeakReference` (resolved through a `using`)
    /// is the SAME [`TypeSymbol`] as `System.WeakReference` -- one type, as conversions,
    /// overload resolution, and identity all require. Source signatures arrive unqualified
    /// (collected structurally by `bind_type`); reference signatures are already canonical, so
    /// this is a no-op for them. Only UNAMBIGUOUS simple names are rewritten: a name declared in
    /// two namespaces is left untouched for the using-aware use-site resolver. Run on the
    /// complete model (references + source) before `link_bases`, so the base chain links canonical.
    pub fn canonicalize_signatures(&mut self) {
        let canon = self.signature_canon();
        for info in self.types.values_mut() {
            for base in &mut info.bases {
                *base = canon.canonicalize(base);
            }
            for field in &mut info.fields {
                field.ty = canon.canonicalize(&field.ty);
            }
            for property in &mut info.properties {
                property.ty = canon.canonicalize(&property.ty);
            }
            for event in &mut info.events {
                event.ty = canon.canonicalize(&event.ty);
            }
            for method in &mut info.methods {
                method.return_type = canon.canonicalize(&method.return_type);
                for parameter in &mut method.parameters {
                    *parameter = canon.canonicalize(parameter);
                }
            }
            for constructor in &mut info.constructors {
                for parameter in &mut constructor.parameters {
                    *parameter = canon.canonicalize(parameter);
                }
            }
        }
    }

    /// The unambiguous simple-name -> full-symbol canon over every known type (references plus
    /// source), reusable for canonicalizing single-part signature names without rebuilding the
    /// map. See [`SignatureCanon`]; this is the map [`canonicalize_signatures`] applies.
    #[must_use]
    pub fn signature_canon(&self) -> SignatureCanon {
        let mut map: BTreeMap<String, Option<TypeSymbol>> = BTreeMap::new();
        for (namespace, type_name) in self.types.keys() {
            match map.get_mut(type_name.as_str()) {
                Some(slot) => *slot = None,
                None => {
                    map.insert(type_name.clone(), Some(symbol_from_key(namespace, type_name)));
                }
            }
        }
        SignatureCanon { map }
    }
}

/// The unambiguous simple-name -> full-symbol map of [`Model::signature_canon`], so a single-part
/// named type can be canonicalized to its qualified form repeatedly without rebuilding the map.
/// An ambiguous name (declared in two namespaces) maps to `None` and is left for the use-site
/// resolver. The emit side ([`crate`] consumers) keeps one to canonicalize the syntax-derived
/// signature names it keys tokens and serializes signatures by, so they match the binder's
/// qualified symbols.
#[derive(Debug, Clone, Default)]
pub struct SignatureCanon {
    map: BTreeMap<String, Option<TypeSymbol>>,
}

impl SignatureCanon {
    /// Rewrites a single-part named type to its unambiguous qualified symbol, folds a
    /// `System` built-in to its special form -- so `System.String` written out and the
    /// `string` keyword are ONE type (4.1.4), whether the built-in comes from a reference
    /// or a corlib-style compilation's own source -- and recurses into arrays, pointers,
    /// and byrefs. Every other type (ambiguous, unknown) is returned unchanged.
    #[must_use]
    pub fn canonicalize(&self, ty: &TypeSymbol) -> TypeSymbol {
        match ty {
            TypeSymbol::Named(parts) => {
                let qualified = if parts.len() == 1 {
                    match self.map.get(parts[0].as_ref()) {
                        Some(Some(full)) => full.clone(),
                        _ => ty.clone(),
                    }
                } else {
                    ty.clone()
                };
                if let TypeSymbol::Named(qualified_parts) = &qualified {
                    if let [namespace, name] = &qualified_parts[..] {
                        if let Some(special) =
                            crate::reference::special_for_named(namespace, name)
                        {
                            return TypeSymbol::Special(special);
                        }
                    }
                }
                qualified
            }
            TypeSymbol::Array { element, rank } => TypeSymbol::Array {
                element: alloc::boxed::Box::new(self.canonicalize(element)),
                rank: *rank,
            },
            TypeSymbol::Pointer(inner) => {
                TypeSymbol::Pointer(alloc::boxed::Box::new(self.canonicalize(inner)))
            }
            TypeSymbol::ByRef(inner) => {
                TypeSymbol::ByRef(alloc::boxed::Box::new(self.canonicalize(inner)))
            }
            TypeSymbol::Instantiation {
                definition,
                arguments,
            } => {
                let canonical_arguments: Vec<TypeSymbol> = arguments
                    .iter()
                    .map(|argument| self.canonicalize(argument))
                    .collect();
                TypeSymbol::Instantiation {
                    definition: canonical_definition(definition, arguments.len(), |mangled| {
                        self.map.get(mangled).cloned().flatten()
                    }),
                    arguments: canonical_arguments.into(),
                }
            }
            other => other.clone(),
        }
    }
}

/// An instantiation's DEFINITION rewritten to the qualified form `lookup` gives for its
/// ARITY-MANGLED name (II.10.7.2), with the source spelling put back in the last part --
/// `["Box"]` at arity 1 becomes `["App", "Box"]` when the model holds `App.Box`1`.
///
/// **THIS RULE HAS THREE CALLERS AND HAD BEEN WRITTEN IN NONE OF THEM.** A simple name is
/// qualified in three places -- the use-site resolver, [`SignatureCanon::canonicalize`], and
/// `Binder::canonicalize` -- each with its own lookup, and every one handled `Named`, arrays and
/// pointers while falling through on an instantiation. The result was one spelling with three
/// answers: `Box<int>` resolved in a local declaration, was silently left unqualified in a
/// parameter, and had no members either way. The lookups genuinely differ (a prebuilt map, a model
/// query); the RULE does not, so it lives here once.
///
/// The mangling is applied for the LOOKUP and undone for the RESULT, deliberately: every consumer
/// of a definition re-mangles from the argument count (`Model::get_by_symbol`, the emitter's token
/// keys), so returning the mangled form would mangle it twice and resolve to nothing.
pub(crate) fn canonical_definition(
    definition: &[Box<str>],
    arity: usize,
    lookup: impl Fn(&str) -> Option<TypeSymbol>,
) -> Box<[Box<str>]> {
    let [only] = definition else {
        return definition.into();
    };
    let Some(TypeSymbol::Named(qualified)) = lookup(&definition_metadata_name(definition, arity))
    else {
        return definition.into();
    };
    let mut parts: Vec<Box<str>> = qualified.to_vec();
    if let Some(last) = parts.last_mut() {
        *last = only.clone();
    }
    parts.into()
}

/// Splits a type's dotted name parts into its namespace and simple name.
/// Whether `method` has a valid program-entry-point signature (10.1): it returns `void` or `int`
/// and takes no parameters or a single one-dimensional `string[]`. The caller checks the name is
/// `Main` and it is `static`.
fn is_entry_point_method(method: &MethodSymbol) -> bool {
    let return_ok = method.return_type.is_void()
        || matches!(method.return_type, TypeSymbol::Special(SpecialType::Int32));
    let parameters_ok = match method.parameters.as_slice() {
        [] => true,
        [TypeSymbol::Array { element, rank: 1 }] => {
            matches!(**element, TypeSymbol::Special(SpecialType::String))
        }
        _ => false,
    };
    return_ok && parameters_ok
}

fn split_named(parts: &[Box<str>]) -> (String, &str) {
    match parts.split_last() {
        Some((name, namespace_parts)) => {
            let mut namespace = String::new();
            for part in namespace_parts {
                if !namespace.is_empty() {
                    namespace.push('.');
                }
                namespace.push_str(part);
            }
            (namespace, name)
        }
        None => (String::new(), ""),
    }
}

/// Builds a named-type symbol from a model key (a dotted `namespace` and a simple `name`).
fn symbol_from_key(namespace: &str, name: &str) -> TypeSymbol {
    let mut parts: Vec<Box<str>> = Vec::new();
    if !namespace.is_empty() {
        for part in namespace.split('.') {
            parts.push(part.into());
        }
    }
    parts.push(name.into());
    TypeSymbol::Named(parts.into_boxed_slice())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::special::SpecialType;

    #[test]
    fn a_definition_is_mangled_with_the_arity_it_declares_not_its_instantiation_s() {
        let parts = |dotted: &str| -> Vec<Box<str>> { dotted.split('.').map(Box::from).collect() };
        let table = [
            ("System.Collections.Generic.List", 1, "List`1"),
            ("System.Collections.Generic.Dictionary", 2, "Dictionary`2"),
            ("System.Collections.Generic.List`1.Enumerator", 1, "Enumerator"),
            ("N.Outer`1.Inner", 2, "Inner`1"),
            ("N.A`1.B`2.C", 4, "C`1"),
            ("N.Plain.Inner", 1, "Inner`1"),
            ("N.Outer`2.Inner", 1, "Inner"),
        ];
        for (dotted, arguments, expected) in table {
            assert_eq!(
                definition_metadata_name(&parts(dotted), arguments),
                expected,
                "{dotted} instantiated with {arguments} argument(s)"
            );
        }
        assert_eq!(mangled_arity("Box`Extra"), 0);
        assert_eq!(mangled_arity("Box"), 0);
        assert_eq!(mangled_arity("Box`3"), 3);
        assert_eq!(
            definition_metadata_name(&parts("N.Box`Extra.Inner"), 1),
            "Inner`1"
        );
    }

    fn widget() -> TypeInfo {
        let mut info = TypeInfo::new("Shapes", "Widget", TypeKind::Class);
        info.fields.push(FieldSymbol {
            name: "count".into(),
            ty: TypeSymbol::Special(SpecialType::Int32),
            is_static: false,
            is_readonly: false,
            is_volatile: false,
            accessibility: Accessibility::Public,
            constant: None,
            is_required: false,
        });
        info.methods.push(MethodSymbol {
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
            accessibility: Accessibility::Public,
            conditional: Vec::new(),
            sets_required_members: false,
            type_parameters: Vec::new(),
            type_parameter_constraints: Vec::new(),
        });
        info.methods.push(MethodSymbol {
            name: "Scale".into(),
            return_type: TypeSymbol::Special(SpecialType::Void),
            parameters: alloc::vec![TypeSymbol::Special(SpecialType::Int32)],
            parameter_info: Vec::new(),
            is_static: false,
            is_params: false,
            is_vararg: false,
            is_virtual: false,
            is_abstract: false,
            is_override: false,
            is_sealed: false,
            accessibility: Accessibility::Public,
            conditional: Vec::new(),
            sets_required_members: false,
            type_parameters: Vec::new(),
            type_parameter_constraints: Vec::new(),
        });
        info.methods.push(MethodSymbol {
            name: "Scale".into(),
            return_type: TypeSymbol::Special(SpecialType::Void),
            parameters: alloc::vec![TypeSymbol::Special(SpecialType::Double)],
            parameter_info: Vec::new(),
            is_static: false,
            is_params: false,
            is_vararg: false,
            is_virtual: false,
            is_abstract: false,
            is_override: false,
            is_sealed: false,
            accessibility: Accessibility::Public,
            conditional: Vec::new(),
            sets_required_members: false,
            type_parameters: Vec::new(),
            type_parameter_constraints: Vec::new(),
        });
        info
    }

    #[test]
    fn finds_fields_and_method_groups() {
        let widget = widget();
        assert_eq!(
            widget.find_field("count").map(|f| &f.ty),
            Some(&TypeSymbol::Special(SpecialType::Int32))
        );
        assert!(widget.find_field("missing").is_none());
        assert_eq!(widget.methods_named("Scale").count(), 2);
        assert_eq!(widget.methods_named("Area").count(), 1);
        assert_eq!(widget.methods_named("Nope").count(), 0);
    }

    #[test]
    fn model_lookup_and_derived_table() {
        let mut model = Model::new();
        model.insert(widget());
        model.insert(TypeInfo::new("", "Program", TypeKind::Class));
        assert_eq!(
            model.get("Shapes", "Widget").map(|t| t.kind),
            Some(TypeKind::Class)
        );
        assert!(model.get("Shapes", "Gadget").is_none());
        let table = model.type_table();
        assert!(table.contains("Shapes", "Widget"));
        assert!(table.contains("", "Program"));
        assert!(!table.contains("", "Widget"));
    }
}
