//! Binding a whole compilation unit (ECMA-334 1st ed, clause 16).

use crate::bind::{bind_type, parameter_symbol};
use crate::bound::{Binder, literal_int_value};
use crate::declaration::{
    accessibility_of, collect_into, is_constant_form, model_const_values, qualified_type_name,
    resolve_const_expr,
};
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
    AttributeSection, CompilationUnit, ConversionDirection, DelegateDecl, EnumDecl, Member, Modifier,
    NamespaceMember, OverloadableOperator, Parameter, ParameterModifier, QualifiedName, TypeDecl,
    TypeKind, TypeRefKind, UsingDirective, UsingKind,
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
    bind_compilation_unit_with_references_and_options(unit, references, false)
}

/// Like [`bind_compilation_unit_with_references`], but told whether the driver's command line
/// OMITTED `/unsafe`, in which case any `unsafe` in the source is `CS0227`. Only a caller that
/// actually parsed options passes `true`; every other entry point leaves the policy unapplied,
/// because a compilation with no command line behind it has no option to have omitted.
#[must_use]
pub fn bind_compilation_unit_with_references_and_options(
    unit: &CompilationUnit,
    references: &[Assembly],
    unsafe_option_missing: bool,
) -> Vec<Diagnostic> {
    let mut model = Model::new();
    for reference in references {
        load_assembly(&mut model, reference);
    }
    collect_into(&mut model, unit);
    model.canonicalize_signatures();
    model.link_bases();
    let mut binder = Binder::with_model(model);
    binder.set_unsafe_option_missing(unsafe_option_missing);
    let mut declared_types: DeclaredTypes = DeclaredTypes::new();
    report_duplicate_types(&mut binder, &unit.members, "", &mut declared_types);
    bind_namespace_body(&mut binder, &unit.usings, &unit.members, "");
    report_multiple_entry_points(&mut binder);
    binder.report_unused_fields();
    binder.into_diagnostics()
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
                if let Some(outermost) = declaration.name.parts.first() {
                    declared
                        .entry(String::from(namespace))
                        .or_default()
                        .insert(outermost.clone());
                }
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
    bind_compilation_units_with_references_and_options(units, references, false)
}

/// Like [`bind_compilation_units_with_references`], but told whether the driver's command line
/// OMITTED `/unsafe` -- the multi-unit twin of
/// [`bind_compilation_unit_with_references_and_options`].
///
/// `/unsafe` is a capability boundary rather than a style flag, so it is enforced over the whole
/// compilation: every unit is bound with the same policy, and CS0227 does not depend on how many
/// source files a program is spread across.
#[must_use]
pub fn bind_compilation_units_with_references_and_options(
    units: &[CompilationUnit],
    references: &[Assembly],
    unsafe_option_missing: bool,
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
    binder.set_unsafe_option_missing(unsafe_option_missing);
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
    let mut aliases: alloc::collections::BTreeSet<&str> = alloc::collections::BTreeSet::new();
    for using in usings {
        match &using.kind {
            UsingKind::Namespace(name) => binder.import_namespace(&dotted(name)),
            UsingKind::Alias { name, target } => {
                if !aliases.insert(name) {
                    binder.report(Diagnostic::new(
                        DiagnosticKind::DuplicateUsingAlias { alias: name.clone() },
                        using.span,
                    ));
                }
                let aliased = TypeSymbol::Named(target.parts.iter().cloned().collect());
                binder.resolve_named_type(&aliased, target.span);
                binder.import_alias(name, aliased);
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
            NamespaceMember::Enum(declaration) => {
                validate_enum_members(binder, namespace, declaration);
            }
        }
    }
    binder.restore_import_scope(scope);
}

/// Validates each `enum` member's initializer (21.4): it must be a compile-time constant (CS0133),
/// and when it folds, its value must fit the enum's underlying integral type (CS0031). A member with
/// no initializer auto-numbers and is not checked here (an auto-increment overflow is a distinct
/// rule). A constant-FORM initializer that does not fold (an unresolved name) is left to name
/// resolution -- this never reports on it, so it cannot false-flag a valid member.
/// Whether `ty` is one of the eight integer types an enum's underlying type may be (21.1).
fn is_valid_enum_underlying(ty: &TypeSymbol) -> bool {
    matches!(
        ty,
        TypeSymbol::Special(
            SpecialType::SByte
                | SpecialType::Byte
                | SpecialType::Int16
                | SpecialType::UInt16
                | SpecialType::Int32
                | SpecialType::UInt32
                | SpecialType::Int64
                | SpecialType::UInt64
        )
    )
}

fn validate_enum_members(binder: &mut Binder, namespace: &str, declaration: &EnumDecl) {
    for modifier in &declaration.modifiers {
        let modifier = match modifier {
            Modifier::New
            | Modifier::Public
            | Modifier::Protected
            | Modifier::Internal
            | Modifier::Private
            | Modifier::Unsafe => continue,
            Modifier::Abstract => "abstract",
            Modifier::Sealed => "sealed",
            Modifier::Static => "static",
            Modifier::Readonly => "readonly",
            Modifier::Volatile => "volatile",
            Modifier::Virtual => "virtual",
            Modifier::Override => "override",
            Modifier::Extern => "extern",
            Modifier::Const => "const",
        };
        binder.report(Diagnostic::new(
            DiagnosticKind::ModifierNotValidForItem {
                modifier: modifier.into(),
            },
            declaration.span,
        ));
    }
    let underlying = declaration
        .base
        .as_ref()
        .map(bind_type)
        .map(|ty| binder.canonicalize(&ty))
        .unwrap_or(TypeSymbol::Special(SpecialType::Int32));
    if let Some(base) = &declaration.base {
        if !is_valid_enum_underlying(&underlying) {
            binder.report(Diagnostic::new(
                DiagnosticKind::EnumUnderlyingTypeExpected,
                base.span,
            ));
            return;
        }
    }
    let enum_full = qualified_type_name(namespace, &declaration.name);
    let mut seen_members: alloc::collections::BTreeSet<&str> = alloc::collections::BTreeSet::new();
    for member in &declaration.members {
        if !seen_members.insert(&member.name) {
            binder.report(Diagnostic::new(
                DiagnosticKind::DuplicateMember {
                    type_name: declaration.name.clone(),
                    member: member.name.clone(),
                },
                member.span,
            ));
        }
    }
    let values = model_const_values(binder.model());
    for member in &declaration.members {
        let Some(initializer) = &member.value else {
            continue;
        };
        if !is_constant_form(initializer) {
            binder.report(Diagnostic::new(
                DiagnosticKind::NonConstantEnumMember {
                    member: alloc::format!("{enum_full}.{}", member.name).into(),
                },
                initializer.span,
            ));
            continue;
        }
        if let Some(literal) = resolve_const_expr(initializer, &enum_full, &values) {
            if let Some(value) = literal_int_value(&literal) {
                if let Some(rendered) = enum_value_out_of_range(i128::from(value), &underlying) {
                    binder.report(Diagnostic::new(
                        DiagnosticKind::ConstantOutOfRange {
                            value: rendered,
                            to: underlying.to_string().into(),
                        },
                        initializer.span,
                    ));
                }
            }
        }
    }
}

/// The rendered value if `value` is outside the range of an enum's underlying integral type (21.1
/// lists the eight integer types), else `None`. It never fires on a value that fits, so it cannot
/// false-flag a valid member; a non-integral underlying type (an invalid enum) yields `None` here
/// and is diagnosed elsewhere.
fn enum_value_out_of_range(value: i128, underlying: &TypeSymbol) -> Option<Box<str>> {
    let TypeSymbol::Special(target) = underlying else {
        return None;
    };
    let (min, max): (i128, i128) = match target {
        SpecialType::SByte => (i128::from(i8::MIN), i128::from(i8::MAX)),
        SpecialType::Byte => (0, i128::from(u8::MAX)),
        SpecialType::Int16 => (i128::from(i16::MIN), i128::from(i16::MAX)),
        SpecialType::UInt16 => (0, i128::from(u16::MAX)),
        SpecialType::Int32 => (i128::from(i32::MIN), i128::from(i32::MAX)),
        SpecialType::UInt32 => (0, i128::from(u32::MAX)),
        SpecialType::Int64 => (i128::from(i64::MIN), i128::from(i64::MAX)),
        SpecialType::UInt64 => (0, i128::from(u64::MAX)),
        _ => return None,
    };
    if value < min || value > max {
        Some(alloc::format!("{value}").into())
    } else {
        None
    }
}

/// CS0058 / CS0059: a delegate's return type and parameter types must each be at least as
/// accessible as the delegate itself (10.5.4). The delegate's effective accessibility comes from
/// its declared modifiers (a top-level delegate defaults to `internal`); an unresolved, reference,
/// or predefined signature type never fires, so this never false-flags a valid program.
/// The keyword of a modifier that is not valid on a delegate declaration (CS0106) -- the
/// inheritance/instance modifiers -- or `None` for a valid one (accessibility, `new`) or a modifier
/// this conservatively does not flag, so a valid delegate is never rejected.
fn invalid_delegate_modifier(modifier: &Modifier) -> Option<&'static str> {
    match modifier {
        Modifier::Abstract => Some("abstract"),
        Modifier::Sealed => Some("sealed"),
        Modifier::Virtual => Some("virtual"),
        Modifier::Override => Some("override"),
        Modifier::Static => Some("static"),
        _ => None,
    }
}

fn check_delegate_accessibility(binder: &mut Binder, namespace: &str, declaration: &DelegateDecl) {
    validate_parameter_names(binder, &declaration.parameters);
    for modifier in &declaration.modifiers {
        if let Some(name) = invalid_delegate_modifier(modifier) {
            binder.report(Diagnostic::new(
                DiagnosticKind::ModifierNotValidForItem {
                    modifier: name.into(),
                },
                declaration.span,
            ));
        }
    }
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

/// Validates each `volatile` field's type (17.4.3): it must be a reference type, one of
/// byte/sbyte/short/ushort/int/uint/char/float/bool, or an enum with one of those underlying
/// bases. Any other type -- long, ulong, double, decimal, a struct, an enum with a 64-bit base --
/// is CS0677. Conservative: a pointer or an unresolved type is never flagged, so a valid program
/// is never rejected.
fn validate_volatile_fields(binder: &mut Binder, namespace: &str, declaration: &TypeDecl) {
    let type_full = qualified_type_name(namespace, &declaration.name);
    for member in &declaration.members {
        let Member::Field {
            modifiers,
            ty,
            declarators,
            ..
        } = member
        else {
            continue;
        };
        if !modifiers.iter().any(|m| matches!(m, Modifier::Volatile)) {
            continue;
        }
        let field_ty = binder.canonicalize(&bind_type(ty));
        if is_permitted_volatile_type(binder.model(), &field_ty) {
            continue;
        }
        let rendered = field_ty.to_string();
        for declarator in declarators {
            binder.report(Diagnostic::new(
                DiagnosticKind::VolatileFieldType {
                    field: alloc::format!("{type_full}.{}", declarator.name).into(),
                    ty: rendered.clone().into(),
                },
                declarator.span,
            ));
        }
    }
}

/// Whether `ty` is a type a `volatile` field may have (17.4.3). Reference types and the permitted
/// integer/char/float/bool value types are allowed; a struct or a wider numeric (long, ulong,
/// double, decimal) is not. Conservative for what the model cannot classify (a pointer, byref,
/// error, or unresolved named type -> allowed), so the caller never false-flags a valid program.
fn is_permitted_volatile_type(model: &Model, ty: &TypeSymbol) -> bool {
    match ty {
        TypeSymbol::Special(special) => matches!(
            special,
            SpecialType::SByte
                | SpecialType::Byte
                | SpecialType::Int16
                | SpecialType::UInt16
                | SpecialType::Int32
                | SpecialType::UInt32
                | SpecialType::Char
                | SpecialType::Single
                | SpecialType::Boolean
                | SpecialType::String
                | SpecialType::Object
        ),
        TypeSymbol::Array { .. } => true,
        TypeSymbol::Named(_) => match model.get_by_symbol(ty) {
            Some(info) => match info.kind {
                crate::symbols::TypeKind::Class
                | crate::symbols::TypeKind::Interface
                | crate::symbols::TypeKind::Delegate => true,
                crate::symbols::TypeKind::Enum => enum_underlying_permitted(info).unwrap_or(true),
                crate::symbols::TypeKind::Struct => false,
            },
            None => true,
        },
        TypeSymbol::Pointer(_) | TypeSymbol::ByRef(_) | TypeSymbol::Error => true,
    }
}

/// Whether an enum's underlying integral base is one a `volatile` field permits
/// (byte/sbyte/short/ushort/int/uint); `None` when the base cannot be read from the model, so the
/// caller leaves it unflagged.
fn enum_underlying_permitted(info: &TypeInfo) -> Option<bool> {
    let TypeSymbol::Special(special) = info.bases.first()? else {
        return None;
    };
    Some(matches!(
        special,
        SpecialType::SByte
            | SpecialType::Byte
            | SpecialType::Int16
            | SpecialType::UInt16
            | SpecialType::Int32
            | SpecialType::UInt32
    ))
}

/// Validates that each user-defined operator is declared `static` and `public` (17.9.1); one
/// missing either modifier is CS0558. Scoped to classes and structs (operators elsewhere are a
/// separate error). The signature is rendered exactly as csc names the operator.
fn validate_operator_modifiers(binder: &mut Binder, declaration: &TypeDecl) {
    if !matches!(declaration.kind, TypeKind::Class | TypeKind::Struct) {
        return;
    }
    for member in &declaration.members {
        let (modifiers, span, signature) = match member {
            Member::Operator {
                operator,
                parameters,
                modifiers,
                span,
                ..
            } => (
                modifiers,
                *span,
                alloc::format!(
                    "{}.operator {}({})",
                    declaration.name,
                    operator_source_symbol(*operator),
                    parameter_type_list(parameters)
                ),
            ),
            Member::ConversionOperator {
                direction,
                target,
                parameters,
                modifiers,
                span,
                ..
            } => {
                let keyword = match direction {
                    ConversionDirection::Implicit => "implicit",
                    ConversionDirection::Explicit => "explicit",
                };
                let target_ty = binder.canonicalize(&bind_type(target));
                (
                    modifiers,
                    *span,
                    alloc::format!(
                        "{}.{} operator {}({})",
                        declaration.name,
                        keyword,
                        target_ty,
                        parameter_type_list(parameters)
                    ),
                )
            }
            _ => continue,
        };
        let public_and_static = modifiers.iter().any(|m| matches!(m, Modifier::Public))
            && modifiers.iter().any(|m| matches!(m, Modifier::Static));
        if !public_and_static {
            binder.report(Diagnostic::new(
                DiagnosticKind::OperatorMustBeStaticAndPublic {
                    signature: signature.into(),
                },
                span,
            ));
        }
    }
}

/// Validates that no class member is both `static` and `virtual`/`abstract`/`override` (CS0112) --
/// a static member is not part of virtual dispatch. Scoped to classes (a struct's virtual/abstract
/// member is the separate CS0106 rule). csc names the offending modifier, not the member.
fn validate_static_member_modifiers(binder: &mut Binder, declaration: &TypeDecl) {
    if declaration.kind != TypeKind::Class {
        return;
    }
    for member in &declaration.members {
        let (modifiers, span) = match member {
            Member::Method { modifiers, span, .. }
            | Member::Property { modifiers, span, .. }
            | Member::Indexer { modifiers, span, .. } => (modifiers, *span),
            _ => continue,
        };
        if !modifiers.iter().any(|m| matches!(m, Modifier::Static)) {
            continue;
        }
        let offending = modifiers.iter().find_map(|m| match m {
            Modifier::Virtual => Some("virtual"),
            Modifier::Abstract => Some("abstract"),
            Modifier::Override => Some("override"),
            _ => None,
        });
        if let Some(modifier) = offending {
            binder.report(Diagnostic::new(
                DiagnosticKind::StaticMemberCannotBeVirtual {
                    modifier: modifier.into(),
                },
                span,
            ));
        }
    }
}

/// Whether an event's type resolves to something that is definitely NOT a delegate (a predefined
/// type, or a class/struct/interface/enum) -- CS0066. Conservative: an unresolved named type (which
/// may be a BCL delegate such as `EventHandler`), and array/pointer types, are not flagged, so a
/// valid event is never rejected.
fn event_type_is_non_delegate(model: &Model, ty: &TypeSymbol) -> bool {
    match ty {
        TypeSymbol::Special(_) => true,
        TypeSymbol::Named(_) => matches!(
            model.get_by_symbol(ty).map(|info| info.kind),
            Some(
                crate::symbols::TypeKind::Class
                    | crate::symbols::TypeKind::Struct
                    | crate::symbols::TypeKind::Interface
                    | crate::symbols::TypeKind::Enum
            )
        ),
        _ => false,
    }
}

/// Validates that every field-like event's type is a delegate (17.7); a non-delegate type is CS0066.
fn validate_event_types(binder: &mut Binder, namespace: &str, declaration: &TypeDecl) {
    let type_full = qualified_type_name(namespace, &declaration.name);
    for member in &declaration.members {
        let Member::EventField {
            ty, declarators, ..
        } = member
        else {
            continue;
        };
        let resolved = binder.resolve_named_type(&bind_type(ty), ty.span);
        if resolved.is_error() {
            continue;
        }
        let event_ty = binder.canonicalize(&resolved);
        if event_type_is_non_delegate(binder.model(), &event_ty) {
            for declarator in declarators {
                binder.report(Diagnostic::new(
                    DiagnosticKind::EventTypeMustBeDelegate {
                        event: alloc::format!("{type_full}.{}", declarator.name).into(),
                    },
                    declarator.span,
                ));
            }
        }
    }
}

/// Whether an overloadable operator can only be unary (17.9.1) -- it takes one operand, so a
/// two-parameter declaration of one is CS1020. `+` and `-` are dual (unary or binary) and are not
/// here.
fn is_unary_only_operator(operator: OverloadableOperator) -> bool {
    use OverloadableOperator as O;
    matches!(
        operator,
        O::LogicalNot | O::BitwiseNot | O::Increment | O::Decrement | O::True | O::False
    )
}

/// Validates user-defined operator arity: a two-parameter declaration must name a
/// binary-overloadable operator (17.9.2), so a unary-only operator given two parameters is CS1020.
/// The attribute sections a member carries, for the attribute rules below.
fn member_attributes(member: &Member) -> Option<&[AttributeSection]> {
    let attributes = match member {
        Member::Method { attributes, .. }
        | Member::Field { attributes, .. }
        | Member::Property { attributes, .. }
        | Member::Indexer { attributes, .. }
        | Member::Constructor { attributes, .. }
        | Member::Destructor { attributes, .. }
        | Member::Operator { attributes, .. }
        | Member::ConversionOperator { attributes, .. }
        | Member::EventField { attributes, .. }
        | Member::Event { attributes, .. } => attributes,
        _ => return None,
    };
    Some(attributes)
}

/// CS0579 / CS0182: the attribute rules that need no knowledge of the attribute CLASS. A section
/// may not name the same attribute twice (24.2), and every argument must be a compile-time
/// constant, a `typeof`, or an array creation -- an attribute is baked into metadata, so nothing
/// evaluated at run time can supply one.
fn validate_attributes(binder: &mut Binder, attributes: &[AttributeSection]) {
    let mut seen: alloc::collections::BTreeSet<String> = alloc::collections::BTreeSet::new();
    for section in attributes {
        for attribute in &section.attributes {
            let name = dotted(&attribute.name);
            let written = TypeSymbol::Named(attribute.name.parts.iter().cloned().collect());
            let resolved = binder.resolve_named_type_quietly(&written, attribute.span);
            if !resolved.is_error()
                && binder.is_provably_not_derived_from_system(&resolved, "Attribute")
            {
                binder.report(Diagnostic::new(
                    DiagnosticKind::NotAnAttributeClass {
                        type_name: name.clone().into(),
                    },
                    attribute.span,
                ));
            }
            if !seen.insert(name.clone()) {
                binder.report(Diagnostic::new(
                    DiagnosticKind::DuplicateAttribute {
                        name: attribute
                            .name
                            .parts
                            .last()
                            .cloned()
                            .unwrap_or_else(|| Box::from("")),
                    },
                    attribute.span,
                ));
            }
            for argument in &attribute.arguments {
                let value = match argument {
                    lamella_syntax::ast::AttributeArgument::Positional(value) => value,
                    lamella_syntax::ast::AttributeArgument::Named { value, .. } => value,
                };
                if !is_attribute_argument_form(value) {
                    binder.report(Diagnostic::new(
                        DiagnosticKind::NonConstantAttributeArgument,
                        value.span,
                    ));
                }
            }
        }
    }
}

/// Whether an expression has a form an attribute argument may take (24.2): a constant expression,
/// a `typeof`, or an array creation. Conservative in the accepting direction -- anything this
/// cannot classify is left alone -- so an unrecognized-but-valid form is a gap, never a refusal.
fn is_attribute_argument_form(value: &lamella_syntax::ast::Expr) -> bool {
    use lamella_syntax::ast::ExprKind;
    match &value.kind {
        ExprKind::TypeOf(_) | ExprKind::ArrayCreation { .. } | ExprKind::ArrayInitializer(_) => true,
        _ => is_constant_form(value),
    }
}

/// CS0768: a constructor that reaches ITSELF through a chain of `: this(...)` initializers
/// (17.10.1). Such a chain would never terminate, so no constructor in the cycle can run. The
/// graph is keyed by parameter-type list, which is what a `this(...)` call resolves on.
fn validate_constructor_initializer_cycles(binder: &mut Binder, declaration: &TypeDecl) {
    let constructors: Vec<(Vec<TypeSymbol>, Option<usize>, Span, &[Parameter])> = declaration
        .members
        .iter()
        .filter_map(|member| match member {
            Member::Constructor {
                modifiers,
                parameters,
                initializer,
                span,
                ..
            } if !modifiers.iter().any(|m| matches!(m, Modifier::Static)) => {
                let chains_to = initializer.as_ref().and_then(|init| {
                    matches!(init.kind, lamella_syntax::ast::ConstructorInitializerKind::This)
                        .then_some(init.arguments.len())
                });
                Some((
                    parameters.iter().map(parameter_symbol).collect(),
                    chains_to,
                    *span,
                    parameters.as_slice(),
                ))
            }
            _ => None,
        })
        .collect();
    let target_of = |count: usize| {
        let mut matches = constructors
            .iter()
            .enumerate()
            .filter(|(_, (_, _, _, parameters))| parameters.len() == count);
        match (matches.next(), matches.next()) {
            (Some((index, _)), None) => Some(index),
            _ => None,
        }
    };
    let mut already_reported = alloc::vec![false; constructors.len()];
    for start in 0..constructors.len() {
        if already_reported[start] {
            continue;
        }
        let mut visited = alloc::vec![false; constructors.len()];
        let mut current = start;
        loop {
            if visited[current] {
                if current == start {
                    binder.report(Diagnostic::new(
                        DiagnosticKind::ConstructorInitializerCycle {
                            constructor: method_signature(
                                &declaration.name,
                                &declaration.name,
                                constructors[start].3,
                            ),
                        },
                        constructors[start].2,
                    ));
                    for (index, seen) in visited.iter().enumerate() {
                        if *seen {
                            already_reported[index] = true;
                        }
                    }
                }
                break;
            }
            visited[current] = true;
            let Some(count) = constructors[current].1 else {
                break;
            };
            let Some(next) = target_of(count) else {
                break;
            };
            current = next;
        }
    }
}

/// CS0227: `unsafe` written in a compilation the driver did not give `/unsafe`. csc gates unsafe
/// code behind that option and so does lcsc -- the language supports all of it, but a compilation
/// opts IN to containing it. Reported at each `unsafe` the source writes (the type's own modifier
/// and each member's), which is where csc reports it too.
fn validate_unsafe_permitted(binder: &mut Binder, declaration: &TypeDecl) {
    if !binder.unsafe_option_missing() {
        return;
    }
    let is_unsafe = |modifiers: &[Modifier]| modifiers.iter().any(|m| matches!(m, Modifier::Unsafe));
    if is_unsafe(&declaration.modifiers) {
        binder.report(Diagnostic::new(
            DiagnosticKind::UnsafeCodeRequiresOption,
            declaration.span,
        ));
    }
    for member in &declaration.members {
        let (modifiers, span) = match member {
            Member::Method { modifiers, span, .. }
            | Member::Field { modifiers, span, .. }
            | Member::Property { modifiers, span, .. }
            | Member::Indexer { modifiers, span, .. }
            | Member::Constructor { modifiers, span, .. }
            | Member::Destructor { modifiers, span, .. }
            | Member::Operator { modifiers, span, .. }
            | Member::ConversionOperator { modifiers, span, .. }
            | Member::EventField { modifiers, span, .. }
            | Member::Event { modifiers, span, .. } => (modifiers, span),
            _ => continue,
        };
        if is_unsafe(modifiers) {
            binder.report(Diagnostic::new(
                DiagnosticKind::UnsafeCodeRequiresOption,
                *span,
            ));
        }
    }
}

/// CS0133: a `const` field's initializer must be a constant expression (17.4.2) -- its value is
/// baked into every use site, so it cannot be a call or anything else evaluated at run time. The
/// same `is_constant_form` test the enum members use, so the two agree on what "constant" means.
fn validate_const_field_initializers(binder: &mut Binder, namespace: &str, declaration: &TypeDecl) {
    let type_full = qualified_type_name(namespace, &declaration.name);
    for member in &declaration.members {
        let Member::Field {
            modifiers,
            declarators,
            ..
        } = member
        else {
            continue;
        };
        if !modifiers.iter().any(|m| matches!(m, Modifier::Const)) {
            continue;
        }
        for declarator in declarators {
            let Some(initializer) = &declarator.initializer else {
                continue;
            };
            if !is_constant_form(initializer) {
                binder.report(Diagnostic::new(
                    DiagnosticKind::NonConstantFieldInitializer {
                        field: alloc::format!("{type_full}.{}", declarator.name).into(),
                    },
                    initializer.span,
                ));
            }
        }
    }
}

/// A conversion operator's own rules (17.9.4). It exists to bridge ITS OWN type and another, so
/// one whose source and target are both foreign converts nothing the enclosing type is party to
/// (`CS0556`). And it is a UNARY form: a second parameter makes it no operator at all, which csc
/// reports as `CS1019`, the same code a two-parameter unary operator gets.
fn validate_conversion_operators(binder: &mut Binder, declaration: &TypeDecl) {
    for member in &declaration.members {
        let Member::ConversionOperator {
            target,
            parameters,
            span,
            ..
        } = member
        else {
            continue;
        };
        if parameters.len() != 1 {
            binder.report(Diagnostic::new(
                DiagnosticKind::OverloadableUnaryOperatorExpected,
                *span,
            ));
            continue;
        }
        let names_enclosing = |ty: &lamella_syntax::ast::TypeRef| {
            matches!(bind_type(ty), TypeSymbol::Named(parts)
                if parts.last().is_some_and(|part| **part == *declaration.name))
        };
        if !names_enclosing(target) && !names_enclosing(&parameters[0].ty) {
            binder.report(Diagnostic::new(
                DiagnosticKind::ConversionMustInvolveEnclosingType,
                *span,
            ));
        }
    }
}

fn validate_operator_arity(binder: &mut Binder, declaration: &TypeDecl) {
    for member in &declaration.members {
        if let Member::Operator {
            operator,
            parameters,
            span,
            ..
        } = member
        {
            if parameters.len() == 2 && is_unary_only_operator(*operator) {
                binder.report(Diagnostic::new(
                    DiagnosticKind::OverloadableBinaryOperatorExpected,
                    *span,
                ));
            }
        }
    }
}

/// The operator a pairable operator requires alongside it (17.9.2), or `None` for one that
/// stands alone. A type cannot support one direction of a comparison without the other, nor
/// `true` without `false`, because the language uses each pair together.
fn required_operator_partner(operator: OverloadableOperator) -> Option<OverloadableOperator> {
    use OverloadableOperator as O;
    Some(match operator {
        O::Equality => O::Inequality,
        O::Inequality => O::Equality,
        O::LessThan => O::GreaterThan,
        O::GreaterThan => O::LessThan,
        O::LessThanOrEqual => O::GreaterThanOrEqual,
        O::GreaterThanOrEqual => O::LessThanOrEqual,
        O::True => O::False,
        O::False => O::True,
        _ => return None,
    })
}

/// Validates that every pairable user-defined operator has its partner declared in the same
/// type: CS0216. Matches on the operator TOKEN alone, not the signature, which is what csc
/// does -- a partner with different parameter types still satisfies the requirement here, and
/// mismatched operand types are a separate diagnosis.
fn validate_operator_pairs(binder: &mut Binder, declaration: &TypeDecl) {
    if !matches!(declaration.kind, TypeKind::Class | TypeKind::Struct) {
        return;
    }
    let declared: alloc::vec::Vec<OverloadableOperator> = declaration
        .members
        .iter()
        .filter_map(|member| match member {
            Member::Operator { operator, .. } => Some(*operator),
            _ => None,
        })
        .collect();
    for member in &declaration.members {
        let Member::Operator {
            operator,
            parameters,
            span,
            ..
        } = member
        else {
            continue;
        };
        let Some(partner) = required_operator_partner(*operator) else {
            continue;
        };
        if declared.contains(&partner) {
            continue;
        }
        binder.report(Diagnostic::new(
            DiagnosticKind::OperatorRequiresMatchingOperator {
                operator: alloc::format!(
                    "{}.operator {}({})",
                    declaration.name,
                    operator_source_symbol(*operator),
                    parameter_type_list(parameters)
                )
                .into(),
                partner: operator_source_symbol(partner),
            },
            *span,
        ));
    }
}

/// The span of a `[Conditional]` attribute (24.4.2) on a member, matched by name as written
/// (`Conditional` or the `Attribute`-suffixed form), or `None` if the member carries none. Matches
/// the name rather than a resolved type, exactly as the call-omission reader does.
fn conditional_attribute_span(sections: &[AttributeSection]) -> Option<Span> {
    for section in sections {
        if section.target.is_some() {
            continue;
        }
        for attribute in &section.attributes {
            let last = attribute.name.parts.last().map(|part| &**part);
            if last == Some("Conditional") || last == Some("ConditionalAttribute") {
                return Some(attribute.span);
            }
        }
    }
    None
}

/// Validates that a `[Conditional]` method returns `void` (24.4.2): a conditional call is omitted
/// wholesale at sites where the symbol is undefined, so a non-`void` return would leave those sites
/// expecting a value that never arrives -- CS0578. An `override` is a distinct rule (csc's CS0243),
/// left to that path.
fn validate_conditional_methods(binder: &mut Binder, declaration: &TypeDecl) {
    for member in &declaration.members {
        if let Member::Method {
            name,
            return_type,
            parameters,
            modifiers,
            attributes,
            ..
        } = member
        {
            if modifiers.contains(&Modifier::Override) || bind_type(return_type).is_void() {
                continue;
            }
            if let Some(span) = conditional_attribute_span(attributes) {
                binder.report(Diagnostic::new(
                    DiagnosticKind::ConditionalMethodMustReturnVoid {
                        method: alloc::format!(
                            "{}.{}({})",
                            declaration.name,
                            name,
                            parameter_type_list(parameters)
                        )
                        .into(),
                    },
                    span,
                ));
            }
        }
    }
}

/// Validates the modifiers on a type defined directly in a namespace (10.5). Such a type may be
/// `public` or `internal` but not `private`/`protected` (CS1527), and `new` -- member hiding
/// (10.2.2) -- is not valid on it (CS0106). A NESTED type is exempt (private and new are both valid
/// there), so the check keys off the model's `enclosing`, which is `None` only for a top-level type.
fn validate_top_level_type_modifiers(binder: &mut Binder, namespace: &str, declaration: &TypeDecl) {
    let symbol = named_symbol(namespace, &declaration.name);
    let is_nested = binder
        .model()
        .get_by_symbol(&symbol)
        .is_some_and(|info| info.enclosing.is_some());
    if is_nested {
        return;
    }
    if declaration
        .modifiers
        .iter()
        .any(|m| matches!(m, Modifier::Private | Modifier::Protected))
    {
        binder.report(Diagnostic::new(
            DiagnosticKind::NamespaceElementBadAccessibility,
            declaration.span,
        ));
    }
    if declaration.modifiers.iter().any(|m| matches!(m, Modifier::New)) {
        binder.report(Diagnostic::new(
            DiagnosticKind::ModifierNotValidForItem {
                modifier: "new".into(),
            },
            declaration.span,
        ));
    }
}

/// The member forms an INTERFACE gained after C# 1.0. In C# 1.0 an interface declares only the
/// signatures of methods, properties, events and indexers (13.2): every accessor is a bare `;`,
/// there is no nested type and no operator, and a member carries no access modifier because every
/// one is implicitly public. Each of those is a later feature, so each is `CS8022` -- except the
/// modifier, which csc gives its own code (`CS8703`) because the repair is to DELETE it rather
/// than to raise the language version alone.
fn validate_interface_members(binder: &mut Binder, declaration: &TypeDecl) {
    if declaration.kind != TypeKind::Interface {
        return;
    }
    let default_implementation = |binder: &mut Binder, span| {
        binder.report(Diagnostic::new(
            DiagnosticKind::FeatureRequiresLaterVersion {
                feature: "default interface implementation".into(),
                required: "C# 8.0".into(),
            },
            span,
        ));
    };
    for member in &declaration.members {
        let modifiers = match member {
            Member::Method { modifiers, .. }
            | Member::Property { modifiers, .. }
            | Member::Indexer { modifiers, .. }
            | Member::EventField { modifiers, .. }
            | Member::Event { modifiers, .. } => Some(modifiers),
            _ => None,
        };
        if let Some(modifiers) = modifiers {
            for modifier in modifiers {
                let name = match modifier {
                    Modifier::Public => "public",
                    Modifier::Private => "private",
                    Modifier::Protected => "protected",
                    Modifier::Internal => "internal",
                    _ => continue,
                };
                binder.report(Diagnostic::new(
                    DiagnosticKind::InterfaceMemberModifier {
                        modifier: name.into(),
                    },
                    declaration.span,
                ));
            }
        }
        match member {
            Member::Method {
                body: Some(body), ..
            } => default_implementation(binder, body.span),
            Member::Property { getter, setter, .. } | Member::Indexer { getter, setter, .. } => {
                for accessor in [getter, setter].into_iter().flatten() {
                    if let Some(body) = &accessor.body {
                        default_implementation(binder, body.span);
                    }
                }
            }
            Member::Event { adder, remover, .. } => {
                for accessor in [adder, remover].into_iter().flatten() {
                    default_implementation(binder, accessor.span);
                }
            }
            Member::Operator { span, .. } | Member::ConversionOperator { span, .. } => {
                default_implementation(binder, *span);
            }
            Member::NestedType(_) => default_implementation(binder, declaration.span),
            _ => {}
        }
    }
}

/// A constructor's own declaration rules. A STATIC constructor is run by the runtime and never
/// called, so an accessibility modifier on it means nothing (`CS0515`, 17.11). And a constructor
/// is recognized by repeating the enclosing type's name, so one that does not is not a
/// constructor at all -- csc reads it as a method missing its return type (`CS1520`).
fn validate_constructors(binder: &mut Binder, declaration: &TypeDecl) {
    for member in &declaration.members {
        let Member::Constructor {
            modifiers,
            name,
            parameters,
            span,
            ..
        } = member
        else {
            continue;
        };
        if **name != *declaration.name {
            binder.report(Diagnostic::new(
                DiagnosticKind::MethodMustHaveReturnType,
                *span,
            ));
            continue;
        }
        if declaration.kind == TypeKind::Struct
            && parameters.is_empty()
            && !modifiers.iter().any(|m| matches!(m, Modifier::Static))
        {
            binder.report(Diagnostic::new(
                DiagnosticKind::FeatureRequiresLaterVersion {
                    feature: "parameterless struct constructors".into(),
                    required: "C# 10.0".into(),
                },
                *span,
            ));
        }
        if !modifiers.iter().any(|m| matches!(m, Modifier::Static)) {
            continue;
        }
        if modifiers.iter().any(|m| {
            matches!(
                m,
                Modifier::Public | Modifier::Protected | Modifier::Internal | Modifier::Private
            )
        }) {
            binder.report(Diagnostic::new(
                DiagnosticKind::StaticConstructorAccessibility {
                    member: method_signature(&declaration.name, &declaration.name, parameters),
                },
                *span,
            ));
        }
    }
}

/// A destructor's own declaration rules (17.12). It is named for the type it finalizes, so a
/// different name is `CS0574`; only a class has a finalizer, so one declared in a struct is
/// `CS0575`; and it takes no accessibility or inheritance modifier -- `extern` (and, in an unsafe
/// context, `unsafe`) are the only ones valid, so any other is `CS0106`.
fn validate_destructors(binder: &mut Binder, declaration: &TypeDecl) {
    for member in &declaration.members {
        let Member::Destructor {
            modifiers,
            name,
            span,
            ..
        } = member
        else {
            continue;
        };
        if declaration.kind != TypeKind::Class {
            binder.report(Diagnostic::new(DiagnosticKind::DestructorNotInClass, *span));
        } else if **name != *declaration.name {
            binder.report(Diagnostic::new(DiagnosticKind::DestructorNameMismatch, *span));
        }
        for modifier in modifiers {
            let name = match modifier {
                Modifier::Extern | Modifier::Unsafe => continue,
                Modifier::Public => "public",
                Modifier::Private => "private",
                Modifier::Protected => "protected",
                Modifier::Internal => "internal",
                Modifier::Static => "static",
                Modifier::Virtual => "virtual",
                Modifier::Override => "override",
                Modifier::Abstract => "abstract",
                Modifier::Sealed => "sealed",
                Modifier::New => "new",
                Modifier::Readonly => "readonly",
                Modifier::Volatile => "volatile",
                Modifier::Const => "const",
            };
            binder.report(Diagnostic::new(
                DiagnosticKind::ModifierNotValidForItem {
                    modifier: name.into(),
                },
                *span,
            ));
        }
    }
}

/// CS0100: reports each parameter name a parameter list declares more than once. A member's
/// parameters share one declaration space (10.3), so the repeat names nothing new -- and the body
/// could not tell the two apart. Every member kind that takes parameters is covered; the report
/// lands on the SECOND declaration, as csc's does.
fn validate_parameter_names(binder: &mut Binder, parameters: &[Parameter]) {
    let mut seen: alloc::collections::BTreeSet<&str> = alloc::collections::BTreeSet::new();
    for parameter in parameters {
        if bind_type(&parameter.ty).is_void() {
            binder.report(Diagnostic::new(
                DiagnosticKind::VoidParameter,
                parameter.ty.span,
            ));
        }
        if !seen.insert(&parameter.name) {
            binder.report(Diagnostic::new(
                DiagnosticKind::DuplicateParameterName {
                    name: parameter.name.clone(),
                },
                parameter.span,
            ));
        }
    }
}

fn bind_type_bodies(binder: &mut Binder, namespace: &str, declaration: &TypeDecl) {
    let enclosing = named_symbol(namespace, &declaration.name);
    for member in &declaration.members {
        let parameters = match member {
            Member::Method { parameters, .. }
            | Member::Constructor { parameters, .. }
            | Member::Indexer { parameters, .. }
            | Member::Operator { parameters, .. } => parameters,
            _ => continue,
        };
        validate_parameter_names(binder, parameters);
    }
    validate_volatile_fields(binder, namespace, declaration);
    validate_operator_modifiers(binder, declaration);
    validate_operator_arity(binder, declaration);
    validate_conversion_operators(binder, declaration);
    validate_const_field_initializers(binder, namespace, declaration);
    validate_unsafe_permitted(binder, declaration);
    validate_constructor_initializer_cycles(binder, declaration);
    validate_attributes(binder, &declaration.attributes);
    for member in &declaration.members {
        if let Some(attributes) = member_attributes(member) {
            validate_attributes(binder, attributes);
        }
    }
    validate_operator_pairs(binder, declaration);
    validate_conditional_methods(binder, declaration);
    validate_top_level_type_modifiers(binder, namespace, declaration);
    validate_static_member_modifiers(binder, declaration);
    validate_destructors(binder, declaration);
    validate_constructors(binder, declaration);
    validate_interface_members(binder, declaration);
    validate_event_types(binder, namespace, declaration);
    let mut seen_names: alloc::collections::BTreeSet<&str> = alloc::collections::BTreeSet::new();
    let mut method_names: alloc::collections::BTreeSet<&str> = alloc::collections::BTreeSet::new();
    let mut duplicate_field_names: alloc::collections::BTreeSet<&str> =
        alloc::collections::BTreeSet::new();
    for member in &declaration.members {
        match member {
            Member::Field { declarators, .. } => {
                for declarator in declarators {
                    if !seen_names.insert(&declarator.name)
                        || method_names.contains(&*declarator.name)
                    {
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
            Member::Property {
                name,
                span,
                explicit_interface: None,
                ..
            } => {
                if !seen_names.insert(name) || method_names.contains(&**name) {
                    binder.report(Diagnostic::new(
                        DiagnosticKind::DuplicateMember {
                            type_name: declaration.name.clone(),
                            member: name.clone(),
                        },
                        *span,
                    ));
                }
            }
            Member::EventField { declarators, .. } => {
                for declarator in declarators {
                    if !seen_names.insert(&declarator.name)
                        || method_names.contains(&*declarator.name)
                    {
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
            Member::Method {
                name,
                span,
                explicit_interface: None,
                ..
            } => {
                if seen_names.contains(&**name) {
                    binder.report(Diagnostic::new(
                        DiagnosticKind::DuplicateMember {
                            type_name: declaration.name.clone(),
                            member: name.clone(),
                        },
                        *span,
                    ));
                }
                method_names.insert(name);
            }
            _ => {}
        }
    }
    let mut seen_methods: alloc::vec::Vec<(Box<str>, alloc::vec::Vec<TypeSymbol>)> =
        alloc::vec::Vec::new();
    for member in &declaration.members {
        let named = match member {
            Member::Method {
                name,
                parameters,
                explicit_interface: None,
                span,
                ..
            } => Some((name.clone(), parameters, span)),
            Member::Indexer {
                parameters, span, ..
            } => Some((Box::from("this"), parameters, span)),
            _ => None,
        };
        if let Some((name, parameters, span)) = named {
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
                        member: name,
                    },
                    *span,
                ));
            } else {
                seen_methods.push(key);
            }
        }
    }
    let mut seen_constructors: alloc::vec::Vec<alloc::vec::Vec<TypeSymbol>> = alloc::vec::Vec::new();
    for member in &declaration.members {
        if let Member::Constructor {
            modifiers,
            parameters,
            span,
            ..
        } = member
        {
            if modifiers.iter().any(|m| matches!(m, Modifier::Static)) {
                continue;
            }
            let key: alloc::vec::Vec<TypeSymbol> =
                parameters.iter().map(parameter_symbol).collect();
            if seen_constructors.contains(&key) {
                binder.report(Diagnostic::new(
                    DiagnosticKind::DuplicateMethod {
                        type_name: declaration.name.clone(),
                        member: declaration.name.clone(),
                    },
                    *span,
                ));
            } else {
                seen_constructors.push(key);
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
    if declaration.kind == TypeKind::Class
        && declaration.modifiers.iter().any(|m| matches!(m, Modifier::Abstract))
        && declaration.modifiers.iter().any(|m| matches!(m, Modifier::Sealed))
    {
        binder.report(Diagnostic::new(
            DiagnosticKind::AbstractTypeSealedOrStatic {
                type_name: declaration.name.clone(),
            },
            declaration.span,
        ));
    }
    if declaration.kind == TypeKind::Class {
        let written: Vec<TypeSymbol> = declaration.bases.iter().map(bind_type).collect();
        let class_bases: Vec<TypeSymbol> = {
            let model = binder.model();
            written
                .iter()
                .filter_map(|base| model.resolve_class_base(base))
                .collect()
        };
        if class_bases.len() > 1 {
            binder.report(Diagnostic::new(
                DiagnosticKind::MultipleClassBases {
                    type_name: declaration.name.clone(),
                    first: class_bases[0].to_string().into(),
                    second: class_bases[1].to_string().into(),
                },
                declaration.span,
            ));
        }
        let base_candidate = class_bases.first().cloned().or_else(|| {
            let model = binder.model();
            written
                .iter()
                .find_map(|base| model.resolve_struct_base(base))
        });
        if let Some(base) = &base_candidate {
            let derives_from_sealed = binder
                .model()
                .get_by_symbol(base)
                .is_some_and(|info| info.is_sealed);
            if derives_from_sealed {
                binder.report(Diagnostic::new(
                    DiagnosticKind::DeriveFromSealed {
                        derived: declaration.name.clone(),
                        base: base.to_string().into(),
                    },
                    declaration.span,
                ));
            }
        }
    }
    if matches!(declaration.kind, TypeKind::Struct | TypeKind::Interface) {
        let written: Vec<TypeSymbol> = declaration.bases.iter().map(bind_type).collect();
        let offenders: Vec<TypeSymbol> = {
            let model = binder.model();
            written
                .iter()
                .filter(|base| {
                    model.get_by_symbol(base).is_some_and(|info| {
                        info.kind != crate::symbols::TypeKind::Interface
                    })
                })
                .cloned()
                .collect()
        };
        for base in offenders {
            binder.report(Diagnostic::new(
                DiagnosticKind::BaseTypeNotInterface {
                    base: base.to_string().into(),
                },
                declaration.span,
            ));
        }
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
                Member::Indexer { modifiers, span, .. } => {
                    if modifiers.iter().any(|m| matches!(m, Modifier::Static)) {
                        binder.report(Diagnostic::new(
                            DiagnosticKind::ModifierNotValidForItem {
                                modifier: "static".into(),
                            },
                            *span,
                        ));
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
    for base in &declaration.bases {
        binder.resolve_named_type(&bind_type(base), base.span);
    }
    for member in &declaration.members {
        match member {
            Member::Field { ty, .. }
            | Member::Indexer { ty, .. }
            | Member::Method { return_type: ty, .. }
            | Member::Property { ty, .. }
            | Member::Operator { return_type: ty, .. } => {
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
                is_vararg,
                body: Some(body),
                ..
            } => {
                let params = bound_parameters(parameters);
                if *is_vararg {
                    binder.set_next_method_vararg();
                }
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
                is_vararg,
                body,
                ..
            } => {
                let params = bound_parameters(parameters);
                if *is_vararg {
                    binder.set_next_method_vararg();
                }
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
            Member::Event {
                modifiers,
                ty,
                name,
                adder,
                remover,
                ..
            } => {
                let event_ty = bind_type(ty);
                let is_static = is_static_member(modifiers);
                let value = [(Box::from("value"), event_ty)];
                let accessors = [("add_", adder), ("remove_", remover)];
                for (prefix, accessor) in accessors {
                    if let Some(body) = accessor.as_ref().and_then(|accessor| accessor.body.as_ref())
                    {
                        binder.bind_method(
                            Some(enclosing.clone()),
                            &accessor_name(prefix, name),
                            TypeSymbol::Special(SpecialType::Void),
                            &value,
                            &[],
                            is_static,
                            body,
                        );
                    }
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
                span: member_span,
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
                        let shared_prefix = Span {
                            start: member_span.start,
                            end: declarators
                                .first()
                                .map_or(member_span.end, |first| first.span.start),
                        };
                        binder.record_private_field(
                            &enclosing,
                            &declarator.name,
                            declarator.span,
                            shared_prefix,
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
    binder.check_property_overrides_have_base(&enclosing, declaration);
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
pub(crate) fn parameter_type_list(parameters: &[Parameter]) -> String {
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

    /// Like [`sorted_codes`], but scanned under the typedref knob so `__arglist` (and the
    /// other csc typed-reference operators) tokenize.
    fn sorted_codes_typedref(unit: &str) -> Vec<u16> {
        let options = lamella_syntax::lexer::LexOptions {
            typedref: true,
            ..lamella_syntax::lexer::LexOptions::default()
        };
        let unit = lamella_syntax::parser::parse_compilation_unit_with(unit, options).unit;
        let mut codes: Vec<u16> = bind_compilation_unit(&unit)
            .iter()
            .map(Diagnostic::code)
            .collect();
        codes.sort_unstable();
        codes
    }

    #[test]
    fn a_name_declared_twice_in_one_declaration_space_is_cs0102_or_cs0111() {
        assert_eq!(
            sorted_codes("class C { int P { get { return 0; } } int P() { return 0; } }"),
            [102]
        );
        assert_eq!(
            sorted_codes(
                "delegate void H(); \
                 class C { int E { get { return 0; } } event H E; }"
            ),
            [102]
        );
        assert_eq!(sorted_codes("class C { int F; int F; }"), [102]);
        assert_eq!(sorted_codes("enum E { A, A }"), [102]);
        assert_eq!(
            sorted_codes("class C { int P() { return 0; } int P { get { return 0; } } }"),
            [102]
        );
        assert_eq!(
            sorted_codes(
                "class C { int this[int i] { get { return i; } } \
                 int this[int i] { get { return i; } } }"
            ),
            [111]
        );
        assert_eq!(
            sorted_codes("class C { static void M(int value, int value) { } }"),
            [100]
        );
        assert_eq!(sorted_codes("delegate void H(int a, int a);"), [100]);

        for clean in [
            "class C { int M() { return 0; } int M(int i) { return i; } }",
            "class C { int this[int i] { get { return i; } } \
             int this[string s] { get { return 0; } } }",
            "class C { int P { get { return 0; } } int this[int i] { get { return i; } } }",
            "class C { public int F; public int G; static void M(int a, int b) { } }",
            "enum E { A, B }",
        ] {
            assert_eq!(sorted_codes(clean), [], "expected no diagnostic for: {clean}");
        }
    }

    /// These need a `System.Attribute` / `System.IDisposable` to exist, so they run against a
    /// model carrying them -- which is what a real compilation has and what the harness now gives
    /// lcsc. Both rules are PROVEN-only, so a model without those types reports nothing.
    #[test]
    fn attribute_classes_and_using_resources_must_be_what_they_claim() {
        use crate::symbols::{Model, TypeInfo, TypeKind};
        fn bcl() -> Model {
            let mut model = Model::new();
            model.insert(TypeInfo::new("System", "Object", TypeKind::Class));
            model.insert(TypeInfo::new("System", "Attribute", TypeKind::Class));
            model.insert(TypeInfo::new("System", "IDisposable", TypeKind::Interface));
            model
        }
        let codes = |source: &str| {
            let unit = parse_compilation_unit(source).unit;
            let mut codes: Vec<u16> = bind_compilation_unit_with_model(&unit, bcl())
                .iter()
                .map(Diagnostic::code)
                .collect();
            codes.sort_unstable();
            codes.dedup();
            codes
        };

        assert_eq!(codes("class NotOne { } [NotOne] class C { }"), [616]);
        assert_eq!(
            codes("class R { } class C { static void M(R r) { using (R x = r) { } } }"),
            [1674]
        );
        assert_eq!(
            codes("class A : System.Attribute { } [A, A] class C { }"),
            [579]
        );
        assert_eq!(
            codes(
                "class A : System.Attribute { } \
                 [A(C.V())] class C { public static int V() { return 1; } }"
            ),
            [182]
        );

        assert_eq!(
            codes("class A : System.Attribute { } [A] class C { }"),
            []
        );
        assert_eq!(
            codes(
                "class R : System.IDisposable { public void Dispose() { } } \
                 class C { static void M(R r) { using (R x = r) { } } }"
            ),
            []
        );
    }

    #[test]
    fn an_undefined_type_in_declaration_position_is_cs0246() {
        assert_eq!(
            sorted_codes("abstract class C { public abstract Missing M(); }"),
            [246]
        );
        assert_eq!(
            sorted_codes("abstract class C { public abstract Missing P { get; } }"),
            [246]
        );
        assert_eq!(sorted_codes("class C : Missing { }"), [246]);
        assert_eq!(sorted_codes("using X = Missing.Type; class C { }"), [246]);
        assert_eq!(sorted_codes("class C { event Missing E; }"), [246]);

        for clean in [
            "class B { } class C { B M() { return null; } }",
            "class B { } class C { B P { get { return null; } } }",
            "class B { } class C : B { }",
            "delegate void H(); class C { public event H E; }",
        ] {
            assert_eq!(sorted_codes(clean), [], "expected no diagnostic for: {clean}");
        }
    }

    #[test]
    fn constructor_chains_attributes_and_foreach_variables() {
        assert_eq!(
            sorted_codes("class C { public C() : this(0) { } public C(int v) : this() { } }"),
            [768]
        );
        assert_eq!(
            sorted_codes(
                "class C { static void M(int[] vs) { foreach (int v in vs) { v = 1; } } }"
            ),
            [1656]
        );

        for clean in [
            "class C { public C() : this(0) { } public C(int v) { } }",
            "class C { static int M(int[] vs) { int t = 0; foreach (int v in vs) { t = v; } return t; } }",
        ] {
            assert_eq!(sorted_codes(clean), [], "expected no diagnostic for: {clean}");
        }
    }

    #[test]
    fn unsafe_code_is_cs0227_only_when_the_driver_omitted_the_option() {
        use lamella_syntax::parser::parse_compilation_unit;
        let codes = |source: &str, unsafe_option_missing: bool| {
            let unit = parse_compilation_unit(source).unit;
            let mut codes: Vec<u16> = crate::bind_compilation_unit_with_references_and_options(
                &unit,
                &[],
                unsafe_option_missing,
            )
            .iter()
            .map(Diagnostic::code)
            .collect();
            codes.sort_unstable();
            codes.dedup();
            codes
        };

        assert_eq!(codes("unsafe class C { }", true), [227]);
        assert_eq!(
            codes("class C { unsafe static void M() { } }", true),
            [227]
        );
        assert_eq!(codes("unsafe class C { }", false), []);
        assert_eq!(
            codes("class C { unsafe static void M() { } }", false),
            []
        );
        assert_eq!(codes("class C { static void M() { } }", true), []);
    }

    #[test]
    fn interface_and_operator_member_forms_that_are_not_csharp_1() {
        assert_eq!(
            sorted_codes("interface I { int M() { return 1; } }"),
            [8022]
        );
        assert_eq!(
            sorted_codes("interface I { int P { get { return 1; } } }"),
            [8022]
        );
        assert_eq!(sorted_codes("interface I { class N { } }"), [8022]);
        assert_eq!(sorted_codes("interface I { public int M(); }"), [8703]);
        assert_eq!(sorted_codes("struct S { public S() { } }"), [8022]);
        assert_eq!(
            sorted_codes(
                "class A { } class B { } \
                 class C { public static implicit operator A(B v) { return null; } }"
            ),
            [556]
        );
        assert_eq!(
            sorted_codes(
                "class C { static void M() { try { } catch { } catch { } } }"
            ),
            [1017]
        );
        assert_eq!(
            sorted_codes("class C { const int V = G(); static int G() { return 1; } }"),
            [133]
        );

        for clean in [
            "interface I { int M(); int P { get; } }",
            "struct S { public S(int v) { } }",
            "class A { public static implicit operator int(A v) { return 1; } }",
            "class C { static void M() { try { } catch { } } }",
            "class C { const int V = 1 + 2; }",
        ] {
            assert_eq!(sorted_codes(clean), [], "expected no diagnostic for: {clean}");
        }
    }

    #[test]
    fn destructor_and_constructor_declaration_rules() {
        assert_eq!(sorted_codes("class C { ~D() { } }"), [574]);
        assert_eq!(sorted_codes("struct S { ~S() { } }"), [575]);
        assert_eq!(sorted_codes("class C { public ~C() { } }"), [106]);
        assert_eq!(sorted_codes("class C { static ~C() { } }"), [106]);
        assert_eq!(sorted_codes("class C { D() { } }"), [1520]);
        assert_eq!(sorted_codes("class C { public static C() { } }"), [515]);
        assert_eq!(sorted_codes("abstract enum E { A }"), [106]);

        for clean in [
            "class C { ~C() { } }",
            "class C { extern ~C(); }",
            "class C { C() { } }",
            "class C { static C() { } }",
            "public enum E { A }",
        ] {
            assert_eq!(sorted_codes(clean), [], "expected no diagnostic for: {clean}");
        }
    }

    #[test]
    fn a_base_list_may_name_only_what_can_be_derived_from() {
        assert_eq!(sorted_codes("struct S { } class C : S { }"), [509]);
        assert_eq!(sorted_codes("class B { } struct S : B { }"), [527]);
        assert_eq!(sorted_codes("class B { } interface I : B { }"), [527]);

        for clean in [
            "interface I { } struct S : I { }",
            "interface I { } interface J : I { }",
            "class B { } class D : B { }",
            "interface I { } class D : I { }",
        ] {
            assert_eq!(sorted_codes(clean), [], "expected no diagnostic for: {clean}");
        }
    }

    #[test]
    fn predefined_type_with_no_backing_type_is_cs0518() {
        let prelude = "namespace System { public class Object { } public struct Void { } \
             public struct Boolean { } public struct Int32 { } public class String { } \
             public abstract class ValueType { } public abstract class Enum { } } ";
        let float_use = alloc::format!(
            "{prelude}class P {{ static void M() {{ double x = 1.0; if (x == x) {{ }} }} }}"
        );
        assert_eq!(sorted_codes(&float_use), [518, 518]);
        let single_use = alloc::format!(
            "{prelude}class P {{ static void M() {{ float f = 1.5f; if (f == f) {{ }} }} }}"
        );
        assert_eq!(sorted_codes(&single_use), [518, 518]);
        let bare_literal = alloc::format!(
            "{prelude}class P {{ static void M() {{ object o = 1.0; if (o == o) {{ }} }} }}"
        );
        assert_eq!(sorted_codes(&bare_literal), [518]);
        let int_only = alloc::format!(
            "{prelude}class P {{ static void M() {{ int fine = 2; if (fine == fine) {{ }} }} }}"
        );
        assert_eq!(sorted_codes(&int_only), []);
    }

    #[test]
    fn vararg_members_accept_arglist_packs() {
        let codes = sorted_codes_typedref(
            "class T { public T(__arglist) { } } \
             class P { \
                static int Sum(int seed, __arglist) { return seed; } \
                static void Main() { \
                    T t = new T(__arglist()); \
                    T u = new T(__arglist(1, \"s\", null, 2.2)); \
                    int s = Sum(2, __arglist(10, 20)); \
                    if (t == u) { } \
                    if (s > 0) { } \
                } \
             }",
        );
        assert_eq!(codes, []);
    }

    #[test]
    fn vararg_call_missing_its_arglist_is_cs7036() {
        let codes = sorted_codes_typedref(
            "class T { public T(__arglist) { } } \
             class P { \
                static void M(int a, __arglist) { } \
                static void Main() { M(1); T t = new T(); if (t == null) { } } \
             }",
        );
        assert_eq!(codes, [7036, 7036]);
    }

    #[test]
    fn arglist_pack_to_a_non_vararg_method_is_cs1503() {
        let codes = sorted_codes_typedref(
            "class P { static void N(int a) { } static void Main() { N(__arglist(1)); } }",
        );
        assert_eq!(codes, [1503]);
    }

    #[test]
    fn bare_arglist_outside_a_vararg_member_is_cs0190() {
        let codes = sorted_codes_typedref(
            "class P { static void M() { object o = __arglist; if (o == null) { } } \
             static void Main() { } }",
        );
        assert_eq!(codes, [190]);
    }

    #[test]
    fn arglist_pack_as_a_value_is_cs0226() {
        let codes = sorted_codes_typedref(
            "class P { static void M(__arglist) { object x = __arglist(1); if (x == null) { } } \
             static void Main() { } }",
        );
        assert_eq!(codes, [226]);
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
    fn a_non_permitted_volatile_field_type_is_cs0677() {
        assert_eq!(sorted_codes("class C { public volatile long a; }"), [677]);
        assert_eq!(sorted_codes("class C { public volatile ulong a; }"), [677]);
        assert_eq!(sorted_codes("class C { public volatile double a; }"), [677]);
        assert_eq!(sorted_codes("class C { public volatile decimal a; }"), [677]);
        assert_eq!(
            sorted_codes("struct S { public int x; } class C { public volatile S a; }"),
            [677]
        );
        assert!(sorted_codes("class C { public volatile int a; }").is_empty());
        assert!(sorted_codes("class C { public volatile bool a; }").is_empty());
        assert!(sorted_codes("class C { public volatile char a; }").is_empty());
        assert!(sorted_codes("class C { public volatile float a; }").is_empty());
        assert!(sorted_codes("class C { public volatile string a; }").is_empty());
    }

    #[test]
    fn a_static_indexer_is_cs0106() {
        assert_eq!(
            sorted_codes("class C { public static int this[int i] { get { return i; } } }"),
            [106]
        );
        assert!(sorted_codes("class C { public int this[int i] { get { return i; } } }").is_empty());
    }

    #[test]
    fn an_operator_that_is_not_public_static_is_cs0558() {
        assert_eq!(sorted_codes("class C { C operator +(C a, C b) { return a; } }"), [558]);
        assert_eq!(sorted_codes("class C { public C operator +(C a, C b) { return a; } }"), [558]);
        assert_eq!(sorted_codes("class C { static C operator +(C a, C b) { return a; } }"), [558]);
        assert_eq!(sorted_codes("class C { explicit operator int(C c) { return 0; } }"), [558]);
        assert!(
            sorted_codes("class C { public static C operator +(C a, C b) { return a; } }").is_empty()
        );
    }

    #[test]
    fn an_abstract_sealed_class_is_cs0418() {
        assert_eq!(sorted_codes("abstract sealed class C { }"), [418]);
        assert_eq!(sorted_codes("sealed abstract class C { }"), [418]);
        assert!(sorted_codes("abstract class C { }").is_empty());
        assert!(sorted_codes("sealed class C { }").is_empty());
    }

    #[test]
    fn a_static_member_marked_virtual_is_cs0112() {
        assert_eq!(sorted_codes("class C { public static virtual void M() { } }"), [112]);
        assert!(sorted_codes("class C { public static void M() { } }").is_empty());
    }

    #[test]
    fn an_enum_with_a_non_integer_underlying_is_cs1008() {
        assert_eq!(sorted_codes("enum E : bool { A }"), [1008]);
        assert_eq!(sorted_codes("enum E : char { A }"), [1008]);
        assert_eq!(sorted_codes("enum E : string { A }"), [1008]);
        assert!(sorted_codes("enum E : byte { A }").is_empty());
        assert!(sorted_codes("enum E : long { A }").is_empty());
        assert!(sorted_codes("enum E { A }").is_empty());
    }

    #[test]
    fn an_invalid_delegate_modifier_is_cs0106() {
        assert_eq!(sorted_codes("abstract delegate void D();"), [106]);
        assert_eq!(sorted_codes("sealed delegate void D();"), [106]);
        assert_eq!(sorted_codes("static delegate void D();"), [106]);
        assert!(sorted_codes("delegate void D();").is_empty());
        assert!(sorted_codes("public delegate void D();").is_empty());
    }

    #[test]
    fn a_class_with_two_class_bases_is_cs1721() {
        assert_eq!(sorted_codes("class A { } class B { } class C : A, B { }"), [1721]);
        assert!(sorted_codes("class A { } interface I { } class C : A, I { }").is_empty());
        assert!(sorted_codes("class A { } class C : A { }").is_empty());
    }

    #[test]
    fn an_event_of_non_delegate_type_is_cs0066() {
        assert_eq!(sorted_codes("class C { public event int E; }"), [66]);
        assert_eq!(sorted_codes("class C { public event string E; }"), [66]);
        assert!(sorted_codes("delegate void D(); class C { public event D E; }").is_empty());
    }

    #[test]
    fn a_duplicate_property_is_cs0102() {
        assert_eq!(
            sorted_codes(
                "class C { public int P { get { return 0; } } public int P { get { return 1; } } }"
            ),
            [102]
        );
        assert!(sorted_codes(
            "class C { public int P { get { return 0; } } public int Q { get { return 1; } } }"
        )
        .is_empty());
    }

    #[test]
    fn a_duplicate_constructor_is_cs0111() {
        assert_eq!(sorted_codes("class C { C(int v) { } C(int v) { } }"), [111]);
        assert!(sorted_codes("class C { C(int v) { } C(string v) { } }").is_empty());
        assert!(sorted_codes("class C { C() { } static C() { } }").is_empty());
    }

    #[test]
    fn a_unary_only_operator_with_two_parameters_is_cs1020() {
        assert_eq!(
            sorted_codes("class C { public static C operator !(C a, C b) { return a; } }"),
            [1020]
        );
        assert_eq!(
            sorted_codes("class C { public static C operator ~(C a, C b) { return a; } }"),
            [1020]
        );
        assert!(
            sorted_codes("class C { public static C operator +(C a, C b) { return a; } }").is_empty()
        );
        assert!(sorted_codes("class C { public static C operator !(C a) { return a; } }").is_empty());
    }

    #[test]
    fn a_conditional_method_with_a_nonvoid_return_is_cs0578() {
        assert_eq!(
            sorted_codes(
                "class C { [System.Diagnostics.Conditional(\"TRACE\")] public static int M() { return 1; } }"
            ),
            [578]
        );
        assert!(sorted_codes(
            "class C { [System.Diagnostics.Conditional(\"TRACE\")] public static void M() { } }"
        )
        .is_empty());
        assert!(sorted_codes("class C { public static int M() { return 1; } }").is_empty());
    }

    #[test]
    fn deriving_from_a_sealed_class_is_cs0509() {
        assert_eq!(sorted_codes("sealed class B { } class C : B { }"), [509]);
        assert!(sorted_codes("class B { } class C : B { }").is_empty());
    }

    #[test]
    fn invalid_modifiers_on_a_top_level_type() {
        assert_eq!(sorted_codes("private class C { }"), [1527]);
        assert_eq!(sorted_codes("protected class C { }"), [1527]);
        assert_eq!(sorted_codes("new class C { }"), [106]);
        assert!(sorted_codes("internal class C { }").is_empty());
        assert!(sorted_codes("class O { private class N { } }").is_empty());
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

        assert_eq!(sorted_codes("class C { Missing f; }"), [246]);
        assert_eq!(sorted_codes("class C { volatile long f; }"), [677]);
        assert_eq!(
            sorted_codes("class C { int a = Missing.Value, b; int Get() { return a; } }"),
            [103, 169]
        );
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
    fn void_local_is_cs1547() {
        assert_eq!(
            sorted_codes("class C { static void M() { void value; } }"),
            [1547]
        );
    }

    #[test]
    fn switch_on_a_non_governing_type_is_cs0151() {
        assert_eq!(
            sorted_codes("class C { static void M(C value) { switch (value) { default: break; } } }"),
            [151]
        );
        assert!(sorted_codes(
            "class C { static void M(int x) { switch (x) { default: break; } } }"
        )
        .is_empty());
        assert!(sorted_codes(
            "enum E { A } class C { static void M(E e) { switch (e) { default: break; } } }"
        )
        .is_empty());
    }

    #[test]
    fn field_initializer_referencing_an_instance_member_is_cs0236() {
        assert_eq!(sorted_codes("class C { int a = 1; int b = a; }"), [236]);
        assert_eq!(sorted_codes("class C { int a = M(); int M() { return 1; } }"), [236]);
        assert_eq!(sorted_codes("class C { int a = P; int P { get { return 1; } } }"), [236]);
        assert_eq!(
            sorted_codes("class B { public int f = 1; } class C : B { int a = f; }"),
            [236]
        );
        assert_eq!(sorted_codes("class C { int a = 1; static int s = a; }"), [236]);
        assert_eq!(sorted_codes("class C { int a = 1; const int k = a; }"), [236]);

        assert!(!sorted_codes("class C { static int s = 1; int x = s; }").contains(&236));
        assert!(
            !sorted_codes("class C { static int SM() { return 1; } int x = SM(); }").contains(&236)
        );
        assert!(!sorted_codes(
            "class C { static int SP { get { return 1; } } int x = SP; }"
        )
        .contains(&236));
        assert!(!sorted_codes("class C { int x = 1 + 2; }").contains(&236));
        assert!(!sorted_codes(
            "class C { static C other; int first = 1; int x = other.first; }"
        )
        .contains(&236));
        assert!(sorted_codes(
            "class C { static int s = 1; int x = s; int Get() { return x; } }"
        )
        .is_empty());
    }

    #[test]
    fn switch_case_label_must_convert_to_the_governing_type() {
        assert_eq!(
            sorted_codes("class C { static void M(string x) { switch (x) { case 1: break; } } }"),
            [29]
        );
        assert_eq!(
            sorted_codes("class C { static void M(byte x) { switch (x) { case 300: break; } } }"),
            [31]
        );
        assert_eq!(
            sorted_codes("class C { static void M(char x) { switch (x) { case 65: break; } } }"),
            [266]
        );
        assert_eq!(
            sorted_codes(
                "enum E { A } class C { static void M(E x) { switch (x) { case 1: break; } } }"
            ),
            [266]
        );
        for ok in [
            "class C { static void M(int x) { switch (x) { case 5: break; } } }",
            "class C { static void M(long x) { switch (x) { case 5: break; } } }",
            "class C { static void M(byte x) { switch (x) { case 200: break; } } }",
            "class C { static void M(char x) { switch (x) { case 'a': break; } } }",
            "class C { static void M(string x) { switch (x) { case \"y\": break; case null: break; } } }",
            "enum E { A } class C { static void M(E x) { switch (x) { case E.A: break; } } }",
            "enum E { A } class C { static void M(E x) { switch (x) { case 0: break; } } }",
        ] {
            assert!(sorted_codes(ok).is_empty(), "false positive on: {ok}");
        }
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
    fn a_field_an_event_accessor_assigns_is_assigned() {
        assert_eq!(
            sorted_codes(
                "delegate void D(); class C { D _h; public event D E { add { _h = value; } remove { _h = null; } } public void Raise() { if (_h != null) _h(); } }"
            ),
            []
        );
        assert_eq!(
            sorted_codes(
                "delegate void D(); class C { D _h; public event D E { add { _h = value; } remove { _h = null; } } }"
            ),
            [414]
        );
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
                is_vararg: false,
                is_virtual: true,
                is_abstract: false,
                is_override: false,
                is_sealed: false,
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
    fn an_endless_loop_with_a_reachable_break_completes_and_a_conditional_assigns_on_both_arms() {
        let codes = |source: &str| {
            let unit = parse_compilation_unit(source).unit;
            let mut codes: Vec<u16> = bind_compilation_unit(&unit)
                .iter()
                .map(Diagnostic::code)
                .collect();
            codes.sort_unstable();
            codes.dedup();
            codes
        };

        assert_eq!(codes("class C { static int M() { while (true) { break; } } }"), [161]);
        assert_eq!(codes("class C { static int M() { for (;;) { break; } } }"), [161]);
        assert_eq!(
            codes("class C { static int M() { int v; while (true) { break; } return v; } }"),
            [165]
        );
        assert_eq!(codes("class C { static int M() { while (true) { return 1; } } }"), []);
        assert_eq!(
            codes("class C { static int M(int x) { while (true) { switch (x) { default: break; } } } }"),
            []
        );
        assert_eq!(codes("class C { static int M() { while (true) { return 1; break; } } }"), [162]);

        assert_eq!(
            codes("class C { static int M(bool c) { int v; int t = c ? (v = 1) : 0; return v + t; } }"),
            [165]
        );
        assert_eq!(
            codes("class C { static int M(bool c) { int v; return c ? (v = 1) : v; } }"),
            [165]
        );
        assert_eq!(
            codes("class C { static int M(bool c) { int v; int t = c ? (v = 1) : (v = 2); return v + t; } }"),
            []
        );
        assert_eq!(
            codes("class C { static int M(bool c) { int v; return (v = 1) == 1 ? v : v; } }"),
            []
        );
    }

    #[test]
    fn the_unsafe_gate_covers_every_unit_of_a_multi_file_compilation() {
        let bind = |sources: &[&str], option_missing: bool| {
            let units: Vec<_> = sources
                .iter()
                .map(|s| parse_compilation_unit(s).unit)
                .collect();
            let mut codes: Vec<u16> =
                bind_compilation_units_with_references_and_options(&units, &[], option_missing)
                    .iter()
                    .flatten()
                    .map(Diagnostic::code)
                    .collect();
            codes.sort_unstable();
            codes.dedup();
            codes
        };
        const SAFE: &str = "class Main2 { static int Main() { return 42; } }";
        const UNSAFE: &str = "class Poke { static unsafe void P() { } }";

        assert_eq!(bind(&[UNSAFE], true), [227]);
        assert_eq!(bind(&[SAFE, UNSAFE], true), [227]);
        assert_eq!(bind(&[UNSAFE, SAFE], true), [227]);
        assert_eq!(bind(&[UNSAFE], false), []);
        assert_eq!(bind(&[SAFE, UNSAFE], false), []);
        assert_eq!(bind(&[SAFE, "class Other { }"], true), []);
    }

    #[test]
    fn a_local_constant_does_not_outlive_its_block() {
        let codes = |body: &str| {
            let source = alloc::format!("class C {{ static int M() {{ {body} }} }}");
            let unit = parse_compilation_unit(&source).unit;
            let mut codes: Vec<u16> = bind_compilation_unit(&unit)
                .iter()
                .map(Diagnostic::code)
                .filter(|code| *code == 103)
                .collect();
            codes.sort_unstable();
            codes.dedup();
            codes
        };

        assert_eq!(codes("{ const int hidden = 42; } return hidden;"), [103]);
        assert_eq!(codes("const int k = 42; return k;"), []);
        assert_eq!(codes("const int k = 42; { return k; } "), []);
        assert_eq!(codes("{ const int k = 42; return k; } "), []);
        assert_eq!(codes("{ const int a = 1; } { const int b = 2; return b; }"), []);
        assert_eq!(
            codes("int t = 0; for (int i = 0; i < 2; i++) { const int s = 5; t += s; } return t;"),
            []
        );
    }

    #[test]
    fn a_compound_assignment_to_an_indexer_is_a_read_modify_write() {
        const INDEXER: &str = "class C { public int this[int i] { get { return 0; } set { } } \
                               static C Next() { return null; } ";

        let codes = |body: &str| {
            let source = alloc::format!("{INDEXER} static void M() {{ {body} }} }}");
            let unit = parse_compilation_unit(&source).unit;
            let mut codes: Vec<u16> = bind_compilation_unit(&unit)
                .iter()
                .map(Diagnostic::code)
                .collect();
            codes.sort_unstable();
            codes.dedup();
            codes
        };

        assert_eq!(codes("C c = new C(); c[0] += 1;"), []);
        assert_eq!(
            codes("C c = new C(); c[0] -= 1; c[0] *= 2; c[0] /= 2; c[0] %= 2;"),
            []
        );
        assert_eq!(
            codes("C c = new C(); c[0] |= 1; c[0] &= 1; c[0] ^= 1; c[0] <<= 1; c[0] >>= 1;"),
            []
        );
        assert_eq!(codes("C c = new C(); int x = (c[0] += 1); x = x;"), []);
        assert_eq!(
            codes("C c = new C(); int k = 1; c[k] += 1; c[k - 1] += 1; c[-k] += 1;"),
            []
        );

        assert_eq!(codes("Next()[0] += 1;"), [131]);
        assert_eq!(codes("C c = new C(); c[c[0]] += 1;"), [131]);
    }

    #[test]
    fn a_nested_type_of_a_referenced_type_is_nameable() {
        use crate::symbols::{Model, TypeInfo, TypeKind};

        let model_with_nested = || {
            let mut model = Model::new();
            model.insert(TypeInfo::new("System", "Object", TypeKind::Class));
            let mut outer = TypeInfo::new("Lib", "Outer", TypeKind::Class);
            outer.is_external = true;
            model.insert(outer);
            let mut nested = TypeInfo::new("Lib.Outer", "Inner", TypeKind::Class);
            nested.is_external = true;
            nested.enclosing = Some("Lib.Outer".into());
            model.insert(nested);
            model
        };
        let codes = |source: &str| {
            let unit = parse_compilation_unit(source).unit;
            let mut codes: Vec<u16> = bind_compilation_unit_with_model(&unit, model_with_nested())
                .iter()
                .map(Diagnostic::code)
                .collect();
            codes.sort_unstable();
            codes.dedup();
            codes
        };

        assert_eq!(codes("class C { void M(Lib.Outer.Inner x) { } }"), []);
        assert_eq!(codes("class C { void M(Lib.Outer x) { } }"), []);
        assert_eq!(codes("using Lib; class C { void M(Outer.Inner x) { } }"), []);
        assert_eq!(codes("class C { void M(Inner x) { } }"), [246]);
    }

    #[test]
    fn cs0507_a_protected_internal_base_takes_a_different_override_spelling_across_assemblies() {
        use crate::symbols::{Accessibility, MethodSymbol, Model, TypeInfo, TypeKind};

        fn model_with(seam_is_external: bool) -> Model {
            let mut model = Model::new();
            let mut object = TypeInfo::new("System", "Object", TypeKind::Class);
            object.methods.push(MethodSymbol {
                name: "ToString".into(),
                return_type: TypeSymbol::Special(SpecialType::String),
                parameters: Vec::new(),
                is_static: false,
                is_params: false,
                is_vararg: false,
                is_virtual: true,
                is_abstract: false,
                is_override: false,
                is_sealed: false,
                accessibility: Accessibility::Public,
                conditional: Vec::new(),
            });
            model.insert(object);
            let mut seam = TypeInfo::new("", "Seam", TypeKind::Class);
            seam.is_external = seam_is_external;
            seam.methods.push(MethodSymbol {
                name: "Read".into(),
                return_type: TypeSymbol::Special(SpecialType::Int32),
                parameters: vec![TypeSymbol::Special(SpecialType::Int32)],
                is_static: false,
                is_params: false,
                is_vararg: false,
                is_virtual: false,
                is_abstract: true,
                is_override: false,
                is_sealed: false,
                accessibility: Accessibility::ProtectedInternal,
                conditional: Vec::new(),
            });
            model.insert(seam);
            model
        }
        let codes = |source: &str, seam_is_external: bool| {
            let unit = parse_compilation_unit(source).unit;
            let mut codes: Vec<u16> =
                bind_compilation_unit_with_model(&unit, model_with(seam_is_external))
                    .iter()
                    .map(Diagnostic::code)
                    .collect();
            codes.sort_unstable();
            codes.dedup();
            codes
        };
        let drv = |spelling: &str| {
            format!("class Drv : Seam {{ {spelling} override int Read(int ch) {{ return ch; }} }}")
        };

        assert_eq!(codes(&drv("protected"), true), []);
        assert_eq!(codes(&drv("protected internal"), true), [507]);
        assert_eq!(codes(&drv("public"), true), [507]);

        assert_eq!(codes(&drv("protected internal"), false), []);
        assert_eq!(codes(&drv("protected"), false), [507]);
        assert_eq!(codes(&drv("public"), false), [507]);
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
            is_vararg: false,
            is_virtual: false,
            is_abstract: false,
            is_override: false,
            is_sealed: false,
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
