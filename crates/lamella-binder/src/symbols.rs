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
}

/// A method of a type (17.5), reduced to what overload resolution needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodSymbol {
    /// The method's name.
    pub name: Box<str>,
    /// The return type (`void` is `SpecialType::Void`).
    pub return_type: TypeSymbol,
    /// The parameter types, in order.
    pub parameters: Vec<TypeSymbol>,
    /// Whether the method is `static`.
    pub is_static: bool,
    /// Whether the last parameter is a `params` array (a variable-length trailing
    /// argument list at the call site).
    pub is_params: bool,
    /// The method's accessibility.
    pub accessibility: Accessibility,
    /// The `[Conditional("SYMBOL")]` symbols (24.4.2): a call to this method is omitted unless
    /// one of these is defined at the call site. Empty for an unconditional method.
    pub conditional: Vec<Box<str>>,
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
    fn resolve_class_base(&self, base: &TypeSymbol) -> Option<TypeSymbol> {
        self.resolve_base_of_kind(base, TypeKind::Class)
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
        let mut canon: BTreeMap<String, Option<TypeSymbol>> = BTreeMap::new();
        for (namespace, type_name) in self.types.keys() {
            match canon.get_mut(type_name.as_str()) {
                Some(slot) => *slot = None,
                None => {
                    canon.insert(type_name.clone(), Some(symbol_from_key(namespace, type_name)));
                }
            }
        }
        for info in self.types.values_mut() {
            for base in &mut info.bases {
                canonicalize_type(base, &canon);
            }
            for field in &mut info.fields {
                canonicalize_type(&mut field.ty, &canon);
            }
            for property in &mut info.properties {
                canonicalize_type(&mut property.ty, &canon);
            }
            for event in &mut info.events {
                canonicalize_type(&mut event.ty, &canon);
            }
            for method in &mut info.methods {
                canonicalize_type(&mut method.return_type, &canon);
                for parameter in &mut method.parameters {
                    canonicalize_type(parameter, &canon);
                }
            }
        }
    }
}

/// Canonicalizes a single signature type in place (see [`Model::canonicalize_signatures`]):
/// an unambiguous single-part named type becomes its full symbol; arrays and pointers recurse
/// into their element type; everything else is left as is.
fn canonicalize_type(ty: &mut TypeSymbol, canon: &BTreeMap<String, Option<TypeSymbol>>) {
    match ty {
        TypeSymbol::Named(parts) if parts.len() == 1 => {
            if let Some(Some(full)) = canon.get(parts[0].as_ref()) {
                *ty = full.clone();
            }
        }
        TypeSymbol::Array { element, .. } => canonicalize_type(element, canon),
        TypeSymbol::Pointer(inner) => canonicalize_type(inner, canon),
        _ => {}
    }
}

/// Splits a type's dotted name parts into its namespace and simple name.
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
            accessibility: Accessibility::Public,
            constant: None,
        });
        info.methods.push(MethodSymbol {
            name: "Area".into(),
            return_type: TypeSymbol::Special(SpecialType::Double),
            parameters: Vec::new(),
            is_static: false,
            is_params: false,
            accessibility: Accessibility::Public,
            conditional: Vec::new(),
        });
        info.methods.push(MethodSymbol {
            name: "Scale".into(),
            return_type: TypeSymbol::Special(SpecialType::Void),
            parameters: alloc::vec![TypeSymbol::Special(SpecialType::Int32)],
            is_static: false,
            is_params: false,
            accessibility: Accessibility::Public,
            conditional: Vec::new(),
        });
        info.methods.push(MethodSymbol {
            name: "Scale".into(),
            return_type: TypeSymbol::Special(SpecialType::Void),
            parameters: alloc::vec![TypeSymbol::Special(SpecialType::Double)],
            is_static: false,
            is_params: false,
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
