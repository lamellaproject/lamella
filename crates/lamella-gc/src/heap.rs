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
    /// Byte offsets *within the payload* of the 4-byte slots that hold child
    /// references. Each names a [`Ref`] the collector traces and relocates.
    pub ref_offsets: Vec<u32>,
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
}

/// The decoded GC stack maps for a lowered program: one entry per safepoint, sorted
/// by `return_pc` for binary search. The decoded counterpart of
/// `lamella_aot::arm32::StackMaps`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StackMapTable {
    entries: Vec<StackMapEntry>,
}

impl StackMapTable {
    /// Decodes the backend's little-endian wire format: `u32 count`, then each entry
    /// `u32 return_pc; u16 frame_size; u16 nrefs; u16 ref_offsets[nrefs]`. Returns
    /// `None` if the bytes are truncated.
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
            });
        }
        Some(StackMapTable { entries })
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
    pub fn collect<R>(&mut self, enumerate_roots: R)
    where
        R: FnMut(&mut dyn FnMut(&mut Ref)),
    {
        let resolver = TableResolver {
            type_descs: &self.type_descs,
        };
        self.top = mark_compact(&mut self.bytes, &resolver, enumerate_roots);
    }

    /// Collects with the roots taken from a single AOT frame, located through one
    /// stack-map entry. `frame` is the frame's byte image and `sp` is the address of
    /// SP-at-the-call within it; each root sits at `sp + entry.ref_offsets[i]` and
    /// holds a [`Ref`]. The relocated references are written back into `frame` so the
    /// caller's frame stays consistent.
    ///
    /// One frame only: multi-frame walking via the saved LR
    /// (`sp + frame_size`) is a later increment.
    pub fn collect_frame(&mut self, frame: &mut [u8], sp: u32, entry: &StackMapEntry) {
        self.collect(|visit| {
            for &ref_offset in &entry.ref_offsets {
                let at = (sp + u32::from(ref_offset)) as usize;
                let mut reference = Ref(u32::from_le_bytes([
                    frame[at],
                    frame[at + 1],
                    frame[at + 2],
                    frame[at + 3],
                ]));
                visit(&mut reference);
                frame[at..at + 4].copy_from_slice(&reference.0.to_le_bytes());
            }
        });
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
    pub fn collect_stack(
        &mut self,
        stack: &mut [u8],
        top_sp: u32,
        top_return_pc: u32,
        stack_maps: &StackMapTable,
    ) {
        const MAX_FRAMES: u32 = 4096;
        self.collect(|visit| {
            let mut sp = top_sp;
            let mut return_pc = top_return_pc;
            let mut frames = 0u32;
            while let Some(entry) = stack_maps.lookup(return_pc) {
                for &ref_offset in &entry.ref_offsets {
                    let at = (sp + u32::from(ref_offset)) as usize;
                    let mut reference = Ref(u32::from_le_bytes([
                        stack[at],
                        stack[at + 1],
                        stack[at + 2],
                        stack[at + 3],
                    ]));
                    visit(&mut reference);
                    stack[at..at + 4].copy_from_slice(&reference.0.to_le_bytes());
                }
                let saved_lr_at = (sp + u32::from(entry.frame_size)) as usize;
                return_pc = u32::from_le_bytes([
                    stack[saved_lr_at],
                    stack[saved_lr_at + 1],
                    stack[saved_lr_at + 2],
                    stack[saved_lr_at + 3],
                ]);
                sp = sp + u32::from(entry.frame_size) + 4;
                frames += 1;
                if frames >= MAX_FRAMES {
                    break;
                }
            }
        });
    }
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
pub(crate) trait TypeResolver {
    /// The payload size, in bytes, of the object whose header holds `header_word`.
    fn payload_size(&self, header_word: u32) -> u32;

    /// Invokes `f` with each byte offset (within the payload) of a reference field of
    /// the object whose header holds `header_word`.
    fn for_each_ref_offset(&self, header_word: u32, f: &mut dyn FnMut(u32));
}

/// The host resolver: an object's header word is an index into a [`TypeDesc`] table.
/// This reproduces exactly the lookup the host engine used before [`mark_compact`] was
/// factored out, so the host tests see identical behaviour.
pub(crate) struct TableResolver<'a> {
    pub(crate) type_descs: &'a [TypeDesc],
}

impl TypeResolver for TableResolver<'_> {
    fn payload_size(&self, header_word: u32) -> u32 {
        self.type_descs[header_word as usize].payload_size
    }

    fn for_each_ref_offset(&self, header_word: u32, f: &mut dyn FnMut(u32)) {
        for &ref_offset in &self.type_descs[header_word as usize].ref_offsets {
            f(ref_offset);
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
/// clobbers an unmoved survivor). RELOCATE rewrites every root and every survivor
/// field through the `old_payload -> new_payload` forwarding map; null stays null. The
/// freed tail is zeroed so a later allocation never reads stale bytes.
pub(crate) fn mark_compact<R>(
    bytes: &mut [u8],
    resolver: &dyn TypeResolver,
    mut enumerate_roots: R,
) -> u32
where
    R: FnMut(&mut dyn FnMut(&mut Ref)),
{
    use alloc::collections::{BTreeMap, BTreeSet};

    let read_word = |bytes: &[u8], addr: u32| -> u32 {
        let at = addr as usize;
        u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
    };
    let read_field = |bytes: &[u8], reference: Ref, ref_offset: u32| -> Ref {
        Ref(read_word(bytes, reference.0 + ref_offset))
    };

    let mut live: BTreeSet<u32> = BTreeSet::new();
    let mut work: Vec<Ref> = Vec::new();
    let mark = |reference: &mut Ref, live: &mut BTreeSet<u32>, work: &mut Vec<Ref>| {
        if !reference.is_null() && live.insert(reference.0) {
            work.push(*reference);
        }
    };
    enumerate_roots(&mut |slot| mark(slot, &mut live, &mut work));
    while let Some(object) = work.pop() {
        let header_word = read_word(bytes, object.header_addr());
        resolver.for_each_ref_offset(header_word, &mut |ref_offset| {
            let mut child = read_field(bytes, object, ref_offset);
            mark(&mut child, &mut live, &mut work);
        });
    }

    let mut forward: BTreeMap<u32, u32> = BTreeMap::new();
    let mut dest = ALIGN;
    for old_payload in live.iter().copied() {
        let header_word = read_word(bytes, old_payload - HEADER_SIZE);
        let reserved = align_up(resolver.payload_size(header_word));
        let object_size = HEADER_SIZE + reserved;
        let new_payload = dest + HEADER_SIZE;
        forward.insert(old_payload, new_payload);
        let src = (old_payload - HEADER_SIZE) as usize;
        let dst = dest as usize;
        if src != dst {
            bytes.copy_within(src..src + object_size as usize, dst);
        }
        dest += object_size;
    }

    let relocate = |reference: &mut Ref| {
        if !reference.is_null() {
            *reference = Ref(forward[&reference.0]);
        }
    };
    enumerate_roots(&mut |slot| relocate(slot));
    for (&_old_payload, &new_payload) in forward.iter() {
        let new_ref = Ref(new_payload);
        let header_word = read_word(bytes, new_ref.header_addr());
        let mut offsets: Vec<u32> = Vec::new();
        resolver.for_each_ref_offset(header_word, &mut |ref_offset| offsets.push(ref_offset));
        for ref_offset in offsets {
            let mut child = read_field(bytes, new_ref, ref_offset);
            relocate(&mut child);
            let at = (new_ref.0 + ref_offset) as usize;
            bytes[at..at + 4].copy_from_slice(&child.0.to_le_bytes());
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
        }
    }

    /// A type with a single reference field at payload offset 0.
    fn one_ref() -> TypeDesc {
        TypeDesc {
            payload_size: 4,
            ref_offsets: vec![0],
        }
    }

    #[test]
    fn alloc_lays_out_header_then_payload_and_returns_payload_ref() {
        let descs = vec![TypeDesc {
            payload_size: 8,
            ref_offsets: vec![4],
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
            },
            StackMapEntry {
                return_pc: 0x40,
                frame_size: 24,
                ref_offsets: vec![8],
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
    fn put_ref(image: &mut [u8], at: usize, reference: Ref) {
        image[at..at + 4].copy_from_slice(&reference.0.to_le_bytes());
    }

    /// Reads a [`Ref`] back from a stack/frame image at `at`.
    fn get_ref(image: &[u8], at: usize) -> Ref {
        Ref(u32::from_le_bytes([
            image[at],
            image[at + 1],
            image[at + 2],
            image[at + 3],
        ]))
    }

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
        };
        let caller = StackMapEntry {
            return_pc: 0x200,
            frame_size: 8,
            ref_offsets: vec![0],
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

        let f0 = StackMapEntry { return_pc: 0x10, frame_size: 8, ref_offsets: vec![0] };
        let f1 = StackMapEntry { return_pc: 0x20, frame_size: 8, ref_offsets: vec![4] };
        let f2 = StackMapEntry { return_pc: 0x30, frame_size: 12, ref_offsets: vec![0] };
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

    #[test]
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
        }]);
        let mut stack = vec![0u8; 16];
        put_ref(&mut stack, 0, a);

        heap.collect_stack(&mut stack, 0, 0x999, &maps);

        assert_eq!(heap.top(), ALIGN);
        assert_eq!(heap.used(), 0);
        let fresh = heap.alloc(0).unwrap();
        assert_eq!(fresh, Ref(ALIGN + HEADER_SIZE));
    }
}
