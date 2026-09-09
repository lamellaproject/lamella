//! Binding the syntax tree to symbols (ECMA-334 1st ed, clauses 10-14).

use crate::special::SpecialType;
use crate::types::TypeSymbol;
use lamella_syntax::ast::{Parameter, ParameterModifier, TypeRef, TypeRefKind};

/// The most elements one `System.ValueTuple` holds before the rest nest inside it.
///
/// ECMA-335 has no tuple; `ValueTuple` is eight ordinary generic structs, and the eighth's last
/// type parameter is `TRest`, which by convention holds another one. So a tuple of nine is
/// `ValueTuple<T1..T7, ValueTuple<T8, T9>>` -- and the runtime, `ToString`, equality and
/// `TupleElementNamesAttribute` all agree on that shape because they were all written to it.
const TUPLE_CHUNK: usize = 7;

/// The `System.ValueTuple<...>` a tuple type IS (C# 7.0, 8.3.11).
///
/// **NOT A CONVERSION AND NOT A LOWERING -- AN IDENTITY.** `(int, string)` and
/// `System.ValueTuple<int, string>` are the same type: each converts to the other by the identity
/// conversion, they share every member, and metadata has only the second name for either. So the
/// binder answers with the instantiation and no pass after this one has to know what a tuple is.
///
/// Past seven elements the rest nest in `TRest`, one chunk of seven at a time, which is what csc
/// emits and what the runtime's own `ToString` walks.
/// One `System.ValueTuple` over exactly these arguments, WITHOUT the chunking.
///
/// For a caller whose list is already in the eight-slot shape -- seven elements and a nested rest
/// -- because [`value_tuple`] would then split it a SECOND time and wrap the rest twice. That is
/// not hypothetical: it is what a fifteen-element tuple did, producing a
/// `ValueTuple<ValueTuple<int>>` where the type said `ValueTuple<int>`.
#[must_use]
pub fn value_tuple_exact(arguments: &[TypeSymbol]) -> TypeSymbol {
    TypeSymbol::Instantiation {
        definition: alloc::vec![
            alloc::boxed::Box::from("System"),
            alloc::boxed::Box::from("ValueTuple")
        ]
        .into_boxed_slice(),
        arguments: arguments.to_vec().into_boxed_slice(),
    }
}

#[must_use]
pub fn value_tuple(elements: &[TypeSymbol]) -> TypeSymbol {
    let definition: alloc::boxed::Box<[alloc::boxed::Box<str>]> =
        alloc::vec![alloc::boxed::Box::from("System"), alloc::boxed::Box::from("ValueTuple")]
            .into_boxed_slice();
    if elements.len() > TUPLE_CHUNK {
        let mut arguments: alloc::vec::Vec<TypeSymbol> = elements[..TUPLE_CHUNK].to_vec();
        arguments.push(value_tuple(&elements[TUPLE_CHUNK..]));
        return TypeSymbol::Instantiation {
            definition,
            arguments: arguments.into_boxed_slice(),
        };
    }
    TypeSymbol::Instantiation {
        definition,
        arguments: elements.to_vec().into_boxed_slice(),
    }
}

/// The element names a TUPLE TYPE was written with, or EMPTY when the type is not a tuple.
///
/// Empty means NOT A NAMED TUPLE, never "a tuple whose elements are all unnamed": a tuple that
/// named NONE of its elements has no names to record and needs none, so the two collapse and the
/// distinction has nothing to decide. An element the source left unnamed inside one that named
/// others is `None` in place -- `(int a, int)` is legal and is `[Some("a"), None]`.
///
/// SHALLOW. `((int a, int b) x, int y)` records `x` and `y`, and the inner pair's names ride with
/// the inner TYPE REFERENCE wherever that is recorded. Flattening them here would produce a list
/// that no longer lines up with the tuple's own elements, which is what every consumer indexes by.
#[must_use]
pub fn tuple_element_names(type_ref: &TypeRef) -> alloc::vec::Vec<Option<alloc::boxed::Box<str>>> {
    let TypeRefKind::Tuple(elements) = &type_ref.kind else {
        return alloc::vec::Vec::new();
    };
    if elements.iter().all(|element| element.name.is_none()) {
        return alloc::vec::Vec::new();
    }
    elements
        .iter()
        .map(|element| element.name.as_ref().map(|name| name.text.clone()))
        .collect()
}

/// Binds a syntactic type reference to a [`TypeSymbol`] (11.1).
#[must_use]
pub fn bind_type(type_ref: &TypeRef) -> TypeSymbol {
    match &type_ref.kind {
        TypeRefKind::Tuple(elements) => {
            let bound: alloc::vec::Vec<TypeSymbol> =
                elements.iter().map(|element| bind_type(&element.ty)).collect();
            value_tuple(&bound)
        }
        TypeRefKind::Predefined(predefined) => {
            TypeSymbol::Special(SpecialType::from_predefined(*predefined))
        }
        TypeRefKind::Name(parts) => {
            TypeSymbol::Named(parts.iter().cloned().collect()).fold_builtin()
        }
        TypeRefKind::Generic { parts } => TypeSymbol::Instantiation {
            definition: parts
                .iter()
                .enumerate()
                .map(|(index, part)| {
                    if index + 1 == parts.len() {
                        part.name.clone()
                    } else {
                        crate::symbols::metadata_type_name(&part.name, part.arguments.len()).into()
                    }
                })
                .collect(),
            arguments: parts
                .iter()
                .flat_map(|part| part.arguments.iter().map(bind_type))
                .collect(),
        },
        TypeRefKind::Unbound { parts, arity } => TypeSymbol::Instantiation {
            definition: parts.iter().cloned().collect(),
            arguments: (0..*arity)
                .map(|_| TypeSymbol::Special(SpecialType::Object))
                .collect(),
        },
        TypeRefKind::Nullable(underlying) => TypeSymbol::Instantiation {
            definition: ["System".into(), "Nullable".into()].into(),
            arguments: [bind_type(underlying)].into(),
        },
        TypeRefKind::Array { element, rank } => bind_type(element).into_array(*rank),
        TypeRefKind::Pointer(element) => {
            TypeSymbol::Pointer(alloc::boxed::Box::new(bind_type(element)))
        }
        TypeRefKind::ByRef { referent, .. } => {
            TypeSymbol::ByRef(alloc::boxed::Box::new(bind_type(referent)))
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
            default: parameter
                .default_value
                .as_ref()
                .and_then(crate::declaration::fold_parameter_default),
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

    /// [`bound_type`] at a dialect that admits generics -- the default one is ISO-1, where a
    /// type-argument list is refused and the tree carries the bare name instead.
    fn bound_type_at_csharp2(source: &str) -> TypeSymbol {
        use lamella_syntax::lexer::LexOptions;
        use lamella_syntax::parser::parse_compilation_unit_with;
        use lamella_syntax::version::LanguageVersion;
        let parsed = parse_compilation_unit_with(
            &alloc::format!("class Holder {{ void M() {{ {source} }} }}"),
            LexOptions {
                version: LanguageVersion::CSharp2,
                ..LexOptions::default()
            },
        );
        assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
        let lamella_syntax::ast::NamespaceMember::Type(declaration) = &parsed.unit.members[0]
        else {
            panic!("expected a type declaration");
        };
        let lamella_syntax::ast::Member::Method { body, .. } = &declaration.members[0] else {
            panic!("expected a method");
        };
        let StmtKind::Block(statements) = &body.as_ref().expect("a body").kind else {
            panic!("a method body is a block");
        };
        match &statements[0].kind {
            StmtKind::LocalDeclaration { ty, .. } => bind_type(ty),
            other => panic!("expected a local declaration, got {other:?}"),
        }
    }

    /// A NESTED type reached through a constructed enclosing one binds to ONE instantiation whose
    /// arguments are flattened enclosing-first, and whose definition keeps the ENCLOSING part's
    /// arity mangled in while leaving the last part bare (ECMA-335 II.9.2, II.10.7.2).
    ///
    /// **THE MANGLING IS THE WHOLE DISTINCTION.** Without it `List<int>.Enumerator` and
    /// `List.Enumerator<int>` -- two different types -- reach the resolver as one symbol, a
    /// definition of `["List", "Enumerator"]` with one argument. Asserted on the parts rather than
    /// on the rendering, because the rendering is derived from them.
    #[test]
    fn a_constructed_nested_name_flattens_its_arguments_and_mangles_its_enclosing_parts() {
        use alloc::vec::Vec;
        let parts = |ty: &TypeSymbol| -> (Vec<alloc::string::String>, usize) {
            match ty {
                TypeSymbol::Instantiation {
                    definition,
                    arguments,
                } => (
                    definition.iter().map(|part| part.to_string()).collect(),
                    arguments.len(),
                ),
                other => panic!("expected an instantiation, got {other:?}"),
            }
        };

        let enumerator = bound_type_at_csharp2("List<int>.Enumerator e;");
        assert_eq!(parts(&enumerator), (alloc::vec!["List`1".to_string(), "Enumerator".to_string()], 1));
        assert_eq!(enumerator.to_string(), "List<int>.Enumerator");

        let pair = bound_type_at_csharp2("Box<int>.Pair<string> p;");
        assert_eq!(parts(&pair), (alloc::vec!["Box`1".to_string(), "Pair".to_string()], 2));
        assert_eq!(pair.to_string(), "Box<int>.Pair<string>");

        let other = bound_type_at_csharp2("List.Enumerator<int> e;");
        assert_eq!(parts(&other), (alloc::vec!["List".to_string(), "Enumerator".to_string()], 1));
        assert_eq!(other.to_string(), "List.Enumerator<int>");
        assert_ne!(enumerator, other);

        assert_eq!(bound_type_at_csharp2("A.B.C c;").to_string(), "A.B.C");
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
