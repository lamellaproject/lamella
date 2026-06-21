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

use crate::heap::{align_up, mark_compact, Ref, StackMapTable, TypeResolver, ALIGN, HEADER_SIZE};

/// A type's on-device GC layout, in the exact memory shape the AOT backend emits and the
/// object header points at: `[u32 payload_size][u32 nrefs][u32 ref_offsets...]`,
/// word-aligned. `#[repr(C)]` with the count immediately followed by the inline offsets,
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
    /// The number of reference fields, i.e. the length of the `ref_offsets` array that
    /// immediately follows this field in memory.
    pub nrefs: u32,
    /// The first element of the inline `ref_offsets` array; the remaining `nrefs - 1`
    /// `u32`s follow it contiguously. Each is a byte offset within the payload of a
    /// 4-byte slot holding a child reference. A zero-`nrefs` descriptor leaves this word
    /// unread (it is the start of the next descriptor / padding).
    pub ref_offsets: [u32; 1],
}

impl DeviceTypeDesc {
    /// The byte offset, within a [`DeviceTypeDesc`], of the inline `ref_offsets` array
    /// (the two leading `u32` words `payload_size` and `nrefs`).
    const REF_OFFSETS_BASE: usize = 2 * 4;

    /// Reads the `i`th reference offset out of the descriptor at `desc` (a raw `*const
    /// TypeDesc` from an object header). `i` must be `< nrefs`.
    ///
    /// # Safety
    /// `desc` must point at a valid [`DeviceTypeDesc`] blob (the backend emits one per
    /// type and stores its address in each object's header) and `i < nrefs`, so the read
    /// stays within the descriptor's inline `ref_offsets` array.
    unsafe fn ref_offset(desc: *const DeviceTypeDesc, i: u32) -> u32 {
        unsafe {
            let base = desc.cast::<u8>().add(Self::REF_OFFSETS_BASE).cast::<u32>();
            base.add(i as usize).read_unaligned()
        }
    }
}

/// The device resolver: an object's header word is the *address* of its
/// [`DeviceTypeDesc`], dereferenced to answer the engine's payload-size and
/// reference-offset questions. This is the one piece that differs from the host's
/// [`crate::heap::TableResolver`] (a table-index lookup); the mark-compact algorithm is
/// otherwise identical.
struct PtrResolver;

impl TypeResolver for PtrResolver {
    fn payload_size(&self, header_word: u32) -> u32 {
        let desc = header_word as *const DeviceTypeDesc;
        unsafe { (*desc).payload_size }
    }

    fn for_each_ref_offset(&self, header_word: u32, f: &mut dyn FnMut(u32)) {
        let desc = header_word as *const DeviceTypeDesc;
        let nrefs = unsafe { (*desc).nrefs };
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
    /// The reserved size is taken from the descriptor's `payload_size`, so the allocator
    /// and the collector agree on every object's footprint.
    ///
    /// # Safety
    /// `type_desc` must be a valid [`DeviceTypeDesc`] address the backend emitted; its
    /// `payload_size` is read here and its whole layout is read on every later trace.
    #[must_use]
    pub unsafe fn alloc(&mut self, type_desc: *const DeviceTypeDesc) -> Option<Ref> {
        let payload_size = unsafe { (*type_desc).payload_size };
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
    pub fn collect<R>(&mut self, enumerate_roots: R)
    where
        R: FnMut(&mut dyn FnMut(&mut Ref)),
    {
        self.top = mark_compact(self.region, &PtrResolver, enumerate_roots);
    }

    /// Collects using the live AOT call stack, for the safepoint-collect path: walks the
    /// frames from the top safepoint (`top_sp` = SP-at-the-call, `top_return_pc` = the
    /// safepoint return address) down through each caller via `stack_maps`, reclaims the
    /// unreachable, compacts the survivors, and writes every relocated reference back
    /// into `stack`. The frame-walk convention is identical to [`crate::heap::Heap::
    /// collect_stack`]; only the heap's type lookup differs (pointer, not table index).
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

    /// A backend-shaped type descriptor built on the host: the wire words
    /// `[payload_size, nrefs, ref_offsets...]`, leaked so its address is stable (the
    /// object header stores that address, exactly as on device).
    fn make_desc(payload_size: u32, ref_offsets: &[u32]) -> *const DeviceTypeDesc {
        let mut words: Vec<u32> = Vec::with_capacity(2 + ref_offsets.len());
        words.push(payload_size);
        words.push(ref_offsets.len() as u32);
        words.extend_from_slice(ref_offsets);
        let leaked: &'static [u32] = Box::leak(words.into_boxed_slice());
        leaked.as_ptr().cast::<DeviceTypeDesc>()
    }

    /// A fixed raw region for the device heap, leaked so its pointer is `'static`.
    fn make_region(len: usize) -> (*mut u8, usize) {
        let buf: &'static mut [u8] = Box::leak(vec![0u8; len].into_boxed_slice());
        (buf.as_mut_ptr(), len)
    }

    #[test]
    fn device_type_desc_layout_matches_the_backend_wire_blob() {
        assert_eq!(core::mem::offset_of!(DeviceTypeDesc, payload_size), 0);
        assert_eq!(core::mem::offset_of!(DeviceTypeDesc, nrefs), 4);
        assert_eq!(core::mem::offset_of!(DeviceTypeDesc, ref_offsets), 8);
        assert_eq!(DeviceTypeDesc::REF_OFFSETS_BASE, 8);
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

        let a = unsafe { heap.alloc(leaf) }.unwrap();
        assert_eq!(a, Ref(ALIGN + HEADER_SIZE));
        assert_eq!(heap.payload_ptr(a), unsafe { base.add(a.0 as usize) });
        assert_eq!(heap.top(), ALIGN + HEADER_SIZE + 4);
        assert_eq!(unsafe { core::slice::from_raw_parts(heap.payload_ptr(a), 4) }, &[0u8; 4]);

        let b = unsafe { heap.alloc(pad) }.unwrap();
        assert_eq!(b, Ref(ALIGN + 2 * HEADER_SIZE + 4));
        assert_eq!(heap.top(), ALIGN + 2 * HEADER_SIZE + 4 + 8);
        assert!(heap.payload_ptr(Ref::NULL).is_null());
    }

    #[test]
    fn alloc_returns_none_when_the_object_does_not_fit() {
        let leaf = make_desc(4, &[]);
        let (base, len) = make_region((ALIGN + HEADER_SIZE + 4) as usize);
        let mut heap = unsafe { DeviceHeap::from_raw(base, len) };
        assert!(unsafe { heap.alloc(leaf) }.is_some());
        assert!(unsafe { heap.alloc(leaf) }.is_none());
    }
}
