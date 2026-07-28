//! The MIR type lattice: the CLI's reduced stack types.

/// A handle to a value type's layout, resolved from metadata during CIL-to-MIR.
///
/// The backend interns these so codegen can reach a type's size, field offsets,
/// and which fields hold references without re-resolving a metadata token each
/// time. The handle is opaque to the IR; only the resolver gives it meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TypeHandle(pub u32);

/// The table byte reserved for arrays a FRONT END synthesizes, which have no metadata token of
/// their own -- a string blob, a delegate's entry list, a Python list's backing store. A handle
/// otherwise carries a metadata type token (TypeRef `0x01` / TypeDef `0x02`) or the backend's
/// reference-owned encoding (`0x03`), so `0x04` can never alias a real type. It stays under bit 27,
/// where the backends' symbol flags begin.
pub const SYNTHETIC_ARRAY_HANDLE_TABLE: u32 = 0x04;

/// The reserved handle for a synthesized array whose elements are `element_kind` -- the identity a
/// front end stamps on an [`Inst::AllocArray`](crate::Inst::AllocArray) it invents.
///
/// The kind IS the identity, so two synthesized arrays collide exactly when they agree about their
/// elements. That is not a convenience: a descriptor is deduplicated BY HANDLE -- one
/// `__lamella_typedesc_<handle>` per handle per image -- so two arrays sharing a handle share one
/// descriptor, and whichever the backend emits first decides the element kind for both. That was
/// harmless while an array's descriptor was an all-zero hole. It stopped being harmless when the
/// descriptor started carrying the kind: a UTF-16 string blob sharing the delegate list's handle
/// would be described as an array of REFERENCES, and a collector tracing it would walk code units
/// as pointers. Deriving the handle from the kind makes that unrepresentable rather than a rule
/// every call site has to remember.
#[must_use]
pub const fn synthetic_array_handle(element_kind: u32) -> TypeHandle {
    TypeHandle((SYNTHETIC_ARRAY_HANDLE_TABLE << 24) | element_kind)
}

/// How far [`array_handle`] lifts a class-identity table byte to reach the array's own: `0x01` ->
/// `0x05`, `0x02` -> `0x06`, `0x03` -> `0x07`. Those three bytes are RESERVED for array identities
/// and are the last the encoding has -- everything must stay under bit 27, where the backends'
/// symbol flags begin (a handle rides the low bits of a descriptor reference word).
pub const ARRAY_HANDLE_TABLE_OFFSET: u32 = 0x04;

/// The handle identifying the rank-1 ARRAY whose elements are `element` -- `T[]`, given `T`.
///
/// An array needs an identity of its OWN because a descriptor is deduplicated BY HANDLE. While an
/// array's handle was its element's token, `int[]` and a boxed `int` were one handle, so one
/// `__lamella_typedesc_*` had to serve both and whichever the backend laid first decided the
/// other's shape -- and an array descriptor cannot name its element type at all if the name it
/// would use is its own. The transform lifts a class-identity table byte (TypeRef `0x01`, TypeDef
/// `0x02`, the backend's reference-owned `0x03`) by [`ARRAY_HANDLE_TABLE_OFFSET`]. That is a
/// BIJECTION on the class-identity space, so two different element types can never name one array,
/// and it needs no side table to stay stable across an assembly boundary: both sides derive the
/// same array handle from the same element identity.
///
/// A handle that is ALREADY an array identity (a synthetic `0x04`) or that names no class
/// descriptor at all (a TypeSpec -- a nested array or a generic instantiation) is its own array
/// handle: there is no class descriptor for it to collide with.
#[must_use]
pub const fn array_handle(element: TypeHandle) -> TypeHandle {
    match element.0 >> 24 {
        0x01..=0x03 => TypeHandle(element.0 + (ARRAY_HANDLE_TABLE_OFFSET << 24)),
        _ => element,
    }
}

/// The type of a MIR value: one of the CLI's stack types (ECMA-335 III.1.1), plus
/// the Python frontend's tagged [`MirType::PyValue`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MirType {
    /// A 32-bit integer (`int32`). Signedness is not part of the stack type; it
    /// belongs to the operation, as in the CLI.
    I32,
    /// A 64-bit integer (`int64`).
    I64,
    /// A native-sized integer (`native int`), the target's pointer width.
    NativeInt,
    /// A 32-bit IEEE-754 float: the CLI stack type `F` narrowed to single width.
    F32,
    /// A 64-bit IEEE-754 float: the CLI stack type `F` narrowed to double width.
    F64,
    /// An object reference (`O`): a pointer to a whole object on the
    /// garbage-collected heap, reported as a root at safepoints.
    ObjectRef,
    /// A managed pointer (`&`): a possibly-interior pointer into managed memory,
    /// also reported to the collector and kept distinct from an unmanaged pointer.
    ManagedPtr,
    /// A Python tagged value (`PyValue`): one target word that is either a small
    /// immediate (a fixnum, `None`/`True`/`False`, ...) or a tagged pointer to a heap
    /// object, distinguished by tag bits. Added for the Python frontend; the C#
    /// lowering never produces one. It is a garbage-collector ROOT, but a CONDITIONAL
    /// one -- the collector decodes the tag at a safepoint and traces the slot only
    /// when it holds a heap pointer (see [`MirType::is_tagged_value`] and the
    /// scan-by-tag stack map). The exact bit layout is the frontend's and the
    /// runtime's contract; the IR treats it as one opaque word.
    PyValue,
    /// A value-type instance: a `size`-byte struct laid out inline, identified by its
    /// layout [`TypeHandle`]. The size is carried for stack-slot allocation; field
    /// offsets and which fields hold `O`/`&` come from the handle's metadata layout.
    ValueType {
        /// The value type's layout handle: its identity for field offsets and GC map.
        handle: TypeHandle,
        /// The instance's size in bytes, for stack-slot allocation.
        size: u32,
    },
}

impl MirType {
    /// Whether a value of this type is itself a garbage-collector root: object
    /// references and managed pointers are. Integers and floats are not. A value
    /// type may *contain* references, but that is resolved through its layout
    /// handle, not reported here.
    #[must_use]
    pub fn is_gc_reference(self) -> bool {
        matches!(self, MirType::ObjectRef | MirType::ManagedPtr)
    }

    /// Whether this is a Python tagged value -- a CONDITIONAL garbage-collector root
    /// the collector decodes by tag at a safepoint, as opposed to
    /// [`MirType::is_gc_reference`], which is an unconditional pointer. Kept distinct
    /// so a safepoint stack map can record it in its own tagged-root list.
    #[must_use]
    pub fn is_tagged_value(self) -> bool {
        matches!(self, MirType::PyValue)
    }

    /// Whether this is one of the integer stack types (`int32`, `int64`, or
    /// `native int`).
    #[must_use]
    pub fn is_integer(self) -> bool {
        matches!(self, MirType::I32 | MirType::I64 | MirType::NativeInt)
    }

    /// Whether this is one of the floating-point types.
    #[must_use]
    pub fn is_float(self) -> bool {
        matches!(self, MirType::F32 | MirType::F64)
    }

    /// The bytes a value of this type occupies in a stack slot: 8 for the 64-bit scalars,
    /// the size rounded up to a word for a value type, 4 for everything else.
    #[must_use]
    pub fn stack_slot_bytes(self) -> u32 {
        match self {
            MirType::I64 | MirType::F64 => 8,
            MirType::ValueType { size, .. } => size.next_multiple_of(4),
            _ => 4,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn references_are_roots_scalars_are_not() {
        assert!(MirType::ObjectRef.is_gc_reference());
        assert!(MirType::ManagedPtr.is_gc_reference());
        assert!(!MirType::I32.is_gc_reference());
        assert!(!MirType::F64.is_gc_reference());
        assert!(
            !MirType::ValueType {
                handle: TypeHandle(1),
                size: 8
            }
            .is_gc_reference()
        );
    }

    #[test]
    fn py_value_is_a_conditional_root_not_an_unconditional_reference() {
        assert!(MirType::PyValue.is_tagged_value());
        assert!(!MirType::PyValue.is_gc_reference());
        assert!(!MirType::PyValue.is_integer());
        assert!(!MirType::PyValue.is_float());
        assert!(!MirType::ObjectRef.is_tagged_value());
        assert!(!MirType::ManagedPtr.is_tagged_value());
        assert_eq!(MirType::PyValue.stack_slot_bytes(), 4);
    }

    #[test]
    fn integer_and_float_classes_are_disjoint() {
        for t in [MirType::I32, MirType::I64, MirType::NativeInt] {
            assert!(t.is_integer() && !t.is_float());
        }
        for t in [MirType::F32, MirType::F64] {
            assert!(t.is_float() && !t.is_integer());
        }
    }
}
