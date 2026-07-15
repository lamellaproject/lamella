//! Binding a whole compilation unit (ECMA-334 1st ed, clause 16).

use crate::bind::{bind_type, parameter_symbol};
use crate::bound::Binder;
use crate::declaration::{accessibility_of, collect_into};
use crate::diagnostic::{Diagnostic, DiagnosticKind, SignaturePosition};
use crate::reference::load_assembly;
use crate::special::SpecialType;
use crate::symbols::{Accessibility, Model, TypeInfo};
use crate::types::TypeSymbol;
use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use lamella_metadata::Assembly;
use lamella_syntax::ast::{
    CompilationUnit, ConversionDirection, DelegateDecl, Member, Modifier, NamespaceMember,
    OverloadableOperator, Parameter, ParameterModifier, QualifiedName, TypeDecl, TypeKind,
    TypeRefKind, UsingDirective, UsingKind,
};
use lamella_syntax::span::Span;

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
    model.canonicalize_signatures();
    model.link_bases();
    let mut binder = Binder::with_model(model);
    let mut declared_types: DeclaredTypes = DeclaredTypes::new();
    report_duplicate_types(&mut binder, &unit.members, "", &mut declared_types);
    bind_namespace_body(&mut binder, &unit.usings, &unit.members, "");
    report_multiple_entry_points(&mut binder);
    binder.report_unused_fields();
    binder.into_diagnostics()
}

/// The set of type names already declared in each namespace, so a second declaration of
/// the same name in the same namespace is CS0101. Keyed by the dotted namespace (empty
/// for the global namespace); spans the whole compilation (a namespace may be reopened).
type DeclaredTypes = alloc::collections::BTreeMap<String, alloc::collections::BTreeSet<Box<str>>>;

/// CS0101: a namespace already contains a definition for a type name (16.3). Every type
/// declared DIRECTLY in a namespace (a class/struct/interface, enum, or delegate) is
/// recorded; a second one of the same name -- even in a reopened namespace block -- is a
/// duplicate. A duplicate NESTED type is CS0102 instead, reported elsewhere, so this walk
/// does not descend into a type's members. C# 1.0 has no partial types, so any repeat is an
/// error.
fn report_duplicate_types(
    binder: &mut Binder,
    members: &[NamespaceMember],
    namespace: &str,
    declared: &mut DeclaredTypes,
) {
    for member in members {
        let (name, span) = match member {
            NamespaceMember::Namespace(declaration) => {
                let inner = join_namespace(namespace, &declaration.name);
                report_duplicate_types(binder, &declaration.members, &inner, declared);
                continue;
            }
            NamespaceMember::Type(declaration) => (&declaration.name, declaration.span),
            NamespaceMember::Enum(declaration) => (&declaration.name, declaration.span),
            NamespaceMember::Delegate(declaration) => (&declaration.name, declaration.span),
        };
        if !declared
            .entry(String::from(namespace))
            .or_default()
            .insert(name.clone())
        {
            binder.report(Diagnostic::new(
                DiagnosticKind::DuplicateTypeInNamespace {
                    namespace: if namespace.is_empty() {
                        Box::from("<global namespace>")
                    } else {
                        namespace.into()
                    },
                    name: name.clone(),
                },
                span,
            ));
        }
    }
}

/// CS0017: a program declares more than one entry point when two or more of its types have a
/// valid `static Main` (10.1). lcsc has no `/main` selector, so any second entry point is an error.
fn report_multiple_entry_points(binder: &mut Binder) {
    if binder.model().entry_point_count() > 1 {
        binder.report(Diagnostic::new(
            DiagnosticKind::MultipleEntryPoints,
            lamella_syntax::span::Span::new(0, 0),
        ));
    }
}

/// Binds several compilation units as ONE program (a multi-file compilation, 16.1):
/// every unit's declared types enter one model first -- so each file names the others'
/// types -- then each unit's bodies are bound against the whole. Returns one diagnostic
/// list per unit, in order, so a driver attributes each to its own source file.
#[must_use]
pub fn bind_compilation_units_with_references(
    units: &[CompilationUnit],
    references: &[Assembly],
) -> Vec<Vec<Diagnostic>> {
    let mut model = Model::new();
    for reference in references {
        load_assembly(&mut model, reference);
    }
    for unit in units {
        collect_into(&mut model, unit);
    }
    model.canonicalize_signatures();
    model.link_bases();
    let mut binder = Binder::with_model(model);
    let mut declared_types: DeclaredTypes = DeclaredTypes::new();
    let mut per_unit: Vec<Vec<Diagnostic>> = units
        .iter()
        .map(|unit| {
            report_duplicate_types(&mut binder, &unit.members, "", &mut declared_types);
            bind_namespace_body(&mut binder, &unit.usings, &unit.members, "");
            binder.report_unused_fields();
            binder.take_diagnostics()
        })
        .collect();
    if binder.model().entry_point_count() > 1 {
        if let Some(first) = per_unit.first_mut() {
            first.push(Diagnostic::new(
                DiagnosticKind::MultipleEntryPoints,
                lamella_syntax::span::Span::new(0, 0),
            ));
        }
    }
    per_unit
}

fn bind_namespace_body(
    binder: &mut Binder,
    usings: &[UsingDirective],
    members: &[NamespaceMember],
    namespace: &str,
) {
    let scope = binder.import_scope();
    for using in usings {
        match &using.kind {
            UsingKind::Namespace(name) => binder.import_namespace(&dotted(name)),
            UsingKind::Alias { name, target } => {
                binder.import_alias(name, TypeSymbol::Named(target.parts.iter().cloned().collect()));
            }
        }
    }
    let mut prefix = String::new();
    for part in namespace.split('.').filter(|part| !part.is_empty()) {
        if !prefix.is_empty() {
            prefix.push('.');
        }
        prefix.push_str(part);
        binder.import_namespace(&prefix);
    }
    for member in members {
        match member {
            NamespaceMember::Namespace(declaration) => {
                let inner = join_namespace(namespace, &declaration.name);
                bind_namespace_body(binder, &declaration.usings, &declaration.members, &inner);
            }
            NamespaceMember::Type(declaration) => bind_type_bodies(binder, namespace, declaration),
            NamespaceMember::Delegate(declaration) => {
                check_delegate_accessibility(binder, namespace, declaration);
            }
            NamespaceMember::Enum(_) => {}
        }
    }
    binder.restore_import_scope(scope);
}

/// CS0058 / CS0059: a delegate's return type and parameter types must each be at least as
/// accessible as the delegate itself (10.5.4). The delegate's effective accessibility comes from
/// its declared modifiers (a top-level delegate defaults to `internal`); an unresolved, reference,
/// or predefined signature type never fires, so this never false-flags a valid program.
fn check_delegate_accessibility(binder: &mut Binder, namespace: &str, declaration: &DelegateDecl) {
    let delegate = named_symbol(namespace, &declaration.name);
    let delegate_mask = {
        let model = binder.model();
        model
            .get_by_symbol(&delegate)
            .map_or(ACCESS_FULL, |info| effective_info_mask(model, info))
    };
    let return_ty = binder.canonicalize(&bind_type(&declaration.return_type));
    if exposes_less_accessible(effective_type_mask(binder.model(), &return_ty), delegate_mask) {
        binder.report(Diagnostic::new(
            DiagnosticKind::InconsistentAccessibility {
                position: SignaturePosition::DelegateReturnType,
                type_name: return_ty.to_string().into(),
                member: declaration.name.clone(),
            },
            declaration.span,
        ));
    }
    for parameter in &declaration.parameters {
        let param_ty = binder.canonicalize(&bind_type(&parameter.ty));
        if exposes_less_accessible(effective_type_mask(binder.model(), &param_ty), delegate_mask) {
            binder.report(Diagnostic::new(
                DiagnosticKind::InconsistentAccessibility {
                    position: SignaturePosition::DelegateParameterType,
                    type_name: param_ty.to_string().into(),
                    member: declaration.name.clone(),
                },
                declaration.span,
            ));
        }
    }
}

fn bind_type_bodies(binder: &mut Binder, namespace: &str, declaration: &TypeDecl) {
    let enclosing = named_symbol(namespace, &declaration.name);
    let mut seen_fields: alloc::collections::BTreeSet<&str> = alloc::collections::BTreeSet::new();
    let mut duplicate_field_names: alloc::collections::BTreeSet<&str> =
        alloc::collections::BTreeSet::new();
    for member in &declaration.members {
        if let Member::Field { declarators, .. } = member {
            for declarator in declarators {
                if !seen_fields.insert(&declarator.name) {
                    duplicate_field_names.insert(&declarator.name);
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
                parameters
                    .iter()
                    .map(parameter_symbol)
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
                .any(|modifier| matches!(modifier, Modifier::Abstract))
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
    if declaration.kind != TypeKind::Interface {
        for member in &declaration.members {
            if let Member::Method {
                modifiers,
                name,
                parameters,
                body: None,
                span,
                ..
            } = member
            {
                let bodyless_allowed = modifiers
                    .iter()
                    .any(|modifier| matches!(modifier, Modifier::Abstract | Modifier::Extern));
                if !bodyless_allowed {
                    binder.report(Diagnostic::new(
                        DiagnosticKind::MethodMustHaveBody {
                            method: method_signature(&declaration.name, name, parameters),
                        },
                        *span,
                    ));
                }
            }
        }
    }
    if declaration.kind != TypeKind::Interface {
        for member in &declaration.members {
            let Member::Property {
                modifiers,
                getter,
                setter,
                span,
                ..
            } = member
            else {
                continue;
            };
            let bodyless_allowed = modifiers
                .iter()
                .any(|modifier| matches!(modifier, Modifier::Abstract | Modifier::Extern));
            let has_bodyless_accessor = [getter, setter]
                .into_iter()
                .flatten()
                .any(|accessor| accessor.body.is_none());
            if !bodyless_allowed && has_bodyless_accessor {
                binder.report(Diagnostic::new(
                    DiagnosticKind::FeatureRequiresLaterVersion {
                        feature: "automatically implemented properties".into(),
                        required: "C# 3.0".into(),
                    },
                    *span,
                ));
            }
        }
    }
    if declaration.kind == TypeKind::Class
        && declaration
            .modifiers
            .iter()
            .any(|modifier| matches!(modifier, Modifier::Static))
    {
        binder.report(Diagnostic::new(
            DiagnosticKind::FeatureRequiresLaterVersion {
                feature: "static classes".into(),
                required: "C# 2.0".into(),
            },
            declaration.span,
        ));
    }
    if matches!(declaration.kind, TypeKind::Class | TypeKind::Struct) {
        let type_is_abstract = declaration
            .modifiers
            .iter()
            .any(|modifier| matches!(modifier, Modifier::Abstract));
        for member in &declaration.members {
            if let Member::Method {
                modifiers,
                name,
                parameters,
                span,
                ..
            } = member
            {
                let is_abstract = modifiers
                    .iter()
                    .any(|modifier| matches!(modifier, Modifier::Abstract));
                let is_virtual = modifiers
                    .iter()
                    .any(|modifier| matches!(modifier, Modifier::Virtual));
                let effectively_private = !modifiers.iter().any(|modifier| {
                    matches!(
                        modifier,
                        Modifier::Public | Modifier::Protected | Modifier::Internal
                    )
                });
                if (is_abstract || is_virtual) && effectively_private {
                    binder.report(Diagnostic::new(
                        DiagnosticKind::VirtualOrAbstractMemberIsPrivate {
                            member: method_signature(&declaration.name, name, parameters),
                        },
                        *span,
                    ));
                } else if is_abstract
                    && declaration.kind == TypeKind::Class
                    && !type_is_abstract
                {
                    binder.report(Diagnostic::new(
                        DiagnosticKind::AbstractMemberInNonAbstractType {
                            member: method_signature(&declaration.name, name, parameters),
                            type_name: declaration.name.clone(),
                        },
                        *span,
                    ));
                }
            }
        }
    }
    if matches!(declaration.kind, TypeKind::Class | TypeKind::Struct) {
        let is_struct = declaration.kind == TypeKind::Struct;
        for member in &declaration.members {
            match member {
                Member::Method {
                    modifiers,
                    name,
                    parameters,
                    span,
                    ..
                } => check_member_modifier_validity(
                    binder,
                    is_struct,
                    modifiers,
                    &method_signature(&declaration.name, name, parameters),
                    *span,
                ),
                Member::Property {
                    modifiers,
                    name,
                    span,
                    ..
                } => check_member_modifier_validity(
                    binder,
                    is_struct,
                    modifiers,
                    &alloc::format!("{}.{}", declaration.name, name),
                    *span,
                ),
                Member::Field {
                    modifiers,
                    declarators,
                    ..
                } => {
                    for declarator in declarators {
                        check_member_modifier_validity(
                            binder,
                            is_struct,
                            modifiers,
                            &alloc::format!("{}.{}", declaration.name, declarator.name),
                            declarator.span,
                        );
                    }
                }
                _ => {}
            }
        }
    }
    if matches!(declaration.kind, TypeKind::Class | TypeKind::Struct) {
        for member in &declaration.members {
            match member {
                Member::Method { name, span, .. }
                | Member::Property { name, span, .. }
                | Member::Event { name, span, .. } => {
                    check_member_name_vs_type(binder, &declaration.name, name, *span);
                }
                Member::Field { declarators, .. } | Member::EventField { declarators, .. } => {
                    for declarator in declarators {
                        check_member_name_vs_type(
                            binder,
                            &declaration.name,
                            &declarator.name,
                            declarator.span,
                        );
                    }
                }
                Member::NestedType(inner) => {
                    if let Some((name, span)) = nested_type_name_span(inner) {
                        check_member_name_vs_type(binder, &declaration.name, name, span);
                    }
                }
                _ => {}
            }
            if let Member::Constructor {
                modifiers,
                name,
                parameters,
                span,
                ..
            } = member
            {
                if modifiers.iter().any(|m| matches!(m, Modifier::Static))
                    && !parameters.is_empty()
                {
                    binder.report(Diagnostic::new(
                        DiagnosticKind::StaticConstructorHasParameters {
                            constructor: method_signature(&declaration.name, name, parameters),
                        },
                        *span,
                    ));
                }
            }
            if let Member::Field {
                ty, declarators, ..
            } = member
            {
                if bind_type(ty).is_void() {
                    for declarator in declarators {
                        binder
                            .report(Diagnostic::new(DiagnosticKind::VoidField, declarator.span));
                    }
                }
            }
        }
    }
    for member in &declaration.members {
        let parameters = match member {
            Member::Method { parameters, .. }
            | Member::Constructor { parameters, .. }
            | Member::Indexer { parameters, .. }
            | Member::Operator { parameters, .. }
            | Member::ConversionOperator { parameters, .. } => parameters.as_slice(),
            _ => continue,
        };
        check_params_usage(binder, parameters);
    }
    let container_mask = {
        let model = binder.model();
        model
            .get_by_symbol(&enclosing)
            .map_or(ACCESS_FULL, |info| effective_info_mask(model, info))
    };
    if declaration.kind == TypeKind::Class {
        if let Some(base) = binder
            .model()
            .get_by_symbol(&enclosing)
            .and_then(|info| info.base.clone())
        {
            if exposes_less_accessible(effective_type_mask(binder.model(), &base), container_mask) {
                binder.report(Diagnostic::new(
                    DiagnosticKind::InconsistentAccessibility {
                        position: SignaturePosition::BaseClass,
                        type_name: base.to_string().into(),
                        member: declaration.name.clone(),
                    },
                    declaration.span,
                ));
            }
        }
    }
    if declaration.kind == TypeKind::Interface {
        let bases = binder
            .model()
            .get_by_symbol(&enclosing)
            .map(|info| info.bases.clone())
            .unwrap_or_default();
        for base in &bases {
            if exposes_less_accessible(effective_type_mask(binder.model(), base), container_mask) {
                binder.report(Diagnostic::new(
                    DiagnosticKind::InconsistentAccessibility {
                        position: SignaturePosition::BaseInterface,
                        type_name: base.to_string().into(),
                        member: declaration.name.clone(),
                    },
                    declaration.span,
                ));
            }
        }
    }
    for member in &declaration.members {
        let member_modifiers = match member {
            Member::Method { modifiers, .. }
            | Member::Constructor { modifiers, .. }
            | Member::Field { modifiers, .. }
            | Member::Property { modifiers, .. }
            | Member::Event { modifiers, .. }
            | Member::EventField { modifiers, .. }
            | Member::Indexer { modifiers, .. }
            | Member::Operator { modifiers, .. }
            | Member::ConversionOperator { modifiers, .. } => modifiers.as_slice(),
            _ => continue,
        };
        let member_access = if declaration.kind == TypeKind::Interface {
            Accessibility::Public
        } else {
            accessibility_of(member_modifiers)
        };
        let member_mask = access_mask(member_access) & container_mask;
        if member_mask == 0 {
            continue;
        }
        match member {
            Member::Method {
                return_type,
                name,
                parameters,
                span,
                ..
            } => {
                let signature = method_signature(&declaration.name, name, parameters);
                let return_ty = binder.canonicalize(&bind_type(return_type));
                if exposes_less_accessible(effective_type_mask(binder.model(), &return_ty), member_mask)
                {
                    binder.report(Diagnostic::new(
                        DiagnosticKind::InconsistentAccessibility {
                            position: SignaturePosition::ReturnType,
                            type_name: return_ty.to_string().into(),
                            member: signature.clone(),
                        },
                        *span,
                    ));
                }
                for parameter in parameters {
                    let param_ty = binder.canonicalize(&bind_type(&parameter.ty));
                    if exposes_less_accessible(effective_type_mask(binder.model(), &param_ty), member_mask)
                    {
                        binder.report(Diagnostic::new(
                            DiagnosticKind::InconsistentAccessibility {
                                position: SignaturePosition::ParameterType,
                                type_name: param_ty.to_string().into(),
                                member: signature.clone(),
                            },
                            *span,
                        ));
                    }
                }
            }
            Member::Constructor {
                name,
                parameters,
                span,
                ..
            } => {
                let signature = method_signature(&declaration.name, name, parameters);
                for parameter in parameters {
                    let param_ty = binder.canonicalize(&bind_type(&parameter.ty));
                    if exposes_less_accessible(effective_type_mask(binder.model(), &param_ty), member_mask)
                    {
                        binder.report(Diagnostic::new(
                            DiagnosticKind::InconsistentAccessibility {
                                position: SignaturePosition::ParameterType,
                                type_name: param_ty.to_string().into(),
                                member: signature.clone(),
                            },
                            *span,
                        ));
                    }
                }
            }
            Member::Field {
                ty, declarators, ..
            } => {
                let field_ty = binder.canonicalize(&bind_type(ty));
                if exposes_less_accessible(effective_type_mask(binder.model(), &field_ty), member_mask)
                {
                    for declarator in declarators {
                        binder.report(Diagnostic::new(
                            DiagnosticKind::InconsistentAccessibility {
                                position: SignaturePosition::FieldType,
                                type_name: field_ty.to_string().into(),
                                member: alloc::format!("{}.{}", declaration.name, declarator.name)
                                    .into(),
                            },
                            declarator.span,
                        ));
                    }
                }
            }
            Member::Property {
                ty, name, span, ..
            } => {
                let property_ty = binder.canonicalize(&bind_type(ty));
                if exposes_less_accessible(effective_type_mask(binder.model(), &property_ty), member_mask)
                {
                    binder.report(Diagnostic::new(
                        DiagnosticKind::InconsistentAccessibility {
                            position: SignaturePosition::PropertyType,
                            type_name: property_ty.to_string().into(),
                            member: alloc::format!("{}.{}", declaration.name, name).into(),
                        },
                        *span,
                    ));
                }
            }
            Member::Event {
                ty, name, span, ..
            } => {
                let event_ty = binder.canonicalize(&bind_type(ty));
                if exposes_less_accessible(effective_type_mask(binder.model(), &event_ty), member_mask)
                {
                    binder.report(Diagnostic::new(
                        DiagnosticKind::InconsistentAccessibility {
                            position: SignaturePosition::EventType,
                            type_name: event_ty.to_string().into(),
                            member: alloc::format!("{}.{}", declaration.name, name).into(),
                        },
                        *span,
                    ));
                }
            }
            Member::EventField {
                ty, declarators, ..
            } => {
                let event_ty = binder.canonicalize(&bind_type(ty));
                if exposes_less_accessible(effective_type_mask(binder.model(), &event_ty), member_mask)
                {
                    for declarator in declarators {
                        binder.report(Diagnostic::new(
                            DiagnosticKind::InconsistentAccessibility {
                                position: SignaturePosition::EventType,
                                type_name: event_ty.to_string().into(),
                                member: alloc::format!("{}.{}", declaration.name, declarator.name)
                                    .into(),
                            },
                            declarator.span,
                        ));
                    }
                }
            }
            Member::Indexer {
                ty, parameters, span, ..
            } => {
                let signature =
                    alloc::format!("{}.this[{}]", declaration.name, parameter_type_list(parameters));
                let element_ty = binder.canonicalize(&bind_type(ty));
                if exposes_less_accessible(effective_type_mask(binder.model(), &element_ty), member_mask)
                {
                    binder.report(Diagnostic::new(
                        DiagnosticKind::InconsistentAccessibility {
                            position: SignaturePosition::IndexerType,
                            type_name: element_ty.to_string().into(),
                            member: signature.clone().into(),
                        },
                        *span,
                    ));
                }
                for parameter in parameters {
                    let param_ty = binder.canonicalize(&bind_type(&parameter.ty));
                    if exposes_less_accessible(effective_type_mask(binder.model(), &param_ty), member_mask)
                    {
                        binder.report(Diagnostic::new(
                            DiagnosticKind::InconsistentAccessibility {
                                position: SignaturePosition::IndexerParameterType,
                                type_name: param_ty.to_string().into(),
                                member: signature.clone().into(),
                            },
                            *span,
                        ));
                    }
                }
            }
            Member::Operator {
                return_type,
                operator,
                parameters,
                span,
                ..
            } => {
                let signature = alloc::format!(
                    "{}.operator {}({})",
                    declaration.name,
                    operator_source_symbol(*operator),
                    parameter_type_list(parameters)
                );
                let return_ty = binder.canonicalize(&bind_type(return_type));
                if exposes_less_accessible(effective_type_mask(binder.model(), &return_ty), member_mask)
                {
                    binder.report(Diagnostic::new(
                        DiagnosticKind::InconsistentAccessibility {
                            position: SignaturePosition::OperatorReturnType,
                            type_name: return_ty.to_string().into(),
                            member: signature.clone().into(),
                        },
                        *span,
                    ));
                }
                for parameter in parameters {
                    let param_ty = binder.canonicalize(&bind_type(&parameter.ty));
                    if exposes_less_accessible(effective_type_mask(binder.model(), &param_ty), member_mask)
                    {
                        binder.report(Diagnostic::new(
                            DiagnosticKind::InconsistentAccessibility {
                                position: SignaturePosition::OperatorParameterType,
                                type_name: param_ty.to_string().into(),
                                member: signature.clone().into(),
                            },
                            *span,
                        ));
                    }
                }
            }
            Member::ConversionOperator {
                direction,
                target,
                parameters,
                span,
                ..
            } => {
                let target_ty = binder.canonicalize(&bind_type(target));
                let keyword = match direction {
                    ConversionDirection::Implicit => "implicit",
                    ConversionDirection::Explicit => "explicit",
                };
                let signature = alloc::format!(
                    "{}.{} operator {}({})",
                    declaration.name,
                    keyword,
                    target_ty,
                    parameter_type_list(parameters)
                );
                if exposes_less_accessible(effective_type_mask(binder.model(), &target_ty), member_mask)
                {
                    binder.report(Diagnostic::new(
                        DiagnosticKind::InconsistentAccessibility {
                            position: SignaturePosition::OperatorReturnType,
                            type_name: target_ty.to_string().into(),
                            member: signature.clone().into(),
                        },
                        *span,
                    ));
                }
                for parameter in parameters {
                    let param_ty = binder.canonicalize(&bind_type(&parameter.ty));
                    if exposes_less_accessible(effective_type_mask(binder.model(), &param_ty), member_mask)
                    {
                        binder.report(Diagnostic::new(
                            DiagnosticKind::InconsistentAccessibility {
                                position: SignaturePosition::OperatorParameterType,
                                type_name: param_ty.to_string().into(),
                                member: signature.clone().into(),
                            },
                            *span,
                        ));
                    }
                }
            }
            _ => {}
        }
    }
    for member in &declaration.members {
        if let Member::Field {
            modifiers,
            declarators,
            ..
        } = member
        {
            if modifiers
                .iter()
                .any(|modifier| matches!(modifier, Modifier::Const))
            {
                for declarator in declarators {
                    if declarator.initializer.is_none() {
                        binder.report(Diagnostic::new(
                            DiagnosticKind::ConstFieldRequiresValue,
                            declarator.span,
                        ));
                    }
                }
            }
        }
    }
    if declaration.kind == TypeKind::Interface {
        for member in &declaration.members {
            if let Member::Field {
                modifiers,
                declarators,
                ..
            } = member
            {
                if !modifiers
                    .iter()
                    .any(|modifier| matches!(modifier, Modifier::Const))
                {
                    for declarator in declarators {
                        binder.report(Diagnostic::new(
                            DiagnosticKind::InterfaceCannotContainInstanceField,
                            declarator.span,
                        ));
                    }
                }
            }
        }
    }
    binder.enter_type(enclosing.clone());
    for member in &declaration.members {
        match member {
            Member::Field { ty, .. } | Member::Indexer { ty, .. } => {
                binder.resolve_named_type(&bind_type(ty), ty.span);
            }
            _ => {}
        }
        match member {
            Member::Method { parameters, .. }
            | Member::Constructor { parameters, .. }
            | Member::Operator { parameters, .. }
            | Member::ConversionOperator { parameters, .. }
            | Member::Indexer { parameters, .. } => {
                for parameter in parameters {
                    binder.resolve_named_type(&bind_type(&parameter.ty), parameter.ty.span);
                }
            }
            _ => {}
        }
    }
    binder.exit_type();
    for member in &declaration.members {
        match member {
            Member::Method {
                modifiers,
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
                    &out_parameter_names(parameters),
                    is_static_member(modifiers),
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
                    &[],
                    true,
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
                    &[],
                    true,
                    body,
                );
            }
            Member::Constructor {
                modifiers,
                parameters,
                body,
                ..
            } => {
                let params = bound_parameters(parameters);
                binder.bind_method(
                    Some(enclosing.clone()),
                    ".ctor",
                    TypeSymbol::Special(SpecialType::Void),
                    &params,
                    &out_parameter_names(parameters),
                    is_static_member(modifiers),
                    body,
                );
            }
            Member::Property {
                modifiers,
                ty,
                name,
                getter,
                setter,
                ..
            } => {
                let property_ty = bind_type(ty);
                let is_static = is_static_member(modifiers);
                if let Some(body) = getter.as_ref().and_then(|accessor| accessor.body.as_ref()) {
                    binder.bind_method(
                        Some(enclosing.clone()),
                        &accessor_name("get_", name),
                        property_ty.clone(),
                        &[],
                        &[],
                        is_static,
                        body,
                    );
                }
                if let Some(body) = setter.as_ref().and_then(|accessor| accessor.body.as_ref()) {
                    binder.bind_method(
                        Some(enclosing.clone()),
                        &accessor_name("set_", name),
                        TypeSymbol::Special(SpecialType::Void),
                        &[(Box::from("value"), property_ty.clone())],
                        &[],
                        is_static,
                        body,
                    );
                }
            }
            Member::Indexer {
                ty,
                parameters,
                getter,
                setter,
                ..
            } => {
                let element = bind_type(ty);
                let indices = bound_parameters(parameters);
                if let Some(body) = getter.as_ref().and_then(|accessor| accessor.body.as_ref()) {
                    binder.bind_method(
                        Some(enclosing.clone()),
                        "get_Item",
                        element.clone(),
                        &indices,
                        &[],
                        false,
                        body,
                    );
                }
                if let Some(body) = setter.as_ref().and_then(|accessor| accessor.body.as_ref()) {
                    let mut indices = indices.clone();
                    indices.push((Box::from("value"), element.clone()));
                    binder.bind_method(
                        Some(enclosing.clone()),
                        "set_Item",
                        TypeSymbol::Special(SpecialType::Void),
                        &indices,
                        &[],
                        false,
                        body,
                    );
                }
            }
            Member::Field {
                ty,
                declarators,
                modifiers,
                ..
            } => {
                let field_ty = binder.canonicalize(&bind_type(ty));
                let is_const = modifiers.iter().any(|m| matches!(m, Modifier::Const));
                for declarator in declarators {
                    let is_candidate = !is_const
                        && !field_ty.is_void()
                        && declarator.name != declaration.name
                        && binder
                            .model()
                            .get_by_symbol(&enclosing)
                            .and_then(|info| info.find_field(&declarator.name))
                            .is_some_and(|field| field.accessibility == Accessibility::Private);
                    if is_candidate {
                        let eligible_never_used = !is_const
                            && type_is_resolvable(binder.model(), &field_ty)
                            && declarator.initializer.is_none()
                            && !duplicate_field_names.contains(&*declarator.name);
                        let default_value = default_value_string(binder.model(), &field_ty);
                        binder.record_private_field(
                            &enclosing,
                            &declarator.name,
                            declarator.span,
                            eligible_never_used,
                            default_value,
                        );
                    }
                    if let Some(initializer) = &declarator.initializer {
                        binder.bind_field_initializer(
                            enclosing.clone(),
                            &declarator.name,
                            &field_ty,
                            initializer,
                        );
                    }
                }
            }
            Member::Destructor { body, .. } => {
                binder.bind_method(
                    Some(enclosing.clone()),
                    "Finalize",
                    TypeSymbol::Special(SpecialType::Void),
                    &[],
                    &[],
                    false,
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
    match declaration.kind {
        TypeKind::Interface => binder.check_interface_cycle(&enclosing, declaration),
        TypeKind::Struct => binder.check_struct_layout_cycle(&enclosing, declaration),
        _ => {}
    }
    check_constant_cycles(binder, declaration);
    binder.check_interface_implementations(&enclosing, declaration);
    binder.check_overrides_have_base(&enclosing, declaration);
    binder.check_abstract_implementations(&enclosing, declaration);
}

/// CS0110: reports a const field whose value evaluation is circular. The declaration-order fold
/// (`const_field_literal`) leaves a cyclic const unresolved rather than looping the compiler, so
/// the cycle is found here from the const-reference graph: each const's initializer contributes an
/// edge to every same-type const it names, and a const that reaches itself is circular. One
/// diagnostic is emitted per cycle, at its earliest-declared member (matching csc).
fn check_constant_cycles(binder: &mut Binder, declaration: &TypeDecl) {
    use alloc::collections::{BTreeMap, BTreeSet};
    let mut const_names: BTreeSet<Box<str>> = BTreeSet::new();
    for member in &declaration.members {
        if let Member::Field {
            modifiers,
            declarators,
            ..
        } = member
        {
            if modifiers.iter().any(|m| matches!(m, Modifier::Const)) {
                for declarator in declarators {
                    const_names.insert(declarator.name.clone());
                }
            }
        }
    }
    if const_names.is_empty() {
        return;
    }
    let mut edges: BTreeMap<Box<str>, Vec<Box<str>>> = BTreeMap::new();
    let mut order: Vec<(Box<str>, Span)> = Vec::new();
    for member in &declaration.members {
        if let Member::Field {
            modifiers,
            declarators,
            ..
        } = member
        {
            if !modifiers.iter().any(|m| matches!(m, Modifier::Const)) {
                continue;
            }
            for declarator in declarators {
                let mut refs = Vec::new();
                if let Some(init) = &declarator.initializer {
                    crate::declaration::const_expr_references(init, &mut refs);
                }
                refs.retain(|name| const_names.contains(name));
                edges.insert(declarator.name.clone(), refs);
                order.push((declarator.name.clone(), declarator.span));
            }
        }
    }
    fn reaches(
        from: &str,
        to: &str,
        edges: &BTreeMap<Box<str>, Vec<Box<str>>>,
        visited: &mut BTreeSet<Box<str>>,
    ) -> bool {
        let Some(deps) = edges.get(from) else {
            return false;
        };
        for dep in deps {
            if dep.as_ref() == to {
                return true;
            }
            if visited.insert(dep.clone()) && reaches(dep, to, edges, visited) {
                return true;
            }
        }
        false
    }
    let mut reported: BTreeSet<Box<str>> = BTreeSet::new();
    for (name, span) in &order {
        if reported.contains(name) {
            continue;
        }
        let mut seen = BTreeSet::new();
        if !reaches(name, name, &edges, &mut seen) {
            continue;
        }
        binder.report(Diagnostic::new(
            DiagnosticKind::CircularConstant {
                member: alloc::format!("{}.{}", declaration.name, name).into(),
            },
            *span,
        ));
        for (other, _) in &order {
            let mut forward = BTreeSet::new();
            let mut backward = BTreeSet::new();
            if reaches(name, other, &edges, &mut forward)
                && reaches(other, name, &edges, &mut backward)
            {
                reported.insert(other.clone());
            }
        }
    }
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

/// The names of the `out` parameters in a list. An out parameter starts unassigned and must be
/// assigned before control leaves the method (CS0177), unlike an ordinary by-value parameter.
fn out_parameter_names(parameters: &[lamella_syntax::ast::Parameter]) -> Vec<Box<str>> {
    parameters
        .iter()
        .filter(|parameter| {
            parameter.modifier == Some(lamella_syntax::ast::ParameterModifier::Out)
        })
        .map(|parameter| parameter.name.clone())
        .collect()
}

/// Whether a member's modifiers declare it `static` -- so its body has no `this`, and an
/// unqualified instance-member access or the `this` keyword inside it is `CS0120`/`CS0026`.
fn is_static_member(modifiers: &[lamella_syntax::ast::Modifier]) -> bool {
    modifiers
        .iter()
        .any(|modifier| matches!(modifier, lamella_syntax::ast::Modifier::Static))
}

/// A named-type symbol from a namespace (empty or dotted) and a simple name; a
/// `System` built-in folds to its special form.
/// The qualified signature csc names a method by in a member diagnostic: `Type.Method(paramtypes)`.
/// Reports the struct/sealed member-modifier errors CS0106 / CS0238 / CS0666 for one member.
/// csc reports at most one, in this precedence: a struct member marked `virtual`/`abstract`
/// with any explicit accessibility is CS0106 (a private one is CS0621, reported by the
/// abstract/virtual-soundness pass); a `sealed` member that is not an `override` is CS0238, in
/// a class or a struct; a struct's `protected` (or `protected internal`) member is CS0666.
/// `display` is the member's qualified name/signature, used by CS0238 and CS0666.
fn check_member_modifier_validity(
    binder: &mut Binder,
    is_struct: bool,
    modifiers: &[Modifier],
    display: &str,
    span: Span,
) {
    let has = |target: fn(&Modifier) -> bool| modifiers.iter().any(target);
    let is_abstract = has(|m| matches!(m, Modifier::Abstract));
    let is_virtual = has(|m| matches!(m, Modifier::Virtual));
    let effectively_private = !has(|m| {
        matches!(
            m,
            Modifier::Public | Modifier::Protected | Modifier::Internal
        )
    });
    if is_struct && (is_virtual || is_abstract) && !effectively_private {
        let modifier = if is_abstract { "abstract" } else { "virtual" };
        binder.report(Diagnostic::new(
            DiagnosticKind::ModifierNotValidForItem {
                modifier: modifier.into(),
            },
            span,
        ));
        return;
    }
    if has(|m| matches!(m, Modifier::Sealed))
        && !has(|m| matches!(m, Modifier::Override))
        && !is_virtual
        && !is_abstract
    {
        binder.report(Diagnostic::new(
            DiagnosticKind::SealedMemberIsNotOverride {
                member: display.into(),
            },
            span,
        ));
        return;
    }
    if is_struct && has(|m| matches!(m, Modifier::Protected)) {
        binder.report(Diagnostic::new(
            DiagnosticKind::ProtectedMemberInStruct {
                member: display.into(),
            },
            span,
        ));
    }
}

/// Reports CS0231 / CS0225 for a `params` parameter that is not the last in its list, or is not a
/// single-dimensional array (17.5.1.4) -- the trailing single-dimensional array is the only valid
/// C# 1.0 `params` form.
fn check_params_usage(binder: &mut Binder, parameters: &[Parameter]) {
    for (index, parameter) in parameters.iter().enumerate() {
        if !matches!(parameter.modifier, Some(ParameterModifier::Params)) {
            continue;
        }
        if index + 1 != parameters.len() {
            binder.report(Diagnostic::new(DiagnosticKind::ParamsNotLast, parameter.span));
        } else if !matches!(parameter.ty.kind, TypeRefKind::Array { rank: 1, .. }) {
            binder.report(Diagnostic::new(DiagnosticKind::ParamsNotArray, parameter.span));
        }
    }
}

/// Reports CS0542 when a member's name repeats its enclosing type's -- illegal for every member
/// except a constructor (a destructor and an indexer have no simple name to collide). `type_name`
/// is the enclosing type; `name`/`span` the member.
fn check_member_name_vs_type(binder: &mut Binder, type_name: &str, name: &str, span: Span) {
    if name == type_name {
        binder.report(Diagnostic::new(
            DiagnosticKind::MemberNamedLikeType {
                type_name: type_name.into(),
            },
            span,
        ));
    }
}

/// The simple name and span of a type nested in another type (class/struct/interface/enum/
/// delegate), for the CS0542 member-name check. A nested namespace cannot occur (grammar).
fn nested_type_name_span(member: &NamespaceMember) -> Option<(&str, Span)> {
    match member {
        NamespaceMember::Type(declaration) => Some((&declaration.name, declaration.span)),
        NamespaceMember::Enum(declaration) => Some((&declaration.name, declaration.span)),
        NamespaceMember::Delegate(declaration) => Some((&declaration.name, declaration.span)),
        NamespaceMember::Namespace(_) => None,
    }
}

pub(crate) fn method_signature(type_name: &str, method: &str, parameters: &[Parameter]) -> Box<str> {
    let mut signature = String::from(type_name);
    signature.push('.');
    signature.push_str(method);
    signature.push('(');
    for (index, parameter) in parameters.iter().enumerate() {
        if index > 0 {
            signature.push_str(", ");
        }
        signature.push_str(&alloc::format!("{}", parameter_symbol(parameter)));
    }
    signature.push(')');
    signature.into()
}

/// The comma-separated parameter-type list for an indexer/operator signature message (the `int` in
/// `C.this[int]`, the `C, int` in `C.operator +(C, int)`). Uses `parameter_symbol`, so a nested
/// type is under-qualified exactly like a CS0051 method signature.
fn parameter_type_list(parameters: &[Parameter]) -> String {
    let mut list = String::new();
    for (index, parameter) in parameters.iter().enumerate() {
        if index > 0 {
            list.push_str(", ");
        }
        list.push_str(&alloc::format!("{}", parameter_symbol(parameter)));
    }
    list
}

/// The SOURCE symbol of a user-defined operator (`+`, `==`, `true`, ...) for a CS0056/CS0057
/// signature message (`C.operator +(C, int)`), distinct from its `op_*` metadata name.
fn operator_source_symbol(operator: OverloadableOperator) -> &'static str {
    use OverloadableOperator as O;
    match operator {
        O::Plus => "+",
        O::Minus => "-",
        O::LogicalNot => "!",
        O::BitwiseNot => "~",
        O::Increment => "++",
        O::Decrement => "--",
        O::True => "true",
        O::False => "false",
        O::Multiply => "*",
        O::Divide => "/",
        O::Remainder => "%",
        O::BitwiseAnd => "&",
        O::BitwiseOr => "|",
        O::ExclusiveOr => "^",
        O::LeftShift => "<<",
        O::RightShift => ">>",
        O::Equality => "==",
        O::Inequality => "!=",
        O::GreaterThan => ">",
        O::LessThan => "<",
        O::GreaterThanOrEqual => ">=",
        O::LessThanOrEqual => "<=",
    }
}

fn named_symbol(namespace: &str, name: &str) -> TypeSymbol {
    let mut parts: Vec<Box<str>> = Vec::new();
    if !namespace.is_empty() {
        for part in namespace.split('.') {
            parts.push(part.into());
        }
    }
    parts.push(name.into());
    TypeSymbol::Named(parts.into_boxed_slice()).fold_builtin()
}

/// Whether a (canonicalized) type resolves to a known type: a keyword/special type, a type in
/// the model, or an array/pointer whose element does. An unresolved named type -- e.g. an
/// undefined `Widget` -- is `Named` (not the `Error` sentinel), so it needs this model check.
/// Used to gate CS0169, which csc suppresses when a field's type does not resolve (CS0246).
/// The default value of a field's type as csc renders it in the CS0649 message: `0` for a numeric
/// type, `false` for `bool`, `null` for a reference type (string, class, interface, delegate,
/// array), and empty for a `char`, an enum, or a struct.
fn default_value_string(model: &Model, ty: &TypeSymbol) -> Box<str> {
    let value = match ty {
        TypeSymbol::Special(special) => {
            if matches!(special, SpecialType::Boolean) {
                "false"
            } else if matches!(special, SpecialType::Char) {
                ""
            } else if special.is_numeric() {
                "0"
            } else if matches!(special, SpecialType::String | SpecialType::Object) {
                "null"
            } else {
                ""
            }
        }
        TypeSymbol::Array { .. } => "null",
        TypeSymbol::Named(_) => match model.get_by_symbol(ty).map(|info| info.kind) {
            Some(crate::symbols::TypeKind::Struct | crate::symbols::TypeKind::Enum) => "",
            Some(_) => "null",
            None => "",
        },
        _ => "",
    };
    value.into()
}

/// The accessibility DOMAIN (10.5.3) as a bitmask of the outer contexts a type or member reaches
/// (the declaring type itself always reaches it, so it needs no bit): bit 0 = a derived type in
/// this assembly, bit 1 = a derived type in another assembly, bit 2 = a non-derived type in this
/// assembly, bit 3 = a non-derived type in another assembly. Intersection -- down a nesting chain,
/// and to combine a member with its container -- is `&`; "at least as accessible" is bucket-superset
/// (`a & b == b`). This encodes the `protected`/`internal` incomparability exactly: their masks
/// (`0b0011`, `0b0101`) share only the derived-in-this-assembly bit, so neither covers the other.
const ACCESS_FULL: u8 = 0b1111;

/// The accessibility domain mask of a single declared accessibility.
fn access_mask(accessibility: Accessibility) -> u8 {
    match accessibility {
        Accessibility::Public => ACCESS_FULL,
        Accessibility::ProtectedInternal => 0b0111,
        Accessibility::Protected => 0b0011,
        Accessibility::Internal => 0b0101,
        Accessibility::Private => 0b0000,
    }
}

/// Whether a signature type (`exposed`) is NOT at least as accessible as the member exposing it
/// (`member`) -- its domain misses a context the member reaches, so the member could hand a consumer
/// a type it cannot name and the accessibility-consistency diagnostic fires (10.5.4).
fn exposes_less_accessible(exposed: u8, member: u8) -> bool {
    exposed & member != member
}

/// The effective accessibility (domain mask) of a type: its own declared accessibility intersected
/// down its nesting chain. A predefined, unresolved, reference, or error type is treated as fully
/// public (a safe under-report -- it never makes a signature look less accessible than it is); an
/// array, pointer, or byref is as accessible as its element.
fn effective_type_mask(model: &Model, ty: &TypeSymbol) -> u8 {
    match ty {
        TypeSymbol::Special(_) | TypeSymbol::Error => ACCESS_FULL,
        TypeSymbol::Array { element, .. }
        | TypeSymbol::Pointer(element)
        | TypeSymbol::ByRef(element) => effective_type_mask(model, element),
        TypeSymbol::Named(_) => model
            .get_by_symbol(ty)
            .map_or(ACCESS_FULL, |info| effective_info_mask(model, info)),
    }
}

/// The effective accessibility of a resolved type: its own accessibility intersected with its
/// enclosing type's effective accessibility, walking the nesting chain outward.
fn effective_info_mask(model: &Model, info: &TypeInfo) -> u8 {
    let own = access_mask(info.accessibility);
    match &info.enclosing {
        None => own,
        Some(enclosing) => own & enclosing_mask(model, enclosing),
    }
}

/// The effective accessibility of the type named by a nested type's `enclosing` full name (e.g.
/// `"N.Outer"`), found by splitting off the last name segment. An unresolved enclosing name imposes
/// no restriction (a safe under-report).
fn enclosing_mask(model: &Model, enclosing: &str) -> u8 {
    let info = match enclosing.rfind('.') {
        Some(dot) => model.get(&enclosing[..dot], &enclosing[dot + 1..]),
        None => model.get("", enclosing),
    };
    info.map_or(ACCESS_FULL, |info| effective_info_mask(model, info))
}

fn type_is_resolvable(model: &Model, ty: &TypeSymbol) -> bool {
    match ty {
        TypeSymbol::Special(_) => true,
        TypeSymbol::Named(_) => model.get_by_symbol(ty).is_some(),
        TypeSymbol::Array { element, .. }
        | TypeSymbol::Pointer(element)
        | TypeSymbol::ByRef(element) => type_is_resolvable(model, element),
        TypeSymbol::Error => false,
    }
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
        assert_eq!(
            sorted_codes("class C { int x; int y; int Sum() { x = 1; y = 2; return x + y; } }"),
            []
        );
    }

    #[test]
    fn field_never_used_is_cs0169() {
        assert_eq!(sorted_codes("class C { private int f; }"), [169]);
        assert_eq!(sorted_codes("class C { int f; }"), [169]);
        assert_eq!(sorted_codes("class C { int f; int Get() { return f; } }"), [649]);
        assert_eq!(sorted_codes("class C { int f; void Set() { f = 1; } }"), [414]);
        assert_eq!(sorted_codes("class C { public int f; }"), []);
        assert_eq!(sorted_codes("class C { const int F = 1; }"), []);
        assert_eq!(sorted_codes("class C { const int F; }"), [145]);
        assert_eq!(sorted_codes("class C { int f = 5; }"), [414]);
        assert_eq!(sorted_codes("class C { Widget w; }"), [246]);
        assert_eq!(sorted_codes("class C { int f; int f; }"), [102]);
    }

    #[test]
    fn struct_virtual_or_abstract_member_is_cs0106() {
        assert_eq!(sorted_codes("struct S { public virtual void M() { } }"), [106]);
        assert_eq!(sorted_codes("struct S { public abstract void M(); }"), [106]);
        assert_eq!(
            sorted_codes("struct S { public virtual int P { get { return 0; } } }"),
            [106]
        );
        assert_eq!(sorted_codes("struct S { protected virtual void M() { } }"), [106]);
        assert_eq!(sorted_codes("struct S { public static void M() { } }"), []);
        assert_eq!(sorted_codes("class C { public virtual void M() { } }"), []);
    }

    #[test]
    fn sealed_non_override_member_is_cs0238() {
        assert_eq!(sorted_codes("class C { public sealed void M() { } }"), [238]);
        assert_eq!(sorted_codes("struct S { public sealed void M() { } }"), [238]);
        assert_eq!(
            sorted_codes("class C { public sealed int P { get { return 0; } } }"),
            [238]
        );
        assert_eq!(sorted_codes("class C { public void M() { } }"), []);
    }

    #[test]
    fn protected_member_in_struct_is_cs0666() {
        assert_eq!(sorted_codes("struct S { protected int x; }"), [666]);
        assert_eq!(sorted_codes("struct S { protected internal int x; }"), [666]);
        assert_eq!(sorted_codes("struct S { protected void M() { } }"), [666]);
        assert_eq!(
            sorted_codes("struct S { protected int P { get { return 0; } } }"),
            [666]
        );
        assert_eq!(sorted_codes("struct S { protected const int X = 1; }"), [666]);
        assert_eq!(
            sorted_codes("class C { protected int x; int Get() { return x; } }"),
            []
        );
    }

    #[test]
    fn constant_overflow_in_checked_context_is_cs0220() {
        assert_eq!(sorted_codes("class C { static int M() { return 2147483647 + 1; } }"), [220]);
        assert_eq!(sorted_codes("class C { static int M() { return 100000 * 100000; } }"), [220]);
        assert_eq!(sorted_codes("class C { static int M() { return -2147483648 - 1; } }"), [220]);
        assert_eq!(
            sorted_codes("class C { static long M() { return 9223372036854775807 + 1; } }"),
            [220]
        );
        assert_eq!(
            sorted_codes("class C { static int M() { checked { return 2147483647 + 1; } } }"),
            [220]
        );
        assert_eq!(
            sorted_codes("class C { static int M() { unchecked { return 2147483647 + 1; } } }"),
            []
        );
        assert_eq!(
            sorted_codes("class C { static int M() { return unchecked(2147483647 + 1); } }"),
            []
        );
        assert_eq!(sorted_codes("class C { static int M() { return 2000000000 + 100; } }"), []);
        assert_eq!(sorted_codes("class C { static byte M() { return 200 + 100; } }"), [31]);
    }

    #[test]
    fn member_named_like_type_is_cs0542() {
        assert_eq!(sorted_codes("class C { int C() { return 0; } }"), [542]);
        assert_eq!(sorted_codes("class C { int C { get { return 0; } } }"), [542]);
        assert_eq!(sorted_codes("class C { const int C = 1; }"), [542]);
        assert_eq!(sorted_codes("class C { int C; }"), [542]);
        assert_eq!(sorted_codes("class C { class C {} }"), [542]);
        assert_eq!(sorted_codes("class C { public C() {} }"), []);
    }

    #[test]
    fn static_constructor_with_parameters_is_cs0132() {
        assert_eq!(sorted_codes("class C { static C(int x) {} }"), [132]);
        assert_eq!(sorted_codes("class C { static C() {} public C(int x) {} }"), []);
    }

    #[test]
    fn void_field_is_cs0670() {
        assert_eq!(sorted_codes("class C { void x; }"), [670]);
    }

    #[test]
    fn field_read_but_never_assigned_is_cs0649() {
        assert_eq!(sorted_codes("class C { private int x; int Get() { return x; } }"), [649]);
        assert_eq!(sorted_codes("class C { string s; string Get() { return s; } }"), [649]);
        assert_eq!(sorted_codes("class C { int x = 5; int Get() { return x; } }"), []);
        assert_eq!(
            sorted_codes("class C { int x; void Set() { x = 1; } int Get() { return x; } }"),
            []
        );
        assert_eq!(sorted_codes("class C { int x; }"), [169]);
    }

    #[test]
    fn inconsistent_accessibility_is_cs0050_to_cs0053() {
        assert_eq!(
            sorted_codes("public class C { private class Priv {} public Priv Get() { return null; } }"),
            [50]
        );
        assert_eq!(
            sorted_codes("public class C { private class Priv {} public void M(Priv p) {} }"),
            [51]
        );
        assert_eq!(
            sorted_codes("public class C { private class Priv {} public Priv f; }"),
            [52]
        );
        assert_eq!(
            sorted_codes("public class C { private class Priv {} public Priv P { get { return null; } } }"),
            [53]
        );
        assert_eq!(sorted_codes("internal class Base {} public class C : Base {}"), [60]);
        assert_eq!(
            sorted_codes("public class C { private class Priv {} public C(Priv p) {} }"),
            [51]
        );
        assert_eq!(sorted_codes("public class Base {} public class C : Base {}"), []);
        assert_eq!(
            sorted_codes("public class C { public class Pub {} public Pub Get() { return null; } }"),
            []
        );
        assert_eq!(sorted_codes("public class C { public int Get() { return 0; } }"), []);
        assert_eq!(
            sorted_codes("public class C { private class Priv {} private Priv Get() { return null; } }"),
            []
        );
    }

    #[test]
    fn inconsistent_accessibility_lattice() {
        assert_eq!(
            sorted_codes("public class C { protected class P {} internal void M(P x) {} }"),
            [51]
        );
        assert_eq!(
            sorted_codes("public class C { internal class I {} protected void M(I x) {} }"),
            [51]
        );
        assert_eq!(
            sorted_codes("public class C { internal class I {} protected internal void M(I x) {} }"),
            [51]
        );
        assert_eq!(
            sorted_codes("public class C { internal class I {} internal void M(I x) {} }"),
            []
        );
        assert_eq!(
            sorted_codes("public class C { protected class P {} protected void M(P x) {} }"),
            []
        );
        assert_eq!(
            sorted_codes("public class C { internal class I {} private void M(I x) {} }"),
            []
        );
        assert_eq!(
            sorted_codes(
                "public class Outer { private class S {} public class Pub { public void M(S x) {} } }"
            ),
            [51]
        );
        assert_eq!(
            sorted_codes("internal class T { private class H {} public void M(H x) {} }"),
            [51]
        );
        assert_eq!(
            sorted_codes("internal class T { public class Pub {} public Pub M() { return null; } }"),
            []
        );
    }

    #[test]
    fn top_level_type_defaults_to_internal_accessibility() {
        assert_eq!(
            sorted_codes("class Plain {} public class C { public Plain M() { return null; } }"),
            [50]
        );
        assert_eq!(
            sorted_codes("class Plain {} internal class D { public Plain M() { return null; } }"),
            []
        );
        assert_eq!(
            sorted_codes("internal delegate void H(); public class C { public H f; }"),
            [52]
        );
        assert_eq!(
            sorted_codes("internal enum E { A } public class C { public E f; }"),
            [52]
        );
        assert_eq!(
            sorted_codes("public delegate void H(); public class C { public H f; }"),
            []
        );
    }

    #[test]
    fn circular_type_dependencies_are_detected_not_looped() {
        assert_eq!(sorted_codes("class A : B {} class B : A {}"), [146, 146]);
        assert_eq!(sorted_codes("interface I : J {} interface J : I {}"), [529, 529]);
        assert_eq!(sorted_codes("struct S { public S f; }"), [523]);
        assert_eq!(
            sorted_codes("struct A { public B b; } struct B { public A a; }"),
            [523, 523]
        );
        assert_eq!(
            sorted_codes("interface I : J {} interface J {} class C : I {}"),
            []
        );
        assert_eq!(
            sorted_codes("struct Inner {} struct Outer { public Inner x; }"),
            []
        );
        assert_eq!(sorted_codes("struct S { public static S s; }"), []);
    }

    #[test]
    fn circular_constants_are_cs0110_once_per_cycle() {
        assert_eq!(
            sorted_codes("class C { const int A = B; const int B = A; }"),
            [110]
        );
        assert_eq!(sorted_codes("class C { const int A = A; }"), [110]);
        assert_eq!(
            sorted_codes("class C { const int A = B; const int B = D; const int D = A; }"),
            [110]
        );
        assert_eq!(
            sorted_codes("class C { const int B = A; const int A = 5; }"),
            []
        );
        assert_eq!(
            sorted_codes("class C { const int A = 5; const int B = A + 1; }"),
            []
        );
        assert_eq!(sorted_codes("class C { const int Unused = 7; }"), []);
    }

    #[test]
    fn static_member_through_explicit_this_is_cs0176() {
        assert_eq!(
            sorted_codes("class C { static int P { get { return 0; } } int M() { return this.P; } }"),
            [176]
        );
        assert_eq!(
            sorted_codes("class C { int P { get { return 0; } } int M() { return this.P; } }"),
            []
        );
        assert_eq!(
            sorted_codes("class C { static int P { get { return 0; } } int M() { return P; } }"),
            []
        );
        assert_eq!(
            sorted_codes("class C { static int S() { return 0; } int M() { return this.S(); } }"),
            [176]
        );
        assert_eq!(
            sorted_codes("class C { static int S() { return 0; } int M() { return S(); } }"),
            []
        );
        assert_eq!(
            sorted_codes("class C { int I() { return 0; } int M() { return this.I(); } }"),
            []
        );
        assert_eq!(
            sorted_codes(
                "class B { public static int S() { return 0; } } \
                 class C : B { int M() { return base.S(); } }"
            ),
            [176]
        );
    }

    #[test]
    fn inconsistent_accessibility_indexer_operator() {
        assert_eq!(
            sorted_codes("public class C { private class Sec {} public Sec this[int i] { get { return null; } } }"),
            [54]
        );
        assert_eq!(
            sorted_codes("public class C { private class Sec {} public int this[Sec s] { get { return 0; } } }"),
            [55]
        );
        assert_eq!(
            sorted_codes("public class C { private class Sec {} public static Sec operator +(C a, int b) { return null; } }"),
            [56]
        );
        assert_eq!(
            sorted_codes("public class C { private class Sec {} public static C operator +(C a, Sec b) { return null; } }"),
            [57]
        );
        assert_eq!(
            sorted_codes("public class C { private class Sec {} public static implicit operator Sec(C c) { return null; } }"),
            [56]
        );
        assert_eq!(
            sorted_codes("public class C { public class Pub {} public Pub this[int i] { get { return null; } } }"),
            []
        );
        assert_eq!(
            sorted_codes("public class C { public static C operator +(C a, C b) { return null; } }"),
            []
        );
    }

    #[test]
    fn inconsistent_accessibility_delegate_event_interface() {
        assert_eq!(sorted_codes("internal class S {} public delegate S D();"), [58]);
        assert_eq!(
            sorted_codes("internal class S {} public delegate void D(S x);"),
            [59]
        );
        assert_eq!(sorted_codes("internal class S {} internal delegate S D();"), []);
        assert_eq!(
            sorted_codes("internal delegate void H(); public class C { public event H E; }"),
            [7025]
        );
        assert_eq!(
            sorted_codes("internal interface I {} public interface J : I {}"),
            [61]
        );
        assert_eq!(
            sorted_codes("public interface I {} public interface J : I {}"),
            []
        );
        assert_eq!(
            sorted_codes("internal class Sec {} public interface I { void M(Sec s); }"),
            [51]
        );
        assert_eq!(
            sorted_codes("internal class Sec {} internal interface I { void M(Sec s); }"),
            []
        );
    }

    #[test]
    fn params_misuse_is_cs0225_or_cs0231() {
        assert_eq!(sorted_codes("class C { void M(params int[] a, int b) {} }"), [231]);
        assert_eq!(sorted_codes("class C { void M(params int a) {} }"), [225]);
        assert_eq!(sorted_codes("class C { void M(params int[,] a) {} }"), [225]);
        assert_eq!(sorted_codes("class C { void M(int b, params int[] a) {} }"), []);
        assert_eq!(sorted_codes("class C { void M(params int[][] a) {} }"), []);
    }

    #[test]
    fn out_parameter_not_assigned_is_cs0177() {
        assert_eq!(sorted_codes("class C { static void M(out int x) { } }"), [177]);
        assert_eq!(sorted_codes("class C { static void M(out int x) { x = 5; } }"), []);
        assert_eq!(
            sorted_codes("class C { static void M(bool b, out int x) { if (b) x = 1; else x = 2; } }"),
            []
        );
        assert_eq!(
            sorted_codes("class C { static void M(bool b, out int x) { if (b) x = 5; } }"),
            [177]
        );
        assert_eq!(
            sorted_codes("class C { static void M(out int x) { throw null; } }"),
            []
        );
        assert_eq!(
            sorted_codes(
                "class C { static int M(bool b, out int x) { if (b) { x = 1; return 1; } return 2; } }"
            ),
            [177]
        );
    }

    #[test]
    fn goto_target_label_is_not_unreachable_cs0162() {
        assert_eq!(
            sorted_codes("class C { static int M() { goto done; done: return 0; } }"),
            []
        );
        assert_eq!(sorted_codes("class C { static void M() { return; return; } }"), [162]);
    }

    #[test]
    fn unused_local_value_is_cs0219_only_for_a_constant() {
        assert_eq!(sorted_codes("class C { void M() { int a = 5; } }"), [219]);
        assert_eq!(sorted_codes("class C { void M() { string s = \"x\"; } }"), [219]);
        assert_eq!(
            sorted_codes("class C { int S() { return 0; } void M() { int a = S(); } }"),
            []
        );
        assert_eq!(
            sorted_codes("class C { void M(int p) { int a = p + 1; } }"),
            []
        );
        assert_eq!(sorted_codes("class C { void M() { int a; } }"), [168]);
    }

    #[test]
    fn multiple_entry_points_is_cs0017() {
        assert_eq!(
            sorted_codes("class A { static void Main() {} } class B { static void Main() {} }"),
            [17]
        );
        assert_eq!(
            sorted_codes("class A { static void Main() {} static void Main(int x) {} }"),
            []
        );
        assert_eq!(
            sorted_codes("class A { void Main() {} } class B { static void Main() {} }"),
            []
        );
    }

    #[test]
    fn instance_member_in_static_method_is_cs0120() {
        assert_eq!(
            sorted_codes("class C { int x = 0; static int M() { return x; } }"),
            [120]
        );
        assert_eq!(
            sorted_codes("class C { void Foo() {} static void M() { Foo(); } }"),
            [120]
        );
        assert_eq!(
            sorted_codes("class C { int P { get { return 1; } } static int M() { return P; } }"),
            [120]
        );
        assert_eq!(
            sorted_codes(
                "class C { static int x = 0; static int Foo() { return 1; } \
                 static int M() { return x + Foo(); } }"
            ),
            []
        );
        assert_eq!(
            sorted_codes("class C { int x = 0; int M() { return x; } }"),
            []
        );
    }

    #[test]
    fn this_in_static_method_is_cs0026() {
        assert_eq!(
            sorted_codes("class C { static object M() { return this; } }"),
            [26]
        );
        assert_eq!(
            sorted_codes("class C { int x = 0; int R() { return x; } static int M() { return this.x; } }"),
            [26]
        );
        assert_eq!(
            sorted_codes("class C { int x = 0; int M() { return this.x; } }"),
            []
        );
    }

    #[test]
    fn undefined_type_in_declaration_position_is_cs0246() {
        assert_eq!(sorted_codes("class C { Widget w; }"), [246]);
        assert_eq!(sorted_codes("class C { void M(Widget w) {} }"), [246]);
        assert_eq!(
            sorted_codes(
                "namespace N { class Helper {} \
                 class C { Helper h = null; class Inner {} Inner i = null; \
                 Helper GetH() { return h; } Inner GetI() { return i; } \
                 void M(Helper x, Inner y) {} } }"
            ),
            []
        );
    }

    #[test]
    fn duplicate_type_in_namespace_is_cs0101() {
        assert_eq!(sorted_codes("class C {} class C {}"), [101]);
        assert_eq!(sorted_codes("class C {} struct C {}"), [101]);
        assert_eq!(sorted_codes("enum E { A } class E {}"), [101]);
        assert_eq!(sorted_codes("namespace N { class C {} class C {} }"), [101]);
        assert_eq!(
            sorted_codes("namespace N { class C {} } namespace N { class C {} }"),
            [101]
        );
        assert_eq!(
            sorted_codes("namespace A { class C {} } namespace B { class C {} }"),
            []
        );
    }

    #[test]
    fn void_as_operator_is_cs0039() {
        assert_eq!(
            sorted_codes("class P { static void V() {} static object M() { return V() as object; } }"),
            [39]
        );
        assert_eq!(
            sorted_codes("class P { static string M(object o) { return o as string; } }"),
            []
        );
    }

    #[test]
    fn out_of_range_constant_is_cs0031() {
        assert_eq!(sorted_codes("class C { byte M() { byte b = 256; return b; } }"), [31]);
        assert_eq!(sorted_codes("class C { short M() { short s = 100000; return s; } }"), [31]);
        assert_eq!(sorted_codes("class C { ulong M() { ulong u = -1; return u; } }"), [31]);
        assert_eq!(
            sorted_codes("class C { byte M() { byte b; b = 256; return b; } }"),
            [31]
        );
        assert_eq!(sorted_codes("class C { byte M() { byte b = 200; return b; } }"), []);
        assert_eq!(
            sorted_codes("class C { byte M(int p) { byte b = p; return b; } }"),
            [266]
        );
        assert_eq!(
            sorted_codes("class C { int M() { int i = 2147483648; return i; } }"),
            [266]
        );
    }

    #[test]
    fn binds_field_initializers() {
        let codes = sorted_codes("class C { public int x = \"s\"; public int y = 1; public long n = 2; }");
        assert_eq!(codes, [29]);
    }

    #[test]
    fn member_soundness_diagnostics() {
        assert_eq!(sorted_codes("class C { void M(); }"), [501]);
        assert_eq!(sorted_codes("struct S { void M(); }"), [501]);
        assert_eq!(
            sorted_codes("abstract class C { public abstract void M(); }"),
            []
        );
        assert_eq!(sorted_codes("class C { static extern int E(); }"), []);
        assert_eq!(sorted_codes("interface I { void M(); }"), []);
        assert_eq!(sorted_codes("class C { void M() {} }"), []);
        assert_eq!(sorted_codes("class C { const int X; }"), [145]);
        assert_eq!(sorted_codes("class C { const int X = 5; }"), []);
        assert_eq!(sorted_codes("interface I { int x; }"), [525]);
        assert_eq!(sorted_codes("interface I { const int X = 5; }"), []);
    }

    #[test]
    fn auto_implemented_property_is_cs8022() {
        assert_eq!(sorted_codes("class C { int P { get; set; } }"), [8022]);
        assert_eq!(sorted_codes("struct S { int P { get; } }"), [8022]);
        assert_eq!(
            sorted_codes("abstract class C { public abstract int P { get; set; } }"),
            []
        );
        assert_eq!(sorted_codes("interface I { int P { get; set; } }"), []);
        assert_eq!(sorted_codes("class C { extern int P { get; set; } }"), []);
        assert_eq!(
            sorted_codes("class C { int _f; int P { get { return _f; } set { _f = value; } } }"),
            []
        );
    }

    #[test]
    fn static_class_is_gated_cs8022() {
        assert_eq!(sorted_codes("static class C { }"), [8022]);
        assert_eq!(
            sorted_codes("static class C { public static int F() { return 1; } }"),
            [8022]
        );
        assert_eq!(sorted_codes("sealed class C { }"), []);
        assert_eq!(sorted_codes("abstract class C { }"), []);
    }

    #[test]
    fn abstract_and_virtual_member_soundness() {
        assert_eq!(sorted_codes("class C { public abstract void M(); }"), [513]);
        assert_eq!(
            sorted_codes("abstract class C { protected abstract void M(); }"),
            []
        );
        assert_eq!(sorted_codes("interface I { void M(); }"), []);
        assert_eq!(sorted_codes("class C { private virtual void M() {} }"), [621]);
        assert_eq!(sorted_codes("class C { virtual void M() {} }"), [621]);
        assert_eq!(sorted_codes("class C { abstract void M(); }"), [621]);
        assert_eq!(sorted_codes("class C { public virtual void M() {} }"), []);
        assert_eq!(sorted_codes("class C { protected virtual void M() {} }"), []);
        assert_eq!(sorted_codes("struct S { virtual void M() {} }"), [621]);
        assert_eq!(sorted_codes("struct S { abstract void M(); }"), [621]);
    }

    #[test]
    fn cs0534_unimplemented_inherited_abstract_member() {
        assert_eq!(
            sorted_codes("abstract class B { public abstract void M(); } class D : B {}"),
            [534]
        );
        assert_eq!(
            sorted_codes(
                "abstract class B { public abstract void M(); } \
                 class D : B { public override void M() {} }"
            ),
            []
        );
        assert_eq!(
            sorted_codes(
                "abstract class B { public abstract void M(); public abstract int N(int x); } \
                 class D : B {}"
            ),
            [534, 534]
        );
        assert_eq!(
            sorted_codes("abstract class B { public abstract void M(); } abstract class D : B {}"),
            []
        );
        assert_eq!(
            sorted_codes(
                "abstract class A { public abstract void M(); } \
                 class B : A { public override void M() {} } \
                 class C : B {}"
            ),
            []
        );
    }

    #[test]
    fn cs0115_override_matches_no_base_method() {
        use crate::symbols::{Accessibility, MethodSymbol, Model, TypeInfo, TypeKind};

        fn object_model() -> Model {
            let mut model = Model::new();
            let mut object = TypeInfo::new("System", "Object", TypeKind::Class);
            object.methods.push(MethodSymbol {
                name: "ToString".into(),
                return_type: TypeSymbol::Special(SpecialType::String),
                parameters: Vec::new(),
                is_static: false,
                is_params: false,
                is_virtual: true,
                is_abstract: false,
                is_override: false,
                accessibility: Accessibility::Public,
                conditional: Vec::new(),
            });
            model.insert(object);
            model
        }
        let codes = |unit: &str| {
            let unit = parse_compilation_unit(unit).unit;
            let mut codes: Vec<u16> = bind_compilation_unit_with_model(&unit, object_model())
                .iter()
                .map(Diagnostic::code)
                .collect();
            codes.sort_unstable();
            codes
        };

        assert_eq!(codes("class C { public override void M() {} }"), [115]);
        assert_eq!(
            codes(
                "class B { public virtual void M() {} } \
                 class D : B { public override void M() {} }"
            ),
            []
        );
        assert_eq!(
            codes("class C { public override string ToString() { return null; } }"),
            []
        );
        assert_eq!(
            codes(
                "class B { public virtual void M(int x) {} } \
                 class D : B { public override void M(string x) {} }"
            ),
            [115]
        );
    }

    #[test]
    fn assign_to_a_method_group_is_cs1656() {
        assert_eq!(
            sorted_codes("class C { void M() {} void S() { M = null; } }"),
            [1656]
        );
    }

    #[test]
    fn null_to_a_value_type_is_cs0037() {
        assert_eq!(sorted_codes("class C { int x = null; }"), [37]);
        assert_eq!(sorted_codes("struct P {} class C { public P p = null; }"), [37]);
        assert_eq!(sorted_codes("class C { public object o = null; }"), []);
        assert_eq!(sorted_codes("class C { public int x = \"s\"; }"), [29]);
    }

    #[test]
    fn a_type_used_as_a_value_is_cs0119() {
        assert_eq!(
            sorted_codes("class C { static int Run() { int y = C; return 0; } }"),
            [119]
        );
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
            is_virtual: false,
            is_abstract: false,
            is_override: false,
            accessibility: crate::symbols::Accessibility::Public,
            conditional: Vec::new(),
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
