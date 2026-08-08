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
/// reference-owned encoding (`0x03`), so `0x04` can never alias a real type.
pub const SYNTHETIC_ARRAY_HANDLE_TABLE: u32 = 0x04;

/// The table byte reserved for a GENERIC INSTANTIATION, whose handle carries a hash of its
/// canonical spelling rather than a metadata row.
///
/// An instantiation has no metadata row of its own, so like a synthesized array it takes a byte no
/// type table occupies. It needs one of its OWN rather than sharing `0x04`: a descriptor is
/// deduplicated BY HANDLE, so an instantiation whose spelling hash equaled a synthesized array's
/// element kind would share one `__lamella_typedesc_*` with it -- and word 0 of an array descriptor
/// is `MARK | rank` where word 0 of a class descriptor is a PAYLOAD SIZE, so whichever was laid
/// first would describe the other.
pub const INSTANTIATION_HANDLE_TABLE: u32 = 0x09;

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

/// How far [`array_handle`] lifts an element's table byte to reach the array's own: `0x01` ->
/// `0x05`, `0x02` -> `0x06`, `0x03` -> `0x07`, and [`INSTANTIATION_HANDLE_TABLE`] `0x09` -> `0x0D`.
/// Those four bytes are RESERVED for array identities.
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
/// descriptor at all (a bare TypeSpec, which is what an element the reader cannot name resolves to)
/// is its own array handle: there is no class descriptor for it to collide with.
#[must_use]
pub const fn array_handle(element: TypeHandle) -> TypeHandle {
    match element.0 >> 24 {
        0x01..=0x03 | INSTANTIATION_HANDLE_TABLE => {
            TypeHandle(element.0 + (ARRAY_HANDLE_TABLE_OFFSET << 24))
        }
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

    /// WHAT BOUNDS THE HANDLE SPACE, AS A PROPERTY RATHER THAN A SENTENCE IN A DOC COMMENT.
    ///
    /// A handle rides the low bits of a descriptor reference word alongside five flags in bits
    /// 31..27, and for years the rule written down here was "every table byte must stay under
    /// `0x08`, where the flags begin".
    #[test]
    fn the_handle_space_is_bounded_by_the_bit_tested_flags() {
        const EXTERN_SYMBOL_FLAG: u32 = 0x8000_0000;
        const DESC_SYMBOL_FLAG: u32 = 0x4000_0000;
        const STRING_SYMBOL_FLAG: u32 = 0x2000_0000;
        const STATICS_BASE_SYMBOL_FLAG: u32 = 0x1000_0000;
        const EH_TAG_SYMBOL_FLAG: u32 = 0x0800_0000;

        let allocated = [
            0x01,
            0x02,
            0x03,
            SYNTHETIC_ARRAY_HANDLE_TABLE,
            0x05,
            0x06,
            0x07,
            INSTANTIATION_HANDLE_TABLE,
            INSTANTIATION_HANDLE_TABLE + ARRAY_HANDLE_TABLE_OFFSET,
            0x1B,
        ];
        for table in allocated {
            let handle = (table << 24) | 0x00ff_ffff;
            for word in [handle, DESC_SYMBOL_FLAG | handle] {
                assert_eq!(
                    word & EXTERN_SYMBOL_FLAG,
                    0,
                    "table byte {table:#04x} reaches bit 31, which is BIT TESTED as an extern call"
                );
                assert_eq!(
                    word & STRING_SYMBOL_FLAG,
                    0,
                    "table byte {table:#04x} reaches bit 29, which is BIT TESTED as a string blob"
                );
                assert_ne!(
                    word >> 24,
                    STATICS_BASE_SYMBOL_FLAG >> 24,
                    "table byte {table:#04x} shares its TOP BYTE with a statics base, which is the \
                     only test that separates the two"
                );
                assert_ne!(
                    word, EH_TAG_SYMBOL_FLAG,
                    "table byte {table:#04x} can spell the EH word, matched by EXACT EQUALITY"
                );
            }
        }

        let mut seen = allocated;
        seen.sort_unstable();
        let mut deduped = seen;
        let unique = {
            let mut n = 0;
            for i in 0..deduped.len() {
                if i == 0 || deduped[i] != deduped[i - 1] {
                    deduped[n] = deduped[i];
                    n += 1;
                }
            }
            n
        };
        assert_eq!(
            unique,
            allocated.len(),
            "two identity kinds were given one table byte; a descriptor is deduplicated BY HANDLE, \
             so whichever the backend lays first decides the other's shape"
        );

        for table in [0x01u32, 0x02, 0x03, INSTANTIATION_HANDLE_TABLE] {
            let element = TypeHandle((table << 24) | 0x0000_ffff);
            assert_eq!(
                array_handle(element).0 >> 24,
                table + ARRAY_HANDLE_TABLE_OFFSET,
                "a class identity's array lift must land on its reserved byte"
            );
        }
        let synthetic = synthetic_array_handle(7);
        assert_eq!(synthetic.0 >> 24, SYNTHETIC_ARRAY_HANDLE_TABLE);
        assert_eq!(
            array_handle(synthetic),
            synthetic,
            "an array identity is its own array handle -- there is nothing for it to collide with"
        );
        let type_spec = TypeHandle((0x1B << 24) | 0x0000_ffff);
        assert_eq!(
            array_handle(type_spec),
            type_spec,
            "a bare TypeSpec owns no class descriptor, so its array needs no separate identity"
        );
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
