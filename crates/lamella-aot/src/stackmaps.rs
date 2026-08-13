//! The GC stack-map RECORD MODEL shared by every record-emitting backend: the `.lamella_stackmaps`
//! record modes and root kinds, the record encoder, the anchor-seam extern list the PINNED analysis
//! keys on, and one assembly's GLOBAL-roots statics record. ARM32 and RISC-V emit the SAME
//! byte format (`lamella-link` gathers both machines' records into one pointer table, and each
//! target's runtime-support walker reads the same layout), so the model lives here rather than
//! being forked per backend. Per-target pieces stay in the backends: which slot a value lives in
//! ([`crate::arm32`]'s `spilled_slot_offsets` vs the RISC-V spilled frame) and each function's
//! frame constants.

use alloc::vec::Vec;
use lamella_ir::{Function, Inst, MirType};

/// `.lamella_stackmaps` record mode: one METHOD_SLOTS record per safepoint-bearing method --
/// `frame_words`/`ret_lr_word` give the fixed frame hop, `roots` enumerate EVERY ref-typed slot
/// (liveness-free; sound because refs are memory-homed across safepoints and ref slots are
/// zero-initialized at the prologue). The record layout, little-endian, word-aligned:
///
/// ```text
///   u32 func_addr    (absolute reloc -- R_ARM_ABS32 / R_RISCV_32 -- to the function symbol;
///                     on ARM bit 0 = Thumb, mask before matching)
///   u32 code_size    (a PC matches iff func_addr <= (pc & !1) < func_addr + code_size)
///   u16 mode         (1 = METHOD_SLOTS, 2 = STATICS)
///   u16 frame_words  (SP delta from the stopped SP to the caller's SP, in words)
///   u16 ret_lr_word  (word offset from the stopped SP of the saved return address -- LR on ARM,
///                     RA on RISC-V)
///   u16 root_count
///   u16 roots[root_count]   bits[13:0] = slot WORD offset from SP, bits[15:14] = kind
///   (pad to u32)
/// ```
pub const STACKMAP_MODE_METHOD_SLOTS: u16 = 1;
/// `.lamella_stackmaps` record mode 2: GLOBAL roots in a fixed RAM region -- `func_addr` holds the
/// region base ADDRESS (no relocation), `code_size` the region size in bytes, and each root's
/// word offset indexes the region. Emitted once per assembly for its ref-bearing static rows.
pub const STACKMAP_MODE_STATICS: u16 = 2;
/// Root kind: an object reference -- points at an object header; relocate = rewrite to the moved
/// header.
pub const STACKMAP_KIND_OBJECT_REF: u16 = 0;
/// Root kind: a managed (maybe-interior, maybe-non-heap) pointer -- the collector range-checks it
/// and, when it lands in the heap, resolves the owning allocation and rebases by the move delta.
pub const STACKMAP_KIND_MANAGED_PTR: u16 = 1;
/// Root kind: a PINNED object reference -- the referenced object must not move while this frame is
/// live, because the frame (a runtime seam body) derived a raw native pointer from it that a parked
/// native callee still holds (e.g. `recv_poll`'s buffer across an Io park).
pub const STACKMAP_KIND_PINNED: u16 = 2;
/// Root kind: a Python tagged value -- traced only when its tag marks a heap pointer (reserved on
/// the C# lane; the Python lowering's `PyValue` slots take it).
pub const STACKMAP_KIND_TAGGED: u16 = 3;

/// The sentinel `ValueType` layout handle marking a one-word ObjectRef CELL: an ADDRESS-taken
/// reference local, memory-homed so `&local` is a real pointer (a `ref`/`out` reference parameter),
/// whose word is STILL enumerated as an `ObjectRef` GC root.
///
/// **THE SENTINEL IS AN IDENTITY, NOT A SIGNAL.** What a slot contributes to the collector is
/// answered by [`slot_roots`], where a ref cell is not a special case at all -- it is the ordinary
/// trace map `RefWords::at_word(0)`. All this handle marks is that the cell is not any metadata
/// type. It is chosen outside the metadata-token space (no type token is `0xFFFF_FFFF` -- the table
/// is the high byte, `0x00..=0x2B`) and distinct from a plain scalar cell's `TypeHandle(0)`. A stack
/// cell never gets a descriptor, so the handle is never looked up for layout --
/// `InitStruct`/`FieldLoad`/`FieldStore`/`FieldAddr` are all size/offset-based.
pub(crate) const REF_CELL_HANDLE: lamella_ir::TypeHandle = lamella_ir::TypeHandle(0xFFFF_FFFF);

/// The sentinel `ValueType` layout handle marking a frame-materialized EXCEPTION OBJECT cell: the
/// `[TypeDesc*][payload...]` slot a binding `catch` lays so the caught exception is an object rather
/// than a bare tag (see `cil::materialize_catch_binding`). Like [`REF_CELL_HANDLE`] it lives outside
/// the metadata-token space and is never looked up for layout -- `InitStruct`/`FieldAddr` are
/// size-and-offset-based.
///
/// **IT IS DELIBERATELY NOT `REF_CELL_HANDLE`, and the difference is the whole point of a second
/// sentinel.** A ref cell's word is enumerated as an `ObjectRef` root because it holds a reference the
/// collector must trace and relocate. This cell's words are a DESCRIPTOR ADDRESS and zeroed fields --
/// neither is a heap reference, and a mark-compact collector REWRITES what it accepts as a root, so
/// enumerating them would corrupt the header. A plain value-type cell is invisible to the type-keyed
/// root walk, which is exactly the treatment this one needs.
pub(crate) const EXCEPTION_CELL_HANDLE: lamella_ir::TypeHandle =
    lamella_ir::TypeHandle(0xFFFF_FFFE);

/// Whether `handle` marks a FRAME CELL rather than a metadata type -- the anonymous scalar cell
/// (`TypeHandle(0)`), [`REF_CELL_HANDLE`], or [`EXCEPTION_CELL_HANDLE`].
///
/// **A FRAME CELL HAS NO TYPE IDENTITY TO RESPELL, WHICH IS WHY `build::rebase_identities` MUST ASK
/// THIS BEFORE IT REBASES A `ValueType` SLOT.** That pass exists to turn an identity the OWNER minted
/// out of its own tables into the CALLER's spelling. These three are minted by the LOWERING, out of
/// no tables at all: the doc comments above say they live outside the metadata-token space and are
/// never looked up for layout. Handing one to the rebase asks which of the caller's types occupies
/// row `0x00000000` or `0xFFFFFFFF`, and the honest answer -- none -- came back as a refusal of the
/// whole body.
///
pub(crate) fn is_frame_cell_handle(handle: lamella_ir::TypeHandle) -> bool {
    matches!(
        handle,
        lamella_ir::TypeHandle(0) | REF_CELL_HANDLE | EXCEPTION_CELL_HANDLE
    )
}

/// Whether `ty` is a [`REF_CELL_HANDLE`] reference cell -- a memory-homed reference local whose word
/// the GC must trace as an object reference.
///
/// **TEST-ONLY.** No emitter asks this question: the entry zero-init and the root record builder
/// both go through [`slot_roots`], where a ref cell is not a special case but the ordinary value
/// `RefWords::at_word(0)`. What this predicate serves is a test asserting that a memory-homed
/// reference local really does take the sentinel handle -- a claim about the LOWERING, worth pinning
/// independently of who reads the handle afterwards.
#[cfg(test)]
pub(crate) fn is_ref_cell(ty: MirType) -> bool {
    matches!(ty, MirType::ValueType { handle, .. } if handle == REF_CELL_HANDLE)
}

/// Every GC root one value's frame slot contributes, as `(byte offset from the slot base, kind)`.
///
/// **THE POINT OF THIS FUNCTION IS THAT TWO PLACES MUST AGREE AND USED TO AGREE BY CONVENTION.**
/// The per-method record builder says which words the collector VISITS; the entry zero-init says
/// which words are guaranteed to read null before anything writes them. A word in the first list and
/// not the second is stack garbage traced -- and RELOCATED -- as a pointer; a word in neither is a
/// live reference the collector never sees. Both backends had their own copy of the first rule and a
/// differently-shaped copy of the second, welded by a comment saying "in LOCKSTEP".
///
/// A VALUE-TYPE cell contributes one root per reference word of its trace map, which is what makes a
/// reference inside a struct local a root at all -- see [`MirType::ValueType`]'s `refs`. Both
/// sentinel cells fall out of the same rule rather than needing an arm: a ref cell's map is word 0,
/// an exception cell's is empty.
pub(crate) fn slot_roots(ty: MirType, pinned: bool) -> impl Iterator<Item = (u32, u16)> {
    let scalar = match ty {
        MirType::ObjectRef if pinned => Some(STACKMAP_KIND_PINNED),
        MirType::ObjectRef => Some(STACKMAP_KIND_OBJECT_REF),
        MirType::ManagedPtr => Some(STACKMAP_KIND_MANAGED_PTR),
        MirType::PyValue => Some(STACKMAP_KIND_TAGGED),
        _ => None,
    };
    let interior = match ty {
        MirType::ValueType { refs, .. } => Some(refs),
        _ => None,
    };
    scalar
        .map(|kind| (0u32, kind))
        .into_iter()
        .chain(
            interior
                .into_iter()
                .flat_map(|refs| refs.offsets().map(|off| (off, STACKMAP_KIND_OBJECT_REF))),
        )
}

/// The runtime-support seams a green thread can be switched away inside (or a collection can run
/// inside): the ANCHOR-writing externs. A frame that passes a `RefToInt`-derived raw pointer into
/// one of these can be parked there arbitrarily long, so the source ObjectRef's slot is emitted
/// [`STACKMAP_KIND_PINNED`]. This list mirrors the anchor shims in `tools/runtime/runtime-support` (and
/// their `tools/runtime/runtime-support-riscv` twins -- the seam NAMES are target-independent); the two
/// sides are welded by the walk contract, not by code -- keep them in step.
pub(crate) const ANCHOR_SEAM_EXTERNS: &[&str] = &[
    "lamella_thread_yield",
    "lamella_thread_join",
    "lamella_thread_sleep",
    "lamella_thread_connect_poll",
    "lamella_thread_accept_poll",
    "lamella_thread_send_poll",
    "lamella_thread_recv_poll",
    "lamella_gc_alloc",
    "lamella_monitor_enter",
    "lamella_monitor_wait",
    "lamella_string_substring",
    "lamella_char_to_string",
    "lamella_double_to_string",
    "lamella_gc_walk_roots",
    "lamella_gc_count_roots",
];

/// One assembly's static region as the OBJECT path emits it: the linker-placed region
/// symbol's identity, its byte size, and its GLOBAL-roots (mode 2) stack-map record rows. The
/// region has NO fixed address -- every `ldsfld`/`stsfld` and the record's base word carry
/// relocations against `__lamella_statics_<suffix>`, and `lamella-link` places the region in a RAM
/// window and defines the symbol. The record's `func_addr` word is therefore emitted 0 + reloc
/// (exactly like a method record's), so the walker reads the LINKED base with no format change.
#[derive(Debug, Clone, Default)]
pub struct AssemblyStatics {
    /// The assembly-identity suffix both symbol names derive from: EIGHT lowercase hex digits
    /// (fnv1a32 of the assembly's CIL bytes -- the same hash that prefixes a library object's
    /// internal `L<hash>.f<rid>` symbols). The linker's region matcher REQUIRES the 8-hex shape.
    pub suffix: alloc::string::String,
    /// The region's size in bytes: `(1 + static field count) * 4` -- word 0 is the reserved
    /// EH-marker slot (dense slots start at 1), present in EVERY region so the entry assembly's
    /// word 0 can serve as the shared `__lamella_eh_tag` home.
    pub region_bytes: u32,
    /// Root entries, encoded exactly like a method record's: word offset | kind << 14.
    pub roots: Vec<u16>,
}

impl AssemblyStatics {
    /// The RAM region symbol the linker defines (`__lamella_statics_<suffix>`).
    #[must_use]
    pub fn region_symbol(&self) -> alloc::string::String {
        alloc::format!("{}{}", lamella_elf::STATICS_BASE_PREFIX, self.suffix)
    }

    /// The mode-2 statics record's data symbol (`__lamella_smstat_<suffix>`).
    #[must_use]
    pub fn record_symbol(&self) -> alloc::string::String {
        alloc::format!("{}{}", lamella_elf::STACKMAP_STATICS_PREFIX, self.suffix)
    }
}

/// Encodes one `.lamella_stackmaps` record (see [`STACKMAP_MODE_METHOD_SLOTS`] for the layout).
/// The `func_addr` word is emitted 0 for a method record -- the absolute relocation the caller
/// registers patches it -- and holds the region base for a statics record.
pub(crate) fn encode_stackmap_record(
    out: &mut Vec<u8>,
    func_addr: u32,
    code_size: u32,
    mode: u16,
    frame_words: u16,
    ret_lr_word: u16,
    roots: &[u16],
) {
    out.extend_from_slice(&func_addr.to_le_bytes());
    out.extend_from_slice(&code_size.to_le_bytes());
    out.extend_from_slice(&mode.to_le_bytes());
    out.extend_from_slice(&frame_words.to_le_bytes());
    out.extend_from_slice(&ret_lr_word.to_le_bytes());
    out.extend_from_slice(&(roots.len() as u16).to_le_bytes());
    for &root in roots {
        out.extend_from_slice(&root.to_le_bytes());
    }
    while out.len() % 4 != 0 {
        out.push(0);
    }
}

/// The values whose slots must be emitted [`STACKMAP_KIND_PINNED`]: ObjectRefs a `RefToInt` derives
/// a raw pointer from, in a function that `CallNative`s an anchor seam (see
/// [`ANCHOR_SEAM_EXTERNS`]). Anywhere else a raw derived pointer cannot outlive a collection --
/// on the cooperative tier a collection only runs inside those seams.
pub(crate) fn pinned_values(func: &Function, externs: &[alloc::string::String]) -> Vec<bool> {
    let mut pinned = alloc::vec![false; func.value_types.len()];
    let parks_or_allocates = func.blocks.iter().any(|b| {
        b.insts.iter().any(|(_, i)| {
            matches!(i, Inst::CallNative { symbol, .. }
                if externs
                    .get(*symbol as usize)
                    .is_some_and(|n| ANCHOR_SEAM_EXTERNS.contains(&n.as_str())))
        })
    });
    if !parks_or_allocates {
        return pinned;
    }
    for block in &func.blocks {
        for (_, inst) in &block.insts {
            if let Inst::Convert {
                value,
                kind: lamella_ir::ConvKind::RefToInt,
            } = inst
            {
                if func.value_type(*value) == Some(MirType::ObjectRef) {
                    pinned[value.index()] = true;
                }
            }
        }
    }
    pinned
}
