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
/// whose word is STILL enumerated as an `ObjectRef` GC root. A reference stored in a bare value-type
/// cell is invisible to the type-keyed root walk (both the entry zero-init and `method_record_roots`
/// key on `is_gc_reference()`), so the sentinel lets those two predicates recognize the cell and
/// treat its word as an object reference. Chosen outside the metadata-token space (no type token is
/// `0xFFFF_FFFF` -- the table is the high byte, `0x00..=0x2B`) and distinct from a plain scalar
/// cell's `TypeHandle(0)`. A stack cell never gets a descriptor, so the handle is never looked up for
/// layout -- `InitStruct`/`FieldLoad`/`FieldStore`/`FieldAddr` are all size/offset-based.
pub(crate) const REF_CELL_HANDLE: lamella_ir::TypeHandle = lamella_ir::TypeHandle(0xFFFF_FFFF);

/// Whether `ty` is a [`REF_CELL_HANDLE`] reference cell -- a memory-homed reference local whose word
/// the GC must trace as an object reference. Used by the entry zero-init and the root record builder.
pub(crate) fn is_ref_cell(ty: MirType) -> bool {
    matches!(ty, MirType::ValueType { handle, .. } if handle == REF_CELL_HANDLE)
}

/// The runtime-support seams a green thread can be switched away inside (or a collection can run
/// inside): the ANCHOR-writing externs. A frame that passes a `RefToInt`-derived raw pointer into
/// one of these can be parked there arbitrarily long, so the source ObjectRef's slot is emitted
/// [`STACKMAP_KIND_PINNED`]. This list mirrors the anchor shims in `tools/runtime-support` (and
/// their `tools/runtime-support-riscv` twins -- the seam NAMES are target-independent); the two
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
