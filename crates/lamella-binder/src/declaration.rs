//! Collecting the types and members declared in source (ECMA-334 1st ed,
//! clauses 16-18).

use crate::bind::bind_type;
use crate::resolve::TypeTable;
use crate::special::SpecialType;
use crate::symbols::{
    Accessibility, FieldSymbol, MethodSymbol, Model, PropertySymbol, TypeInfo, TypeKind,
};
use crate::types::TypeSymbol;
use alloc::string::String;
use lamella_syntax::ast::{
    CompilationUnit, Expr, ExprKind, Literal, Member, Modifier, NamespaceMember, QualifiedName,
    TypeDecl, TypeKind as SyntaxTypeKind, UnaryOperator,
};

/// Builds the [`Model`] of every type and member declared in `unit`.
#[must_use]
pub fn collect_model(unit: &CompilationUnit) -> Model {
    let mut model = Model::new();
    collect_into(&mut model, unit);
    model.link_bases();
    model
}

/// Adds `unit`'s declared types to an existing model (e.g. one already holding the
/// reference assemblies). The caller links bases once the model is complete.
pub fn collect_into(model: &mut Model, unit: &CompilationUnit) {
    for member in &unit.members {
        collect_namespace_member(member, "", model);
    }
}

/// Builds a [`TypeTable`] of every type declared in `unit` (the existence-only
/// view derived from the full [`Model`]).
#[must_use]
pub fn collect_types(unit: &CompilationUnit) -> TypeTable {
    collect_model(unit).type_table()
}

fn collect_namespace_member(member: &NamespaceMember, namespace: &str, model: &mut Model) {
    match member {
        NamespaceMember::Namespace(declaration) => {
            let inner = join_namespace(namespace, &declaration.name);
            for inner_member in &declaration.members {
                collect_namespace_member(inner_member, &inner, model);
            }
        }
        NamespaceMember::Type(declaration) => model.insert(type_info(namespace, declaration)),
        NamespaceMember::Enum(declaration) => {
            let mut info = TypeInfo::new(namespace, &declaration.name, TypeKind::Enum);
            let enum_ty = named_symbol(namespace, &declaration.name);
            let mut next_value: i64 = 0;
            for member in &declaration.members {
                let value = member
                    .value
                    .as_ref()
                    .and_then(enum_member_value)
                    .unwrap_or(next_value);
                next_value = value.wrapping_add(1);
                info.fields.push(FieldSymbol {
                    name: member.name.clone(),
                    ty: enum_ty.clone(),
                    is_static: true,
                    accessibility: Accessibility::Public,
                    constant: Some(value),
                });
            }
            model.insert(info);
        }
        NamespaceMember::Delegate(declaration) => {
            let mut info = TypeInfo::new(namespace, &declaration.name, TypeKind::Delegate);
            info.methods.push(MethodSymbol {
                name: "Invoke".into(),
                return_type: bind_type(&declaration.return_type),
                parameters: declaration
                    .parameters
                    .iter()
                    .map(|p| bind_type(&p.ty))
                    .collect(),
                is_static: false,
                accessibility: Accessibility::Public,
            });
            model.insert(info);
        }
    }
}

/// Evaluates an enum member's value expression to its underlying integral value.
/// The v1 forms are an integer or character literal, optionally negated; anything
/// else yields `None`, and the caller continues the auto-increment.
fn enum_member_value(expr: &Expr) -> Option<i64> {
    match &expr.kind {
        ExprKind::Literal(Literal::Integer { value, .. }) => i64::try_from(*value).ok(),
        ExprKind::Literal(Literal::Character(unit)) => Some(i64::from(*unit)),
        ExprKind::Unary {
            operator: UnaryOperator::Minus,
            operand,
        } => enum_member_value(operand).map(|value| -value),
        _ => None,
    }
}

/// Builds the [`TypeInfo`] for one type declaration, collecting its fields and
/// methods.
fn type_info(namespace: &str, declaration: &TypeDecl) -> TypeInfo {
    let mut info = TypeInfo::new(namespace, &declaration.name, map_kind(declaration.kind));
    info.bases = declaration.bases.iter().map(bind_type).collect();
    let is_interface = matches!(declaration.kind, SyntaxTypeKind::Interface);
    let access = |modifiers: &[Modifier]| {
        if is_interface {
            Accessibility::Public
        } else {
            accessibility_of(modifiers)
        }
    };
    for member in &declaration.members {
        match member {
            Member::Field {
                modifiers,
                ty,
                declarators,
                ..
            } => {
                let field_ty = bind_type(ty);
                let is_static = is_static(modifiers);
                let accessibility = access(modifiers);
                for declarator in declarators {
                    info.fields.push(FieldSymbol {
                        name: declarator.name.clone(),
                        ty: field_ty.clone(),
                        is_static,
                        accessibility,
                        constant: None,
                    });
                }
            }
            Member::Method {
                modifiers,
                return_type,
                name,
                parameters,
                ..
            } => info.methods.push(MethodSymbol {
                name: name.clone(),
                return_type: bind_type(return_type),
                parameters: parameters.iter().map(|p| bind_type(&p.ty)).collect(),
                is_static: is_static(modifiers),
                accessibility: access(modifiers),
            }),
            Member::Property {
                modifiers,
                ty,
                name,
                ..
            } => info.properties.push(PropertySymbol {
                name: name.clone(),
                ty: bind_type(ty),
                is_static: is_static(modifiers),
                accessibility: access(modifiers),
            }),
            Member::Constructor {
                modifiers,
                parameters,
                ..
            } if !is_static(modifiers) => info.constructors.push(constructor(parameters)),
            _ => {}
        }
    }
    let has_parameterless = info.constructors.iter().any(|c| c.parameters.is_empty());
    match info.kind {
        TypeKind::Struct if !has_parameterless => info.constructors.push(constructor(&[])),
        TypeKind::Class if info.constructors.is_empty() => info.constructors.push(constructor(&[])),
        _ => {}
    }
    info
}

/// A constructor symbol from its parameters. The return type is unused (a `new`
/// expression takes the created type), so it is left as `void`.
fn constructor(parameters: &[lamella_syntax::ast::Parameter]) -> MethodSymbol {
    MethodSymbol {
        name: ".ctor".into(),
        return_type: TypeSymbol::Special(SpecialType::Void),
        parameters: parameters.iter().map(|p| bind_type(&p.ty)).collect(),
        is_static: false,
        accessibility: Accessibility::Public,
    }
}

/// A named-type symbol from a namespace and simple name, e.g. `"A.B"` + `Color`
/// gives `A.B.Color`.
fn named_symbol(namespace: &str, name: &str) -> TypeSymbol {
    let mut parts: alloc::vec::Vec<alloc::boxed::Box<str>> = alloc::vec::Vec::new();
    if !namespace.is_empty() {
        for part in namespace.split('.') {
            parts.push(part.into());
        }
    }
    parts.push(name.into());
    TypeSymbol::Named(parts.into_boxed_slice())
}

fn map_kind(kind: SyntaxTypeKind) -> TypeKind {
    match kind {
        SyntaxTypeKind::Class => TypeKind::Class,
        SyntaxTypeKind::Struct => TypeKind::Struct,
        SyntaxTypeKind::Interface => TypeKind::Interface,
    }
}

fn is_static(modifiers: &[Modifier]) -> bool {
    modifiers.contains(&Modifier::Static)
}

/// The accessibility a member's modifiers declare; a class member with none is
/// `private` (10.5.1).
fn accessibility_of(modifiers: &[Modifier]) -> Accessibility {
    let protected = modifiers.contains(&Modifier::Protected);
    let internal = modifiers.contains(&Modifier::Internal);
    if modifiers.contains(&Modifier::Public) {
        Accessibility::Public
    } else if protected && internal {
        Accessibility::ProtectedInternal
    } else if protected {
        Accessibility::Protected
    } else if internal {
        Accessibility::Internal
    } else {
        Accessibility::Private
    }
}

/// Appends a (possibly dotted) namespace declaration name to the enclosing
/// namespace, e.g. `"A"` and `B.C` give `"A.B.C"`.
fn join_namespace(outer: &str, name: &QualifiedName) -> String {
    let mut joined = String::from(outer);
    for part in &name.parts {
        if !joined.is_empty() {
            joined.push('.');
        }
        joined.push_str(part);
    }
    joined
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;
    use lamella_syntax::parser::parse_compilation_unit;

    #[test]
    fn collects_top_level_namespaced_and_nested_namespace_types() {
        let unit = parse_compilation_unit(
            "class Bar {} enum E { A } \
             namespace A.B { class Foo {} delegate void D(); \
                namespace C { struct S {} } }",
        )
        .unit;
        let table = collect_types(&unit);
        assert!(table.contains("", "Bar"));
        assert!(table.contains("", "E"));
        assert!(table.contains("A.B", "Foo"));
        assert!(table.contains("A.B", "D"));
        assert!(table.contains("A.B.C", "S"));
        assert!(!table.contains("", "Foo"));
        assert!(!table.contains("", "Missing"));
    }

    #[test]
    fn collects_fields_and_methods_of_a_source_type() {
        let unit = parse_compilation_unit(
            "namespace N { class Widget { \
                int count; \
                static int Make(int n, string s) { } \
                double Area() { } \
             } }",
        )
        .unit;
        let model = collect_model(&unit);
        let widget = model
            .get("N", "Widget")
            .expect("Widget should be collected");
        assert_eq!(widget.kind, TypeKind::Class);
        assert_eq!(
            widget.find_field("count").map(|field| field.ty.to_string()),
            Some("int".to_string())
        );
        let make = widget.methods_named("Make").next().expect("Make");
        assert!(make.is_static);
        assert_eq!(make.parameters.len(), 2);
        assert_eq!(make.return_type.to_string(), "int");
        let area = widget.methods_named("Area").next().expect("Area");
        assert!(!area.is_static);
        assert!(area.return_type.to_string() == "double");
    }
}
