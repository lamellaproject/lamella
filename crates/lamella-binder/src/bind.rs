//! Binding the syntax tree to symbols (ECMA-334 1st ed, clauses 10-14).

use crate::special::SpecialType;
use crate::types::TypeSymbol;
use lamella_syntax::ast::{Parameter, ParameterModifier, TypeRef, TypeRefKind};

/// Binds a syntactic type reference to a [`TypeSymbol`] (11.1).
#[must_use]
pub fn bind_type(type_ref: &TypeRef) -> TypeSymbol {
    match &type_ref.kind {
        TypeRefKind::Predefined(predefined) => {
            TypeSymbol::Special(SpecialType::from_predefined(*predefined))
        }
        TypeRefKind::Name(parts) => {
            TypeSymbol::Named(parts.iter().cloned().collect()).fold_builtin()
        }
        TypeRefKind::Array { element, rank } => bind_type(element).into_array(*rank),
        TypeRefKind::Pointer(element) => {
            TypeSymbol::Pointer(alloc::boxed::Box::new(bind_type(element)))
        }
        TypeRefKind::Error => TypeSymbol::Error,
    }
}

/// Binds a formal parameter to the type its SIGNATURE carries: a `ref`/`out` parameter
/// is a byref (`T&`), distinct from `T` for overloading and duplicate-signature checks
/// (ECMA-334 1st ed 10.6, 17.5.1; ECMA-335 II.23.2.10). A `params` array is its array
/// type -- the modifier is not part of the signature (17.5.1.4).
#[must_use]
pub fn parameter_symbol(parameter: &Parameter) -> TypeSymbol {
    let ty = bind_type(&parameter.ty);
    match parameter.modifier {
        Some(ParameterModifier::Ref | ParameterModifier::Out) => {
            TypeSymbol::ByRef(alloc::boxed::Box::new(ty))
        }
        _ => ty,
    }
}

/// The DECLARATION facts for a parameter list -- each parameter's name and whether it is `ref` or
/// `out` -- parallel to the types [`parameter_symbol`] produces, and the same length by
/// construction.
///
/// SEPARATE FROM THE SIGNATURE ON PURPOSE. `parameter_symbol` answers "what does overload
/// resolution compare", and for that `ref` and `out` are the same thing (`T&`) and the name is
/// nothing at all. This answers "what did the programmer write", which is what a DIAGNOSTIC has to
/// quote: CS0181 names the offending parameter, CS7036 names the one with no argument, and CS1620
/// exists solely to tell `ref` and `out` apart. Neither question is a refinement of the other.
#[must_use]
pub fn parameter_infos(parameters: &[Parameter]) -> alloc::vec::Vec<crate::symbols::ParameterInfo> {
    use crate::symbols::{ParameterInfo, ParameterMode};
    parameters
        .iter()
        .map(|parameter| ParameterInfo {
            name: parameter.name.clone(),
            mode: match parameter.modifier {
                Some(ParameterModifier::Ref) => ParameterMode::Ref,
                Some(ParameterModifier::Out) => ParameterMode::Out,
                _ => ParameterMode::Value,
            },
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;
    use lamella_syntax::ast::StmtKind;
    use lamella_syntax::parser::parse_statement;

    /// Parses a local declaration and binds its declared type, exercising the
    /// real parser -> binder seam.
    fn bound_type(source: &str) -> TypeSymbol {
        let parsed = parse_statement(source);
        match parsed.statement.kind {
            StmtKind::LocalDeclaration { ty, .. } => bind_type(&ty),
            other => panic!("expected a local declaration, got {other:?}"),
        }
    }

    #[test]
    fn predefined_keywords_bind_to_special_types() {
        assert_eq!(
            bound_type("int x;"),
            TypeSymbol::special(SpecialType::Int32)
        );
        assert_eq!(
            bound_type("string s;"),
            TypeSymbol::special(SpecialType::String)
        );
        assert_eq!(
            bound_type("bool b;"),
            TypeSymbol::special(SpecialType::Boolean)
        );
    }

    #[test]
    fn dotted_names_bind_to_named_types() {
        assert_eq!(
            bound_type("System.IO.Stream s;").to_string(),
            "System.IO.Stream"
        );
        assert_eq!(bound_type("Widget w;").to_string(), "Widget");
    }

    #[test]
    fn array_types_nest() {
        assert_eq!(bound_type("int[] a;").to_string(), "int[]");
        assert_eq!(bound_type("int[,] m;").to_string(), "int[,]");
        assert_eq!(bound_type("string[][] j;").to_string(), "string[][]");
    }
}
