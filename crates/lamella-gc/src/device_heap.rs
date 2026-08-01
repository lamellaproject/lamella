//! The on-device GC heap: the same mark-compact engine as [`crate::heap::Heap`], but
//! over a fixed raw memory region with the on-device object/type representation the AOT
//! backend (`lamella_aot::arm32`) emits. Where the host-test [`crate::heap::Heap`] backs
//! its bytes with a `Vec<u8>` and names a type by a *table index* in the object header,
//! this heap backs its bytes with a caller-provided `(*mut u8, len)` region (the linker's
//! `.heap` section) and names a type by the *`*const TypeDesc` pointer* in the header --
//! exactly the two device swaps the module headers flagged. The mark-compact *algorithm*
//! is reused verbatim ([`crate::heap::mark_compact`]); only the header-word -> type lookup
//! differs, supplied here by [`PtrResolver`] (dereference the pointer) versus the host's
//! table-index lookup. One collector, two resolvers.

#[cfg(test)]
extern crate alloc;

use core::slice;

use crate::heap::{align_up, Ref, ALIGN, HEADER_SIZE};
#[cfg(feature = "gc-collect")]
use crate::heap::{mark_compact, StackMapTable, TypeResolver};

/// A type's on-device GC layout, in the exact memory shape the AOT backend emits and the
/// object header points at:
/// `[u32 payload_size][u32 nrefs][u32 type_tag][u32 base_ptr][u32 ref_offsets...]`, word-aligned. `#[repr(C)]` with the count immediately followed by the inline offsets,
/// so the collector reads it straight from the descriptor address in an object's header.
///
/// This is the device counterpart of [`crate::heap::TypeDesc`] (the host's owned,
/// `Vec`-backed form); the two describe the same thing, but this one *is* the wire bytes
/// rather than a decoded copy, because on device the header holds the descriptor's address.
#[repr(C)]
pub struct DeviceTypeDesc {
    /// The payload size in bytes (excluding the header). The allocator rounds the
    /// reserved space up to [`ALIGN`].
    pub payload_size: u32,
    /// The number of reference fields, i.e. the length of the `ref_offsets` array.
    pub nrefs: u32,
    /// The type's FNV tag, at a FIXED offset so a mixed-mode `type_tag <-> type_id` read is a
    /// constant-offset load rather than `nrefs` arithmetic. Not used by the collector; declared so
    /// this struct is the emitted layout rather than a subset of it.
    pub type_tag: u32,
    /// A `data_word_diff` to the base type's descriptor (0 for `System.Object`), which the
    /// backend's broad-base `castclass` and the mixed-mode base-chain walk follow. Not used by the
    /// collector; declared for the same reason as `type_tag`.
    pub base_ptr: u32,
    /// The first element of the inline `ref_offsets` array; the remaining `nrefs - 1`
    /// `u32`s follow it contiguously. Each is a byte offset within the payload of a
    /// 4-byte slot holding a child reference. A zero-`nrefs` descriptor leaves this word
    /// unread (it is the start of the next descriptor / padding).
    ///
    /// **These two words were absent from this struct** while the emitters wrote them, so the
    /// declared shape and the wire shape disagreed and `REF_OFFSETS_BASE` was computed from the
    /// declared one. Keeping every emitted word here means the next divergence is a compile-time
    /// mismatch rather than a silently shifted read.
    pub ref_offsets: [u32; 1],
}

impl DeviceTypeDesc {
    /// The byte offset, within a [`DeviceTypeDesc`], of the inline `ref_offsets` array: past the
    /// FOUR fixed header words `payload_size@0`, `nrefs@4`, `type_tag@8`, `base_ptr@12`.
    ///
    #[cfg_attr(not(feature = "gc-collect"), allow(dead_code))]
    const REF_OFFSETS_BASE: usize = 4 * 4;

    /// Reads the `i`th reference offset out of the descriptor at `desc` (a raw `*const
    /// TypeDesc` from an object header). `i` must be `< nrefs`.
    ///
    /// # Safety
    /// `desc` must point at a valid [`DeviceTypeDesc`] blob (the backend emits one per
    /// type and stores its address in each object's header) and `i < nrefs`, so the read
    /// stays within the descriptor's inline `ref_offsets` array.
    #[cfg_attr(not(feature = "gc-collect"), allow(dead_code))]
    unsafe fn ref_offset(desc: *const DeviceTypeDesc, i: u32) -> u32 {
        unsafe {
            let base = desc.cast::<u8>().add(Self::REF_OFFSETS_BASE).cast::<u32>();
            base.add(i as usize).read_unaligned()
        }
    }

    /// The descriptor's first two words, read under BOTH spellings at once: for a class they are
    /// `payload_size` and `nrefs`; for an array they are `MARK | rank` and `element_kind` (see the
    /// array-descriptor note below). The struct declares the class spelling because that is the
    /// general form; a caller decides which reading applies with [`array_shape`].
    ///
    /// # Safety
    /// `desc` must point at a valid [`DeviceTypeDesc`] blob, as for [`DeviceTypeDesc::ref_offset`].
    #[cfg_attr(not(feature = "gc-collect"), allow(dead_code))]
    unsafe fn header_words(desc: *const DeviceTypeDesc) -> (u32, u32) {
        unsafe { ((*desc).payload_size, (*desc).nrefs) }
    }
}


/// Marks a descriptor as describing an ARRAY rather than a class, in the high bits of word 0; the
/// low bits carry the rank (1 = a single-dimensional array). No real payload size can collide:
/// payload sizes are object byte sizes, far below this.
pub const ARRAY_DESC_MARK: u32 = 0xA500_0000;

/// The mask selecting [`ARRAY_DESC_MARK`] out of word 0; the remainder is the rank.
pub const ARRAY_DESC_MARK_MASK: u32 = 0xFF00_0000;

/// Element kind 0 -- the elements are REFERENCES, one 4-byte slot each, which the collector traces
/// and relocates. Collision-free by construction: the primitive code space starts at 1.
pub const ELEMENT_KIND_REFERENCE: u32 = 0;

/// Element kind for a value type that is not one of the frozen primitives (a struct element). It
/// deliberately carries NO width, so this scheme can neither stride it nor scan it.
pub const ELEMENT_KIND_OPAQUE: u32 = 0xFF;

/// The byte offset of an array's first element: past the element-count word at payload offset 0.
const ARRAY_ELEMENTS_BASE: u32 = 4;

/// A descriptor's `(rank, element_kind)` when word 0 marks it an ARRAY, else `None` (a class).
#[cfg_attr(not(feature = "gc-collect"), allow(dead_code))]
fn array_shape(word0: u32, word1: u32) -> Option<(u32, u32)> {
    (word0 & ARRAY_DESC_MARK_MASK == ARRAY_DESC_MARK).then(|| (word0 & !ARRAY_DESC_MARK_MASK, word1))
}

/// The byte width of one element of `element_kind`, or `None` for a kind that carries no width:
/// [`ELEMENT_KIND_OPAQUE`] and any code this build does not know.
///
/// The primitive codes are the frozen image code space -- `I1=1 U1=2 I2=3 U2=4 I4=5 I8=6 F4=7
/// F8=8` -- so one code covers every element type it is byte-identical to, and the width follows
/// from the code alone. That is exactly what lets the collector stride an array it cannot name.
#[cfg_attr(not(feature = "gc-collect"), allow(dead_code))]
fn element_width(element_kind: u32) -> Option<u32> {
    Some(match element_kind {
        ELEMENT_KIND_REFERENCE => 4,
        1 | 2 => 1,
        3 | 4 => 2,
        5 | 7 => 4,
        6 | 8 => 8,
        _ => return None,
    })
}

/// The payload footprint, in bytes, of an object whose descriptor's first two words are `word0` and
/// `word1` and whose first payload word is `payload_head`.
///
/// For a class this is word 0 verbatim. For an array it is the element count times the element
/// width, plus the count word -- the same arithmetic the emitted allocation site does, which is
/// what keeps the allocator and the collector agreeing on every object's footprint.
///
/// # Panics
/// When the array cannot be strided: a rank other than 1 (whose dimension words this scheme does
/// not read), a value-type element (whose width the descriptor does not carry), or a footprint past
/// the addressable range. Refusing is deliberate -- a wrong footprint does not corrupt one object,
/// it desynchronizes the walk over every object above it.
#[cfg_attr(not(feature = "gc-collect"), allow(dead_code))]
fn payload_extent(word0: u32, word1: u32, payload_head: u32) -> u32 {
    let Some((rank, element_kind)) = array_shape(word0, word1) else {
        return word0;
    };
    assert!(
        rank == 1,
        "only a single-dimensional array carries its length in one word",
    );
    let width = element_width(element_kind)
        .expect("an array of value-type elements carries no element width");
    payload_head
        .checked_mul(width)
        .and_then(|bytes| bytes.checked_add(ARRAY_ELEMENTS_BASE))
        .expect("array footprint exceeds the addressable range")
}

/// Invokes `f` with the byte offset of each REFERENCE slot of an array of `element_kind` holding
/// `length` elements: `[count][e0][e1]...`, so element `i` sits at `4 + 4 * i`. An array of
/// primitives holds no references and yields nothing.
///
/// # Panics
/// On the same unstrideable arrays [`payload_extent`] refuses, and for the same reason: tracing
/// runs before compaction, so an array the walk cannot step over must be refused at the first
/// question asked about it, not the second.
#[cfg(feature = "gc-collect")]
fn for_each_array_ref_offset(rank: u32, element_kind: u32, length: u32, f: &mut dyn FnMut(u32)) {
    let _ = payload_extent(ARRAY_DESC_MARK | rank, element_kind, length);
    if element_kind != ELEMENT_KIND_REFERENCE {
        return;
    }
    for i in 0..length {
        f(ARRAY_ELEMENTS_BASE + i * 4);
    }
}

/// The device resolver: an object's header word is the *address* of its
/// [`DeviceTypeDesc`], dereferenced to answer the engine's payload-size and
/// reference-offset questions. This is the one piece that differs from the host's
/// [`crate::heap::TableResolver`] (a table-index lookup); the mark-compact algorithm is
/// otherwise identical.
#[cfg(feature = "gc-collect")]
struct PtrResolver;

#[cfg(feature = "gc-collect")]
impl TypeResolver for PtrResolver {
    fn payload_size(&self, header_word: u32, payload_head: u32) -> u32 {
        let desc = header_word as *const DeviceTypeDesc;
        let (word0, word1) = unsafe { DeviceTypeDesc::header_words(desc) };
        payload_extent(word0, word1, payload_head)
    }

    fn for_each_ref_offset(&self, header_word: u32, payload_head: u32, f: &mut dyn FnMut(u32)) {
        let desc = header_word as *const DeviceTypeDesc;
        let (word0, word1) = unsafe { DeviceTypeDesc::header_words(desc) };
        if let Some((rank, element_kind)) = array_shape(word0, word1) {
            for_each_array_ref_offset(rank, element_kind, payload_head, f);
            return;
        }
        let nrefs = word1;
        for i in 0..nrefs {
            f(unsafe { DeviceTypeDesc::ref_offset(desc, i) });
        }
    }
}

/// A garbage-collected heap over a fixed raw memory region, with the on-device object and
/// type representation. Bump-allocates `[header][payload]` blocks and mark-compacts on
/// out-of-memory, reusing the host engine ([`mark_compact`]) through [`PtrResolver`].
///
/// Addresses are offsets into the region (address `0` reserved as the null reference, so
/// allocation begins at [`ALIGN`]); the region's base pointer turns an offset into the
/// real `*mut u8` a [`crate::ObjectRef`]/payload pointer needs.
pub struct DeviceHeap {
    /// The raw heap region as a slice. Held as `&'static mut [u8]` because the device
    /// heap lives for the whole program; offsets into it are addresses, and the
    /// mark-compact engine operates on it safely.
    region: &'static mut [u8],
    /// The bump pointer: the next free address (offset). Survivors compact below it.
    top: u32,
}

impl DeviceHeap {
    /// Builds a heap over the raw region `[base, base + len)` -- the backend's linker
    /// `.heap` section. The whole region is zeroed (so the reserved null word and every
    /// future payload start zero, matching the host [`crate::heap::Heap::new`] and the
    /// invariant [`Self::alloc`] relies on -- the linker `.heap` may not be BSS); the
    /// first [`ALIGN`] bytes are then reserved so the bump pointer never hands out address
    /// `0` (null), and the rest is the allocatable arena.
    ///
    /// # Safety
    /// `base` must point at `len` bytes of memory that are valid, exclusively owned by
    /// this heap for the program's lifetime (the device heap is never freed), and not
    /// aliased elsewhere. `len` must be at least [`ALIGN`]. On device this region is the
    /// fixed `.heap` section the linker reserves and nothing else touches, so the
    /// `'static` exclusive borrow is sound.
    pub unsafe fn from_raw(base: *mut u8, len: usize) -> DeviceHeap {
        debug_assert!(!base.is_null(), "DeviceHeap::from_raw(null)");
        debug_assert!(len >= ALIGN as usize, "DeviceHeap region smaller than ALIGN");
        let region = unsafe { slice::from_raw_parts_mut(base, len) };
        region.fill(0);
        DeviceHeap {
            region,
            top: ALIGN,
        }
    }

    /// The base address of the region as a raw pointer, so an offset (a [`Ref`]) becomes
    /// the real `*mut u8` the backend's emitted code dereferences.
    fn base_ptr(&self) -> *mut u8 {
        self.region.as_ptr() as *mut u8
    }

    /// The bump pointer (the next free address/offset). Equals [`ALIGN`] on an empty
    /// heap; after a collection it is the end of the last survivor.
    #[must_use]
    pub fn top(&self) -> u32 {
        self.top
    }

    /// Bump-allocates an object of the type described by `type_desc`: writes the header
    /// word (the descriptor *address*), reserves a zeroed, 4-aligned payload, advances
    /// the bump pointer, and returns the *payload* offset as a [`Ref`]. Returns `None`
    /// if the object does not fit (no collection is attempted here -- the C-ABI entry
    /// drives [`DeviceHeap::collect_stack`] and retries).
    ///
    /// `payload_size` is the footprint the CALLER computed, which is the only source that
    /// exists at this point for a length-dependent object: an array's element count is
    /// written into its payload immediately AFTER this call returns, so the allocator
    /// cannot derive the size from the object the way the collector later does. For a
    /// class the two agree by construction and a debug build asserts it
    /// ([`crate::device::lamella_gc_alloc_impl`]).
    ///
    /// # Safety
    /// `type_desc` must be a valid [`DeviceTypeDesc`] address the backend emitted; its
    /// whole layout is read on every later trace.
    #[must_use]
    pub unsafe fn alloc(
        &mut self,
        payload_size: u32,
        type_desc: *const DeviceTypeDesc,
    ) -> Option<Ref> {
        let reserved = align_up(payload_size);
        let object_start = self.top;
        let next = object_start.checked_add(HEADER_SIZE)?.checked_add(reserved)?;
        if next as usize > self.region.len() {
            return None;
        }
        let header_word = type_desc as u32;
        let at = object_start as usize;
        self.region[at..at + 4].copy_from_slice(&header_word.to_le_bytes());
        self.top = next;
        Some(Ref(object_start + HEADER_SIZE))
    }

    /// Reclaims unreachable objects and compacts survivors, with the roots reported by
    /// `enumerate_roots`. Delegates to the shared [`mark_compact`] engine through
    /// [`PtrResolver`] (the device header-word -> type lookup), so the device collection
    /// is byte-for-byte the same algorithm the host tests exercise.
    #[cfg(feature = "gc-collect")]
    pub fn collect<R>(&mut self, enumerate_roots: R)
    where
        R: FnMut(&mut dyn FnMut(&mut Ref)),
    {
        self.collect_with_pins(enumerate_roots, &[]);
    }

    /// [`Self::collect`], with `pinned` naming the payload addresses (region-relative, as a
    /// [`Ref`] carries them) of objects the compaction must leave WHERE THEY ARE -- what a C#
    /// `fixed` statement's holder slot promises. See [`crate::heap::Heap::collect_with_pins`] for
    /// the semantics and the space cost; the engine is the same one.
    #[cfg(feature = "gc-collect")]
    pub fn collect_with_pins<R>(&mut self, enumerate_roots: R, pinned: &[u32])
    where
        R: FnMut(&mut dyn FnMut(&mut Ref)),
    {
        let top = self.top;
        self.top = mark_compact(self.region, top, &PtrResolver, enumerate_roots, pinned);
    }

    /// Collects using the live AOT call stack, for the safepoint-collect path: walks the
    /// frames from the top safepoint (`top_sp` = SP-at-the-call, `top_return_pc` = the
    /// safepoint return address) down through each caller via `stack_maps`, reclaims the
    /// unreachable, compacts the survivors, and writes every relocated reference back
    /// into `stack`. Pinned roots (a `fixed` statement's holder slot) keep their addresses.
    ///
    /// The walk itself is the SHARED one ([`crate::heap::visit_stack_roots`]) rather than a second
    /// copy of it: the host rehearsal and the device collection must not be able to walk
    /// differently, and only the heap's type lookup differs (pointer, not table index).
    #[cfg(feature = "gc-collect")]
    pub fn collect_stack(
        &mut self,
        stack: &mut [u8],
        top_sp: u32,
        top_return_pc: u32,
        stack_maps: &StackMapTable,
    ) {
        let pinned = crate::heap::pinned_stack_roots(stack, top_sp, top_return_pc, stack_maps);
        self.collect_with_pins(
            |visit| {
                crate::heap::visit_stack_roots(stack, top_sp, top_return_pc, stack_maps, visit)
            },
            &pinned,
        );
    }

    /// Turns a payload offset (a [`Ref`]) into the real `*mut u8` the backend's emitted
    /// code uses, by adding the region base. The null reference maps to a null pointer.
    #[must_use]
    pub fn payload_ptr(&self, reference: Ref) -> *mut u8 {
        if reference.is_null() {
            core::ptr::null_mut()
        } else {
            unsafe { self.base_ptr().add(reference.0 as usize) }
        }
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::boxed::Box;
    use alloc::vec;
    use alloc::vec::Vec;

    /// A backend-shaped type descriptor built on the host, leaked so its address is stable (the
    /// object header stores that address, exactly as on device).
    ///
    /// **Built from the EMITTER's shape, deliberately.** This helper used to write a two-word
    /// header -- the READER's assumption -- so every test here compared the reader against a
    /// replica of itself and passed while the reader disagreed with both backends. One of them was
    /// even named for the property it was not checking. The words below mirror
    /// `riscv32.rs`'s `emit_type_desc` (`DESC_HEADER_WORDS = 4`), which is the conformant emitter;
    /// if the emitted header ever changes, these tests must fail, and that is the point.
    fn make_desc(payload_size: u32, ref_offsets: &[u32]) -> *const DeviceTypeDesc {
        let mut words: Vec<u32> = Vec::with_capacity(4 + ref_offsets.len());
        words.push(payload_size);
        words.push(ref_offsets.len() as u32);
        words.push(FNV_TAG_PLACEHOLDER);
        words.push(0);
        words.extend_from_slice(ref_offsets);
        let leaked: &'static [u32] = Box::leak(words.into_boxed_slice());
        leaked.as_ptr().cast::<DeviceTypeDesc>()
    }

    /// A non-zero stand-in for the `type_tag` word, chosen to look like the FNV hash it really is:
    /// if the reader ever slips back to a two-word header it will read THIS as `ref_offsets[0]`,
    /// and an offset that large walks straight out of any payload -- so the mistake surfaces as a
    /// failure here rather than as a wild trace on silicon.
    const FNV_TAG_PLACEHOLDER: u32 = 0x811C_9DC5;

    /// A fixed raw region for the device heap, leaked so its pointer is `'static`.
    fn make_region(len: usize) -> (*mut u8, usize) {
        let buf: &'static mut [u8] = Box::leak(vec![0u8; len].into_boxed_slice());
        (buf.as_mut_ptr(), len)
    }

    #[test]
    fn device_type_desc_layout_matches_the_backend_wire_blob() {
        assert_eq!(core::mem::offset_of!(DeviceTypeDesc, payload_size), 0);
        assert_eq!(core::mem::offset_of!(DeviceTypeDesc, nrefs), 4);
        assert_eq!(core::mem::offset_of!(DeviceTypeDesc, type_tag), 8);
        assert_eq!(core::mem::offset_of!(DeviceTypeDesc, base_ptr), 12);
        assert_eq!(core::mem::offset_of!(DeviceTypeDesc, ref_offsets), 16);
        assert_eq!(DeviceTypeDesc::REF_OFFSETS_BASE, 16);
        assert_eq!(
            DeviceTypeDesc::REF_OFFSETS_BASE,
            core::mem::offset_of!(DeviceTypeDesc, ref_offsets),
            "the ref_offsets constant and the declared layout must not drift apart"
        );
    }

    #[test]
    fn a_descriptor_carrying_a_type_tag_does_not_leak_it_into_the_ref_offsets() {
        let desc = make_desc(12, &[0, 4, 8]);
        let read: Vec<u32> = (0..unsafe { (*desc).nrefs })
            .map(|i| unsafe { DeviceTypeDesc::ref_offset(desc, i) })
            .collect();
        assert_eq!(read, vec![0, 4, 8], "the tag must not appear among the reference offsets");
        assert!(
            !read.contains(&FNV_TAG_PLACEHOLDER),
            "reading the tag as an offset is the exact defect this pins"
        );
    }

    #[test]
    fn descriptor_accessors_read_payload_size_and_every_inline_ref_offset() {
        let desc = make_desc(12, &[0, 4, 8]);
        assert_eq!(unsafe { (*desc).payload_size }, 12);
        assert_eq!(unsafe { (*desc).nrefs }, 3);
        let mut seen: Vec<u32> = Vec::new();
        for i in 0..3 {
            seen.push(unsafe { DeviceTypeDesc::ref_offset(desc, i) });
        }
        assert_eq!(seen, vec![0, 4, 8]);
    }

    #[test]
    fn from_raw_reserves_the_null_word_and_alloc_lays_out_objects_at_aligned_offsets() {
        let leaf = make_desc(4, &[]);
        let pad = make_desc(5, &[]);
        let (base, len) = make_region(128);
        let mut heap = unsafe { DeviceHeap::from_raw(base, len) };
        assert_eq!(heap.top(), ALIGN);

        let a = unsafe { heap.alloc(4, leaf) }.unwrap();
        assert_eq!(a, Ref(ALIGN + HEADER_SIZE));
        assert_eq!(heap.payload_ptr(a), unsafe { base.add(a.0 as usize) });
        assert_eq!(heap.top(), ALIGN + HEADER_SIZE + 4);
        assert_eq!(unsafe { core::slice::from_raw_parts(heap.payload_ptr(a), 4) }, &[0u8; 4]);

        let b = unsafe { heap.alloc(5, pad) }.unwrap();
        assert_eq!(b, Ref(ALIGN + 2 * HEADER_SIZE + 4));
        assert_eq!(heap.top(), ALIGN + 2 * HEADER_SIZE + 4 + 8);
        assert!(heap.payload_ptr(Ref::NULL).is_null());
    }

    #[test]
    fn alloc_returns_none_when_the_object_does_not_fit() {
        let leaf = make_desc(4, &[]);
        let (base, len) = make_region((ALIGN + HEADER_SIZE + 4) as usize);
        let mut heap = unsafe { DeviceHeap::from_raw(base, len) };
        assert!(unsafe { heap.alloc(4, leaf) }.is_some());
        assert!(unsafe { heap.alloc(4, leaf) }.is_none());
    }


    /// Word 0 of a single-dimensional array descriptor.
    const ARRAY_RANK1: u32 = ARRAY_DESC_MARK | 1;

    #[test]
    fn array_descriptor_constants_are_the_emitted_ones() {
        assert_eq!(ARRAY_DESC_MARK & ARRAY_DESC_MARK_MASK, ARRAY_DESC_MARK);
        for rank in 1..=4u32 {
            let word0 = ARRAY_DESC_MARK | rank;
            assert_eq!(array_shape(word0, 0), Some((rank, 0)), "rank {rank}");
        }
        assert_eq!(array_shape(0, 0), None);
        assert_eq!(array_shape(12, 2), None);
        assert_eq!(array_shape(0x00FF_FFFF, 0), None);

        let widths = [
            (ELEMENT_KIND_REFERENCE, Some(4)),
            (1, Some(1)),
            (2, Some(1)),
            (3, Some(2)),
            (4, Some(2)),
            (5, Some(4)),
            (6, Some(8)),
            (7, Some(4)),
            (8, Some(8)),
            (9, None),
            (ELEMENT_KIND_OPAQUE, None),
        ];
        for (kind, width) in widths {
            assert_eq!(element_width(kind), width, "element kind {kind}");
        }
    }

    #[test]
    fn an_array_is_sized_from_its_element_count_not_from_word_zero() {
        assert_eq!(payload_extent(ARRAY_RANK1, 5, 3), 16);
        assert_eq!(payload_extent(ARRAY_RANK1, 2, 7), 11);
        assert_eq!(payload_extent(ARRAY_RANK1, 4, 5), 14);
        assert_eq!(payload_extent(ARRAY_RANK1, 6, 2), 20);
        assert_eq!(payload_extent(ARRAY_RANK1, ELEMENT_KIND_REFERENCE, 4), 20);
        assert_eq!(payload_extent(ARRAY_RANK1, 5, 0), 4);
    }

    #[test]
    fn the_collector_sizes_an_array_exactly_as_the_emitted_allocation_site_does() {
        for (kind, element_size) in [(ELEMENT_KIND_REFERENCE, 4), (1, 1), (2, 1), (3, 2), (4, 2), (5, 4), (6, 8), (7, 4), (8, 8)] {
            for length in [0u32, 1, 3, 64] {
                let emitted = length * element_size + 4;
                assert_eq!(
                    payload_extent(ARRAY_RANK1, kind, length),
                    emitted,
                    "element kind {kind}, length {length}",
                );
            }
        }
    }

    #[test]
    fn a_class_is_still_sized_from_word_zero() {
        assert_eq!(payload_extent(12, 2, 0xDEAD_BEEF), 12);
        assert_eq!(payload_extent(0, 0, 0xDEAD_BEEF), 0);
    }

    /// An array descriptor now carries `System.Array`'s VTABLE, laid BEFORE its words so that a
    /// virtual call on an array receiver has somewhere to dispatch. That moves the SYMBOL by the
    /// vtable span and leaves word 0 where it was, and this pins the half of it that is the
    /// collector's: the descriptor is read AT THE ADDRESS IT IS GIVEN, so whatever sits in front
    /// of it cannot change a footprint.
    ///
    /// The vtable words here are CODE-POINTER shaped on purpose, because that is the failure the
    /// arrangement invites: if an allocation site ever handed over the symbol instead of word 0,
    /// word 0 would be read out of the last vtable slot and `payload_extent` would take a function
    /// address for `MARK | rank`. The assertions below show that value is neither a plausible
    /// footprint nor even a recognizable array shape, so the mistake cannot degrade quietly.
    ///
    /// The OTHER half -- that the emitted site really does pass word 0 -- is the backend's, and
    /// they red-proved it both ways (forcing the vtable empty, and dropping the vtable span from
    /// the relocation, each fails their pin). This side owns only "reading is position-independent".
    #[test]
    fn a_vtable_laid_before_a_descriptor_cannot_change_what_the_collector_reads() {
        const CODE_POINTER: u32 = 0x1000_0164;
        let words: Vec<u32> = vec![
            CODE_POINTER,
            CODE_POINTER + 4,
            ARRAY_RANK1,
            5,
            FNV_TAG_PLACEHOLDER,
            0,
        ];
        let leaked: &'static [u32] = Box::leak(words.into_boxed_slice());
        let desc = leaked[2..].as_ptr().cast::<DeviceTypeDesc>();

        let (word0, word1) = unsafe { DeviceTypeDesc::header_words(desc) };
        assert_eq!(
            (word0, word1),
            (ARRAY_RANK1, 5),
            "the reader must start at word 0, not at the symbol the vtable now begins"
        );
        assert_eq!(payload_extent(word0, word1, 3), 16, "int[3] is still 4 + 3 * 4");

        assert_eq!(
            array_shape(leaked[1], leaked[2]),
            None,
            "a vtable slot must not be mistakable for an array descriptor's word 0"
        );
    }

    #[cfg(feature = "gc-collect")]
    fn collected_ref_offsets(word1: u32, length: u32) -> Vec<u32> {
        let mut seen: Vec<u32> = Vec::new();
        for_each_array_ref_offset(1, word1, length, &mut |offset| seen.push(offset));
        seen
    }

    #[test]
    #[cfg(feature = "gc-collect")]
    fn a_reference_array_traces_every_element_slot() {
        assert_eq!(collected_ref_offsets(ELEMENT_KIND_REFERENCE, 3), vec![4, 8, 12]);
        assert_eq!(collected_ref_offsets(ELEMENT_KIND_REFERENCE, 0), Vec::<u32>::new());
    }

    #[test]
    #[cfg(feature = "gc-collect")]
    fn a_primitive_array_yields_no_reference_slots() {
        for kind in [1u32, 2, 3, 4, 5, 6, 7, 8] {
            assert_eq!(collected_ref_offsets(kind, 4), Vec::<u32>::new(), "element kind {kind}");
        }
    }

    #[test]
    #[should_panic(expected = "value-type elements")]
    fn a_value_type_array_is_refused_rather_than_mis_strided() {
        let _ = payload_extent(ARRAY_RANK1, ELEMENT_KIND_OPAQUE, 3);
    }

    #[test]
    #[should_panic(expected = "single-dimensional")]
    fn a_multi_dimensional_array_is_refused() {
        let _ = payload_extent(ARRAY_DESC_MARK | 2, 5, 3);
    }

    #[test]
    #[should_panic(expected = "addressable range")]
    fn an_array_whose_footprint_overflows_is_refused() {
        let _ = payload_extent(ARRAY_RANK1, 6, u32::MAX / 4);
    }
}
