//! Collecting the types and members declared in source (ECMA-334 1st ed,
//! clauses 16-18).

use crate::bind::bind_type;
use crate::resolve::TypeTable;
use crate::special::SpecialType;
use crate::symbols::{FieldSymbol, MethodSymbol, Model, PropertySymbol, TypeInfo, TypeKind};
use crate::types::TypeSymbol;
use alloc::string::String;
use lamella_syntax::ast::{
    CompilationUnit, Member, Modifier, NamespaceMember, QualifiedName, TypeDecl,
    TypeKind as SyntaxTypeKind,
};

/// Builds the [`Model`] of every type and member declared in `unit`.
#[must_use]
pub fn collect_model(unit: &CompilationUnit) -> Model {
    let mut model = Model::new();
    for member in &unit.members {
        collect_namespace_member(member, "", &mut model);
    }
    model.link_bases();
    model
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
            model.insert(TypeInfo::new(namespace, &declaration.name, TypeKind::Enum));
        }
        NamespaceMember::Delegate(declaration) => {
            model.insert(TypeInfo::new(
                namespace,
                &declaration.name,
                TypeKind::Delegate,
            ));
        }
    }
}

/// Builds the [`TypeInfo`] for one type declaration, collecting its fields and
/// methods.
fn type_info(namespace: &str, declaration: &TypeDecl) -> TypeInfo {
    let mut info = TypeInfo::new(namespace, &declaration.name, map_kind(declaration.kind));
    info.bases = declaration.bases.iter().map(bind_type).collect();
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
                for declarator in declarators {
                    info.fields.push(FieldSymbol {
                        name: declarator.name.clone(),
                        ty: field_ty.clone(),
                        is_static,
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
            }),
            Member::Constructor {
                modifiers,
                parameters,
                ..
            } if !is_static(modifiers) => info.constructors.push(constructor(parameters)),
            _ => {}
        }
    }
    if matches!(info.kind, TypeKind::Class | TypeKind::Struct) && info.constructors.is_empty() {
        info.constructors.push(constructor(&[]));
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
    }
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
