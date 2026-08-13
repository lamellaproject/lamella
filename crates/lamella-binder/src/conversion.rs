//! Implicit conversions (ECMA-334 1st ed, 13.1).

use crate::special::SpecialType;
use crate::symbols::{Model, TypeKind};
use crate::types::TypeSymbol;
use alloc::vec::Vec;

/// Whether an implicit conversion exists from `from` to `to`, including the
/// reference conversions that walk `model`'s inheritance graph (13.1).
#[must_use]
pub fn converts(model: &Model, from: &TypeSymbol, to: &TypeSymbol) -> bool {
    if matches!(from, TypeSymbol::Special(SpecialType::Void)) {
        return false;
    }
    if matches!(from, TypeSymbol::Named(parts) if parts.len() == 1 && &**parts.first().expect("len checked") == "__arglist")
    {
        return false;
    }
    if let TypeSymbol::ByRef(element) = to {
        return from == element.as_ref();
    }
    if matches!(from, TypeSymbol::Special(SpecialType::Null)) {
        return is_reference_type(model, to)
            || matches!(to, TypeSymbol::Special(SpecialType::Null))
            || matches!(to, TypeSymbol::Pointer(_));
    }
    has_implicit_conversion(from, to)
        || reference_conversion(model, from, to)
        || delegate_to_base(model, from, to)
}

/// Every delegate type derives from `System.MulticastDelegate` (and so `System.Delegate`),
/// an implicit reference conversion the reference model does not spell out -- so a delegate
/// argument satisfies a `Delegate` parameter (e.g. `Delegate.Combine`).
fn delegate_to_base(model: &Model, from: &TypeSymbol, to: &TypeSymbol) -> bool {
    (is_system_type(to, "Delegate") || is_system_type(to, "MulticastDelegate"))
        && model
            .get_by_symbol(from)
            .is_some_and(|info| info.kind == TypeKind::Delegate)
}

/// Whether `ty` is the named BCL type `System.<name>`.
fn is_system_type(ty: &TypeSymbol, name: &str) -> bool {
    matches!(ty, TypeSymbol::Named(parts)
        if parts.len() == 2 && &*parts[0] == "System" && &*parts[1] == name)
}

/// Whether an explicit conversion (a cast) exists from `from` to `to` (13.2): any
/// implicit conversion, the reverse of one (numeric narrowing, a reference
/// downcast), any numeric-to-numeric conversion, or a cast to/from `object`
/// (boxing/unboxing and reference downcast). User-defined and enum casts follow.
#[must_use]
pub fn can_cast(model: &Model, from: &TypeSymbol, to: &TypeSymbol) -> bool {
    converts(model, from, to)
        || converts(model, to, from)
        || (is_numeric_type(from) && is_numeric_type(to))
        || is_object(from)
        || is_object(to)
        || enum_cast(model, from, to)
        || interface_cast(model, from, to)
        || pointer_cast(from, to)
}

/// Explicit conversions involving pointers (unsafe, 18.4): any pointer to/from any other pointer,
/// and a pointer to/from an integer type. The integer set is exactly sbyte/byte/short/ushort/int/
/// uint/long/ulong -- NOT char, floating-point, bool, or decimal, each of which csc rejects
/// (CS0030). Using `is_numeric` here wrongly accepted `(char)p`/`(float)p` and their inverses.
fn pointer_cast(from: &TypeSymbol, to: &TypeSymbol) -> bool {
    let from_ptr = matches!(from, TypeSymbol::Pointer(_));
    let to_ptr = matches!(to, TypeSymbol::Pointer(_));
    (from_ptr && (to_ptr || is_pointer_integer(to)))
        || (to_ptr && (from_ptr || is_pointer_integer(from)))
}

/// The integer types a pointer converts to/from (18.4): sbyte/byte/short/ushort/int/uint/long/
/// ulong. Unlike [`SpecialType::is_integral`], `char` is NOT included -- 18.4 lists only the eight
/// signed/unsigned integer types.
fn is_pointer_integer(ty: &TypeSymbol) -> bool {
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

/// The explicit conversions involving enums (13.2.2): an enum to and from any
/// integral type, and an enum to another enum.
fn enum_cast(model: &Model, from: &TypeSymbol, to: &TypeSymbol) -> bool {
    let from_enum = is_enum(model, from);
    let to_enum = is_enum(model, to);
    (from_enum && (to_enum || is_numeric_type(to))) || (to_enum && is_numeric_type(from))
}

fn is_enum(model: &Model, ty: &TypeSymbol) -> bool {
    model
        .get_by_symbol(ty)
        .is_some_and(|info| info.kind == TypeKind::Enum)
}

/// The explicit reference conversions that involve an interface (13.2.3). None of these can be
/// decided at compile time, because the run-time type may implement more than the static type
/// says: an UNSEALED class casts to any interface (a derived class could implement it), an
/// interface casts to any unsealed class, and an interface casts to any other interface (one
/// object may implement both). The conversion is checked at run time -- `castclass` -- which is
/// the point of allowing it.
///
/// A SEALED type is the exception and stays `CS0030`: a `sealed` class, a struct or an enum has
/// no derived type left to supply the implementation, so if it does not already implement the
/// interface the cast can never succeed. `converts` has already accepted the cases where it DOES
/// implement it, so reaching here means it does not.
fn interface_cast(model: &Model, from: &TypeSymbol, to: &TypeSymbol) -> bool {
    let from_interface = is_interface(model, from);
    let to_interface = is_interface(model, to);
    if from_interface && to_interface {
        return true;
    }
    if from_interface {
        return is_unsealed_class(model, to);
    }
    if to_interface {
        return is_unsealed_class(model, from);
    }
    false
}

fn is_interface(model: &Model, ty: &TypeSymbol) -> bool {
    model
        .get_by_symbol(ty)
        .is_some_and(|info| info.kind == TypeKind::Interface)
}

/// A class that is not `sealed`, so a type derived from it could still implement an interface it
/// does not. A struct and an enum are implicitly sealed and answer false.
fn is_unsealed_class(model: &Model, ty: &TypeSymbol) -> bool {
    model
        .get_by_symbol(ty)
        .is_some_and(|info| info.kind == TypeKind::Class && !info.is_sealed)
}

fn is_numeric_type(ty: &TypeSymbol) -> bool {
    matches!(ty, TypeSymbol::Special(special) if special.is_numeric())
}

/// The named types an array implicitly converts to (13.1.4): System.Array, ICloneable, and
/// the non-generic IList / ICollection / IEnumerable.
fn is_array_base_type(to: &TypeSymbol) -> bool {
    let TypeSymbol::Named(parts) = to else {
        return false;
    };
    let joined: Vec<&str> = parts.iter().map(|part| &**part).collect();
    matches!(
        joined.as_slice(),
        ["System", "Array"]
            | ["System", "ICloneable"]
            | ["System", "Collections", "IList" | "ICollection" | "IEnumerable"]
    )
}

fn is_object(ty: &TypeSymbol) -> bool {
    matches!(ty, TypeSymbol::Special(SpecialType::Object))
}

/// Whether `ty` is a reference type (4.2) -- the test array covariance (13.1.4) applies to
/// both element types: `object`/`string`, any array, or a class/interface/delegate; never a
/// value type (numeric/bool/char/struct/enum) or pointer.
pub(crate) fn is_reference_type(model: &Model, ty: &TypeSymbol) -> bool {
    match ty {
        TypeSymbol::Special(special) => {
            matches!(special, SpecialType::Object | SpecialType::String)
        }
        TypeSymbol::Array { .. } => true,
        TypeSymbol::Named(_) | TypeSymbol::Instantiation { .. } => {
            model.get_by_symbol(ty).is_some_and(|info| {
                matches!(
                    info.kind,
                    TypeKind::Class | TypeKind::Interface | TypeKind::Delegate
                )
            })
        }
        TypeSymbol::Pointer(_) | TypeSymbol::ByRef(_) | TypeSymbol::Error => false,
    }
}

/// An implicit reference conversion from `from` to a base class or implemented
/// interface, transitively (13.1.4).
fn reference_conversion(model: &Model, from: &TypeSymbol, to: &TypeSymbol) -> bool {
    if let TypeSymbol::Array {
        element: from_element,
        rank: from_rank,
    } = from
    {
        if let TypeSymbol::Array {
            element: to_element,
            rank: to_rank,
        } = to
        {
            return from_rank == to_rank
                && is_reference_type(model, from_element)
                && is_reference_type(model, to_element)
                && converts(model, from_element, to_element);
        }
        return is_array_base_type(to);
    }
    is_base_type_of(model, to, from)
}

/// Whether `base` is reachable from `derived` through the model's base list, transitively --
/// a base class, or an interface `derived` implements (the model carries both in `bases`).
///
/// One walk with two callers on purpose: the implicit reference conversion (13.1.4) and the
/// test in [`no_conversion_operator_can_exist`] ask the same question of the same graph, and
/// two spellings of it would be free to drift apart.
fn is_base_type_of(model: &Model, base: &TypeSymbol, derived: &TypeSymbol) -> bool {
    let mut stack: Vec<TypeSymbol> = match model.get_by_symbol(derived) {
        Some(info) => info.bases.to_vec(),
        None => return false,
    };
    let mut seen: Vec<TypeSymbol> = Vec::new();
    while let Some(ty) = stack.pop() {
        if &ty == base {
            return true;
        }
        if seen.contains(&ty) {
            continue;
        }
        if let Some(info) = model.get_by_symbol(&ty) {
            stack.extend(info.bases.iter().cloned());
        }
        seen.push(ty);
    }
    false
}

/// Whether 17.9.3 forbids DECLARING a conversion operator between `from` and `to`, so no search
/// for one may answer for this pair: either side is `object` or an interface-type, or one is a
/// base type of the other. 17.9.3 gives the reason as well as the rule -- *"It is not possible to
/// redefine a pre-defined conversion. Thus, conversion operators are not allowed to convert from
/// or to `object` because implicit and explicit conversions already exist between `object` and
/// all other types. Likewise, neither the source nor the target types of a conversion can be a
/// base type of the other, since a conversion would then already exist"* -- and states the
/// consequence for interfaces directly: *"no user-defined transformations occur when converting
/// to an interface-type"*.
///
/// **This is deliberately NOT the general "a pre-defined conversion already exists" test**, which
/// is 17.9.3's fourth declaration rule but would be wrong here: `int` -> `decimal` is a standard
/// implicit NUMERIC conversion (13.1.2) that this compiler routes through `Decimal.op_Implicit`
/// because CIL has no primitive form for it, so a guard covering every pre-defined conversion
/// would silently drop every decimal conversion in the language. The reference cases above are
/// the ones where the search can otherwise answer with an operator whose return type merely
/// converts to the target -- and every type converts to `object`.
#[must_use]
pub fn no_conversion_operator_can_exist(
    model: &Model,
    from: &TypeSymbol,
    to: &TypeSymbol,
) -> bool {
    is_object(from)
        || is_object(to)
        || is_interface(model, from)
        || is_interface(model, to)
        || is_base_type_of(model, to, from)
        || is_base_type_of(model, from, to)
}

/// Whether a standard implicit conversion exists from `from` to `to`, using no
/// type hierarchy (13.1.1, 13.1.2, and to-`object`).
#[must_use]
pub fn has_implicit_conversion(from: &TypeSymbol, to: &TypeSymbol) -> bool {
    if from == to {
        return true;
    }
    if matches!(to, TypeSymbol::Special(SpecialType::Object)) {
        return true;
    }
    if let (TypeSymbol::Special(source), TypeSymbol::Special(target)) = (from, to) {
        return implicit_numeric(*source, *target);
    }
    false
}

/// The implicit numeric conversions (13.1.2): widening between the numeric types,
/// including the integer-to-floating conversions (which may lose precision).
fn implicit_numeric(from: SpecialType, to: SpecialType) -> bool {
    use SpecialType::{
        Byte, Char, Decimal, Double, Int16, Int32, Int64, SByte, Single, UInt16, UInt32, UInt64,
    };
    matches!(
        (from, to),
        (SByte, Int16 | Int32 | Int64 | Single | Double | Decimal)
            | (
                Byte,
                Int16 | UInt16 | Int32 | UInt32 | Int64 | UInt64 | Single | Double | Decimal
            )
            | (Int16, Int32 | Int64 | Single | Double | Decimal)
            | (
                UInt16,
                Int32 | UInt32 | Int64 | UInt64 | Single | Double | Decimal
            )
            | (Int32, Int64 | Single | Double | Decimal)
            | (UInt32, Int64 | UInt64 | Single | Double | Decimal)
            | (Int64, Single | Double | Decimal)
            | (UInt64, Single | Double | Decimal)
            | (
                Char,
                UInt16 | Int32 | UInt32 | Int64 | UInt64 | Single | Double | Decimal
            )
            | (Single, Double)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(special: SpecialType) -> TypeSymbol {
        TypeSymbol::Special(special)
    }

    #[test]
    fn identity_always_converts() {
        assert!(has_implicit_conversion(
            &t(SpecialType::Int32),
            &t(SpecialType::Int32)
        ));
        assert!(has_implicit_conversion(
            &t(SpecialType::String),
            &t(SpecialType::String)
        ));
    }

    /// **THE DECIMAL ROW IS THE ONE THAT MATTERS.** `int` -> `decimal` is a standard implicit
    /// numeric conversion that this compiler routes through `Decimal.op_Implicit`, so a guard
    /// stated as "a pre-defined conversion already exists" rather than as 17.9.3's three reference
    /// cases drops every decimal conversion in the language -- measured, not predicted.
    #[test]
    fn only_the_reference_cases_foreclose_a_conversion_operator() {
        let model = Model::new();
        assert!(no_conversion_operator_can_exist(
            &model,
            &t(SpecialType::String),
            &t(SpecialType::Object)
        ));
        assert!(no_conversion_operator_can_exist(
            &model,
            &t(SpecialType::Object),
            &t(SpecialType::String)
        ));
        assert!(!no_conversion_operator_can_exist(
            &model,
            &t(SpecialType::Int32),
            &t(SpecialType::Decimal)
        ));
        assert!(!no_conversion_operator_can_exist(
            &model,
            &t(SpecialType::Decimal),
            &t(SpecialType::Int32)
        ));
    }

    #[test]
    fn void_converts_to_nothing() {
        let model = Model::new();
        assert!(!converts(&model, &t(SpecialType::Void), &t(SpecialType::Object)));
        assert!(!converts(&model, &t(SpecialType::Void), &t(SpecialType::Int32)));
        assert!(!converts(&model, &t(SpecialType::Void), &t(SpecialType::Void)));
    }

    #[test]
    fn widening_numeric_conversions_exist_narrowing_do_not() {
        assert!(has_implicit_conversion(
            &t(SpecialType::Int32),
            &t(SpecialType::Int64)
        ));
        assert!(has_implicit_conversion(
            &t(SpecialType::Byte),
            &t(SpecialType::Int32)
        ));
        assert!(has_implicit_conversion(
            &t(SpecialType::Char),
            &t(SpecialType::Int32)
        ));
        assert!(has_implicit_conversion(
            &t(SpecialType::Int32),
            &t(SpecialType::Double)
        ));
        assert!(has_implicit_conversion(
            &t(SpecialType::Single),
            &t(SpecialType::Double)
        ));
        assert!(!has_implicit_conversion(
            &t(SpecialType::Int64),
            &t(SpecialType::Int32)
        ));
        assert!(!has_implicit_conversion(
            &t(SpecialType::Double),
            &t(SpecialType::Single)
        ));
        assert!(!has_implicit_conversion(
            &t(SpecialType::Int32),
            &t(SpecialType::Char)
        ));
        assert!(!has_implicit_conversion(
            &t(SpecialType::Int32),
            &t(SpecialType::Boolean)
        ));
    }

    #[test]
    fn pointer_integer_casts_are_exactly_the_eight_integer_types() {
        let model = Model::new();
        let ptr = TypeSymbol::Pointer(alloc::boxed::Box::new(t(SpecialType::Byte)));
        let other_ptr = TypeSymbol::Pointer(alloc::boxed::Box::new(t(SpecialType::Int32)));
        assert!(can_cast(&model, &ptr, &other_ptr));
        for integer in [
            SpecialType::SByte,
            SpecialType::Byte,
            SpecialType::Int16,
            SpecialType::UInt16,
            SpecialType::Int32,
            SpecialType::UInt32,
            SpecialType::Int64,
            SpecialType::UInt64,
        ] {
            assert!(can_cast(&model, &ptr, &t(integer)), "ptr -> {integer:?}");
            assert!(can_cast(&model, &t(integer), &ptr), "{integer:?} -> ptr");
        }
        for rejected in [
            SpecialType::Char,
            SpecialType::Single,
            SpecialType::Double,
            SpecialType::Decimal,
            SpecialType::Boolean,
        ] {
            assert!(
                !can_cast(&model, &ptr, &t(rejected)),
                "ptr -> {rejected:?} is CS0030"
            );
            assert!(
                !can_cast(&model, &t(rejected), &ptr),
                "{rejected:?} -> ptr is CS0030"
            );
        }
    }

    #[test]
    fn anything_converts_to_object() {
        assert!(has_implicit_conversion(
            &t(SpecialType::Int32),
            &t(SpecialType::Object)
        ));
        assert!(has_implicit_conversion(
            &t(SpecialType::String),
            &t(SpecialType::Object)
        ));
        let named = TypeSymbol::Named(["Widget".into()].into());
        assert!(has_implicit_conversion(&named, &t(SpecialType::Object)));
    }

    #[test]
    fn null_converts_to_an_instantiated_class_but_not_to_an_instantiated_struct() {
        use crate::symbols::{Model, TypeInfo, TypeKind};

        let mut model = Model::new();
        let mut boxed = TypeInfo::new("", "Box`1", TypeKind::Class);
        boxed.type_parameters = alloc::vec!["T".into()];
        model.insert(boxed);
        let mut val = TypeInfo::new("", "Val`1", TypeKind::Struct);
        val.type_parameters = alloc::vec!["T".into()];
        model.insert(val);

        let null = t(SpecialType::Null);
        let inst = |name: &str| TypeSymbol::Instantiation {
            definition: [name.into()].into(),
            arguments: [t(SpecialType::Int32)].into(),
        };

        assert!(
            converts(&model, &null, &inst("Box")),
            "an instantiated CLASS is a reference type, so `Box<int> b = null;` is legal"
        );
        assert!(
            !converts(&model, &null, &inst("Val")),
            "an instantiated STRUCT is a value type and holds no null"
        );
        assert!(!converts(&model, &null, &inst("Absent")));
        assert!(
            !converts(
                &model,
                &null,
                &TypeSymbol::Instantiation {
                    definition: ["Box".into()].into(),
                    arguments: [t(SpecialType::Int32), t(SpecialType::Int32)].into(),
                }
            ),
            "Box`2 is not Box`1"
        );
    }
}
