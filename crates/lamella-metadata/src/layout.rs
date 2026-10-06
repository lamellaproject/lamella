//! Value-type layout: the one shared computation of a struct's (or enum's) size,
//! alignment, per-field byte offsets, and reference-offset map.

use crate::signature::SigType;
use alloc::vec::Vec;
use lamella_token::Token;

/// The target's data-layout parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TargetLayout {
    /// The size and alignment of a managed reference or native pointer: 4 on ARMv6-M
    /// and wasm32, 8 on a 64-bit target.
    pub pointer_size: u32,
    /// An optional cap on field alignment, for a packed or C-interop layout; `None`
    /// uses natural alignment (the default).
    pub max_alignment: Option<u32>,
}

impl TargetLayout {
    /// A 32-bit target with natural alignment (ARMv6-M, wasm32).
    #[must_use]
    pub const fn ilp32() -> TargetLayout {
        TargetLayout {
            pointer_size: 4,
            max_alignment: None,
        }
    }

    /// A 64-bit target with natural alignment.
    #[must_use]
    pub const fn lp64() -> TargetLayout {
        TargetLayout {
            pointer_size: 8,
            max_alignment: None,
        }
    }
}

/// The computed layout of a value type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeLayout {
    /// Total size in bytes (a multiple of `alignment`).
    pub size: u32,
    /// Alignment in bytes (the largest field alignment, capped by the target).
    pub alignment: u32,
    /// The byte offset of each field, in declaration order.
    pub field_offsets: Vec<u32>,
    /// The byte offsets of the managed-reference slots within the type -- the GC map.
    /// Empty for a blittable value type. Ascending, by construction.
    pub reference_offsets: Vec<u32>,
}

/// Why a value type could not be laid out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayoutError {
    /// A field's type is not one a field may have (`void`, a by-ref, a typed
    /// reference, or a function pointer).
    NotAFieldType(SigType),
    /// A nested value-type field's token could not be resolved to its layout.
    UnresolvedValueType(Token),
    /// A field's type is an instantiation of a value type (`int?`, `KeyValuePair<int, string>`)
    /// that the resolver could not lay out.
    ///
    /// Such a field's size and trace map are its definition's fields with the type arguments
    /// substituted, which takes the definition's assembly and a substitution this crate does not
    /// perform; the resolver passed to [`layout_value_type`] answers it, or this is the refusal.
    UnresolvedInstantiation(SigType),
    /// A field's type still mentions a type parameter, so it has no layout yet.
    ///
    /// **A REFUSAL, NOT A GAP.** `T` has no size until an instantiation supplies one, so there is
    /// no correct number to return and any guess would be a size the collector later walks. Under
    /// bake-time lowering nothing generic should reach layout at all: the baker substitutes first.
    /// Reaching this means the lowering did not run, and saying so loudly is the whole point.
    GenericNotInstantiated(SigType),
}

/// Rounds `offset` up to the next multiple of `align` (a power of two >= 1).
const fn align_up(offset: u32, align: u32) -> u32 {
    (offset + align - 1) & !(align - 1)
}

/// Lays out a value type whose fields have the given signature types, in declaration
/// order.
///
/// `resolve` supplies the layout of a nested value type, so the caller drives recursion with
/// its own assembly or model. It is called with the field's signature, which is one of two
/// shapes: a `ValueType` token, or a `GenericInst` whose definition is a `ValueType` (an
/// instantiation such as `int?`). An instantiation whose definition is a `Class` is a managed
/// reference whatever its arguments are, so it takes a pointer and one reference offset and is
/// never passed to `resolve`.
pub fn layout_value_type(
    fields: &[SigType],
    target: &TargetLayout,
    resolve: &impl Fn(&SigType) -> Option<TypeLayout>,
) -> Result<TypeLayout, LayoutError> {
    let mut offset = 0u32;
    let mut alignment = 1u32;
    let mut field_offsets = Vec::with_capacity(fields.len());
    let mut reference_offsets = Vec::new();

    for field in fields {
        let (size, align, field_refs) = field_shape(field, target, resolve)?;
        let align = target.max_alignment.map_or(align, |cap| align.min(cap));
        offset = align_up(offset, align);
        field_offsets.push(offset);
        for reference in field_refs {
            reference_offsets.push(offset + reference);
        }
        offset += size;
        alignment = alignment.max(align);
    }

    Ok(TypeLayout {
        size: align_up(offset, alignment),
        alignment,
        field_offsets,
        reference_offsets,
    })
}

/// One field's size, alignment, and the reference offsets *within* it (relative to
/// the field's own start): empty for a primitive, `[0]` for a reference, and a nested
/// value type's own map for a `ValueType`.
fn field_shape(
    field: &SigType,
    target: &TargetLayout,
    resolve: &impl Fn(&SigType) -> Option<TypeLayout>,
) -> Result<(u32, u32, Vec<u32>), LayoutError> {
    let primitive = |size: u32| Ok((size, size, Vec::new()));
    let pointer = |is_reference: bool| {
        let references = if is_reference {
            alloc::vec![0]
        } else {
            Vec::new()
        };
        Ok((target.pointer_size, target.pointer_size, references))
    };
    match field {
        SigType::Boolean | SigType::I1 | SigType::U1 => primitive(1),
        SigType::Char | SigType::I2 | SigType::U2 => primitive(2),
        SigType::I4 | SigType::U4 | SigType::R4 => primitive(4),
        SigType::I8 | SigType::U8 | SigType::R8 => primitive(8),
        SigType::IntPtr | SigType::UIntPtr | SigType::Pointer(_) => pointer(false),
        SigType::String
        | SigType::Object
        | SigType::Class(_)
        | SigType::SzArray(_)
        | SigType::Array { .. } => pointer(true),
        SigType::ValueType(token) => {
            let nested = resolve(field).ok_or(LayoutError::UnresolvedValueType(*token))?;
            Ok((nested.size, nested.alignment, nested.reference_offsets))
        }
        // An instantiation of a class (`List<int>`, `IEqualityComparer<TKey>`, `Func<T>`) is a
        // managed reference whatever its arguments are, open ones included: one pointer, one
        // reference offset, and no lookup.
        SigType::GenericInst { definition, .. } if matches!(**definition, SigType::Class(_)) => {
            pointer(true)
        }
        // An instantiation of a value type composes like a nested value type, from the layout
        // the resolver substitutes for it.
        SigType::GenericInst { definition, .. }
            if matches!(**definition, SigType::ValueType(_)) =>
        {
            let nested = resolve(field)
                .ok_or_else(|| LayoutError::UnresolvedInstantiation(field.clone()))?;
            Ok((nested.size, nested.alignment, nested.reference_offsets))
        }
        SigType::Var(_) | SigType::MVar(_) | SigType::GenericInst { .. } => {
            Err(LayoutError::GenericNotInstantiated(field.clone()))
        }
        SigType::Void | SigType::ByRef(_) | SigType::TypedByRef => {
            Err(LayoutError::NotAFieldType(field.clone()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(fields: &[SigType]) -> TypeLayout {
        layout_value_type(fields, &TargetLayout::ilp32(), &|_| None).expect("lays out")
    }

    #[test]
    fn two_int_struct_is_eight_bytes() {
        let layout = layout(&[SigType::I4, SigType::I4]);
        assert_eq!(layout.field_offsets, [0, 4]);
        assert_eq!(layout.size, 8);
        assert_eq!(layout.alignment, 4);
    }

    #[test]
    fn byte_then_int_pads_to_eight_bytes() {
        let layout = layout(&[SigType::U1, SigType::I4]);
        assert_eq!(layout.field_offsets, [0, 4]);
        assert_eq!(layout.size, 8);
        assert_eq!(layout.alignment, 4);
    }

    #[test]
    fn packs_primitives_with_natural_alignment_and_padding() {
        let layout = layout(&[SigType::I1, SigType::I4, SigType::I8]);
        assert_eq!(layout.field_offsets, [0, 4, 8]);
        assert_eq!(layout.size, 16);
        assert_eq!(layout.alignment, 8);
        assert!(layout.reference_offsets.is_empty());
    }

    #[test]
    fn a_blittable_struct_has_an_empty_reference_map() {
        let layout = layout(&[SigType::I4, SigType::R8, SigType::Char]);
        assert!(layout.reference_offsets.is_empty());
    }

    #[test]
    fn references_are_pointer_sized_and_listed_in_the_map() {
        let layout = layout(&[SigType::I4, SigType::Object, SigType::I4]);
        assert_eq!(layout.field_offsets, [0, 4, 8]);
        assert_eq!(layout.size, 12);
        assert_eq!(layout.alignment, 4);
        assert_eq!(layout.reference_offsets, [4]);
    }

    #[test]
    fn unmanaged_pointers_and_native_ints_are_not_references() {
        let layout = layout(&[
            SigType::IntPtr,
            SigType::Pointer(alloc::boxed::Box::new(SigType::I4)),
        ]);
        assert!(layout.reference_offsets.is_empty());
    }

    #[test]
    fn the_max_alignment_cap_packs_wider_fields() {
        let target = TargetLayout {
            pointer_size: 4,
            max_alignment: Some(4),
        };
        let layout = layout_value_type(&[SigType::I4, SigType::I8], &target, &|_| None).unwrap();
        assert_eq!(layout.field_offsets, [0, 4]);
        assert_eq!(layout.alignment, 4);
        assert_eq!(layout.size, 12);
    }

    #[test]
    fn a_nested_value_type_composes_its_map_shifted_by_its_offset() {
        let inner = TypeLayout {
            size: 8,
            alignment: 4,
            field_offsets: alloc::vec![0, 4],
            reference_offsets: alloc::vec![0],
        };
        let nested_token = Token::new(crate::tables::table::TYPE_DEF, 2);
        let resolve =
            |field: &SigType| (*field == SigType::ValueType(nested_token)).then(|| inner.clone());
        let layout = layout_value_type(
            &[SigType::I4, SigType::ValueType(nested_token)],
            &TargetLayout::ilp32(),
            &resolve,
        )
        .unwrap();
        assert_eq!(layout.field_offsets, [0, 4]);
        assert_eq!(layout.size, 12);
        assert_eq!(layout.reference_offsets, [4]);
    }

    #[test]
    fn an_unresolved_nested_value_type_is_an_error() {
        let token = Token::new(crate::tables::table::TYPE_DEF, 9);
        let result = layout_value_type(
            &[SigType::ValueType(token)],
            &TargetLayout::ilp32(),
            &|_| None,
        );
        assert_eq!(result, Err(LayoutError::UnresolvedValueType(token)));
    }

    fn instantiation(definition: SigType, arguments: alloc::vec::Vec<SigType>) -> SigType {
        SigType::GenericInst {
            definition: alloc::boxed::Box::new(definition),
            arguments,
        }
    }

    #[test]
    fn a_class_instantiation_is_one_traced_pointer_without_a_lookup() {
        let list = Token::new(crate::tables::table::TYPE_REF, 4);
        let readings = instantiation(SigType::Class(list), alloc::vec![SigType::I4]);
        let layout = layout_value_type(&[SigType::I4, readings], &TargetLayout::ilp32(), &|_| {
            panic!("a class instantiation needs no resolver")
        })
        .unwrap();
        assert_eq!(layout.field_offsets, [0, 4]);
        assert_eq!(layout.size, 8);
        assert_eq!(layout.reference_offsets, [4]);
    }

    #[test]
    fn an_open_class_instantiation_is_a_reference_too() {
        let cell = Token::new(crate::tables::table::TYPE_DEF, 3);
        let field = instantiation(SigType::Class(cell), alloc::vec![SigType::Var(0)]);
        let layout = layout_value_type(&[field], &TargetLayout::ilp32(), &|_| None).unwrap();
        assert_eq!(layout.reference_offsets, [0]);
        assert_eq!(layout.size, 4);
    }

    #[test]
    fn a_value_type_instantiation_composes_the_layout_the_resolver_substitutes() {
        let nullable = Token::new(crate::tables::table::TYPE_REF, 7);
        let limit = instantiation(SigType::ValueType(nullable), alloc::vec![SigType::I4]);
        let wanted = limit.clone();
        let resolve = |field: &SigType| {
            (*field == wanted).then(|| TypeLayout {
                size: 8,
                alignment: 4,
                field_offsets: alloc::vec![0, 4],
                reference_offsets: alloc::vec![],
            })
        };
        let layout =
            layout_value_type(&[SigType::U1, limit], &TargetLayout::ilp32(), &resolve).unwrap();
        assert_eq!(layout.field_offsets, [0, 4]);
        assert_eq!(layout.size, 12);
        assert!(layout.reference_offsets.is_empty());
    }

    #[test]
    fn a_value_type_instantiation_the_resolver_cannot_lay_out_is_named() {
        let pair = Token::new(crate::tables::table::TYPE_DEF, 5);
        let field = instantiation(SigType::ValueType(pair), alloc::vec![SigType::String]);
        let result = layout_value_type(
            core::slice::from_ref(&field),
            &TargetLayout::ilp32(),
            &|_| None,
        );
        assert_eq!(result, Err(LayoutError::UnresolvedInstantiation(field)));
    }

    #[test]
    fn a_type_parameter_still_has_no_layout() {
        let result = layout_value_type(&[SigType::Var(0)], &TargetLayout::ilp32(), &|_| None);
        assert_eq!(
            result,
            Err(LayoutError::GenericNotInstantiated(SigType::Var(0)))
        );
    }

    #[test]
    fn void_is_not_a_field_type() {
        let result = layout_value_type(&[SigType::Void], &TargetLayout::ilp32(), &|_| None);
        assert_eq!(result, Err(LayoutError::NotAFieldType(SigType::Void)));
    }
}
