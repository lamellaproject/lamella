//! Binding the syntax tree to symbols (ECMA-334 1st ed, clauses 10-14).

use crate::special::SpecialType;
use crate::types::TypeSymbol;
use lamella_syntax::ast::{TypeRef, TypeRefKind};

/// Binds a syntactic type reference to a [`TypeSymbol`] (11.1).
#[must_use]
pub fn bind_type(type_ref: &TypeRef) -> TypeSymbol {
    match &type_ref.kind {
        TypeRefKind::Predefined(predefined) => {
            TypeSymbol::Special(SpecialType::from_predefined(*predefined))
        }
        TypeRefKind::Name(parts) => TypeSymbol::Named(parts.iter().cloned().collect()),
        TypeRefKind::Array { element, rank } => bind_type(element).into_array(*rank),
        TypeRefKind::Pointer(element) => {
            TypeSymbol::Pointer(alloc::boxed::Box::new(bind_type(element)))
        }
        TypeRefKind::Error => TypeSymbol::Error,
    }
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
