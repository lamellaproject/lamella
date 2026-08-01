//! The type and member symbol model (ECMA-334 1st ed, clauses 17-18).

use crate::resolve::TypeTable;
use crate::special::SpecialType;
use crate::types::TypeSymbol;
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
}

impl MethodSymbol {
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
            events: Vec::new(),
            constructors: Vec::new(),
            enclosing: None,
            is_external: false,
            assembly: None,
            accessibility: Accessibility::Public,
            is_sealed: false,
            is_abstract: false,
        }
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
    #[must_use]
    pub fn get_by_symbol(&self, ty: &TypeSymbol) -> Option<&TypeInfo> {
        match ty {
            TypeSymbol::Named(parts) => {
                let (namespace, name) = split_named(parts);
                self.get(&namespace, name)
            }
            TypeSymbol::Special(SpecialType::Null) => None,
            TypeSymbol::Special(special) => {
                let (namespace, name) = special.full_name();
                self.get(namespace, name)
            }
            TypeSymbol::Array { .. }
            | TypeSymbol::Pointer(_)
            | TypeSymbol::ByRef(_)
            | TypeSymbol::Error => None,
        }
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
        for (namespace, name) in self.types.keys() {
            table.insert(namespace, name);
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
            other => other.clone(),
        }
    }
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
