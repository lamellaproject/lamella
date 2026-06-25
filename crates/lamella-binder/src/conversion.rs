//! Implicit conversions (ECMA-334 1st ed, 13.1).

use crate::special::SpecialType;
use crate::symbols::{Model, TypeKind};
use crate::types::TypeSymbol;
use alloc::vec::Vec;

/// Whether an implicit conversion exists from `from` to `to`, including the
/// reference conversions that walk `model`'s inheritance graph (13.1).
#[must_use]
pub fn converts(model: &Model, from: &TypeSymbol, to: &TypeSymbol) -> bool {
    if let TypeSymbol::ByRef(element) = to {
        return from == element.as_ref();
    }
    if matches!(from, TypeSymbol::Special(SpecialType::Null)) {
        return is_reference_type(model, to)
            || matches!(to, TypeSymbol::Special(SpecialType::Null));
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
        || pointer_cast(from, to)
}

/// Explicit conversions involving pointers (unsafe): any pointer to/from any other pointer,
/// and a pointer to/from an integer.
fn pointer_cast(from: &TypeSymbol, to: &TypeSymbol) -> bool {
    let from_ptr = matches!(from, TypeSymbol::Pointer(_));
    let to_ptr = matches!(to, TypeSymbol::Pointer(_));
    (from_ptr && (to_ptr || is_numeric_type(to))) || (to_ptr && (from_ptr || is_numeric_type(from)))
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
fn is_reference_type(model: &Model, ty: &TypeSymbol) -> bool {
    match ty {
        TypeSymbol::Special(special) => {
            matches!(special, SpecialType::Object | SpecialType::String)
        }
        TypeSymbol::Array { .. } => true,
        TypeSymbol::Named(_) => model.get_by_symbol(ty).is_some_and(|info| {
            matches!(
                info.kind,
                TypeKind::Class | TypeKind::Interface | TypeKind::Delegate
            )
        }),
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
    let mut stack: Vec<TypeSymbol> = match model.get_by_symbol(from) {
        Some(info) => info.bases.to_vec(),
        None => return false,
    };
    let mut seen: Vec<TypeSymbol> = Vec::new();
    while let Some(ty) = stack.pop() {
        if &ty == to {
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
}
