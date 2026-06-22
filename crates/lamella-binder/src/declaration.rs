//! Collecting the types and members declared in source (ECMA-334 1st ed,
//! clauses 16-18).

use crate::bind::bind_type;
use crate::resolve::TypeTable;
use crate::special::SpecialType;
use crate::symbols::{
    Accessibility, EventSymbol, FieldSymbol, MethodSymbol, Model, PropertySymbol, TypeInfo,
    TypeKind,
};
use crate::types::TypeSymbol;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use lamella_syntax::ast::{
    AttributeArgument, AttributeSection, CompilationUnit, Expr, ExprKind, Literal, Member,
    Modifier, NamespaceMember, QualifiedName, TypeDecl, TypeKind as SyntaxTypeKind, UnaryOperator,
    explicit_interface_member_name,
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
        NamespaceMember::Type(declaration) => {
            model.insert(type_info(namespace, declaration));
            collect_nested_types(declaration, namespace, model);
        }
        NamespaceMember::Enum(declaration) => {
            let mut info = TypeInfo::new(namespace, &declaration.name, TypeKind::Enum);
            let enum_base = named_symbol("System", "Enum");
            info.bases.push(enum_base.clone());
            info.base = Some(enum_base);
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
                    is_readonly: false,
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
                is_params: has_params_array(&declaration.parameters),
                accessibility: Accessibility::Public,
                conditional: Vec::new(),
            });
            model.insert(info);
        }
    }
}

/// Collects the class/struct types nested in `declaration`, each keyed under the
/// enclosing type's full name (so `Outer.Inner` resolves to it) and marked with its
/// enclosing type (driving the `NestedClass` row + empty namespace at emission). Recurses
/// for deeper nesting. Nested enums/delegates are a follow-up.
fn collect_nested_types(declaration: &TypeDecl, namespace: &str, model: &mut Model) {
    let enclosing_full = qualified_type_name(namespace, &declaration.name);
    for member in &declaration.members {
        if let Member::NestedType(nested) = member {
            collect_namespace_member(nested, &enclosing_full, model);
            if let Some(name) = nested_member_name(nested) {
                model.set_enclosing(&enclosing_full, name, &enclosing_full);
            }
        }
    }
}

/// The simple name of a nested type member (a class/struct/interface/enum/delegate).
fn nested_member_name(member: &NamespaceMember) -> Option<&str> {
    match member {
        NamespaceMember::Type(declaration) => Some(declaration.name.as_ref()),
        NamespaceMember::Enum(declaration) => Some(declaration.name.as_ref()),
        NamespaceMember::Delegate(declaration) => Some(declaration.name.as_ref()),
        NamespaceMember::Namespace(_) => None,
    }
}

/// Joins a namespace (possibly empty) and a simple name into a dotted full name.
fn qualified_type_name(namespace: &str, name: &str) -> alloc::string::String {
    if namespace.is_empty() {
        alloc::string::String::from(name)
    } else {
        alloc::format!("{namespace}.{name}")
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
    if matches!(declaration.kind, SyntaxTypeKind::Struct) {
        info.bases.push(named_symbol("System", "ValueType"));
    }
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
                let is_const = modifiers.iter().any(|m| matches!(m, Modifier::Const));
                let is_static = is_static(modifiers) || is_const;
                let accessibility = access(modifiers);
                for declarator in declarators {
                    let constant = if is_const {
                        declarator.initializer.as_ref().and_then(enum_member_value)
                    } else {
                        None
                    };
                    info.fields.push(FieldSymbol {
                        name: declarator.name.clone(),
                        ty: field_ty.clone(),
                        is_static,
                        is_readonly: modifiers.iter().any(|m| matches!(m, Modifier::Readonly)),
                        accessibility,
                        constant,
                    });
                }
            }
            Member::EventField {
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
                        is_readonly: false,
                        accessibility,
                        constant: None,
                    });
                    info.events.push(EventSymbol {
                        name: declarator.name.clone(),
                        ty: field_ty.clone(),
                        is_static,
                        accessibility,
                    });
                }
            }
            Member::Event {
                modifiers,
                ty,
                name,
                explicit_interface: None,
                ..
            } => info.events.push(EventSymbol {
                name: name.clone(),
                ty: bind_type(ty),
                is_static: is_static(modifiers),
                accessibility: access(modifiers),
            }),
            Member::Method {
                modifiers,
                return_type,
                name,
                parameters,
                explicit_interface,
                attributes,
                ..
            } => info.methods.push(MethodSymbol {
                name: match explicit_interface {
                    Some(interface) => explicit_interface_member_name(interface, name).into(),
                    None => name.clone(),
                },
                return_type: bind_type(return_type),
                parameters: parameters.iter().map(|p| bind_type(&p.ty)).collect(),
                is_static: explicit_interface.is_none() && is_static(modifiers),
                is_params: has_params_array(parameters),
                accessibility: match explicit_interface {
                    Some(_) => Accessibility::Private,
                    None => access(modifiers),
                },
                conditional: conditional_symbols_from_attributes(attributes),
            }),
            Member::Operator {
                return_type,
                operator,
                parameters,
                ..
            } => info.methods.push(MethodSymbol {
                name: operator.method_name(parameters.len()).into(),
                return_type: bind_type(return_type),
                parameters: parameters.iter().map(|p| bind_type(&p.ty)).collect(),
                is_static: true,
                is_params: false,
                accessibility: Accessibility::Public,
                conditional: Vec::new(),
            }),
            Member::ConversionOperator {
                direction,
                target,
                parameters,
                ..
            } => info.methods.push(MethodSymbol {
                name: direction.method_name().into(),
                return_type: bind_type(target),
                parameters: parameters.iter().map(|p| bind_type(&p.ty)).collect(),
                is_static: true,
                is_params: false,
                accessibility: Accessibility::Public,
                conditional: Vec::new(),
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
        is_params: has_params_array(parameters),
        accessibility: Accessibility::Public,
        conditional: Vec::new(),
    }
}

/// The `[Conditional("X")]` symbols declared on a source member (24.4.2): the attribute name is
/// matched as written (`Conditional` or the `Attribute`-suffixed form) and the symbol is its
/// first positional string-literal argument. A call to a source method marked this way is
/// omitted unless `X` is defined at the call site, like a BCL `Debug`/`Trace` method.
fn conditional_symbols_from_attributes(sections: &[AttributeSection]) -> Vec<Box<str>> {
    let mut symbols = Vec::new();
    for section in sections {
        if section.target.is_some() {
            continue;
        }
        for attribute in &section.attributes {
            let last = attribute.name.parts.last().map(|part| &**part);
            if last != Some("Conditional") && last != Some("ConditionalAttribute") {
                continue;
            }
            if let Some(AttributeArgument::Positional(expr)) = attribute.arguments.first() {
                if let ExprKind::Literal(Literal::String(units)) = &expr.kind {
                    if let Ok(symbol) = String::from_utf16(units) {
                        symbols.push(symbol.into_boxed_str());
                    }
                }
            }
        }
    }
    symbols
}

/// Whether a parameter list ends in a `params` array.
fn has_params_array(parameters: &[lamella_syntax::ast::Parameter]) -> bool {
    parameters.last().is_some_and(|parameter| {
        parameter.modifier == Some(lamella_syntax::ast::ParameterModifier::Params)
    })
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
