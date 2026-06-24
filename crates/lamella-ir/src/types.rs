//! The MIR type lattice: the CLI's reduced stack types.

/// A handle to a value type's layout, resolved from metadata during CIL-to-MIR.
///
/// The backend interns these so codegen can reach a type's size, field offsets,
/// and which fields hold references without re-resolving a metadata token each
/// time. The handle is opaque to the IR; only the resolver gives it meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TypeHandle(pub u32);

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
    /// runtime's contract (`docs/python-mir-seams.md`); the IR treats it as one
    /// opaque word.
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
