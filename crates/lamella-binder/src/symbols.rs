//! The type and member symbol model (ECMA-334 1st ed, clauses 17-18).

use crate::resolve::TypeTable;
use crate::types::TypeSymbol;
use alloc::boxed::Box;
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

/// A field of a type (17.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldSymbol {
    /// The field's name.
    pub name: Box<str>,
    /// The field's type.
    pub ty: TypeSymbol,
    /// Whether the field is `static`.
    pub is_static: bool,
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
    /// The direct base type, if any (a class's base; `None` for `object`).
    pub base: Option<TypeSymbol>,
    /// The type's fields.
    pub fields: Vec<FieldSymbol>,
    /// The type's methods.
    pub methods: Vec<MethodSymbol>,
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
            fields: Vec::new(),
            methods: Vec::new(),
        }
    }

    /// The field with the given name declared directly on this type (no
    /// inheritance walk yet).
    #[must_use]
    pub fn find_field(&self, name: &str) -> Option<&FieldSymbol> {
        self.fields.iter().find(|field| &*field.name == name)
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

    /// The existence-only [`TypeTable`] for plain type-name resolution.
    #[must_use]
    pub fn type_table(&self) -> TypeTable {
        let mut table = TypeTable::new();
        for (namespace, name) in self.types.keys() {
            table.insert(namespace, name);
        }
        table
    }
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
        });
        info.methods.push(MethodSymbol {
            name: "Area".into(),
            return_type: TypeSymbol::Special(SpecialType::Double),
            parameters: Vec::new(),
            is_static: false,
        });
        info.methods.push(MethodSymbol {
            name: "Scale".into(),
            return_type: TypeSymbol::Special(SpecialType::Void),
            parameters: alloc::vec![TypeSymbol::Special(SpecialType::Int32)],
            is_static: false,
        });
        info.methods.push(MethodSymbol {
            name: "Scale".into(),
            return_type: TypeSymbol::Special(SpecialType::Void),
            parameters: alloc::vec![TypeSymbol::Special(SpecialType::Double)],
            is_static: false,
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
