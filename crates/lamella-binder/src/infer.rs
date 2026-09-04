//! Inference of a generic method's type arguments -- ECMA-334 4th ed **25.6.4**.

use crate::symbols::{MethodSymbol, Model};
use crate::special::SpecialType;
use crate::types::TypeSymbol;
use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;
#[cfg(test)]
use alloc::borrow::ToOwned;

/// The type arguments inferred for `method` from `arguments`, in the method's declaration order --
/// or `None` when 25.6.4 fails for it, which is not itself an error (the method simply does not
/// participate in overload resolution).
///
/// `arguments` are the ARGUMENT types in call order, as [`argument_type`](crate::bound) produces
/// them -- so a `ref x` argument arrives as [`TypeSymbol::ByRef`], matching the parameter's own
/// spelling.
///
/// **A non-generic method is not this function's business** and yields `None`; the caller keeps it
/// as a candidate for the ordinary reason.
#[must_use]
pub(crate) fn infer_method_type_arguments(
    model: &Model,
    method: &MethodSymbol,
    arguments: &[TypeSymbol],
) -> Option<Vec<TypeSymbol>> {
    if method.type_parameters.is_empty() {
        return None;
    }
    if arguments.len() != method.parameters.len() {
        return None;
    }
    let names: BTreeSet<&str> = method
        .type_parameters
        .iter()
        .map(|parameter| &**parameter)
        .collect();
    let mut pool = Pool::new(&names);
    for (argument, parameter) in arguments.iter().zip(&method.parameters) {
        if !relate(model, argument, parameter, &mut pool) {
            return None;
        }
    }
    pool.complete(&method.type_parameters)
}

/// The type arguments inferred for a `params` `method` called in its EXPANDED form (17.5.1.4): each
/// trailing argument is related to the array's ELEMENT type. `None` when the method takes no
/// parameter array, cannot accept this many arguments expanded, or inference fails.
///
/// 25.6.4 orders the two forms -- normal first, expanded only if that fails -- and the caller keeps
/// that order. Written as a separate entry point rather than a flag, so the normal-form path stays
/// exactly the algorithm the clause describes.
///
/// **ONE NARROW GAP, STATED RATHER THAN LEFT TO BE FOUND.** The clause falls back to the expanded
/// form when *"type inference succeeds, AND the resultant method is applicable"* is false -- so it
/// covers inference SUCCEEDING and the closed method then failing to convert. The caller falls back
/// only when inference itself fails, because applicability is decided later by machinery that knows
/// nothing about this. Every shape reached so far fails in the NORMAL form first (`Sum(1)` against
/// `params T[]` fails the array rule before anything is inferred), so the gap has no known case;
/// it would under-accept, and loudly, if one appeared.
#[must_use]
pub(crate) fn infer_expanded_type_arguments(
    model: &Model,
    method: &MethodSymbol,
    arguments: &[TypeSymbol],
) -> Option<Vec<TypeSymbol>> {
    if method.type_parameters.is_empty() || !method.is_params {
        return None;
    }
    let fixed = method.parameters.len().checked_sub(1)?;
    if arguments.len() < fixed {
        return None;
    }
    let TypeSymbol::Array { element, rank: 1 } = &method.parameters[fixed] else {
        return None;
    };
    let names: BTreeSet<&str> = method
        .type_parameters
        .iter()
        .map(|parameter| &**parameter)
        .collect();
    let mut pool = Pool::new(&names);
    for (index, argument) in arguments.iter().enumerate() {
        let parameter = if index < fixed {
            &method.parameters[index]
        } else {
            &**element
        };
        if !relate(model, argument, parameter, &mut pool) {
            return None;
        }
    }
    pool.complete(&method.type_parameters)
}

/// The pooled inferences: at most one type per method type parameter, because 25.6.4 requires the
/// set to be CONSISTENT and a second, different inference for the same parameter is exactly the
/// inconsistency. Recording the conflict as a failure at the moment it appears is the same verdict
/// the clause reaches after pooling, and it keeps the failing parameter identifiable.
struct Pool<'a> {
    /// The method's own type parameter names -- the ONLY names substitution may bind. A `T` that
    /// belongs to the declaring TYPE is an ordinary type here, exactly as `!0` is not `!!0`.
    names: &'a BTreeSet<&'a str>,
    inferred: BTreeMap<&'a str, TypeSymbol>,
}

impl<'a> Pool<'a> {
    fn new(names: &'a BTreeSet<&'a str>) -> Pool<'a> {
        Pool {
            names,
            inferred: BTreeMap::new(),
        }
    }

    /// Whether `name` is one of the METHOD's own type parameters -- the only names inference may
    /// bind. A `T` belonging to the declaring type answers `false` here and is an ordinary type.
    fn is_parameter(&self, name: &str) -> bool {
        self.names.contains(name)
    }

    /// Records `ty` for the parameter named `name`; `false` when a DIFFERENT type is already
    /// recorded (the pool is inconsistent, so inference fails for the whole method) or when `name`
    /// is not one of this method's parameters at all.
    ///
    /// The key is looked up rather than taken from the caller, because the caller holds a name
    /// borrowed from the PARAMETER TYPE it is walking and the map outlives that walk.
    fn record(&mut self, name: &str, ty: &TypeSymbol) -> bool {
        let Some(&key) = self.names.get(name) else {
            return false;
        };
        match self.inferred.get(key) {
            Some(existing) => existing == ty,
            None => {
                self.inferred.insert(key, ty.clone());
                true
            }
        }
    }

    /// The pool as a full argument list, or `None` when it is INCOMPLETE -- some type parameter
    /// received no inference at all. `T Make<T>()` is the pure case: nothing in the argument list
    /// mentions `T`, every argument relates successfully, and the method is still uninferable.
    fn complete(&self, parameters: &[Box<str>]) -> Option<Vec<TypeSymbol>> {
        parameters
            .iter()
            .map(|parameter| self.inferred.get(&**parameter).cloned())
            .collect()
    }
}

/// Relates one argument type `a` to one parameter type `p`, adding to `pool`. `false` means
/// *"type inference fails for the generic method"* -- distinct from relating successfully while
/// inferring nothing, which the clause is careful to call success.
fn relate(model: &Model, a: &TypeSymbol, p: &TypeSymbol, pool: &mut Pool<'_>) -> bool {
    let (mut a, mut p) = (a, p);
    loop {
        if !involves(p, pool.names) {
            return true;
        }
        if matches!(a, TypeSymbol::Special(SpecialType::Null)) || a.is_error() {
            return true;
        }
        match p {
            TypeSymbol::Array {
                element: parameter_element,
                rank: parameter_rank,
            } => match a {
                TypeSymbol::Array {
                    element: argument_element,
                    rank,
                } if rank == parameter_rank => {
                    a = argument_element;
                    p = parameter_element;
                }
                TypeSymbol::Instantiation {
                    definition,
                    arguments,
                } if *parameter_rank == 1
                    && arguments.len() == 1
                    && is_sequence_interface(definition) =>
                {
                    a = &arguments[0];
                    p = parameter_element;
                }
                _ => return false,
            },
            TypeSymbol::Named(parts) => {
                return match parts.split_first() {
                    Some((name, [])) if pool.is_parameter(name) => pool.record(name, a),
                    _ => false,
                };
            }
            TypeSymbol::Instantiation { .. } => return relate_constructed(model, a, p, pool),
            TypeSymbol::ByRef(parameter_element) => match a {
                TypeSymbol::ByRef(argument_element) => {
                    a = argument_element;
                    p = parameter_element;
                }
                _ => return false,
            },
            TypeSymbol::Pointer(parameter_element) => match a {
                TypeSymbol::Pointer(argument_element) => {
                    a = argument_element;
                    p = parameter_element;
                }
                _ => return false,
            },
            TypeSymbol::Special(_) | TypeSymbol::Error => return true,
        }
    }
}

/// The constructed-type bullet: *"for each method type parameter MX that occurs in P, exactly one
/// type TX can be determined such that replacing each MX with each TX produces a type to which A is
/// convertible by a standard implicit conversion."*
///
/// **THE CONVERTIBILITY IS WHY THIS SEARCHES A's BASES RATHER THAN COMPARING TWO TYPES.** For
/// `Unwrap<T>(Box<T> b)` called with an `IntBox : Box<int>`, no comparison of `IntBox` against
/// `Box<T>` finds anything; what makes `T` be `int` is that `Box<int>` is the one instantiation of
/// `Box` that `IntBox` converts to. A direct `Box<int>` argument is the same search finding itself.
///
/// *"If, for a given MX, no TX exists, or MORE THAN ONE TX exists, then type inference fails ... a
/// situation where more than one TX exists can only occur if P is a generic interface type and A
/// implements multiple constructed versions of that interface."* Hence the dedupe-then-count: two
/// PATHS to `IEnumerable<int>` are one TX; `IEnumerable<int>` beside `IEnumerable<string>` is two.
fn relate_constructed(model: &Model, a: &TypeSymbol, p: &TypeSymbol, pool: &mut Pool<'_>) -> bool {
    let TypeSymbol::Instantiation {
        definition,
        arguments: parameter_arguments,
    } = p
    else {
        return false;
    };
    let mut found: Vec<Vec<TypeSymbol>> = Vec::new();
    let mut visited: Vec<TypeSymbol> = Vec::new();
    let mut pending: Vec<TypeSymbol> = alloc::vec![a.clone()];
    while let Some(current) = pending.pop() {
        if visited.contains(&current) {
            continue;
        }
        visited.push(current.clone());
        if let TypeSymbol::Instantiation {
            definition: current_definition,
            arguments,
        } = &current
        {
            if current_definition == definition && arguments.len() == parameter_arguments.len() {
                let arguments = arguments.to_vec();
                if !found.contains(&arguments) {
                    found.push(arguments);
                }
            }
        }
        let Some(info) = model.get_by_symbol(&current) else {
            continue;
        };
        for base in &info.bases {
            pending.push(base.clone());
        }
        if let Some(base) = &info.base {
            pending.push(base.clone());
        }
    }
    let [arguments] = &found[..] else {
        return false;
    };
    let arguments = arguments.clone();
    parameter_arguments
        .iter()
        .zip(&arguments)
        .all(|(parameter, argument)| relate(model, argument, parameter, pool))
}

/// Whether `IList<>`, `ICollection<>` or `IEnumerable<>` is the definition named -- the three the
/// array bullet lists, and only those three.
fn is_sequence_interface(definition: &[Box<str>]) -> bool {
    matches!(
        definition.iter().map(|part| &**part).collect::<Vec<_>>()[..],
        ["System", "Collections", "Generic", "IList" | "ICollection" | "IEnumerable"]
    )
}

/// Whether `ty` mentions any of `names` -- *"P does not involve any method type parameters"*, from
/// the other side. Recurses through every position a parameter can hide in, the same set
/// [`substitute`](crate::symbols) rewrites, because the two must agree about where a parameter can
/// be: a position this misses is one that reports "nothing to infer" and then substitutes nothing.
fn involves(ty: &TypeSymbol, names: &BTreeSet<&str>) -> bool {
    match ty {
        TypeSymbol::Named(parts) => {
            matches!(parts.split_first(), Some((name, [])) if names.contains(&**name))
        }
        TypeSymbol::Instantiation { arguments, .. } => {
            arguments.iter().any(|argument| involves(argument, names))
        }
        TypeSymbol::Array { element, .. }
        | TypeSymbol::Pointer(element)
        | TypeSymbol::ByRef(element) => involves(element, names),
        TypeSymbol::Special(_) | TypeSymbol::Error => false,
    }
}

/// A method type parameter's name as a symbol, for building the `T` a test relates against.
#[cfg(test)]
fn parameter_named(name: &str) -> TypeSymbol {
    TypeSymbol::Named(alloc::vec![name.to_owned().into_boxed_str()].into_boxed_slice())
}

#[cfg(test)]
#[allow(clippy::items_after_statements)]
mod tests {
    use super::*;
    use crate::symbols::{Model, TypeInfo, TypeKind};
    use crate::types::TypeSymbol;

    fn method(type_parameters: &[&str], parameters: Vec<TypeSymbol>) -> MethodSymbol {
        MethodSymbol {
            return_required_modifiers: Vec::new(),
            explicit_interface: None,
            name: "M".into(),
            return_type: TypeSymbol::special(SpecialType::Void),
            parameters,
            parameter_info: Vec::new(),
            is_static: true,
            is_params: false,
            is_vararg: false,
            is_virtual: false,
            is_abstract: false,
            is_override: false,
            is_sealed: false,
            accessibility: crate::symbols::Accessibility::Public,
            conditional: Vec::new(),
            sets_required_members: false,
            type_parameters: type_parameters
                .iter()
                .map(|name| (*name).to_owned().into_boxed_str())
                .collect(),
            type_parameter_constraints: Vec::new(),
        }
    }

    fn int() -> TypeSymbol {
        TypeSymbol::special(SpecialType::Int32)
    }

    fn string() -> TypeSymbol {
        TypeSymbol::special(SpecialType::String)
    }

    fn instantiation(name: &str, arguments: &[TypeSymbol]) -> TypeSymbol {
        TypeSymbol::Instantiation {
            definition: alloc::vec![name.to_owned().into_boxed_str()].into_boxed_slice(),
            arguments: arguments.to_vec().into_boxed_slice(),
        }
    }

    #[test]
    fn the_pool_must_be_complete_and_consistent_and_each_failure_is_its_own() {
        let model = Model::new();
        let t = parameter_named("T");
        let u = parameter_named("U");
        let cases: [(&str, MethodSymbol, Vec<TypeSymbol>, Option<Vec<TypeSymbol>>); 8] = [
            (
                "one parameter, one argument",
                method(&["T"], alloc::vec![t.clone()]),
                alloc::vec![int()],
                Some(alloc::vec![int()]),
            ),
            (
                "two arguments agreeing",
                method(&["T"], alloc::vec![t.clone(), t.clone()]),
                alloc::vec![int(), int()],
                Some(alloc::vec![int()]),
            ),
            (
                "two arguments disagreeing -- INCONSISTENT",
                method(&["T"], alloc::vec![t.clone(), t.clone()]),
                alloc::vec![int(), string()],
                None,
            ),
            (
                "two parameters, one each",
                method(&["T", "U"], alloc::vec![t.clone(), u.clone()]),
                alloc::vec![int(), string()],
                Some(alloc::vec![int(), string()]),
            ),
            (
                "a parameter nothing mentions -- INCOMPLETE",
                method(&["T", "U"], alloc::vec![t.clone()]),
                alloc::vec![int()],
                None,
            ),
            (
                "no parameters at all -- INCOMPLETE",
                method(&["T"], Vec::new()),
                Vec::new(),
                None,
            ),
            (
                "argument count differs -- immediate failure",
                method(&["T"], alloc::vec![t.clone()]),
                alloc::vec![int(), int()],
                None,
            ),
            (
                "the null type infers nothing, so the pool is INCOMPLETE",
                method(&["T"], alloc::vec![t.clone()]),
                alloc::vec![TypeSymbol::special(SpecialType::Null)],
                None,
            ),
        ];
        for (label, symbol, arguments, expected) in cases {
            assert_eq!(
                infer_method_type_arguments(&model, &symbol, &arguments),
                expected,
                "{label}"
            );
        }
    }

    #[test]
    fn an_ordinary_parameter_beside_a_generic_one_infers_nothing_and_blocks_nothing() {
        let model = Model::new();
        let symbol = method(&["T"], alloc::vec![parameter_named("T"), int()]);
        assert_eq!(
            infer_method_type_arguments(&model, &symbol, &alloc::vec![string(), int()]),
            Some(alloc::vec![string()])
        );
        assert_eq!(
            infer_method_type_arguments(&model, &symbol, &alloc::vec![string(), string()]),
            Some(alloc::vec![string()])
        );
    }

    #[test]
    fn an_array_parameter_peels_rank_by_rank_and_refuses_a_non_array() {
        let model = Model::new();
        let t = parameter_named("T");
        let one = method(&["T"], alloc::vec![t.clone().into_array(1)]);
        assert_eq!(
            infer_method_type_arguments(&model, &one, &alloc::vec![int().into_array(1)]),
            Some(alloc::vec![int()])
        );
        let two = method(&["T"], alloc::vec![t.clone().into_array(1).into_array(1)]);
        assert_eq!(
            infer_method_type_arguments(
                &model,
                &two,
                &alloc::vec![int().into_array(1).into_array(1)]
            ),
            Some(alloc::vec![int()])
        );
        assert_eq!(
            infer_method_type_arguments(&model, &one, &alloc::vec![int().into_array(2)]),
            None
        );
        assert_eq!(
            infer_method_type_arguments(&model, &one, &alloc::vec![int()]),
            None
        );
    }

    #[test]
    fn an_array_parameter_takes_its_element_from_a_sequence_interface_and_only_those_three() {
        let model = Model::new();
        let symbol = method(&["T"], alloc::vec![parameter_named("T").into_array(1)]);
        let sequence = |name: &str| TypeSymbol::Instantiation {
            definition: ["System", "Collections", "Generic", name]
                .iter()
                .map(|part| (*part).to_owned().into_boxed_str())
                .collect(),
            arguments: alloc::vec![int()].into_boxed_slice(),
        };
        for name in ["IList", "ICollection", "IEnumerable"] {
            assert_eq!(
                infer_method_type_arguments(&model, &symbol, &alloc::vec![sequence(name)]),
                Some(alloc::vec![int()]),
                "{name}"
            );
        }
        assert_eq!(
            infer_method_type_arguments(
                &model,
                &symbol,
                &alloc::vec![TypeSymbol::Instantiation {
                    definition: ["System", "Collections", "Generic", "ISet"]
                        .iter()
                        .map(|part| (*part).to_owned().into_boxed_str())
                        .collect(),
                    arguments: alloc::vec![int()].into_boxed_slice(),
                }]
            ),
            None
        );
    }

    #[test]
    fn a_constructed_parameter_matches_through_a_base_class_and_nests() {
        let mut model = Model::new();
        let mut box_definition = TypeInfo::new("", "Box`1", TypeKind::Class);
        box_definition.type_parameters = alloc::vec!["T".to_owned().into_boxed_str()];
        model.insert(box_definition);
        let mut int_box = TypeInfo::new("", "IntBox", TypeKind::Class);
        int_box.base = Some(instantiation("Box", &[int()]));
        int_box.bases = alloc::vec![instantiation("Box", &[int()])];
        model.insert(int_box);

        let symbol = method(
            &["T"],
            alloc::vec![instantiation("Box", &[parameter_named("T")])],
        );
        assert_eq!(
            infer_method_type_arguments(&model, &symbol, &alloc::vec![instantiation("Box", &[int()])]),
            Some(alloc::vec![int()])
        );
        assert_eq!(
            infer_method_type_arguments(
                &model,
                &symbol,
                &alloc::vec![TypeSymbol::Named(
                    alloc::vec!["IntBox".to_owned().into_boxed_str()].into_boxed_slice()
                )]
            ),
            Some(alloc::vec![int()])
        );
        let nested = method(
            &["T"],
            alloc::vec![instantiation(
                "Box",
                &[instantiation("Box", &[parameter_named("T")])]
            )],
        );
        assert_eq!(
            infer_method_type_arguments(
                &model,
                &nested,
                &alloc::vec![instantiation("Box", &[instantiation("Box", &[int()])])]
            ),
            Some(alloc::vec![int()])
        );
        assert_eq!(
            infer_method_type_arguments(&model, &symbol, &alloc::vec![string()]),
            None
        );
    }

    #[test]
    fn two_constructed_versions_of_one_interface_are_ambiguous_and_two_paths_to_one_are_not() {
        let mut model = Model::new();
        let mut interface = TypeInfo::new("", "IHas`1", TypeKind::Interface);
        interface.type_parameters = alloc::vec!["T".to_owned().into_boxed_str()];
        model.insert(interface);

        let mut two_versions = TypeInfo::new("", "Both", TypeKind::Class);
        two_versions.bases = alloc::vec![
            instantiation("IHas", &[int()]),
            instantiation("IHas", &[string()]),
        ];
        model.insert(two_versions);

        let mut middle = TypeInfo::new("", "Middle", TypeKind::Class);
        middle.bases = alloc::vec![instantiation("IHas", &[int()])];
        model.insert(middle);
        let mut diamond = TypeInfo::new("", "Diamond", TypeKind::Class);
        diamond.base = Some(TypeSymbol::Named(
            alloc::vec!["Middle".to_owned().into_boxed_str()].into_boxed_slice(),
        ));
        diamond.bases = alloc::vec![
            TypeSymbol::Named(alloc::vec!["Middle".to_owned().into_boxed_str()].into_boxed_slice()),
            instantiation("IHas", &[int()]),
        ];
        model.insert(diamond);

        let symbol = method(
            &["T"],
            alloc::vec![instantiation("IHas", &[parameter_named("T")])],
        );
        let named = |name: &str| {
            TypeSymbol::Named(alloc::vec![name.to_owned().into_boxed_str()].into_boxed_slice())
        };
        assert_eq!(
            infer_method_type_arguments(&model, &symbol, &alloc::vec![named("Both")]),
            None,
            "two constructed versions -- more than one TX"
        );
        assert_eq!(
            infer_method_type_arguments(&model, &symbol, &alloc::vec![named("Diamond")]),
            Some(alloc::vec![int()]),
            "two paths to ONE version is one TX"
        );
    }

    #[test]
    fn a_byref_parameter_relates_referent_to_referent() {
        let model = Model::new();
        let symbol = method(
            &["T"],
            alloc::vec![
                TypeSymbol::ByRef(Box::new(parameter_named("T"))),
                parameter_named("T"),
            ],
        );
        assert_eq!(
            infer_method_type_arguments(
                &model,
                &symbol,
                &alloc::vec![TypeSymbol::ByRef(Box::new(int())), int()]
            ),
            Some(alloc::vec![int()])
        );
        assert_eq!(
            infer_method_type_arguments(&model, &symbol, &alloc::vec![int(), int()]),
            None
        );
    }

    #[test]
    fn the_expanded_form_relates_each_trailing_argument_to_the_element_type() {
        let model = Model::new();
        let mut symbol = method(&["T"], alloc::vec![parameter_named("T").into_array(1)]);
        symbol.is_params = true;
        assert_eq!(
            infer_method_type_arguments(&model, &symbol, &alloc::vec![int(), int(), int()]),
            None,
            "the normal form fails on argument count"
        );
        assert_eq!(
            infer_expanded_type_arguments(&model, &symbol, &alloc::vec![int(), int(), int()]),
            Some(alloc::vec![int()])
        );
        assert_eq!(
            infer_expanded_type_arguments(&model, &symbol, &alloc::vec![int(), string()]),
            None
        );
        symbol.is_params = false;
        assert_eq!(
            infer_expanded_type_arguments(&model, &symbol, &alloc::vec![int(), int()]),
            None
        );
    }

    #[test]
    fn the_declaring_types_parameter_is_not_the_methods_and_is_never_inferred() {
        let model = Model::new();
        let symbol = method(
            &["U"],
            alloc::vec![parameter_named("U"), parameter_named("T")],
        );
        assert_eq!(
            infer_method_type_arguments(&model, &symbol, &alloc::vec![string(), int()]),
            Some(alloc::vec![string()]),
            "U is inferred from its own argument; T is an ordinary type here"
        );
    }
}
