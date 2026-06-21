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
}

/// Rounds `offset` up to the next multiple of `align` (a power of two >= 1).
const fn align_up(offset: u32, align: u32) -> u32 {
    (offset + align - 1) & !(align - 1)
}

/// Lays out a value type whose fields have the given signature types, in declaration
/// order. `resolve` supplies the layout of a nested value type by its `ValueType`
/// token (so the caller drives recursion with its assembly/model); it is only called
/// for a `ValueType` field.
pub fn layout_value_type(
    fields: &[SigType],
    target: &TargetLayout,
    resolve: &impl Fn(Token) -> Option<TypeLayout>,
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
    resolve: &impl Fn(Token) -> Option<TypeLayout>,
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
            let nested = resolve(*token).ok_or(LayoutError::UnresolvedValueType(*token))?;
            Ok((nested.size, nested.alignment, nested.reference_offsets))
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
        let resolve = |token: Token| (token == nested_token).then(|| inner.clone());
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

    #[test]
    fn void_is_not_a_field_type() {
        let result = layout_value_type(&[SigType::Void], &TargetLayout::ilp32(), &|_| None);
        assert_eq!(result, Err(LayoutError::NotAFieldType(SigType::Void)));
    }
}
