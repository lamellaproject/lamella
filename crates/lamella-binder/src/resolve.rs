//! Type-name resolution against the reference world (ECMA-334 1st ed, 10.8, 11.1).

use crate::diagnostic::{Diagnostic, DiagnosticKind};
use crate::types::TypeSymbol;
use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::vec::Vec;
use lamella_syntax::span::Span;

/// The set of named types in scope, keyed by namespace then simple name.
#[derive(Debug, Default, Clone)]
pub struct TypeTable {
    by_namespace: BTreeMap<String, BTreeSet<String>>,
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
        self.by_namespace
            .entry(namespace.into())
            .or_default()
            .insert(name.into());
    }

    /// Whether a type with `namespace` and `name` is in scope.
    #[must_use]
    pub fn contains(&self, namespace: &str, name: &str) -> bool {
        self.by_namespace
            .get(namespace)
            .is_some_and(|names| names.contains(name))
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
            "The type or namespace name 'Nope' could not be found"
        );
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
