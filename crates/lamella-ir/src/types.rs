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
///
/// The byte is `0x09` and not the next one up because `0x08` is the bare-handle spelling of the
/// backends' EH-tag symbol word, which is matched by EXACT EQUALITY -- an instantiation whose hash
/// payload happened to be zero would land on it. Leaving a byte between the two costs nothing and
/// removes the knife edge rather than sizing a bound to it.
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
///
/// A handle rides the low bits of a descriptor reference word, so it is bounded by that word's
/// flags -- but NOT, as this comment once said, by the lowest of them. The flags occupy bits 31..27
/// and only three of them constrain a handle: the two that are BIT TESTED (bit 31 extern, bit 29
/// string) and the one whose payload is matched by TOP BYTE (bit 28 statics, top byte `0x10`). The
/// EH word at bit 27 is matched by EXACT EQUALITY and the descriptor flag at bit 30 is decoded
/// FIRST, so neither bounds the space. A TypeSpec handle -- table byte `0x1B`, every rank-N array
/// in shipping code -- has ridden above bit 27 for exactly this reason since before generics
/// existed. The real rule is pinned as a property by
/// `the_handle_space_is_bounded_by_the_bit_tested_flags`.
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
///
/// **THAT FALL-THROUGH READS THE TOP BYTE, SO IT ALSO SWALLOWS A HANDLE WHOSE TOP BYTE IS A FLAG
/// RATHER THAN A TABLE** -- the answer is then the ELEMENT, and the collision this function removes
/// is back with nothing to report it. A caller that carries such a flag must clear it, lift, and set
/// it again; a handle reaching here is expected to be spelled as a table byte over a row.
///
/// A generic INSTANTIATION is lifted like a class identity rather than passed through, because it
/// is the one handle in that group that does own a class descriptor -- payload size, GC trace map
/// and type tag, laid by the same emitter that lays an ordinary type's. Passing it through would
/// give `Box<int>` and `Box<int>[]` one handle and therefore one descriptor, which is the exact
/// collision this function was written to remove for `int` and `int[]`.
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
    /// layout [`TypeHandle`], and carrying WHICH OF ITS WORDS HOLD OBJECT REFERENCES.
    ///
    /// **`refs` IS HERE BECAUSE THE HANDLE CANNOT ANSWER IT.** Field offsets and which fields hold
    /// `O`/`&` are recorded in the handle's metadata layout, but no backend holds metadata at the
    /// point it needs them: the stack-map root builder sees only a [`crate::Function`]. Without
    /// this field a frame holding a live reference inside a struct local enumerates that reference
    /// nowhere -- the containing slot is not a pointer, and its interior words are invisible to a
    /// type-keyed walk. On a MARK-COMPACT heap that is an object collected while live, and a stale
    /// word if it survives.
    ///
    /// **THE TWO SENTINEL HANDLES ARE WHAT SAID THE IR WAS UNDER-SPECIFIED.** `REF_CELL_HANDLE` and
    /// `EXCEPTION_CELL_HANDLE` were smuggled into `handle` because a caller needed to express
    /// per-cell GC semantics the type could not carry; both are ordinary values of this field
    /// (`RefWords::at_word(0)` and [`RefWords::NONE`]), which is what makes it the right vocabulary
    /// rather than a third special case.
    ValueType {
        /// The value type's layout handle: its identity for field offsets.
        handle: TypeHandle,
        /// The instance's size in bytes, for stack-slot allocation.
        size: u32,
        /// Which words of the instance hold object references, for the GC stack map.
        refs: RefWords,
    },
}

/// Which WORDS of a value-type instance hold object references -- the GC trace map of one inline
/// struct, as a bitmask: bit `i` marks the word at byte offset `i * 4`.
///
/// # Why a bitmask, and what it costs
///
/// [`MirType`] is `Copy` and is copied everywhere a compiler copies a type, so a `Vec` is not
/// available here. A bitmask keeps that and bounds the map at **32 words (128 bytes)**. Past the
/// bound [`RefWords::from_offsets`] REFUSES rather than truncating: a truncated trace map is a
/// reference the collector never visits, which is the exact defect this type exists to close, and a
/// frame-resident aggregate that large is pathological on this tier. Refuse loudly, do not round.
///
/// **DELIBERATELY NOT `Default`**, for the reason [`crate::TypeHandle`]'s owner-stamping analogue in
/// the backend is not: the safe-looking value is `NONE`, which is exactly what a forgotten
/// assignment would take and exactly the one that drops a live root. Constructing a value-type slot
/// must state its trace map.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RefWords(u32);

impl RefWords {
    /// No word of the instance holds an object reference -- every scalar struct, and the
    /// frame-materialized exception cell whose words are a descriptor address and zeroed fields.
    pub const NONE: RefWords = RefWords(0);

    /// The map whose only reference is the word at `word` (`REF_CELL_HANDLE`'s shape).
    #[must_use]
    pub const fn at_word(word: u32) -> RefWords {
        RefWords(1u32 << word)
    }

    /// The map for references at these BYTE offsets, or `None` if any lies past the 32-word bound
    /// or is not word-aligned. Both refusals are the same rule: a map that cannot be represented
    /// exactly is not narrowed to one that can.
    #[must_use]
    pub fn from_offsets(offsets: &[u32]) -> Option<RefWords> {
        let mut bits = 0u32;
        for offset in offsets {
            if offset % 4 != 0 || offset / 4 >= 32 {
                return None;
            }
            bits |= 1u32 << (offset / 4);
        }
        Some(RefWords(bits))
    }

    /// Whether the word at byte offset `offset` holds an object reference.
    #[must_use]
    pub const fn contains_offset(self, offset: u32) -> bool {
        offset % 4 == 0 && offset / 4 < 32 && (self.0 >> (offset / 4)) & 1 == 1
    }

    /// The byte offsets of the reference words, ascending.
    pub fn offsets(self) -> impl Iterator<Item = u32> {
        (0..32).filter(move |w| (self.0 >> w) & 1 == 1).map(|w| w * 4)
    }

    /// Whether any word holds a reference.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
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
                size: 8,
                refs: RefWords::at_word(0),
            }
            .is_gc_reference()
        );
    }

    #[test]
    fn a_trace_map_refuses_rather_than_narrowing_past_its_bound() {
        assert_eq!(
            RefWords::from_offsets(&[0, 124]).map(|m| m.offsets().collect::<Vec<_>>()),
            Some(vec![0, 124])
        );
        assert_eq!(RefWords::from_offsets(&[128]), None);
        assert_eq!(RefWords::from_offsets(&[2]), None);
        assert!(RefWords::NONE.is_empty());
        assert!(RefWords::at_word(0).contains_offset(0));
        assert!(!RefWords::at_word(0).contains_offset(4));
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
    /// `0x08`, where the flags begin". THAT RULE WAS NEVER TRUE OF SHIPPING CODE: a TypeSpec
    /// handle -- table byte `0x1B`, every rank-N array -- has ridden above bit 27 the whole time.
    /// What actually keeps the word unambiguous is which flags are BIT TESTED:
    ///
    /// - bit 31 (extern) and bit 29 (string) are bit tested, so a table byte may not reach either;
    /// - bit 28 (statics) is matched by TOP BYTE, so a handle need only differ there from `0x10`;
    /// - bit 27 (EH) is matched by EXACT EQUALITY and bit 30 (descriptor) is decoded FIRST, so
    ///   neither bounds a handle at all.
    ///
    /// Stating it as a ceiling made the space look one byte from full and cost a real decision:
    /// a generic instantiation was minted onto `0x04`, already the front-end synthetic array's,
    /// because that read left nowhere else for it to go. So this test pins the BOUND rather than
    /// the CENSUS -- it fails if a new flag is added below bit 30, if a bit-tested flag moves down
    /// onto an allocated byte, or if two identity kinds are given one byte, and it stays silent
    /// about how many bytes remain, which is not a property anything depends on. Its backend-side
    /// twin is `arm32::tests::a_typespec_handle_never_aliases_a_bit_tested_symbol_flag`, which
    /// pins the same discipline from the descriptor reference word's side.
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
