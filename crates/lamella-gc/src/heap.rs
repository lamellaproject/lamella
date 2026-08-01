//! A precise, moving (mark-compact) collector over a flat byte heap, in the exact
//! object/type-descriptor/stack-map formats the AOT backend (`lamella_aot::arm32`)
//! emits. This is the device collector's first increment, exercised on the host in
//! safe Rust: the heap is a `Vec<u8>` and an address is an offset into it, so the
//! whole thing runs under `#![forbid(unsafe_code)]`. The device wiring (raw memory
//! behind the `lamella_gc_alloc` C ABI, a real frame walk through the saved LR) is a
//! later increment and is deliberately absent here.

extern crate alloc;

use alloc::vec::Vec;

/// The size of an object header, in bytes: one little-endian `u32` holding the
/// object's [`TypeDesc`] id.
pub const HEADER_SIZE: u32 = 4;

/// The heap alignment: every object start, payload, and reference slot is a
/// multiple of this, so payloads are padded up to it.
pub const ALIGN: u32 = 4;

/// A managed reference: the *payload* address of an object (its header sits at
/// `address - HEADER_SIZE`). Address `0` is the null reference, matching the
/// backend, so it can never be a real payload (the heap base reserves it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Ref(pub u32);

impl Ref {
    /// The null reference (address `0`).
    pub const NULL: Ref = Ref(0);

    /// Whether this is the null reference.
    #[must_use]
    pub const fn is_null(self) -> bool {
        self.0 == 0
    }

    /// The address of this object's header (`payload - HEADER_SIZE`). Only valid
    /// on a non-null reference.
    #[must_use]
    pub(crate) const fn header_addr(self) -> u32 {
        self.0 - HEADER_SIZE
    }
}

/// A type's GC layout: how big its payload is and where its reference fields live.
/// This is the decoded form of the backend's `[u32 payload_size][u32 nrefs][u32
/// ref_offsets...]` descriptor.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TypeDesc {
    /// The payload size in bytes (excluding the header). The allocator rounds the
    /// reserved space up to [`ALIGN`].
    pub payload_size: u32,
    /// Byte offsets *within the payload* of 4-byte slots holding a child reference as a
    /// BARE [`Ref`] (a raw payload address, or null). Each is traced and relocated
    /// unconditionally -- the layout a C# object reports.
    pub ref_offsets: Vec<u32>,
    /// Byte offsets *within the payload* of 4-byte slots holding a TAGGED value (the
    /// interpreter's `Value`): such a slot is a managed reference -- traced and relocated
    /// -- iff its low two bits are clear and it is non-null; otherwise (a fixnum or
    /// singleton, which set a low bit) it is left untouched. This is how a Python
    /// container's interior is scanned: its elements are tagged words, not bare pointers.
    pub tagged_offsets: Vec<u32>,
}

impl TypeDesc {
    /// Decodes one descriptor from the backend's little-endian blob `[u32
    /// payload_size][u32 nrefs][u32 ref_offsets...]`, returning the descriptor and
    /// the number of bytes consumed, or `None` if `bytes` is truncated.
    #[must_use]
    pub fn decode(bytes: &[u8]) -> Option<(TypeDesc, usize)> {
        let payload_size = read_u32(bytes, 0)?;
        let nrefs = read_u32(bytes, 4)? as usize;
        let mut ref_offsets = Vec::with_capacity(nrefs);
        let mut pos = 8;
        for _ in 0..nrefs {
            ref_offsets.push(read_u32(bytes, pos)?);
            pos += 4;
        }
        Some((
            TypeDesc {
                payload_size,
                ref_offsets,
                tagged_offsets: Vec::new(),
            },
            pos,
        ))
    }
}

/// Reads a little-endian `u32` at byte offset `at` in `bytes`, or `None` if it
/// would run past the end.
fn read_u32(bytes: &[u8], at: usize) -> Option<u32> {
    let end = at.checked_add(4)?;
    let slice = bytes.get(at..end)?;
    Some(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

/// Reads a little-endian `u16` at byte offset `at` in `bytes`, or `None` if it
/// would run past the end.
fn read_u16(bytes: &[u8], at: usize) -> Option<u16> {
    let end = at.checked_add(2)?;
    let slice = bytes.get(at..end)?;
    Some(u16::from_le_bytes([slice[0], slice[1]]))
}

/// Rounds `n` up to the next multiple of [`ALIGN`].
pub(crate) const fn align_up(n: u32) -> u32 {
    (n + (ALIGN - 1)) & !(ALIGN - 1)
}

/// One GC safepoint's stack map: where the live roots sit in a frame when a call or
/// allocation returns. Mirrors `lamella_aot::arm32::StackMapEntry`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackMapEntry {
    /// The safepoint's return address (a native code offset) -- the lookup key.
    pub return_pc: u32,
    /// The frame the safepoint opened, in bytes. (The saved LR a multi-frame walk
    /// would read sits at `SP-at-the-call + frame_size`; that walk is a later
    /// increment.)
    pub frame_size: u16,
    /// Byte offsets from SP-at-the-call of the live root slots, each holding a [`Ref`].
    pub ref_offsets: Vec<u16>,
    /// Byte offsets from SP-at-the-call of the PINNED root slots -- roots whose object the
    /// collection must leave AT ITS CURRENT ADDRESS (`STACKMAP_KIND_PINNED` in the backend's
    /// record model).
    ///
    /// A pinned slot is still a root: it is marked and its object survives, it simply does not
    /// move. This is what a C# `fixed` statement needs, and it is the ONLY thing that can serve
    /// it: `fixed` hands the program a `T*`, an unmanaged pointer is not GC-tracked, so a raw
    /// interior address into a relocated object cannot be corrected -- nobody knows where it is.
    ///
    /// **Disjoint from [`Self::ref_offsets`]**: a slot appears in exactly one of the two lists.
    /// The relocate pass rewrites each reported slot through the forwarding map, and a slot
    /// reported twice would be looked up again by its already-rewritten value -- so double
    /// reporting is a corruption, not a redundancy.
    pub pinned_offsets: Vec<u16>,
}

/// The decoded GC stack maps for a lowered program: one entry per safepoint, sorted
/// by `return_pc` for binary search. The decoded counterpart of
/// `lamella_aot::arm32::StackMaps`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StackMapTable {
    entries: Vec<StackMapEntry>,
}

impl StackMapTable {
    /// Decodes the little-endian wire format `u32 count`, then each entry
    /// `u32 return_pc; u16 frame_size; u16 nrefs; u16 ref_offsets[nrefs]`. Returns
    /// `None` if the bytes are truncated.
    ///
    /// **This format carries no root KIND, so a table decoded here pins nothing** --
    /// every entry's [`StackMapEntry::pinned_offsets`] is empty. Pins reach a collection
    /// through [`Self::from_entries`], which is what the device install path and the
    /// collector's own harnesses use. The format that does carry kinds (including
    /// `STACKMAP_KIND_PINNED`) is the backend's per-method `.lamella_stackmaps` record, read
    /// by the target's runtime-support root walker; this decoder is not that reader.
    #[must_use]
    pub fn decode(bytes: &[u8]) -> Option<StackMapTable> {
        let count = read_u32(bytes, 0)? as usize;
        let mut entries = Vec::with_capacity(count);
        let mut pos = 4;
        for _ in 0..count {
            let return_pc = read_u32(bytes, pos)?;
            let frame_size = read_u16(bytes, pos + 4)?;
            let nrefs = read_u16(bytes, pos + 6)? as usize;
            pos += 8;
            let mut ref_offsets = Vec::with_capacity(nrefs);
            for _ in 0..nrefs {
                ref_offsets.push(read_u16(bytes, pos)?);
                pos += 2;
            }
            entries.push(StackMapEntry {
                return_pc,
                frame_size,
                ref_offsets,
                pinned_offsets: Vec::new(),
            });
        }
        Some(StackMapTable { entries })
    }

    /// Whether any entry pins a root -- the cheap check that lets a collection skip the
    /// read-only pre-pass that gathers pinned addresses when a program uses no `fixed` at all.
    #[must_use]
    pub fn has_pins(&self) -> bool {
        self.entries
            .iter()
            .any(|entry| !entry.pinned_offsets.is_empty())
    }

    /// Builds a table from already-decoded entries (sorting them by `return_pc` so
    /// [`Self::lookup`] can binary-search regardless of input order).
    #[must_use]
    pub fn from_entries(mut entries: Vec<StackMapEntry>) -> StackMapTable {
        entries.sort_by_key(|e| e.return_pc);
        StackMapTable { entries }
    }

    /// The entries, in `return_pc` order.
    #[must_use]
    pub fn entries(&self) -> &[StackMapEntry] {
        &self.entries
    }

    /// The stack-map entry whose `return_pc` equals `return_pc`, or `None` if no
    /// safepoint matches. Binary search over the sorted entries -- the backend emits
    /// the safepoint PC as the address of the instruction *after* the call, so the
    /// collector looks up an exact return address.
    #[must_use]
    pub fn lookup(&self, return_pc: u32) -> Option<&StackMapEntry> {
        self.entries
            .binary_search_by_key(&return_pc, |e| e.return_pc)
            .ok()
            .map(|i| &self.entries[i])
    }
}

/// A flat byte heap with a bump allocator and a precise, moving (mark-compact)
/// collector, in the backend's on-device formats. Offsets into [`Self::bytes`] are
/// addresses; address `0` is reserved as the null reference, so allocation begins at
/// [`ALIGN`].
#[derive(Debug)]
pub struct Heap {
    /// The backing store; an address is an index into this.
    bytes: Vec<u8>,
    /// The bump pointer: the next free address. Survivors compact below it.
    top: u32,
    /// The type-descriptor table; an object's header word indexes it.
    type_descs: Vec<TypeDesc>,
}

impl Heap {
    /// Creates a heap with `capacity` bytes of backing store and the given
    /// type-descriptor table (an object's header word is an index into it). The
    /// first [`ALIGN`] bytes are reserved so no live payload can collide with the
    /// null address `0`.
    #[must_use]
    pub fn new(capacity: usize, type_descs: Vec<TypeDesc>) -> Heap {
        let mut bytes = alloc::vec![0u8; capacity.max(ALIGN as usize)];
        bytes[..ALIGN as usize].fill(0);
        Heap {
            bytes,
            top: ALIGN,
            type_descs,
        }
    }

    /// The bump pointer (the next free address). Equals [`ALIGN`] on an empty heap;
    /// after a collection it is the end of the last survivor.
    #[must_use]
    pub fn top(&self) -> u32 {
        self.top
    }

    /// The total capacity of the backing store, in bytes.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.bytes.len()
    }

    /// The bytes currently in use (`top` minus the reserved null slot).
    #[must_use]
    pub fn used(&self) -> u32 {
        self.top - ALIGN
    }

    /// The type-descriptor table.
    #[must_use]
    pub fn type_descs(&self) -> &[TypeDesc] {
        &self.type_descs
    }

    /// Allocates an object of type `type_desc_id`: writes the header word, reserves a
    /// zeroed, 4-aligned payload, bumps the pointer, and returns the *payload*
    /// address as a [`Ref`]. Returns `None` if `type_desc_id` is unknown or the heap
    /// is full (no collection is attempted here -- the caller drives [`Self::collect`]).
    #[must_use]
    pub fn alloc(&mut self, type_desc_id: u32) -> Option<Ref> {
        let payload_size = self.type_descs.get(type_desc_id as usize)?.payload_size;
        let reserved = align_up(payload_size);
        let object_start = self.top;
        let next = object_start.checked_add(HEADER_SIZE)?.checked_add(reserved)?;
        if next as usize > self.bytes.len() {
            return None;
        }
        self.write_u32(object_start, type_desc_id);
        self.top = next;
        Some(Ref(object_start + HEADER_SIZE))
    }

    /// The [`TypeDesc`] id in `reference`'s header (read at `reference - HEADER_SIZE`).
    /// Panics if `reference` is null.
    #[must_use]
    pub fn type_id_of(&self, reference: Ref) -> u32 {
        debug_assert!(!reference.is_null(), "type_id_of(null)");
        self.read_u32(reference.header_addr())
    }

    /// Reads a little-endian `u32` at address `addr`. Panics if out of bounds.
    #[must_use]
    pub fn read_u32(&self, addr: u32) -> u32 {
        let at = addr as usize;
        u32::from_le_bytes([
            self.bytes[at],
            self.bytes[at + 1],
            self.bytes[at + 2],
            self.bytes[at + 3],
        ])
    }

    /// Writes a little-endian `u32` at address `addr`. Panics if out of bounds.
    pub fn write_u32(&mut self, addr: u32, value: u32) {
        let at = addr as usize;
        self.bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }

    /// Reads the child [`Ref`] at byte offset `ref_offset` within `reference`'s payload.
    #[must_use]
    pub fn read_ref_field(&self, reference: Ref, ref_offset: u32) -> Ref {
        Ref(self.read_u32(reference.0 + ref_offset))
    }

    /// Writes the child [`Ref`] at byte offset `ref_offset` within `reference`'s payload.
    pub fn write_ref_field(&mut self, reference: Ref, ref_offset: u32, value: Ref) {
        self.write_u32(reference.0 + ref_offset, value.0);
    }

    /// Reclaims every object unreachable from the roots and compacts the survivors
    /// toward the heap base, rewriting every reference (roots and object fields) to
    /// the survivor's new address. Stop-the-world, non-generational, no finalizers.
    ///
    /// `enumerate_roots` mirrors the interpreter's collector signature
    /// (`lamella_cil_runtime::object::Heap::collect`): it visits each root slot mutably and
    /// is called **twice** -- once to seed the mark, once to relocate. The caller
    /// exposes whatever holds roots (frames decoded via a stack map, statics, ...);
    /// see [`Heap::collect_frame`] for the single-frame stack-map driver.
    ///
    /// Relocation in brief: MARK seeds from the roots and BFS-traces object fields
    /// with a worklist (no recursion). COMPACT assigns survivors new addresses packed
    /// from the base in ascending heap order and moves their bytes down (ascending,
    /// so a move never clobbers an unmoved survivor). RELOCATE rewrites every root and
    /// every survivor field through the `old_payload -> new_payload` forwarding map;
    /// null stays null. `top` becomes the end of the last survivor.
    ///
    /// The algorithm itself lives in [`mark_compact`], shared verbatim with the
    /// device collector ([`crate::device_heap::DeviceHeap`]); the only difference is
    /// the header-word -> type lookup, supplied here by [`TableResolver`] (a
    /// table-index lookup) and on device by a raw-pointer dereference. This keeps one
    /// collector serving both the host-test heap and the on-device heap.
    #[cfg(feature = "gc-collect")]
    pub fn collect<R>(&mut self, enumerate_roots: R)
    where
        R: FnMut(&mut dyn FnMut(&mut Ref)),
    {
        self.collect_with_pins(enumerate_roots, &[]);
    }

    /// [`Self::collect`], with `pinned` naming the payload addresses of objects the compaction
    /// must leave WHERE THEY ARE.
    ///
    /// A pinned object is an ordinary survivor in every other respect -- it must still be
    /// reported by `enumerate_roots` so it is marked, and its forwarding entry maps it to
    /// itself, so every reference to it is "relocated" to the address it already had. What it
    /// buys is the one thing a raw interior pointer needs: an address that does not change.
    ///
    /// The cost is stated rather than hidden: the survivors below a pinned object cannot slide
    /// past it, so the space reclaimed beneath it becomes a GAP this bump-allocated heap cannot
    /// hand out until the pin is released and a later collection closes it. That is the standard
    /// price of pinning a compacting heap, and it is why a pin must be as short-lived as the
    /// `fixed` block that asks for it.
    #[cfg(feature = "gc-collect")]
    pub fn collect_with_pins<R>(&mut self, enumerate_roots: R, pinned: &[u32])
    where
        R: FnMut(&mut dyn FnMut(&mut Ref)),
    {
        let resolver = TableResolver {
            type_descs: &self.type_descs,
        };
        let top = self.top;
        self.top = mark_compact(&mut self.bytes, top, &resolver, enumerate_roots, pinned);
    }

    /// Collects with the roots taken from a single AOT frame, located through one
    /// stack-map entry. `frame` is the frame's byte image and `sp` is the address of
    /// SP-at-the-call within it; each root sits at `sp + entry.ref_offsets[i]` (or
    /// `sp + entry.pinned_offsets[i]`) and holds a [`Ref`]. The relocated references are
    /// written back into `frame` so the caller's frame stays consistent -- a pinned root's
    /// slot is written back unchanged, which is the point.
    ///
    /// One frame only: multi-frame walking via the saved LR
    /// (`sp + frame_size`) is a later increment.
    #[cfg(feature = "gc-collect")]
    pub fn collect_frame(&mut self, frame: &mut [u8], sp: u32, entry: &StackMapEntry) {
        let pinned = pinned_frame_roots(frame, sp, entry);
        self.collect_with_pins(|visit| visit_frame_roots(frame, sp, entry, visit), &pinned);
    }

    /// Collects with the roots gathered by walking the whole AOT call stack, from the
    /// innermost (top) frame down through each caller. `stack` is the call stack's byte
    /// image; `top_sp`/`top_return_pc` identify the top frame's safepoint (SP-at-the-call
    /// and the safepoint return address). The relocated references are written back into
    /// `stack`, so the relocate pass persists into the stack image and every frame's root
    /// slots end up holding the survivors' new addresses.
    ///
    /// The frame-walk convention is the all-spilled baseline of `lamella_aot::arm32`: at a
    /// frame with safepoint return address `return_pc` and SP-at-the-call `sp`, with
    /// `entry = stack_maps.lookup(return_pc)`, the roots are the [`Ref`]s at
    /// `sp + entry.ref_offsets[i]`; the caller's return address (the saved LR) sits at
    /// `sp + entry.frame_size` (no extra callee-saved words in this baseline); and the
    /// caller's SP-at-the-call is `sp + entry.frame_size + 4` (just above that saved LR).
    /// The walk continues while `stack_maps.lookup(return_pc)` finds an entry and stops
    /// when it returns `None` -- the bottom frame's saved LR is the runtime entry
    /// trampoline's return address, which has no safepoint. A frame cap guards against a
    /// malformed or cyclic walk so a corrupt chain stops rather than looping forever.
    #[cfg(feature = "gc-collect")]
    pub fn collect_stack(
        &mut self,
        stack: &mut [u8],
        top_sp: u32,
        top_return_pc: u32,
        stack_maps: &StackMapTable,
    ) {
        let pinned = pinned_stack_roots(stack, top_sp, top_return_pc, stack_maps);
        self.collect_with_pins(
            |visit| visit_stack_roots(stack, top_sp, top_return_pc, stack_maps, visit),
            &pinned,
        );
    }
}

/// A frame budget for a stack walk: a real call stack is far shallower, so hitting this means
/// the saved-LR chain is malformed (or cyclic); stop walking rather than spin forever.
#[cfg(feature = "gc-collect")]
const MAX_FRAMES: u32 = 4096;

/// Reads a [`Ref`] out of a frame/stack image at byte index `at`.
#[cfg(feature = "gc-collect")]
fn read_slot(image: &[u8], at: usize) -> Ref {
    Ref(u32::from_le_bytes([
        image[at],
        image[at + 1],
        image[at + 2],
        image[at + 3],
    ]))
}

/// Reports every root slot of ONE frame to `visit` and writes each (possibly relocated)
/// reference back. Both root lists are walked: a pinned root is a root, so leaving it out would
/// let the collection reclaim the very object the pin exists to hold still.
#[cfg(feature = "gc-collect")]
fn visit_frame_roots(
    frame: &mut [u8],
    sp: u32,
    entry: &StackMapEntry,
    visit: &mut dyn FnMut(&mut Ref),
) {
    for &offset in entry.ref_offsets.iter().chain(entry.pinned_offsets.iter()) {
        let at = (sp + u32::from(offset)) as usize;
        let mut reference = read_slot(frame, at);
        visit(&mut reference);
        frame[at..at + 4].copy_from_slice(&reference.0.to_le_bytes());
    }
}

/// The payload addresses named by ONE frame's pinned slots. A READ-ONLY pre-pass, because
/// compaction has to know which objects may not move before it places the first survivor.
#[cfg(feature = "gc-collect")]
fn pinned_frame_roots(frame: &[u8], sp: u32, entry: &StackMapEntry) -> Vec<u32> {
    entry
        .pinned_offsets
        .iter()
        .map(|&offset| read_slot(frame, (sp + u32::from(offset)) as usize).0)
        .filter(|&address| address != Ref::NULL.0)
        .collect()
}

/// Walks the AOT call stack from `(top_sp, top_return_pc)` down through each caller, reporting
/// every frame's root slots to `visit` and writing the relocated references back into `stack`.
///
/// The frame-walk convention is the all-spilled baseline of `lamella_aot::arm32`: at a frame with
/// safepoint return address `return_pc` and SP-at-the-call `sp`, with
/// `entry = stack_maps.lookup(return_pc)`, the roots are the [`Ref`]s at `sp + offset`; the
/// caller's return address (the saved LR) sits at `sp + entry.frame_size`; and the caller's
/// SP-at-the-call is `sp + entry.frame_size + 4` (just above that saved LR). The walk continues
/// while each return address names a safepoint and stops when one does not -- the bottom frame's
/// saved LR is the runtime entry trampoline's return address, which has no safepoint.
///
/// Shared by [`Heap::collect_stack`] and [`crate::device_heap::DeviceHeap::collect_stack`] so the
/// host rehearsal and the device collection cannot walk differently.
#[cfg(feature = "gc-collect")]
pub(crate) fn visit_stack_roots(
    stack: &mut [u8],
    top_sp: u32,
    top_return_pc: u32,
    stack_maps: &StackMapTable,
    visit: &mut dyn FnMut(&mut Ref),
) {
    let mut sp = top_sp;
    let mut return_pc = top_return_pc;
    let mut frames = 0u32;
    while let Some(entry) = stack_maps.lookup(return_pc) {
        visit_frame_roots(stack, sp, entry, visit);
        let saved_lr_at = (sp + u32::from(entry.frame_size)) as usize;
        return_pc = read_slot(stack, saved_lr_at).0;
        sp = sp + u32::from(entry.frame_size) + 4;
        frames += 1;
        if frames >= MAX_FRAMES {
            break;
        }
    }
}

/// The payload addresses of every pinned root on the whole call stack -- the same walk as
/// [`visit_stack_roots`], read-only, run first so compaction knows what it may not move.
/// Returns empty immediately when no entry pins anything, which is every program that uses no
/// `fixed` statement.
#[cfg(feature = "gc-collect")]
pub(crate) fn pinned_stack_roots(
    stack: &[u8],
    top_sp: u32,
    top_return_pc: u32,
    stack_maps: &StackMapTable,
) -> Vec<u32> {
    let mut pinned = Vec::new();
    if !stack_maps.has_pins() {
        return pinned;
    }
    let mut sp = top_sp;
    let mut return_pc = top_return_pc;
    let mut frames = 0u32;
    while let Some(entry) = stack_maps.lookup(return_pc) {
        pinned.extend(pinned_frame_roots(stack, sp, entry));
        let saved_lr_at = (sp + u32::from(entry.frame_size)) as usize;
        return_pc = read_slot(stack, saved_lr_at).0;
        sp = sp + u32::from(entry.frame_size) + 4;
        frames += 1;
        if frames >= MAX_FRAMES {
            break;
        }
    }
    pinned
}

/// The header-word -> type lookup the [`mark_compact`] algorithm needs, abstracted so
/// the one algorithm serves both the host-test heap and the on-device heap. The only
/// thing that differs between the two is how an object's header word names its
/// [`TypeDesc`]:
/// - host ([`TableResolver`]): the header word is an *index* into a [`TypeDesc`] table;
/// - device ([`crate::device_heap::PtrResolver`]): the header word is a raw `*const
///   TypeDesc` to dereference.
///
/// The resolver answers the two questions compaction and tracing ask of an object's
/// type: how big its payload is (to size the move) and where its reference fields are
/// (to trace and relocate). `for_each_ref_offset` is a callback rather than a returned
/// slice so the device side reads the inline `ref_offsets` array straight out of the
/// descriptor with no per-object allocation (the host side stays `alloc`-free here too).
///
/// # Why both questions also take the object's first payload word
///
/// An ARRAY's footprint is LENGTH-dependent, so its descriptor cannot state it -- the
/// device array descriptor spends word 0 on a mark plus rank and word 1 on the element
/// kind, and the length lives at payload offset 0 of the object itself. A resolver that
/// saw only the header word could not size an array or place its element slots, so both
/// questions carry `payload_head`: the object's first payload word, which an array
/// descriptor reads as the element count and a class descriptor ignores. The engine
/// reads it defensively (0 when the object has no first word), so a zero-payload class
/// never indexes past the region.
#[cfg(feature = "gc-collect")]
pub(crate) trait TypeResolver {
    /// The payload size, in bytes, of the object whose header holds `header_word` and
    /// whose first payload word is `payload_head` (an array's element count).
    fn payload_size(&self, header_word: u32, payload_head: u32) -> u32;

    /// Invokes `f` with each byte offset (within the payload) of a reference field of
    /// the object whose header holds `header_word` and whose first payload word is
    /// `payload_head` (an array's element count).
    fn for_each_ref_offset(&self, header_word: u32, payload_head: u32, f: &mut dyn FnMut(u32));

    /// Invokes `f` with each byte offset (within the payload) of a TAGGED-value slot of
    /// the object whose header holds `header_word` -- traced by tag (see
    /// [`TypeDesc::tagged_offsets`]). The default is none, so a resolver that has no
    /// tagged layout (e.g. the device path until its wire format carries them) need not
    /// override it.
    fn for_each_tagged_offset(&self, _header_word: u32, _f: &mut dyn FnMut(u32)) {}
}

/// The host resolver: an object's header word is an index into a [`TypeDesc`] table.
/// This reproduces exactly the lookup the host engine used before [`mark_compact`] was
/// factored out, so the host tests see identical behaviour.
#[cfg(feature = "gc-collect")]
pub(crate) struct TableResolver<'a> {
    pub(crate) type_descs: &'a [TypeDesc],
}

#[cfg(feature = "gc-collect")]
impl TypeResolver for TableResolver<'_> {
    /// `payload_head` is unread: a host header word is a table index, and the host table
    /// states every payload size outright (the host heap has no length-dependent form).
    fn payload_size(&self, header_word: u32, _payload_head: u32) -> u32 {
        self.type_descs[header_word as usize].payload_size
    }

    /// `payload_head` is unread, for the reason given on [`TableResolver::payload_size`].
    fn for_each_ref_offset(&self, header_word: u32, _payload_head: u32, f: &mut dyn FnMut(u32)) {
        for &ref_offset in &self.type_descs[header_word as usize].ref_offsets {
            f(ref_offset);
        }
    }

    fn for_each_tagged_offset(&self, header_word: u32, f: &mut dyn FnMut(u32)) {
        for &tagged_offset in &self.type_descs[header_word as usize].tagged_offsets {
            f(tagged_offset);
        }
    }
}

/// The mark-compact algorithm itself, over a flat byte heap, shared verbatim by the
/// host [`Heap::collect`] and the device collector. `bytes` is the heap's backing
/// store (offsets into it are addresses, address `0` reserved as null); `resolver`
/// turns an object's header word into its type's payload size and reference offsets;
/// `enumerate_roots` reports the root slots (called twice -- once to seed the mark,
/// once to relocate). Returns the new bump pointer (`top`): the end of the last
/// survivor, or [`ALIGN`] if none survived.
///
/// MARK seeds from the roots and BFS-traces object fields with a worklist (no
/// recursion). COMPACT assigns survivors new addresses packed from the base in
/// ascending heap order and moves their bytes down (ascending, so a move never
/// clobbers an unmoved survivor) -- except a PINNED survivor, which forwards to itself and
/// which the packing cursor steps over. RELOCATE rewrites every root and every survivor
/// field through the `old_payload -> new_payload` forwarding map; null stays null. The
/// freed tail is zeroed so a later allocation never reads stale bytes.
/// NON-HEAP references are expected and are SKIPPED, not traced. An `ObjectRef` root or
/// reference field may legitimately hold an address outside this region: a string literal
/// lowers to a flash/rodata blob and a `Type` is never heap-allocated, so both are real
/// references that the allocator never handed out. Such a word is left exactly as it is --
/// tracing it would index the region out of bounds, and relocating it would rewrite a
/// flash pointer into a heap address. `top` bounds the live region for that test.
/// `pinned` names the payload addresses of survivors that must keep their CURRENT address: each
/// gets a forwarding entry to itself, and the packing cursor steps over it rather than through it,
/// leaving whatever it reclaimed below as a gap. Empty is the ordinary case and costs one
/// `is_empty` test per survivor.
#[cfg(feature = "gc-collect")]
pub(crate) fn mark_compact<R>(
    bytes: &mut [u8],
    top: u32,
    resolver: &dyn TypeResolver,
    mut enumerate_roots: R,
    pinned: &[u32],
) -> u32
where
    R: FnMut(&mut dyn FnMut(&mut Ref)),
{
    let is_heap = |reference: Ref| reference.0 >= HEADER_SIZE && reference.0 < top;
    use alloc::collections::{BTreeMap, BTreeSet};

    let read_word = |bytes: &[u8], addr: u32| -> u32 {
        let at = addr as usize;
        u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
    };
    let read_field = |bytes: &[u8], reference: Ref, ref_offset: u32| -> Ref {
        Ref(read_word(bytes, reference.0 + ref_offset))
    };
    let read_head = |bytes: &[u8], reference: Ref| -> u32 { read_word(bytes, reference.0) };

    let mut live: BTreeSet<u32> = BTreeSet::new();
    let mut work: Vec<Ref> = Vec::new();
    let mark = |reference: &mut Ref, live: &mut BTreeSet<u32>, work: &mut Vec<Ref>| {
        if !reference.is_null() && is_heap(*reference) && live.insert(reference.0) {
            work.push(*reference);
        }
    };
    enumerate_roots(&mut |slot| mark(slot, &mut live, &mut work));
    while let Some(object) = work.pop() {
        let header_word = read_word(bytes, object.header_addr());
        let payload_head = read_head(bytes, object);
        resolver.for_each_ref_offset(header_word, payload_head, &mut |ref_offset| {
            let mut child = read_field(bytes, object, ref_offset);
            mark(&mut child, &mut live, &mut work);
        });
        resolver.for_each_tagged_offset(header_word, &mut |tagged_offset| {
            let word = read_word(bytes, object.0 + tagged_offset);
            if word != 0 && word & 0b11 == 0 {
                let mut child = Ref(word);
                mark(&mut child, &mut live, &mut work);
            }
        });
    }

    let mut forward: BTreeMap<u32, u32> = BTreeMap::new();
    let mut dest = ALIGN;
    for old_payload in live.iter().copied() {
        let header_word = read_word(bytes, old_payload - HEADER_SIZE);
        let payload_head = read_head(bytes, Ref(old_payload));
        let reserved = align_up(resolver.payload_size(header_word, payload_head));
        let object_size = HEADER_SIZE + reserved;
        let start = old_payload - HEADER_SIZE;
        if !pinned.is_empty() && pinned.contains(&old_payload) {
            debug_assert!(dest <= start, "the packing cursor overran a pinned object");
            if dest < start {
                bytes[dest as usize..start as usize].fill(0);
            }
            forward.insert(old_payload, old_payload);
            dest = start + object_size;
            continue;
        }
        let new_payload = dest + HEADER_SIZE;
        forward.insert(old_payload, new_payload);
        let src = start as usize;
        let dst = dest as usize;
        if src != dst {
            bytes.copy_within(src..src + object_size as usize, dst);
        }
        dest += object_size;
    }

    let relocate = |reference: &mut Ref| {
        if !reference.is_null() && is_heap(*reference) {
            *reference = Ref(forward[&reference.0]);
        }
    };
    enumerate_roots(&mut |slot| relocate(slot));
    for (&_old_payload, &new_payload) in forward.iter() {
        let new_ref = Ref(new_payload);
        let header_word = read_word(bytes, new_ref.header_addr());
        let mut offsets: Vec<u32> = Vec::new();
        let payload_head = read_head(bytes, new_ref);
        resolver.for_each_ref_offset(header_word, payload_head, &mut |ref_offset| {
            offsets.push(ref_offset);
        });
        for ref_offset in offsets {
            let mut child = read_field(bytes, new_ref, ref_offset);
            relocate(&mut child);
            let at = (new_ref.0 + ref_offset) as usize;
            bytes[at..at + 4].copy_from_slice(&child.0.to_le_bytes());
        }
        let mut tagged: Vec<u32> = Vec::new();
        resolver.for_each_tagged_offset(header_word, &mut |tagged_offset| tagged.push(tagged_offset));
        for tagged_offset in tagged {
            let word = read_word(bytes, new_ref.0 + tagged_offset);
            if word != 0 && word & 0b11 == 0 {
                let mut child = Ref(word);
                relocate(&mut child);
                let at = (new_ref.0 + tagged_offset) as usize;
                bytes[at..at + 4].copy_from_slice(&child.0.to_le_bytes());
            }
        }
    }

    bytes[dest as usize..].fill(0);
    dest
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    /// A leaf type: one word, no references.
    fn leaf() -> TypeDesc {
        TypeDesc {
            payload_size: 4,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        }
    }

    /// A type with a single reference field at payload offset 0.
    #[cfg_attr(not(feature = "gc-collect"), allow(dead_code))]
    fn one_ref() -> TypeDesc {
        TypeDesc {
            payload_size: 4,
            ref_offsets: vec![0],
            tagged_offsets: Vec::new(),
        }
    }

    #[test]
    fn alloc_lays_out_header_then_payload_and_returns_payload_ref() {
        let descs = vec![TypeDesc {
            payload_size: 8,
            ref_offsets: vec![4],
            tagged_offsets: Vec::new(),
        }];
        let mut heap = Heap::new(1024, descs);
        let a = heap.alloc(0).unwrap();
        assert_eq!(a, Ref(ALIGN + HEADER_SIZE));
        assert_eq!(heap.type_id_of(a), 0);
        assert_eq!(heap.read_u32(a.header_addr()), 0);
        assert_eq!(heap.read_ref_field(a, 4), Ref::NULL);
        assert_eq!(heap.top(), ALIGN + HEADER_SIZE + 8);
    }

    #[test]
    fn alloc_pads_payload_up_to_alignment() {
        let descs = vec![TypeDesc {
            payload_size: 5,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        }];
        let mut heap = Heap::new(1024, descs);
        let _ = heap.alloc(0).unwrap();
        assert_eq!(heap.top(), ALIGN + HEADER_SIZE + 8);
    }

    #[test]
    fn alloc_returns_none_when_full() {
        let descs = vec![leaf()];
        let mut heap = Heap::new((ALIGN + HEADER_SIZE + 4) as usize, descs);
        assert!(heap.alloc(0).is_some());
        assert!(heap.alloc(0).is_none());
    }

    #[cfg(feature = "gc-collect")]
    #[test]
    fn linear_chain_reclaims_garbage_and_relocates_the_field() {
        let mut heap = Heap::new(4096, vec![one_ref(), leaf()]);
        let a = heap.alloc(0).unwrap();
        let b = heap.alloc(1).unwrap();
        let c = heap.alloc(1).unwrap();
        heap.write_ref_field(a, 0, b);
        let top_before = heap.top();
        assert!(c.0 < top_before);

        let mut root = a;
        heap.collect(|visit| visit(&mut root));

        assert_eq!(root, Ref(ALIGN + HEADER_SIZE));
        let a_new = root;
        let b_new = heap.read_ref_field(a_new, 0);
        assert_eq!(b_new, Ref(ALIGN + (HEADER_SIZE + 4) + HEADER_SIZE));
        assert_eq!(heap.type_id_of(a_new), 0);
        assert_eq!(heap.type_id_of(b_new), 1);
        let two_objects = 2 * (HEADER_SIZE + 4);
        assert_eq!(heap.top(), ALIGN + two_objects);
        assert!(heap.top() < top_before);
    }

    #[cfg(feature = "gc-collect")]
    #[test]
    fn cycle_survives_with_both_refs_consistent() {
        let mut heap = Heap::new(4096, vec![one_ref()]);
        let a = heap.alloc(0).unwrap();
        let b = heap.alloc(0).unwrap();
        heap.write_ref_field(a, 0, b);
        heap.write_ref_field(b, 0, a);

        let mut root = a;
        heap.collect(|visit| visit(&mut root));

        let a_new = root;
        let b_new = heap.read_ref_field(a_new, 0);
        assert_ne!(a_new, b_new);
        assert_eq!(heap.read_ref_field(b_new, 0), a_new);
        assert_eq!(heap.top(), ALIGN + 2 * (HEADER_SIZE + 4));
    }

    #[cfg(feature = "gc-collect")]
    #[test]
    fn no_garbage_keeps_every_object() {
        let mut heap = Heap::new(4096, vec![one_ref(), leaf()]);
        let a = heap.alloc(0).unwrap();
        let b = heap.alloc(1).unwrap();
        heap.write_ref_field(a, 0, b);
        let top_before = heap.top();

        let mut roots = [a, b];
        heap.collect(|visit| {
            for r in &mut roots {
                visit(r);
            }
        });

        assert_eq!(heap.top(), top_before);
        let a_new = roots[0];
        assert_eq!(heap.read_ref_field(a_new, 0), roots[1]);
        assert_eq!(heap.type_id_of(roots[1]), 1);
    }

    #[cfg(feature = "gc-collect")]
    #[test]
    fn all_garbage_resets_to_base() {
        let mut heap = Heap::new(4096, vec![leaf(), leaf()]);
        let _ = heap.alloc(0).unwrap();
        let _ = heap.alloc(1).unwrap();
        assert!(heap.top() > ALIGN);

        heap.collect(|_visit| {});

        assert_eq!(heap.top(), ALIGN);
        assert_eq!(heap.used(), 0);
        let fresh = heap.alloc(0).unwrap();
        assert_eq!(fresh, Ref(ALIGN + HEADER_SIZE));
    }

    #[cfg(feature = "gc-collect")]
    #[test]
    fn null_root_and_null_field_stay_null() {
        let mut heap = Heap::new(4096, vec![one_ref()]);
        let a = heap.alloc(0).unwrap();
        let mut roots = [a, Ref::NULL];
        heap.collect(|visit| {
            for r in &mut roots {
                visit(r);
            }
        });
        assert_eq!(roots[1], Ref::NULL);
        assert_eq!(heap.read_ref_field(roots[0], 0), Ref::NULL);
    }

    #[cfg(feature = "gc-collect")]
    #[test]
    fn tagged_interior_relocates_pointers_and_leaves_fixnums() {
        let container = TypeDesc {
            payload_size: 8,
            ref_offsets: Vec::new(),
            tagged_offsets: vec![0, 4],
        };
        let mut heap = Heap::new(4096, vec![container, leaf()]);
        let a = heap.alloc(0).unwrap();
        let garbage = heap.alloc(1).unwrap();
        let b = heap.alloc(1).unwrap();
        let fixnum = 0x15u32;
        heap.write_u32(a.0, b.0);
        heap.write_u32(a.0 + 4, fixnum);
        let _ = garbage;
        let top_before = heap.top();

        let mut root = a;
        heap.collect(|visit| visit(&mut root));

        let a_new = root;
        assert_eq!(a_new, Ref(ALIGN + HEADER_SIZE));
        let b_new = Ref(heap.read_u32(a_new.0));
        assert_eq!(heap.type_id_of(b_new), 1);
        assert_eq!(b_new.0 & 0b11, 0);
        assert_eq!(heap.read_u32(a_new.0 + 4), fixnum);
        assert!(heap.top() < top_before);
    }

    #[test]
    fn type_desc_decode_matches_backend_blob() {
        let mut blob = Vec::new();
        blob.extend_from_slice(&12u32.to_le_bytes());
        blob.extend_from_slice(&2u32.to_le_bytes());
        blob.extend_from_slice(&4u32.to_le_bytes());
        blob.extend_from_slice(&8u32.to_le_bytes());
        let (desc, consumed) = TypeDesc::decode(&blob).unwrap();
        assert_eq!(consumed, blob.len());
        assert_eq!(
            desc,
            TypeDesc {
                payload_size: 12,
                ref_offsets: vec![4, 8],
                tagged_offsets: Vec::new(),
            }
        );
        assert!(TypeDesc::decode(&blob[..6]).is_none());
    }

    /// Builds the backend's stack-map wire bytes for a set of entries, mirroring
    /// `lamella_aot::arm32::StackMaps::encode` so the round-trip is real.
    fn encode_stack_maps(entries: &[StackMapEntry]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        for e in entries {
            out.extend_from_slice(&e.return_pc.to_le_bytes());
            out.extend_from_slice(&e.frame_size.to_le_bytes());
            out.extend_from_slice(&(e.ref_offsets.len() as u16).to_le_bytes());
            for &o in &e.ref_offsets {
                out.extend_from_slice(&o.to_le_bytes());
            }
        }
        out
    }

    #[test]
    fn stack_map_decode_round_trip_and_lookup() {
        let entries = vec![
            StackMapEntry {
                return_pc: 0x10,
                frame_size: 16,
                ref_offsets: vec![0, 4],
                pinned_offsets: vec![],
            },
            StackMapEntry {
                return_pc: 0x40,
                frame_size: 24,
                ref_offsets: vec![8],
                pinned_offsets: vec![],
            },
        ];
        let bytes = encode_stack_maps(&entries);
        let table = StackMapTable::decode(&bytes).unwrap();
        assert_eq!(table.entries(), entries.as_slice());

        let first = table.lookup(0x10).unwrap();
        assert_eq!(first.frame_size, 16);
        assert_eq!(first.ref_offsets, vec![0, 4]);
        let second = table.lookup(0x40).unwrap();
        assert_eq!(second.ref_offsets, vec![8]);
        assert!(table.lookup(0x20).is_none());
        assert!(table.lookup(0).is_none());

        assert!(StackMapTable::decode(&bytes[..bytes.len() - 1]).is_none());
    }

    #[cfg(feature = "gc-collect")]
    #[test]
    fn frame_integration_relocates_roots_through_the_stack_map() {
        let mut heap = Heap::new(4096, vec![one_ref(), leaf()]);
        let a = heap.alloc(0).unwrap();
        let garbage = heap.alloc(1).unwrap();
        let b = heap.alloc(1).unwrap();
        let c = heap.alloc(1).unwrap();
        heap.write_ref_field(a, 0, c);
        let _ = garbage;

        let entry = StackMapEntry {
            return_pc: 0x100,
            frame_size: 32,
            ref_offsets: vec![4, 12],
            pinned_offsets: vec![],
        };
        let mut frame = vec![0u8; 32];
        let sp = 0u32;
        frame[4..8].copy_from_slice(&a.0.to_le_bytes());
        frame[12..16].copy_from_slice(&b.0.to_le_bytes());

        heap.collect_frame(&mut frame, sp, &entry);

        let a_new = Ref(u32::from_le_bytes([frame[4], frame[5], frame[6], frame[7]]));
        let b_new = Ref(u32::from_le_bytes([frame[12], frame[13], frame[14], frame[15]]));
        assert_eq!(a_new, Ref(ALIGN + HEADER_SIZE));
        assert_eq!(heap.type_id_of(a_new), 0);
        assert_eq!(heap.type_id_of(b_new), 1);
        let c_new = heap.read_ref_field(a_new, 0);
        assert_eq!(heap.type_id_of(c_new), 1);
        assert_ne!(c_new, Ref::NULL);
        assert_eq!(
            heap.top(),
            ALIGN + (HEADER_SIZE + 4) + 2 * (HEADER_SIZE + 4)
        );
    }

    /// Writes a [`Ref`] as 4 little-endian bytes into a stack/frame image at `at`.
    #[cfg_attr(not(feature = "gc-collect"), allow(dead_code))]
    fn put_ref(image: &mut [u8], at: usize, reference: Ref) {
        image[at..at + 4].copy_from_slice(&reference.0.to_le_bytes());
    }

    /// Reads a [`Ref`] back from a stack/frame image at `at`.
    #[cfg_attr(not(feature = "gc-collect"), allow(dead_code))]
    fn get_ref(image: &[u8], at: usize) -> Ref {
        Ref(u32::from_le_bytes([
            image[at],
            image[at + 1],
            image[at + 2],
            image[at + 3],
        ]))
    }

    /// A four-word data buffer -- the `int[4]` a C# `fixed (int* p = arr)` pointer walks.
    #[cfg_attr(not(feature = "gc-collect"), allow(dead_code))]
    fn buffer() -> TypeDesc {
        TypeDesc {
            payload_size: 16,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        }
    }

    /// The heap the two `fixed` arms below share, and its exact layout -- garbage, then the
    /// buffer, then more garbage, then a survivor ABOVE the buffer so each arm also shows what
    /// the pin does NOT stop. Returns `(heap, buffer, mover)`.
    ///
    /// Addresses (`HEADER_SIZE` 4, `ALIGN` 4, so an object is 4 + its rounded payload):
    /// ```text
    ///   [ 4, 12)  garbage   (leaf, payload at  8)  -- unrooted
    ///   [12, 32)  buffer    (16 bytes, payload at 16)
    ///   [32, 40)  garbage2  (leaf, payload at 36)  -- unrooted
    ///   [40, 48)  mover     (leaf, payload at 44)
    /// ```
    #[cfg(feature = "gc-collect")]
    fn fixed_statement_heap() -> (Heap, Ref, Ref) {
        let mut heap = Heap::new(4096, vec![buffer(), leaf()]);
        let garbage = heap.alloc(1).unwrap();
        let data = heap.alloc(0).unwrap();
        let garbage2 = heap.alloc(1).unwrap();
        let mover = heap.alloc(1).unwrap();
        let (_, _) = (garbage, garbage2);
        assert_eq!((data, mover), (Ref(16), Ref(44)), "the layout the arms reason about");
        for i in 0..4u32 {
            heap.write_u32(data.0 + i * 4, 0xA0 + i);
        }
        assert_eq!(heap.top(), 48);
        (heap, data, mover)
    }

    /// **The pin is what makes a C# `fixed` pointer survive a collection, and this is the proof.**
    ///
    /// A `fixed` pointer is a RAW interior address held in a slot the collector is never told
    /// about -- an `int*` is not a GC-tracked type, which is exactly why the `fixed` statement
    /// exists. So nothing can correct it after a move; the only thing that can keep it valid is
    /// the object not moving. The invariant under test is therefore an ADDRESS, not a value.
    ///
    /// Both arms run, and the unpinned one asserts the DEFECT: the same raw pointer silently
    /// reads a DIFFERENT ELEMENT of the same array. Stating it here is what stops the bug from
    /// quietly ceasing to reproduce -- and it is the whole reason a pin, rather than a cleverer
    /// relocation, is the answer.
    #[cfg(feature = "gc-collect")]
    #[test]
    fn a_pinned_root_keeps_its_address_so_a_raw_interior_pointer_still_reads_its_element() {
        let holder_at = 0usize;
        let mover_at = 4usize;

        let (mut heap, data, mover) = fixed_statement_heap();
        let raw = data.0 + 4;
        assert_eq!(heap.read_u32(raw), 0xA1, "the pointer's element before collecting");
        let entry = StackMapEntry {
            return_pc: 0x100,
            frame_size: 8,
            ref_offsets: vec![mover_at as u16],
            pinned_offsets: vec![holder_at as u16],
        };
        let mut frame = vec![0u8; 8];
        put_ref(&mut frame, holder_at, data);
        put_ref(&mut frame, mover_at, mover);

        heap.collect_frame(&mut frame, 0, &entry);

        assert_eq!(get_ref(&frame, holder_at), data, "a pinned root must not move");
        assert_eq!(heap.read_u32(raw), 0xA1, "the pinned pointer reads its own element");
        assert_eq!(get_ref(&frame, mover_at), Ref(36), "an unpinned survivor still compacts");
        assert_eq!(heap.top(), 40);
        assert_eq!(heap.read_u32(ALIGN), 0, "the gap the pin left is zeroed");

        let (mut heap, data, mover) = fixed_statement_heap();
        let raw = data.0 + 4;
        let entry = StackMapEntry {
            return_pc: 0x100,
            frame_size: 8,
            ref_offsets: vec![holder_at as u16, mover_at as u16],
            pinned_offsets: vec![],
        };
        let mut frame = vec![0u8; 8];
        put_ref(&mut frame, holder_at, data);
        put_ref(&mut frame, mover_at, mover);

        heap.collect_frame(&mut frame, 0, &entry);

        assert_eq!(get_ref(&frame, holder_at), Ref(8), "an unpinned root relocates");
        assert_eq!(heap.read_u32(8), 0xA0, "element 0, at the new address");
        assert_eq!(
            heap.read_u32(raw),
            0xA3,
            "the unpinned raw pointer silently reads a DIFFERENT element"
        );
        assert_eq!(get_ref(&frame, mover_at), Ref(28));
        assert_eq!(heap.top(), 32);
    }

    /// A pin is released by the next collection that does not ask for it: the gap it left closes
    /// and the space comes back. Pinning costs space for as long as the `fixed` block lasts and
    /// not one collection longer -- which is why the constraint belongs to a SLOT's lifetime.
    #[cfg(feature = "gc-collect")]
    #[test]
    fn releasing_a_pin_closes_the_gap_it_left() {
        let (mut heap, data, mover) = fixed_statement_heap();
        let pinned_entry = StackMapEntry {
            return_pc: 0x100,
            frame_size: 8,
            ref_offsets: vec![4],
            pinned_offsets: vec![0],
        };
        let mut frame = vec![0u8; 8];
        put_ref(&mut frame, 0, data);
        put_ref(&mut frame, 4, mover);
        heap.collect_frame(&mut frame, 0, &pinned_entry);
        assert_eq!(heap.top(), 40, "the gap is still there while the pin holds");

        let released = StackMapEntry {
            return_pc: 0x100,
            frame_size: 8,
            ref_offsets: vec![0, 4],
            pinned_offsets: vec![],
        };
        heap.collect_frame(&mut frame, 0, &released);
        assert_eq!(heap.top(), 32);
        assert_eq!(get_ref(&frame, 0), Ref(8));
        assert_eq!(heap.read_u32(8 + 4), 0xA1, "the buffer's bytes came along intact");
    }

    /// A pinned root is still a ROOT: reporting it must keep its object alive. If the pinned list
    /// were treated as "do not move" without also seeding the mark, a `fixed` array reachable
    /// only through the holder slot would be reclaimed -- the pin would cause the exact
    /// destruction it exists to prevent, and every read through the pointer would still "work"
    /// because the bytes are only zeroed on the next allocation.
    #[cfg(feature = "gc-collect")]
    #[test]
    fn a_pinned_root_is_marked_not_merely_held_still() {
        let mut heap = Heap::new(4096, vec![buffer(), leaf()]);
        let data = heap.alloc(0).unwrap();
        let garbage = heap.alloc(1).unwrap();
        let _ = garbage;
        heap.write_u32(data.0, 0xBEEF);
        let entry = StackMapEntry {
            return_pc: 0x100,
            frame_size: 4,
            ref_offsets: vec![],
            pinned_offsets: vec![0],
        };
        let mut frame = vec![0u8; 4];
        put_ref(&mut frame, 0, data);

        heap.collect_frame(&mut frame, 0, &entry);

        assert_eq!(get_ref(&frame, 0), data);
        assert_eq!(heap.read_u32(data.0), 0xBEEF);
        assert_eq!(heap.top(), data.0 + 16);
    }

    /// A null holder slot is not an address to pin. `fixed (int* p = arr)` on a null (or empty)
    /// array leaves the holder null, and a pinned-list entry of 0 would name the reserved null
    /// address -- which no survivor can occupy, so it would be a silent no-op rather than an
    /// error. Filtering at the source keeps the pinned list meaning only "real objects".
    #[cfg(feature = "gc-collect")]
    #[test]
    fn a_null_pinned_slot_pins_nothing_and_collects_normally() {
        let mut heap = Heap::new(4096, vec![buffer(), leaf()]);
        let garbage = heap.alloc(1).unwrap();
        let kept = heap.alloc(1).unwrap();
        let _ = garbage;
        let entry = StackMapEntry {
            return_pc: 0x100,
            frame_size: 8,
            ref_offsets: vec![4],
            pinned_offsets: vec![0],
        };
        let mut frame = vec![0u8; 8];
        put_ref(&mut frame, 0, Ref::NULL);
        put_ref(&mut frame, 4, kept);

        heap.collect_frame(&mut frame, 0, &entry);

        assert_eq!(get_ref(&frame, 0), Ref::NULL, "null stays null");
        assert_eq!(get_ref(&frame, 4), Ref(ALIGN + HEADER_SIZE));
        assert_eq!(heap.top(), ALIGN + HEADER_SIZE + 4);
    }

    /// A table decoded from the kind-less wire format pins nothing, and that is asserted rather
    /// than assumed: the format carries no root kind, so pins reach a collection through
    /// [`StackMapTable::from_entries`] (the device install path) and not through
    /// [`StackMapTable::decode`]. `has_pins` is the cheap gate the stack walk skips its
    /// read-only pinned pre-pass on.
    #[test]
    fn a_decoded_table_pins_nothing_and_has_pins_says_so() {
        let entries = vec![StackMapEntry {
            return_pc: 0x10,
            frame_size: 16,
            ref_offsets: vec![0],
            pinned_offsets: vec![4],
        }];
        assert!(StackMapTable::from_entries(entries.clone()).has_pins());
        let decoded = StackMapTable::decode(&encode_stack_maps(&entries)).expect("decodes");
        assert!(
            !decoded.has_pins(),
            "the wire format carries no kind, so a decoded table must not claim pins"
        );
        assert_eq!(decoded.entries()[0].ref_offsets, vec![0]);
    }

    /// The pin must survive the MULTI-FRAME walk, not just a single frame -- because that walk is
    /// the device path, and because the pinned pre-pass is a SECOND traversal of the same saved-LR
    /// chain. A `fixed` block that calls a method holds its pin in the CALLER's frame while the
    /// collection is triggered by an allocation in the CALLEE, so the frame the pin lives in is
    /// not the frame the collection starts from.
    #[cfg(feature = "gc-collect")]
    #[test]
    fn a_pin_in_a_caller_frame_is_honored_by_the_stack_walk() {
        let mut heap = Heap::new(4096, vec![buffer(), leaf()]);
        let garbage = heap.alloc(1).unwrap();
        let data = heap.alloc(0).unwrap();
        let mover = heap.alloc(1).unwrap();
        let _ = garbage;
        assert_eq!((data, mover), (Ref(16), Ref(36)));
        heap.write_u32(data.0 + 4, 0xA1);
        let raw = data.0 + 4;

        let callee = StackMapEntry {
            return_pc: 0x100,
            frame_size: 8,
            ref_offsets: vec![0],
            pinned_offsets: vec![],
        };
        let caller = StackMapEntry {
            return_pc: 0x200,
            frame_size: 8,
            ref_offsets: vec![],
            pinned_offsets: vec![0],
        };
        let maps = StackMapTable::from_entries(vec![callee, caller]);
        assert!(maps.has_pins(), "the pre-pass gate must see the caller's pin");
        let mut stack = vec![0u8; 24];
        put_ref(&mut stack, 0, mover);
        put_ref(&mut stack, 8, Ref(0x200));
        put_ref(&mut stack, 12, data);
        put_ref(&mut stack, 20, Ref(0x999));

        heap.collect_stack(&mut stack, 0, 0x100, &maps);

        assert_eq!(get_ref(&stack, 12), data);
        assert_eq!(heap.read_u32(raw), 0xA1);
        assert_eq!(get_ref(&stack, 0), Ref(36));
        assert_eq!(heap.top(), 40);
    }

    #[cfg(feature = "gc-collect")]
    #[test]
    fn stack_walk_two_frames_relocates_every_frame_and_reclaims_garbage() {
        let mut heap = Heap::new(4096, vec![one_ref(), leaf()]);
        let a = heap.alloc(0).unwrap();
        let garbage = heap.alloc(1).unwrap();
        let b = heap.alloc(1).unwrap();
        let c = heap.alloc(1).unwrap();
        let d = heap.alloc(0).unwrap();
        let e = heap.alloc(1).unwrap();
        heap.write_ref_field(a, 0, c);
        heap.write_ref_field(d, 0, e);
        let _ = garbage;
        let top_before = heap.top();

        let callee = StackMapEntry {
            return_pc: 0x100,
            frame_size: 16,
            ref_offsets: vec![4, 12],
            pinned_offsets: vec![],
        };
        let caller = StackMapEntry {
            return_pc: 0x200,
            frame_size: 8,
            ref_offsets: vec![0],
            pinned_offsets: vec![],
        };
        let maps = StackMapTable::from_entries(vec![callee.clone(), caller.clone()]);

        let top_sp = 0u32;
        let saved_lr_callee = top_sp + u32::from(callee.frame_size);
        let caller_sp = saved_lr_callee + 4;
        let saved_lr_caller = caller_sp + u32::from(caller.frame_size);
        let mut stack = vec![0u8; (saved_lr_caller + 4) as usize];
        put_ref(&mut stack, (top_sp + 4) as usize, a);
        put_ref(&mut stack, (top_sp + 12) as usize, b);
        put_ref(&mut stack, saved_lr_callee as usize, Ref(0x200));
        put_ref(&mut stack, caller_sp as usize, d);
        put_ref(&mut stack, saved_lr_caller as usize, Ref(0x999));

        heap.collect_stack(&mut stack, top_sp, 0x100, &maps);

        let a_new = get_ref(&stack, (top_sp + 4) as usize);
        let b_new = get_ref(&stack, (top_sp + 12) as usize);
        let d_new = get_ref(&stack, caller_sp as usize);
        assert_eq!(a_new, Ref(ALIGN + HEADER_SIZE));
        assert_eq!(get_ref(&stack, saved_lr_callee as usize), Ref(0x200));
        assert_eq!(get_ref(&stack, saved_lr_caller as usize), Ref(0x999));
        assert_eq!(heap.type_id_of(a_new), 0);
        assert_eq!(heap.type_id_of(b_new), 1);
        assert_eq!(heap.type_id_of(d_new), 0);
        let c_new = heap.read_ref_field(a_new, 0);
        let e_new = heap.read_ref_field(d_new, 0);
        assert_ne!(c_new, Ref::NULL);
        assert_ne!(e_new, Ref::NULL);
        assert_eq!(heap.type_id_of(c_new), 1);
        assert_eq!(heap.type_id_of(e_new), 1);
        assert!(heap.top() < top_before);
        let five_objects =
            2 * (HEADER_SIZE + 4) + 3 * (HEADER_SIZE + 4);
        assert_eq!(heap.top(), ALIGN + five_objects);
    }

    #[cfg(feature = "gc-collect")]
    #[test]
    fn stack_walk_three_frames_traverses_two_saved_lr_hops() {
        let mut heap = Heap::new(4096, vec![one_ref(), leaf()]);
        let a = heap.alloc(0).unwrap();
        let garbage = heap.alloc(1).unwrap();
        let x = heap.alloc(1).unwrap();
        let b = heap.alloc(1).unwrap();
        let c = heap.alloc(1).unwrap();
        heap.write_ref_field(a, 0, x);
        let _ = garbage;
        let top_before = heap.top();

        let f0 = StackMapEntry { return_pc: 0x10, frame_size: 8, ref_offsets: vec![0], pinned_offsets: vec![] };
        let f1 = StackMapEntry { return_pc: 0x20, frame_size: 8, ref_offsets: vec![4], pinned_offsets: vec![] };
        let f2 = StackMapEntry { return_pc: 0x30, frame_size: 12, ref_offsets: vec![0], pinned_offsets: vec![] };
        let maps = StackMapTable::from_entries(vec![f0.clone(), f1.clone(), f2.clone()]);

        let f0_sp = 0u32;
        let lr0 = f0_sp + u32::from(f0.frame_size);
        let f1_sp = lr0 + 4;
        let lr1 = f1_sp + u32::from(f1.frame_size);
        let f2_sp = lr1 + 4;
        let lr2 = f2_sp + u32::from(f2.frame_size);
        let mut stack = vec![0u8; (lr2 + 4) as usize];
        put_ref(&mut stack, f0_sp as usize, a);
        put_ref(&mut stack, lr0 as usize, Ref(0x20));
        put_ref(&mut stack, (f1_sp + 4) as usize, b);
        put_ref(&mut stack, lr1 as usize, Ref(0x30));
        put_ref(&mut stack, f2_sp as usize, c);
        put_ref(&mut stack, lr2 as usize, Ref(0x7777));

        heap.collect_stack(&mut stack, f0_sp, 0x10, &maps);

        let a_new = get_ref(&stack, f0_sp as usize);
        let b_new = get_ref(&stack, (f1_sp + 4) as usize);
        let c_new = get_ref(&stack, f2_sp as usize);
        assert_eq!(a_new, Ref(ALIGN + HEADER_SIZE));
        assert_eq!(heap.type_id_of(a_new), 0);
        assert_eq!(heap.type_id_of(b_new), 1);
        assert_eq!(heap.type_id_of(c_new), 1);
        let x_new = heap.read_ref_field(a_new, 0);
        assert_ne!(x_new, Ref::NULL);
        assert_eq!(heap.type_id_of(x_new), 1);
        assert!(heap.top() < top_before);
        let four_objects = (HEADER_SIZE + 4) + 3 * (HEADER_SIZE + 4);
        assert_eq!(heap.top(), ALIGN + four_objects);
    }

    #[cfg(feature = "gc-collect")]
    #[test]
    fn stack_walk_single_frame_matches_collect_frame() {
        let make = || {
            let mut heap = Heap::new(4096, vec![one_ref(), leaf()]);
            let a = heap.alloc(0).unwrap();
            let garbage = heap.alloc(1).unwrap();
            let b = heap.alloc(1).unwrap();
            let c = heap.alloc(1).unwrap();
            heap.write_ref_field(a, 0, c);
            let _ = garbage;
            (heap, a, b)
        };
        let entry = StackMapEntry {
            return_pc: 0x100,
            frame_size: 32,
            ref_offsets: vec![4, 12],
            pinned_offsets: vec![],
        };

        let (mut ref_heap, a, b) = make();
        let mut ref_frame = vec![0u8; 32];
        put_ref(&mut ref_frame, 4, a);
        put_ref(&mut ref_frame, 12, b);
        ref_heap.collect_frame(&mut ref_frame, 0, &entry);

        let (mut heap, a, b) = make();
        let maps = StackMapTable::from_entries(vec![entry.clone()]);
        let mut stack = vec![0u8; 32 + 4];
        put_ref(&mut stack, 4, a);
        put_ref(&mut stack, 12, b);
        put_ref(&mut stack, 32, Ref(0xDEAD));
        heap.collect_stack(&mut stack, 0, 0x100, &maps);

        assert_eq!(get_ref(&stack, 4), get_ref(&ref_frame, 4));
        assert_eq!(get_ref(&stack, 12), get_ref(&ref_frame, 12));
        assert_eq!(heap.top(), ref_heap.top());
        let a_new = get_ref(&stack, 4);
        assert_eq!(a_new, Ref(ALIGN + HEADER_SIZE));
        assert_eq!(heap.type_id_of(a_new), 0);
        let c_new = heap.read_ref_field(a_new, 0);
        assert_eq!(heap.type_id_of(c_new), 1);
    }

    #[cfg(feature = "gc-collect")]
    #[test]
    fn a_non_heap_root_is_skipped_not_traced_or_relocated() {
        let mut heap = Heap::new(4096, vec![one_ref(), leaf()]);
        let garbage = heap.alloc(1).unwrap();
        let a = heap.alloc(0).unwrap();
        let b = heap.alloc(1).unwrap();
        heap.write_ref_field(a, 0, b);
        let _ = garbage;
        let top_before = heap.top();

        let flash_literal = Ref(0x0004_1234);
        let at_top = Ref(top_before);

        let entry = StackMapEntry { return_pc: 0x100, frame_size: 16, ref_offsets: vec![0, 4, 8], pinned_offsets: vec![] };
        let maps = StackMapTable::from_entries(vec![entry]);
        let mut stack = vec![0u8; 32];
        put_ref(&mut stack, 0, a);
        put_ref(&mut stack, 4, flash_literal);
        put_ref(&mut stack, 8, at_top);
        put_ref(&mut stack, 16, Ref(0x999));

        heap.collect_stack(&mut stack, 0, 0x100, &maps);

        let a_new = get_ref(&stack, 0);
        assert_ne!(a_new, a, "the surviving heap root actually moved");
        assert_eq!(a_new, Ref(ALIGN + HEADER_SIZE), "survivors pack from the base");
        assert_eq!(heap.type_id_of(a_new), 0, "type preserved across the move");
        assert_eq!(
            get_ref(&stack, 4),
            flash_literal,
            "a flash literal root must survive a collection verbatim"
        );
        assert_eq!(
            get_ref(&stack, 8),
            at_top,
            "an address AT the bump pointer is not a payload the allocator handed out"
        );
        assert!(heap.top() < top_before, "unrooted garbage was still reclaimed");
    }

    #[test]
    #[cfg(feature = "gc-collect")]
    fn stack_walk_with_unmapped_top_pc_collects_with_no_roots() {
        let mut heap = Heap::new(4096, vec![one_ref(), leaf()]);
        let a = heap.alloc(0).unwrap();
        let b = heap.alloc(1).unwrap();
        heap.write_ref_field(a, 0, b);
        assert!(heap.top() > ALIGN);

        let maps = StackMapTable::from_entries(vec![StackMapEntry {
            return_pc: 0x100,
            frame_size: 8,
            ref_offsets: vec![0],
            pinned_offsets: vec![],
        }]);
        let mut stack = vec![0u8; 16];
        put_ref(&mut stack, 0, a);

        heap.collect_stack(&mut stack, 0, 0x999, &maps);

        assert_eq!(heap.top(), ALIGN);
        assert_eq!(heap.used(), 0);
        let fresh = heap.alloc(0).unwrap();
        assert_eq!(fresh, Ref(ALIGN + HEADER_SIZE));
    }

    /// A resolver that answers exactly like [`TableResolver`] but RECORDS the `payload_head` it
    /// was handed for each object. The device path needs that word to be the object's own first
    /// payload word -- it is where an array keeps its element count -- so this pins the engine's
    /// half of that contract without restating the device's array arithmetic, which would only
    /// compare the reader against a replica of itself.
    #[cfg(feature = "gc-collect")]
    struct HeadRecordingResolver<'a> {
        inner: TableResolver<'a>,
        seen: core::cell::RefCell<Vec<(u32, u32)>>,
    }

    #[cfg(feature = "gc-collect")]
    impl TypeResolver for HeadRecordingResolver<'_> {
        fn payload_size(&self, header_word: u32, payload_head: u32) -> u32 {
            self.seen.borrow_mut().push((header_word, payload_head));
            self.inner.payload_size(header_word, payload_head)
        }

        fn for_each_ref_offset(&self, header_word: u32, payload_head: u32, f: &mut dyn FnMut(u32)) {
            self.seen.borrow_mut().push((header_word, payload_head));
            self.inner.for_each_ref_offset(header_word, payload_head, f);
        }
    }

    #[test]
    #[cfg(feature = "gc-collect")]
    fn the_engine_hands_the_resolver_each_object_s_own_first_payload_word() {
        let descs = vec![
            TypeDesc { payload_size: 8, ref_offsets: vec![4], tagged_offsets: Vec::new() },
            TypeDesc { payload_size: 8, ref_offsets: Vec::new(), tagged_offsets: Vec::new() },
        ];
        let mut bytes = vec![0u8; 128];
        let a_payload = ALIGN + HEADER_SIZE;
        let b_payload = a_payload + 8 + HEADER_SIZE;
        let put = |bytes: &mut Vec<u8>, at: u32, word: u32| {
            let at = at as usize;
            bytes[at..at + 4].copy_from_slice(&word.to_le_bytes());
        };
        put(&mut bytes, a_payload - HEADER_SIZE, 0);
        put(&mut bytes, a_payload, 0x1111);
        put(&mut bytes, a_payload + 4, b_payload);
        put(&mut bytes, b_payload - HEADER_SIZE, 1);
        put(&mut bytes, b_payload, 0x2222);
        let top = b_payload + 8;

        let resolver = HeadRecordingResolver {
            inner: TableResolver { type_descs: &descs },
            seen: core::cell::RefCell::new(Vec::new()),
        };
        let mut root = Ref(a_payload);
        mark_compact(&mut bytes, top, &resolver, |visit| visit(&mut root), &[]);

        let seen = resolver.seen.borrow().clone();
        assert!(seen.len() >= 4, "both objects are visited at mark and at compact: {seen:?}");
        for (header_word, head) in seen {
            let expected = if header_word == 0 { 0x1111 } else { 0x2222 };
            assert_eq!(head, expected, "type {header_word} was handed the wrong payload head");
        }
    }

    #[test]
    #[cfg(feature = "gc-collect")]
    fn a_zero_payload_object_at_the_top_of_the_region_is_never_asked_about() {
        let descs = vec![TypeDesc { payload_size: 0, ref_offsets: Vec::new(), tagged_offsets: Vec::new() }];
        let top = ALIGN + HEADER_SIZE;
        let mut bytes = vec![0u8; top as usize];
        let mut root = Ref(top);
        let new_top = mark_compact(&mut bytes, top, &TableResolver { type_descs: &descs }, |visit| {
            visit(&mut root)
        }, &[]);
        assert_eq!(new_top, ALIGN);
    }
}
