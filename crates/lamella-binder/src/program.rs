//! Binding a whole compilation unit (ECMA-334 1st ed, clause 16).

use crate::bind::bind_type;
use crate::bound::Binder;
use crate::declaration::collect_into;
use crate::diagnostic::{Diagnostic, DiagnosticKind};
use crate::reference::load_assembly;
use crate::special::SpecialType;
use crate::symbols::Model;
use crate::types::TypeSymbol;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use lamella_metadata::Assembly;
use lamella_syntax::ast::{
    CompilationUnit, Member, NamespaceMember, QualifiedName, TypeDecl, UsingDirective, UsingKind,
};

/// Binds `unit` against its own declared types, returning every semantic
/// diagnostic.
#[must_use]
pub fn bind_compilation_unit(unit: &CompilationUnit) -> Vec<Diagnostic> {
    bind_compilation_unit_with_model(unit, Model::new())
}

/// Binds `unit` against the types in `references` (the BCL / the parity reference
/// set) plus its own declared types.
#[must_use]
pub fn bind_compilation_unit_with_references(
    unit: &CompilationUnit,
    references: &[Assembly],
) -> Vec<Diagnostic> {
    let mut model = Model::new();
    for reference in references {
        load_assembly(&mut model, reference);
    }
    bind_compilation_unit_with_model(unit, model)
}

/// Binds `unit` against an already-built reference `model`, into which the unit's
/// own declared types are merged. The base-class chain is linked over the whole.
#[must_use]
pub fn bind_compilation_unit_with_model(
    unit: &CompilationUnit,
    mut model: Model,
) -> Vec<Diagnostic> {
    collect_into(&mut model, unit);
    model.link_bases();
    let mut binder = Binder::with_model(model);
    bind_namespace_body(&mut binder, &unit.usings, &unit.members, "");
    binder.into_diagnostics()
}

fn bind_namespace_body(
    binder: &mut Binder,
    usings: &[UsingDirective],
    members: &[NamespaceMember],
    namespace: &str,
) {
    let scope = binder.import_scope();
    for using in usings {
        if let UsingKind::Namespace(name) = &using.kind {
            binder.import_namespace(&dotted(name));
        }
    }
    for member in members {
        match member {
            NamespaceMember::Namespace(declaration) => {
                let inner = join_namespace(namespace, &declaration.name);
                bind_namespace_body(binder, &declaration.usings, &declaration.members, &inner);
            }
            NamespaceMember::Type(declaration) => bind_type_bodies(binder, namespace, declaration),
            NamespaceMember::Enum(_) | NamespaceMember::Delegate(_) => {}
        }
    }
    binder.restore_import_scope(scope);
}

fn bind_type_bodies(binder: &mut Binder, namespace: &str, declaration: &TypeDecl) {
    let enclosing = named_symbol(namespace, &declaration.name);
    let mut seen_fields: alloc::collections::BTreeSet<&str> = alloc::collections::BTreeSet::new();
    for member in &declaration.members {
        if let Member::Field { declarators, .. } = member {
            for declarator in declarators {
                if !seen_fields.insert(&declarator.name) {
                    binder.report(Diagnostic::new(
                        DiagnosticKind::DuplicateMember {
                            type_name: declaration.name.clone(),
                            member: declarator.name.clone(),
                        },
                        declarator.span,
                    ));
                }
            }
        }
    }
    let mut seen_methods: alloc::vec::Vec<(Box<str>, alloc::vec::Vec<TypeSymbol>)> =
        alloc::vec::Vec::new();
    for member in &declaration.members {
        if let Member::Method {
            name,
            parameters,
            explicit_interface: None,
            span,
            ..
        } = member
        {
            let key = (
                name.clone(),
                bound_parameters(parameters)
                    .into_iter()
                    .map(|(_, ty)| ty)
                    .collect::<alloc::vec::Vec<_>>(),
            );
            if seen_methods.contains(&key) {
                binder.report(Diagnostic::new(
                    DiagnosticKind::DuplicateMethod {
                        type_name: declaration.name.clone(),
                        member: name.clone(),
                    },
                    *span,
                ));
            } else {
                seen_methods.push(key);
            }
        }
    }
    for member in &declaration.members {
        if let Member::Method {
            modifiers,
            name,
            body: Some(_),
            span,
            ..
        } = member
        {
            if modifiers
                .iter()
                .any(|modifier| matches!(modifier, lamella_syntax::ast::Modifier::Abstract))
            {
                binder.report(Diagnostic::new(
                    DiagnosticKind::AbstractMethodWithBody {
                        member: name.clone(),
                    },
                    *span,
                ));
            }
        }
    }
    for member in &declaration.members {
        match member {
            Member::Method {
                return_type,
                name,
                parameters,
                body: Some(body),
                ..
            } => {
                let params = bound_parameters(parameters);
                binder.bind_method(
                    Some(enclosing.clone()),
                    name,
                    bind_type(return_type),
                    &params,
                    body,
                );
            }
            Member::Operator {
                return_type,
                operator,
                parameters,
                body,
                ..
            } => {
                let params = bound_parameters(parameters);
                binder.bind_method(
                    Some(enclosing.clone()),
                    operator.method_name(parameters.len()),
                    bind_type(return_type),
                    &params,
                    body,
                );
            }
            Member::ConversionOperator {
                direction,
                target,
                parameters,
                body,
                ..
            } => {
                let params = bound_parameters(parameters);
                binder.bind_method(
                    Some(enclosing.clone()),
                    direction.method_name(),
                    bind_type(target),
                    &params,
                    body,
                );
            }
            Member::Constructor {
                parameters, body, ..
            } => {
                let params = bound_parameters(parameters);
                binder.bind_method(
                    Some(enclosing.clone()),
                    ".ctor",
                    TypeSymbol::Special(SpecialType::Void),
                    &params,
                    body,
                );
            }
            Member::Property {
                ty,
                name,
                getter,
                setter,
                ..
            } => {
                let property_ty = bind_type(ty);
                if let Some(body) = getter.as_ref().and_then(|accessor| accessor.body.as_ref()) {
                    binder.bind_method(
                        Some(enclosing.clone()),
                        &accessor_name("get_", name),
                        property_ty.clone(),
                        &[],
                        body,
                    );
                }
                if let Some(body) = setter.as_ref().and_then(|accessor| accessor.body.as_ref()) {
                    binder.bind_method(
                        Some(enclosing.clone()),
                        &accessor_name("set_", name),
                        TypeSymbol::Special(SpecialType::Void),
                        &[(Box::from("value"), property_ty.clone())],
                        body,
                    );
                }
            }
            Member::Field {
                ty, declarators, ..
            } => {
                let field_ty = bind_type(ty);
                for declarator in declarators {
                    if let Some(initializer) = &declarator.initializer {
                        binder.bind_field_initializer(enclosing.clone(), &field_ty, initializer);
                    }
                }
            }
            Member::Destructor { body, .. } => {
                binder.bind_method(
                    Some(enclosing.clone()),
                    "Finalize",
                    TypeSymbol::Special(SpecialType::Void),
                    &[],
                    body,
                );
            }
            Member::NestedType(nested) => {
                if let NamespaceMember::Type(nested_decl) = nested.as_ref() {
                    let enclosing_full = if namespace.is_empty() {
                        String::from(&*declaration.name)
                    } else {
                        alloc::format!("{namespace}.{}", declaration.name)
                    };
                    bind_type_bodies(binder, &enclosing_full, nested_decl);
                }
            }
            _ => {}
        }
    }
    binder.check_base_cycle(&enclosing, declaration);
    binder.check_interface_implementations(&enclosing, declaration);
}

/// The accessor method name (`get_Name` / `set_Name`), for diagnostics.
fn accessor_name(prefix: &str, property: &str) -> String {
    let mut name = String::from(prefix);
    name.push_str(property);
    name
}

fn bound_parameters(parameters: &[lamella_syntax::ast::Parameter]) -> Vec<(Box<str>, TypeSymbol)> {
    parameters
        .iter()
        .map(|parameter| (parameter.name.clone(), bind_type(&parameter.ty)))
        .collect()
}

/// A named-type symbol from a namespace (empty or dotted) and a simple name.
fn named_symbol(namespace: &str, name: &str) -> TypeSymbol {
    let mut parts: Vec<Box<str>> = Vec::new();
    if !namespace.is_empty() {
        for part in namespace.split('.') {
            parts.push(part.into());
        }
    }
    parts.push(name.into());
    TypeSymbol::Named(parts.into_boxed_slice())
}

/// Appends a (possibly dotted) namespace declaration name to the enclosing one.
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

fn dotted(name: &QualifiedName) -> String {
    let mut text = String::new();
    for part in &name.parts {
        if !text.is_empty() {
            text.push('.');
        }
        text.push_str(part);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;
    use lamella_syntax::parser::parse_compilation_unit;

    fn sorted_codes(unit: &str) -> Vec<u16> {
        let unit = parse_compilation_unit(unit).unit;
        let mut codes: Vec<u16> = bind_compilation_unit(&unit)
            .iter()
            .map(Diagnostic::code)
            .collect();
        codes.sort_unstable();
        codes
    }

    #[test]
    fn binds_every_method_body_and_collects_diagnostics() {
        let codes = sorted_codes(
            "class C { \
                int Unassigned() { int x; return x; } \
                int WrongType() { return \"s\"; } \
                void Void() { return 1; } \
                int Ok(int n) { return n; } \
             }",
        );
        assert_eq!(codes, [29, 127, 165]);
    }

    #[test]
    fn binds_property_accessor_bodies() {
        let codes = sorted_codes(
            "class Box { \
                int Bad { get { return \"s\"; } } \
                int Sink { set { if (value > 0) { } } } \
                int Oops { set { return 1; } } \
             }",
        );
        assert_eq!(codes, [29, 127]);
    }

    #[test]
    fn duplicate_field_name_is_cs0102() {
        assert_eq!(sorted_codes("class C { int x; int x; }"), [102]);
        assert_eq!(sorted_codes("class C { int x; int y; }"), []);
    }

    #[test]
    fn binds_field_initializers() {
        let codes = sorted_codes("class C { int x = \"s\"; int y = 1; long n = 2; }");
        assert_eq!(codes, [29]);
    }

    #[test]
    fn a_clean_program_has_no_diagnostics() {
        let codes = sorted_codes(
            "namespace App { \
                class Math { int Twice(int n) { return n + n; } } \
             }",
        );
        assert_eq!(codes, []);
    }

    #[test]
    fn binds_a_program_against_a_reference_model() {
        use crate::symbols::{MethodSymbol, Model, TypeInfo, TypeKind};

        let mut bcl = Model::new();
        let mut console = TypeInfo::new("System", "Console", TypeKind::Class);
        console.methods.push(MethodSymbol {
            name: "WriteLine".into(),
            return_type: TypeSymbol::Special(SpecialType::Void),
            parameters: alloc::vec![TypeSymbol::Special(SpecialType::String)],
            is_static: true,
            is_params: false,
            accessibility: crate::symbols::Accessibility::Public,
        });
        bcl.insert(console);

        let bind = |source: &str| {
            let unit = parse_compilation_unit(source).unit;
            let mut codes: Vec<u16> = bind_compilation_unit_with_model(&unit, bcl.clone())
                .iter()
                .map(Diagnostic::code)
                .collect();
            codes.sort_unstable();
            codes
        };

        assert_eq!(
            bind("using System; class P { void M() { Console.WriteLine(\"hi\"); } }"),
            []
        );
        assert_eq!(
            bind("using System; class P { void M() { Console.WriteLine(123); } }"),
            [1503]
        );
        assert_eq!(
            bind("class P { void M() { Console.WriteLine(\"hi\"); } }"),
            [103]
        );
    }
}
