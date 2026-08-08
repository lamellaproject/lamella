//! Type-name resolution against the reference world (ECMA-334 1st ed, 10.8, 11.1).

use crate::diagnostic::{Diagnostic, DiagnosticKind, GenericMember};
use crate::types::TypeSymbol;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use lamella_syntax::span::Span;

/// The set of named types in scope, keyed by namespace then simple name.
///
/// A generic type is keyed by its METADATA name, arity mangled in
/// ([`crate::symbols::metadata_type_name`]), so `Box`, `` Box`1 `` and `` Box`2 `` are three
/// separate entries -- which is what they are. Each entry carries the type's declared
/// type-parameter NAMES, for the one diagnostic that quotes them (CS0305).
#[derive(Debug, Default, Clone)]
pub struct TypeTable {
    by_namespace: BTreeMap<String, BTreeMap<String, Vec<Box<str>>>>,
}

impl TypeTable {
    /// An empty table.
    #[must_use]
    pub fn new() -> TypeTable {
        TypeTable::default()
    }

    /// Records that a type with `namespace` (empty for the global namespace) and
    /// `name` is in scope.
    pub fn insert(&mut self, namespace: &str, name: &str) {
        self.insert_generic(namespace, name, Vec::new());
    }

    /// Records a type along with its declared type-parameter names. `name` is the METADATA name,
    /// so it already carries the arity; `parameters` is only what CS0305 quotes back.
    pub fn insert_generic(&mut self, namespace: &str, name: &str, parameters: Vec<Box<str>>) {
        self.by_namespace
            .entry(namespace.into())
            .or_default()
            .insert(name.into(), parameters);
    }

    /// Takes a type back out of scope.
    ///
    /// For a name that is in scope only for part of a compilation -- a generic declaration's type
    /// parameters, which are types inside their own declaration and nowhere else.
    pub fn remove(&mut self, namespace: &str, name: &str) {
        if let Some(names) = self.by_namespace.get_mut(namespace) {
            names.remove(name);
        }
    }

    /// Whether a type with `namespace` and `name` is in scope.
    #[must_use]
    pub fn contains(&self, namespace: &str, name: &str) -> bool {
        self.by_namespace
            .get(namespace)
            .is_some_and(|names| names.contains_key(name))
    }

    /// The arities of `name` that ARE in scope in `namespace`, each with that declaration's
    /// type-parameter names -- the candidates for a use site whose own arity found nothing.
    /// An entry of arity 0 is the non-generic type of that name.
    #[must_use]
    pub fn candidates(&self, namespace: &str, name: &str) -> Vec<(usize, &[Box<str>])> {
        let Some(names) = self.by_namespace.get(namespace) else {
            return Vec::new();
        };
        let mut found = Vec::new();
        if let Some(parameters) = names.get(name) {
            found.push((0, parameters.as_slice()));
        }
        let prefix = alloc::format!("{name}`");
        for (key, parameters) in names.range(prefix.clone()..) {
            let Some(arity) = key.strip_prefix(&prefix) else {
                break;
            };
            if let Ok(arity) = arity.parse::<usize>() {
                found.push((arity, parameters.as_slice()));
            }
        }
        found
    }
}

/// Resolves `ty` against `table`, confirming named types exist (11.1). Reports
/// `CS0246` for an unknown name and returns the error type so binding continues.
#[must_use]
pub fn resolve_type(
    table: &TypeTable,
    ty: &TypeSymbol,
    diagnostics: &mut Vec<Diagnostic>,
    span: Span,
) -> TypeSymbol {
    match ty {
        TypeSymbol::Special(_) | TypeSymbol::Error => ty.clone(),
        TypeSymbol::Named(parts) => {
            let (namespace, name) = split_name(parts);
            if table.contains(&namespace, name) {
                ty.clone()
            } else {
                diagnostics.push(Diagnostic::new(
                    DiagnosticKind::TypeNotFound {
                        name: dotted(parts),
                    },
                    span,
                ));
                TypeSymbol::Error
            }
        }
        TypeSymbol::Instantiation {
            definition,
            arguments,
        } => {
            let mut resolved = Vec::with_capacity(arguments.len());
            let mut failed = false;
            for argument in arguments {
                let argument = resolve_type(table, argument, diagnostics, span);
                failed |= argument.is_error();
                resolved.push(argument);
            }
            if let Some(diagnostic) = definition_refusal(table, definition, arguments.len()) {
                diagnostics.push(Diagnostic::new(diagnostic, span));
                return TypeSymbol::Error;
            }
            if failed {
                return TypeSymbol::Error;
            }
            TypeSymbol::Instantiation {
                definition: definition.clone(),
                arguments: resolved.into(),
            }
        }
        TypeSymbol::Array { element, rank } => {
            let resolved = resolve_type(table, element, diagnostics, span);
            if resolved.is_error() {
                TypeSymbol::Error
            } else {
                resolved.into_array(*rank)
            }
        }
        TypeSymbol::Pointer(element) => {
            let resolved = resolve_type(table, element, diagnostics, span);
            if resolved.is_error() {
                TypeSymbol::Error
            } else {
                TypeSymbol::Pointer(alloc::boxed::Box::new(resolved))
            }
        }
        TypeSymbol::ByRef(element) => {
            let resolved = resolve_type(table, element, diagnostics, span);
            if resolved.is_error() {
                TypeSymbol::Error
            } else {
                TypeSymbol::ByRef(alloc::boxed::Box::new(resolved))
            }
        }
    }
}

/// Why a generic use site's DEFINITION cannot be used at `arity`, or `None` when it can.
/// The three answers are csc's, measured rather than reconstructed:
///
/// | the name in scope | answer |
/// |---|---|
/// | at this arity | `None` -- it resolves |
/// | at some OTHER arity | **CS0305**, quoting one candidate and its parameter count |
/// | at arity 0 only | **CS0308** -- a non-generic type used with type arguments |
/// | not at all | CS0246, as any unknown name |
fn definition_refusal(
    table: &TypeTable,
    definition: &[Box<str>],
    arity: usize,
) -> Option<DiagnosticKind> {
    let (namespace, name) = split_name(definition);
    let metadata_name = crate::symbols::metadata_type_name(name, arity);
    if table.contains(&namespace, &metadata_name) {
        return None;
    }
    let candidates = table.candidates(&namespace, name);
    match candidates.iter().find(|&&(candidate, _)| candidate != 0) {
        Some(&(required, parameters)) => Some(DiagnosticKind::GenericArityMismatch {
            candidate: quote_candidate(name, required, parameters),
            required,
            member: GenericMember::Type,
        }),
        None if !candidates.is_empty() => Some(DiagnosticKind::NonGenericTypeWithTypeArguments {
            name: dotted(definition),
            member: GenericMember::Type,
        }),
        None => Some(DiagnosticKind::TypeNotFound {
            name: dotted(definition),
        }),
    }
}

/// A CS0305 candidate as csc quotes it: `Box<T>`, the simple name with the declared parameter
/// names, `, `-separated.
fn quote_candidate(name: &str, required: usize, parameters: &[Box<str>]) -> Box<str> {
    let mut text = String::from(name);
    text.push('<');
    if parameters.len() == required {
        for (index, parameter) in parameters.iter().enumerate() {
            if index > 0 {
                text.push_str(", ");
            }
            text.push_str(parameter);
        }
    } else {
        for index in 1..required {
            let _ = index;
            text.push(',');
        }
    }
    text.push('>');
    text.into()
}

/// Splits a dotted name into its namespace (the leading parts joined by `.`) and
/// its simple name (the last part).
fn split_name(parts: &[Box<str>]) -> (String, &str) {
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

/// The whole dotted name, as written, for a diagnostic.
fn dotted(parts: &[Box<str>]) -> Box<str> {
    let mut text = String::new();
    for part in parts {
        if !text.is_empty() {
            text.push('.');
        }
        text.push_str(part);
    }
    text.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::special::SpecialType;
    use alloc::string::ToString;

    fn world() -> TypeTable {
        let mut table = TypeTable::new();
        table.insert("System", "String");
        table.insert("System.IO", "Stream");
        table.insert("", "Widget");
        table
    }

    fn named(parts: &[&str]) -> TypeSymbol {
        TypeSymbol::Named(parts.iter().map(|&p| p.into()).collect())
    }

    #[test]
    fn known_named_types_resolve() {
        let table = world();
        let mut diagnostics = Vec::new();
        let resolved = resolve_type(
            &table,
            &named(&["System", "String"]),
            &mut diagnostics,
            Span::empty_at(0),
        );
        assert_eq!(resolved, named(&["System", "String"]));
        assert!(
            !resolve_type(
                &table,
                &named(&["Widget"]),
                &mut diagnostics,
                Span::empty_at(0)
            )
            .is_error()
        );
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn unknown_named_types_are_cs0246() {
        let table = world();
        let mut diagnostics = Vec::new();
        let resolved = resolve_type(
            &table,
            &named(&["Nope"]),
            &mut diagnostics,
            Span::empty_at(0),
        );
        assert!(resolved.is_error());
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code(), 246);
        assert_eq!(
            diagnostics[0].kind.to_string(),
            "The type or namespace name 'Nope' could not be found \
             (are you missing a using directive or an assembly reference?)"
        );
    }

    fn instantiation(definition: &[&str], arguments: &[TypeSymbol]) -> TypeSymbol {
        TypeSymbol::Instantiation {
            definition: definition.iter().map(|&p| p.into()).collect(),
            arguments: arguments.to_vec().into(),
        }
    }

    #[test]
    fn an_instantiation_resolves_its_definition_by_the_one_metadata_spelling() {
        let mut table = world();
        table.insert("System.Collections.Generic", "List`1");
        table.insert_generic("", "Box`1", vec!["T".into()]);

        let mut diagnostics = Vec::new();
        let int = TypeSymbol::special(SpecialType::Int32);
        let list = instantiation(&["System", "Collections", "Generic", "List"], &[int.clone()]);
        assert_eq!(
            resolve_type(&table, &list, &mut diagnostics, Span::empty_at(0)),
            list
        );
        let boxed = instantiation(&["Box"], &[int.clone()]);
        assert_eq!(
            resolve_type(&table, &boxed, &mut diagnostics, Span::empty_at(0)),
            boxed
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");

        let absent = instantiation(&["Missing"], &[int.clone()]);
        assert!(resolve_type(&table, &absent, &mut diagnostics, Span::empty_at(0)).is_error());
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code(), 246);
        assert!(diagnostics[0].kind.to_string().contains("'Missing'"));
    }

    #[test]
    fn a_wrong_arity_is_cs0305_and_names_the_candidate_csc_names() {
        let mut table = world();
        table.insert_generic("", "Box`1", vec!["T".into()]);
        table.insert("System.Collections.Generic", "List`1");
        let int = TypeSymbol::special(SpecialType::Int32);
        let mut diagnostics = Vec::new();

        let two = instantiation(&["Box"], &[int.clone(), int.clone()]);
        assert!(resolve_type(&table, &two, &mut diagnostics, Span::empty_at(0)).is_error());
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].code(), 305);
        assert_eq!(
            diagnostics[0].kind.to_string(),
            "Using the generic type 'Box<T>' requires 1 type arguments"
        );

        diagnostics.clear();
        let list_two = instantiation(
            &["System", "Collections", "Generic", "List"],
            &[int.clone(), int.clone()],
        );
        assert!(resolve_type(&table, &list_two, &mut diagnostics, Span::empty_at(0)).is_error());
        assert_eq!(diagnostics[0].code(), 305);
        assert_eq!(
            diagnostics[0].kind.to_string(),
            "Using the generic type 'List<>' requires 1 type arguments"
        );

        diagnostics.clear();
        let widget = instantiation(&["Widget"], &[int.clone()]);
        assert!(resolve_type(&table, &widget, &mut diagnostics, Span::empty_at(0)).is_error());
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code(), 308);
        assert_eq!(
            diagnostics[0].kind.to_string(),
            "The non-generic type 'Widget' cannot be used with type arguments"
        );

        diagnostics.clear();
        let nowhere = instantiation(&["Nowhere"], &[int]);
        assert!(resolve_type(&table, &nowhere, &mut diagnostics, Span::empty_at(0)).is_error());
        assert_eq!(diagnostics[0].code(), 246);
    }

    #[test]
    fn an_instantiations_arguments_are_resolved_too() {
        let mut table = world();
        table.insert_generic("", "Box`1", vec!["T".into()]);
        let mut diagnostics = Vec::new();

        let bad = instantiation(&["Box"], &[named(&["Nope"])]);
        assert!(resolve_type(&table, &bad, &mut diagnostics, Span::empty_at(0)).is_error());
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code(), 246);
        assert!(diagnostics[0].kind.to_string().contains("'Nope'"));

        diagnostics.clear();
        let nested = instantiation(&["Box"], &[instantiation(&["Box"], &[named(&["Nope"])])]);
        assert!(resolve_type(&table, &nested, &mut diagnostics, Span::empty_at(0)).is_error());
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code(), 246);
    }

    #[test]
    fn predefined_and_arrays() {
        let table = world();
        let mut diagnostics = Vec::new();
        let int = TypeSymbol::Special(SpecialType::Int32);
        assert_eq!(
            resolve_type(&table, &int, &mut diagnostics, Span::empty_at(0)),
            int
        );
        let widget_array = named(&["Widget"]).into_array(1);
        assert_eq!(
            resolve_type(&table, &widget_array, &mut diagnostics, Span::empty_at(0)),
            widget_array
        );
        let bad = named(&["Nope"]).into_array(2);
        assert!(resolve_type(&table, &bad, &mut diagnostics, Span::empty_at(0)).is_error());
        assert!(diagnostics.iter().any(|d| d.code() == 246));
    }
}
