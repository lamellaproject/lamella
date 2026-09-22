//! The device GC link: a process/device-global heap and the C-ABI allocation and collection
//! entry points over it. The mark-compact engine itself is [`crate::heap`]; this module owns
//! only the *global heap* and the *entry points*, reusing [`Heap::alloc`] /
//! [`Heap::collect`] / [`Heap::collect_stack`] unchanged.

#[cfg(feature = "host-heap")]
extern crate alloc;

#[cfg(feature = "host-heap")]
use alloc::vec::Vec;
use core::cell::UnsafeCell;

use crate::device_heap::{ARRAY_DESC_MARK, ARRAY_DESC_MARK_MASK, DeviceHeap, DeviceTypeDesc};
use crate::heap::Ref;
#[cfg(feature = "host-heap")]
use crate::heap::{Heap, StackMapTable, TypeDesc};


/// The signature of the out-of-memory roots hook: given the live heap, report every
/// root slot to `visit` so the subsequent compaction relocates them. See
/// [`set_oom_roots_hook`].
#[cfg(feature = "host-heap")]
pub type OomRootsHook = fn(&mut Heap, visit: &mut dyn FnMut(&mut Ref));

/// The process/device-global garbage-collected heap and its OOM roots hook, behind a
/// single-threaded critical-section cell.
///
/// Modelled on [`lamella_alloc::BumpAllocator`]: an `UnsafeCell` made `Sync` by the
/// promise that *every* access happens inside [`critical_section`], which is mutually
/// exclusive on the single core the device profile targets (interrupts off on
/// Cortex-M; a no-op on the host). `None` means "not yet initialised".
#[cfg(feature = "host-heap")]
struct GcCell {
    /// The global heap; `None` until [`lamella_gc_init`] installs one.
    heap: UnsafeCell<Option<Heap>>,
    /// The roots reported on an OOM collection. `None` means the collection runs with no
    /// roots, which reclaims every object -- live ones included.
    oom_roots: UnsafeCell<Option<OomRootsHook>>,
}

#[cfg(feature = "host-heap")]
unsafe impl Sync for GcCell {}

/// The one global heap the AOT-emitted allocator and collector operate on.
#[cfg(feature = "host-heap")]
static GC: GcCell = GcCell {
    heap: UnsafeCell::new(None),
    oom_roots: UnsafeCell::new(None),
};

/// One-time GC setup: install the global heap over a `capacity`-byte region with the
/// given TypeDesc table (an object's header word indexes it). Replaces any previously
/// installed heap, so it doubles as the per-test reset.
///
/// This installs the `Vec`-backed [`Heap`]. The device's fixed raw region is a separate
/// entry point, [`lamella_gc_init_region`], and a device build uses that one. The TypeDesc
/// table is moved in once and lives for the program's lifetime.
#[cfg(feature = "host-heap")]
pub fn lamella_gc_init(capacity: usize, type_descs: Vec<TypeDesc>) {
    let heap = Heap::new(capacity, type_descs);
    critical_section(|| unsafe {
        *GC.heap.get() = Some(heap);
        *GC.oom_roots.get() = None;
    });
}

/// Tears the global heap down (drops it and clears the OOM hook), so an independent test
/// can `lamella_gc_init` a fresh one without interference. Not part of the device ABI --
/// the device heap lives forever -- but the global state needs a reset between host tests.
#[cfg(feature = "host-heap")]
pub fn lamella_gc_teardown() {
    critical_section(|| unsafe {
        *GC.heap.get() = None;
        *GC.oom_roots.get() = None;
    });
}

/// Installs the hook that reports the live roots on an out-of-memory collection (see
/// [`lamella_gc_alloc`]). Unset, an out-of-memory collection runs with no roots and reclaims
/// every object, live ones included. Exposed for the host tests that prove the
/// retry-after-collect path.
#[cfg(feature = "host-heap")]
pub fn set_oom_roots_hook(hook: OomRootsHook) {
    critical_section(|| unsafe {
        *GC.oom_roots.get() = Some(hook);
    });
}

/// Runs `body` with exclusive `&mut Heap` access to the global heap inside a critical
/// section. Panics if the heap is uninitialised (a `newobj` before `lamella_gc_init` is
/// a backend bug, never a recoverable runtime state).
#[cfg(feature = "host-heap")]
fn with_heap<R>(body: impl FnOnce(&mut Heap) -> R) -> R {
    critical_section(|| unsafe {
        let heap = (*GC.heap.get())
            .as_mut()
            .expect("lamella_gc used before lamella_gc_init");
        body(heap)
    })
}

/// Allocates an object for the AOT backend's `newobj` / `box` / array-alloc: reserves a
/// zeroed `[header][payload]` block by bumping the global heap and returns the
/// heap-relative *payload* address (a [`crate::Ref`] value). On out-of-memory it triggers
/// one collection and retries; it returns `0` (the null reference) only if the object
/// still does not fit.
///
/// `payload_size` is the size the backend computed for the object (so the device
/// allocator need not dereference the TypeDesc to size the bump); `type_desc_id` selects
/// the layout the engine sizes and later traces from. They must agree -- `payload_size`
/// must equal the table entry's `payload_size` -- which a `debug_assert` checks; the
/// reserved size is always taken from the table so the allocator and collector agree.
///
/// Device ABI note (see module header): the real entry takes a `*const TypeDesc` and
/// returns a `*mut u8`; here it takes a table index and returns a `u32` offset.
#[must_use]
#[cfg(feature = "host-heap")]
pub fn lamella_gc_alloc(payload_size: u32, type_desc_id: u32) -> u32 {
    with_heap(|heap| {
        debug_assert!(
            heap.type_descs()
                .get(type_desc_id as usize)
                .is_none_or(|d| d.payload_size == payload_size),
            "lamella_gc_alloc payload_size {payload_size} disagrees with TypeDesc {type_desc_id}",
        );
        if let Some(reference) = heap.alloc(type_desc_id) {
            return reference.0;
        }
        #[cfg(feature = "gc-collect")]
        {
            let hook = unsafe { *GC.oom_roots.get() };
            match hook {
                Some(hook) => collect_via_hook(heap, hook),
                None => heap.collect(|_visit| {}),
            }
            heap.alloc(type_desc_id).map_or(Ref::NULL.0, |r| r.0)
        }
        #[cfg(not(feature = "gc-collect"))]
        {
            Ref::NULL.0
        }
    })
}

/// Runs one collection whose roots are reported by `hook`. Split out so the borrow of
/// `heap` by the hook and by `collect` is expressed in one place: the hook is handed the
/// heap to read its roots from (it may inspect object layouts) and the `visit` sink that
/// `Heap::collect` drives twice (mark, then relocate).
#[cfg(all(feature = "gc-collect", feature = "host-heap"))]
fn collect_via_hook(heap: &mut Heap, hook: OomRootsHook) {
    let mut roots: Vec<Ref> = Vec::new();
    hook(heap, &mut |slot: &mut Ref| roots.push(*slot));
    heap.collect(|visit| {
        for root in &mut roots {
            visit(root);
        }
    });
    let mut i = 0usize;
    hook(heap, &mut |slot: &mut Ref| {
        *slot = roots[i];
        i += 1;
    });
}

/// Collects using the live AOT call stack, for the backend's safepoint-collect call:
/// walks the frames from the top safepoint (`sp` = SP-at-the-call, `return_pc` = the
/// safepoint return address) down through each caller via `stack_maps`, reclaims the
/// unreachable, compacts the survivors, and writes every relocated reference back into
/// `stack`. Delegates wholesale to [`Heap::collect_stack`] on the global heap.
#[cfg(all(feature = "gc-collect", feature = "host-heap"))]
pub fn lamella_gc_collect(
    stack: &mut [u8],
    sp: u32,
    return_pc: u32,
    stack_maps: &StackMapTable,
) {
    with_heap(|heap| heap.collect_stack(stack, sp, return_pc, stack_maps));
}


/// The process/device-global raw-region heap, behind the same single-threaded
/// critical-section cell as [`GC`]. Separate from [`GC`] because the device heap uses the
/// raw-region/pointer representation ([`DeviceHeap`]) while [`GC`] uses the host-test
/// `Vec`/index representation ([`Heap`]); a device build drives this one, host tests the
/// other.
struct DeviceGcCell {
    /// The global device heap; `None` until [`lamella_gc_init_region`] installs one.
    heap: UnsafeCell<Option<DeviceHeap>>,
    /// The decoded stack maps for the lowered program, installed once at startup. They are what
    /// [`DeviceHeap::collect_stack`] resolves a safepoint's roots against. `None` until installed.
    #[cfg(feature = "host-heap")]
    stack_maps: UnsafeCell<Option<StackMapTable>>,
    /// The roots reported on this heap's out-of-memory collection, installed by
    /// [`set_device_oom_roots_hook`]. `None` means the OOM path does not collect at all -- see
    /// [`lamella_gc_alloc_impl`] for why that refusal is the safe default rather than a
    /// rootless collection.
    #[cfg(feature = "gc-collect")]
    oom_roots: UnsafeCell<Option<DeviceOomRootsHook>>,
    /// The collection the embedder runs when this heap is exhausted, installed by
    /// [`set_device_collect_hook`]. `None` means the out-of-memory path does not collect.
    #[cfg(feature = "gc-collect")]
    collect: UnsafeCell<Option<DeviceCollectHook>>,
}

unsafe impl Sync for DeviceGcCell {}

/// The one global device heap the AOT-emitted allocator and collector operate on.
static DEVICE_GC: DeviceGcCell = DeviceGcCell {
    heap: UnsafeCell::new(None),
    #[cfg(feature = "host-heap")]
    stack_maps: UnsafeCell::new(None),
    #[cfg(feature = "gc-collect")]
    oom_roots: UnsafeCell::new(None),
    #[cfg(feature = "gc-collect")]
    collect: UnsafeCell::new(None),
};

/// The signature of the device out-of-memory roots hook: report every root SLOT to `visit`, which
/// reads the slot to mark from it and writes the relocated reference back into it.
///
/// It takes no heap, unlike [`OomRootsHook`]: a device hook reads FRAME memory and the program's
/// global root regions, never the heap's own bookkeeping.
///
/// **IT IS CALLED TWICE PER COLLECTION** -- once to mark and once to relocate -- so it must
/// enumerate the same slots in the same order both times, and it must not allocate. Re-walking the
/// stack is exactly that, which is why the device form replays the walk instead of snapshotting the
/// roots into a `Vec` the way the host's [`collect_via_hook`] does; there is no allocator here.
#[cfg(feature = "gc-collect")]
pub type DeviceOomRootsHook = fn(visit: &mut dyn FnMut(&mut Ref));

/// A collection the EMBEDDER runs on this heap when it is exhausted, answering whether it ran.
///
/// **This is how a device image collects, and the division of labour is the point.** The embedder --
/// the runtime-support archive -- is the side that knows where its roots are (its own stack-map
/// walker), which of them are PINNED, and where a mark bitmap can live, because it owns the image's
/// memory map. None of those can be handed through a fixed signature without this crate inventing a
/// storage policy for a binary it knows nothing about.
///
/// It is given the live heap and is expected to call [`DeviceHeap::collect_no_alloc`], which
/// allocates nothing. **Answering `false` means no collection happened**, and the allocation that
/// triggered it then fails as it would have anyway -- which is the honest outcome when the embedder
/// cannot prove what is live (a pin list that overflowed, a mark bitmap too small for the region).
///
/// It must not allocate, for the reason the whole path exists: it runs at the moment the only
/// allocator in the image has just failed.
#[cfg(feature = "gc-collect")]
pub type DeviceCollectHook = fn(&mut DeviceHeap) -> bool;

/// Installs the collection an exhausted device heap runs. See [`DeviceCollectHook`].
///
/// Until one is installed the out-of-memory path does not collect, which is the safe default: a
/// collector that cannot prove an object dead must not reclaim it.
#[cfg(feature = "gc-collect")]
pub fn set_device_collect_hook(hook: DeviceCollectHook) {
    critical_section(|| unsafe {
        *DEVICE_GC.collect.get() = Some(hook);
    });
}

/// Installs the hook that reports live roots when the device heap runs out of memory.
///
/// Until one is installed the out-of-memory path does not collect (see [`lamella_gc_alloc_impl`]),
/// so an image whose programs hold a reference across an allocation installs this at startup,
/// before the first allocation.
///
/// It is INDEPENDENT of [`lamella_gc_init_region`] and deliberately not cleared by it, so the two
/// may be called in either order and a re-init does not silently drop the hook.
#[cfg(feature = "gc-collect")]
pub fn set_device_oom_roots_hook(hook: DeviceOomRootsHook) {
    critical_section(|| unsafe {
        *DEVICE_GC.oom_roots.get() = Some(hook);
    });
}

/// Removes the device OOM roots hook, so the next out-of-memory allocation refuses to collect.
///
/// **TEST-ONLY.** The hook is global and outlives one test, so a test asserting the no-hook
/// refusal has to state that it has none -- otherwise it passes or fails on whichever test ran
/// before it. A device installs its hook once and never removes it.
#[cfg(all(test, feature = "gc-collect"))]
fn clear_device_oom_roots_hook() {
    critical_section(|| unsafe {
        *DEVICE_GC.oom_roots.get() = None;
    });
}

/// One-time device GC setup WITHOUT stack maps: install the global heap over the raw region
/// `[base, base + len)` and nothing else.
///
/// **This is the init a C# AOT image calls**, and it exists separately because
/// [`lamella_gc_init_region`]'s `stack_maps` parameter is a `Vec`-backed [`StackMapTable`] -- a type
/// that cannot exist in a binary with no global allocator. The maps it carries are the FLAT tier's
/// format anyway; the linked tier reports its roots through `.lamella_stackmaps` records that a
/// walker in the runtime-support archive reads, and hands them over through
/// [`set_device_oom_roots_hook`].
///
/// # Safety
/// As [`lamella_gc_init_region`]: `base`/`len` must name `len` bytes owned exclusively by the GC for
/// the program's lifetime and not aliased elsewhere, with `len >= ALIGN`. See [`DeviceHeap::from_raw`].
pub unsafe fn lamella_gc_init_device_heap(base: *mut u8, len: usize) {
    let heap = unsafe { DeviceHeap::from_raw(base, len) };
    critical_section(|| unsafe {
        *DEVICE_GC.heap.get() = Some(heap);
    });
}

/// One-time device GC setup: install the global heap over the raw region `[base, base +
/// len)` -- the backend's linker `.heap` section -- and the program's decoded stack maps.
/// The region and maps live for the program's lifetime (the device heap is never torn
/// down). Call once, before the first allocation.
///
/// # Safety
/// `base`/`len` must name `len` bytes of memory exclusively owned by the GC for the
/// program's lifetime and not aliased elsewhere (the `.heap` section); `len >= ALIGN`.
/// See [`DeviceHeap::from_raw`].
#[cfg(feature = "host-heap")]
pub unsafe fn lamella_gc_init_region(base: *mut u8, len: usize, stack_maps: StackMapTable) {
    let heap = unsafe { DeviceHeap::from_raw(base, len) };
    critical_section(|| unsafe {
        *DEVICE_GC.heap.get() = Some(heap);
        *DEVICE_GC.stack_maps.get() = Some(stack_maps);
    });
}

/// Runs `body` with exclusive `&mut DeviceHeap` access to the global device heap inside a
/// critical section. Panics if the heap is uninitialised (an alloc before
/// [`lamella_gc_init_region`] is a backend bug, never a recoverable runtime state).
fn with_device_heap<R>(body: impl FnOnce(&mut DeviceHeap) -> R) -> R {
    critical_section(|| unsafe {
        let heap = (*DEVICE_GC.heap.get())
            .as_mut()
            .expect("lamella_gc used before lamella_gc_init_region");
        body(heap)
    })
}

/// The device allocator body, the impl half of the `lamella_gc_alloc` C-ABI entry (the
/// naked SP/PC shim, below, is the entry on ARM and tail-calls this). Bump-allocates a
/// zeroed `[header][payload]` block for the backend's `newobj` / `box` / array-alloc and
/// returns the real *payload* pointer (`region_base + offset`). On out-of-memory with
/// `gc-collect` it runs one collection and retries, returning null (`0`) if the object still
/// does not fit; without `gc-collect` an out-of-memory allocation returns null.
///
/// The object header holds the `type_desc` *pointer* (so the collector reads the
/// `payload_size` and `ref_offsets` by dereferencing it -- the device representation),
/// where the host [`lamella_gc_alloc`] entry uses a table index. `sp` and `return_pc` are
/// the mutator's SP-at-the-call and the safepoint return address, captured for free by the
/// shim from `r2`/`r3`. Both are accepted and ignored.
///
/// # Safety
/// `type_desc` must be a valid [`DeviceTypeDesc`] address the backend emitted (its
/// `payload_size`/`nrefs`/`ref_offsets` are read on alloc and on every trace).
///
/// # The out-of-memory collection has no roots
///
/// The collection marks nothing, so it reclaims every object -- including objects the caller
/// still holds -- and the retry can hand back memory a live object occupies. That is correct
/// only for a program that holds no reference across an allocation. A program that does must
/// not use this entry with `gc-collect` enabled.
#[cfg_attr(all(target_arch = "arm", feature = "device-entry"), unsafe(no_mangle))]
pub unsafe extern "C" fn lamella_gc_alloc_impl(
    payload_size: u32,
    type_desc: *const DeviceTypeDesc,
    sp: u32,
    return_pc: u32,
) -> *mut u8 {
    let _ = (sp, return_pc);
    with_device_heap(|heap| {
        debug_assert!(
            {
                let word0 = unsafe { (*type_desc).payload_size };
                word0 & ARRAY_DESC_MARK_MASK == ARRAY_DESC_MARK || word0 == payload_size
            },
            "lamella_gc_alloc payload_size disagrees with the TypeDesc",
        );
        if let Some(reference) = unsafe { heap.alloc(payload_size, type_desc) } {
            return heap.payload_ptr(reference);
        }
        #[cfg(all(feature = "gc-collect", feature = "host-heap"))]
        {
            let hook = unsafe { *DEVICE_GC.oom_roots.get() };
            let Some(hook) = hook else {
                return core::ptr::null_mut();
            };
            heap.collect(|visit| hook(visit));
            unsafe { heap.alloc(payload_size, type_desc) }
                .map_or(core::ptr::null_mut(), |r| heap.payload_ptr(r))
        }
        #[cfg(all(feature = "gc-collect", not(feature = "host-heap")))]
        {
            let hook = unsafe { *DEVICE_GC.collect.get() };
            let Some(hook) = hook else {
                return core::ptr::null_mut();
            };
            if !hook(heap) {
                return core::ptr::null_mut();
            }
            unsafe { heap.alloc(payload_size, type_desc) }
                .map_or(core::ptr::null_mut(), |r| heap.payload_ptr(r))
        }
        #[cfg(not(feature = "gc-collect"))]
        {
            core::ptr::null_mut()
        }
    })
}

/// The device safepoint-collect entry: walks the live AOT call stack from the captured
/// `(sp, return_pc)` against the installed stack maps and relocates the survivors,
/// rewriting the roots in `stack`. The pointer-ABI counterpart of [`lamella_gc_collect`],
/// over the global [`DeviceHeap`].
#[cfg(all(feature = "gc-collect", feature = "host-heap"))]
pub fn lamella_gc_collect_device(stack: &mut [u8], sp: u32, return_pc: u32) {
    with_device_heap(|heap| {
        let maps = unsafe { &*DEVICE_GC.stack_maps.get() };
        if let Some(maps) = maps {
            heap.collect_stack(stack, sp, return_pc, maps);
        } else {
            heap.collect(|_visit| {});
        }
    });
}

#[cfg(all(target_arch = "arm", feature = "device-entry"))]
core::arch::global_asm!(
    ".section .text.lamella_gc_alloc,\"ax\",%progbits",
    ".global lamella_gc_alloc",
    ".thumb_func",
    ".type lamella_gc_alloc,%function",
    "lamella_gc_alloc:",
    "    mov   r2, sp",
    "    mov   r3, lr",
    "    b     lamella_gc_alloc_impl",
);

/// Runs `body` with interrupts disabled on a Cortex-M target, restoring the prior
/// interrupt state afterward, so a bump or collection is atomic against an interrupt
/// handler that allocates (Cortex-M0 has no atomic CAS). This is the same critical
/// section [`lamella_alloc::BumpAllocator`] uses; the two crates intentionally match.
#[cfg(target_arch = "arm")]
fn critical_section<R>(body: impl FnOnce() -> R) -> R {
    use core::arch::asm;
    let primask: u32;
    unsafe {
        asm!("mrs {}, PRIMASK", out(reg) primask, options(nomem, nostack, preserves_flags));
        asm!("cpsid i", options(nomem, nostack, preserves_flags));
    }
    let result = body();
    if primask & 1 == 0 {
        unsafe { asm!("cpsie i", options(nomem, nostack, preserves_flags)) };
    }
    result
}

/// Off-target (the host, for tests) execution is single-threaded, so no actual critical
/// section is needed -- matching [`lamella_alloc::BumpAllocator`]'s host stub.
#[cfg(not(target_arch = "arm"))]
fn critical_section<R>(body: impl FnOnce() -> R) -> R {
    body()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heap::{ALIGN, HEADER_SIZE};
    #[cfg(feature = "gc-collect")]
    use crate::heap::StackMapEntry;
    use alloc::vec;
    use std::sync::{Mutex, MutexGuard};

    /// The global heap [`GC`] is shared process state, but `cargo test` runs test
    /// functions on multiple threads at once. This mutex serializes the tests that
    /// install / use / tear down the global so they never interleave on the single
    /// `static` (the on-device single-core, interrupts-off invariant the `unsafe`
    /// relies on). Each test holds the guard for its whole body. Poisoning is ignored:
    /// if a prior test panicked, the next still re-inits a fresh heap.
    static SERIALIZE: Mutex<()> = Mutex::new(());

    /// Acquires the serializing guard for a test body (recovering from poisoning).
    fn lock() -> MutexGuard<'static, ()> {
        SERIALIZE.lock().unwrap_or_else(|p| p.into_inner())
    }

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

    /// Reads a [`Ref`] back from a stack image at `at`.
    #[cfg_attr(not(feature = "gc-collect"), allow(dead_code))]
    fn get_ref(image: &[u8], at: usize) -> Ref {
        Ref(u32::from_le_bytes([
            image[at],
            image[at + 1],
            image[at + 2],
            image[at + 3],
        ]))
    }

    /// Writes a [`Ref`] as 4 little-endian bytes into a stack image at `at`.
    #[cfg_attr(not(feature = "gc-collect"), allow(dead_code))]
    fn put_ref(image: &mut [u8], at: usize, reference: Ref) {
        image[at..at + 4].copy_from_slice(&reference.0.to_le_bytes());
    }

    #[test]
    fn alloc_bumps_header_and_zeroed_payload_then_nulls_when_full() {
        let _guard = lock();
        let capacity = (ALIGN + 2 * (HEADER_SIZE + 4)) as usize;
        lamella_gc_init(capacity, vec![leaf()]);

        let a = lamella_gc_alloc(4, 0);
        assert_eq!(a, ALIGN + HEADER_SIZE);
        with_heap(|heap| {
            assert_eq!(heap.type_id_of(Ref(a)), 0);
            assert_eq!(heap.read_u32(a), 0);
        });

        let b = lamella_gc_alloc(4, 0);
        assert_eq!(b, ALIGN + 2 * HEADER_SIZE + 4);
        assert_ne!(a, b);

        let c = lamella_gc_alloc(4, 0);
        #[cfg(feature = "gc-collect")]
        assert_eq!(c, ALIGN + HEADER_SIZE, "OOM collect freed unrooted a,b; retry reuses front");
        #[cfg(not(feature = "gc-collect"))]
        assert_eq!(c, Ref::NULL.0, "the bump tier's OOM is final: null, no collect, no retry");

        lamella_gc_teardown();
    }

    #[cfg(feature = "gc-collect")]
    #[test]
    fn alloc_returns_null_when_object_cannot_fit_even_after_collect() {
        let _guard = lock();
        let capacity = (ALIGN + HEADER_SIZE + 4) as usize;
        lamella_gc_init(
            capacity,
            vec![
                leaf(),
                TypeDesc {
                    payload_size: 64,
                    ref_offsets: Vec::new(),
                    tagged_offsets: Vec::new(),
                },
            ],
        );
        assert_eq!(lamella_gc_alloc(64, 1), Ref::NULL.0);
        lamella_gc_teardown();
    }

    #[cfg(feature = "gc-collect")]
    #[test]
    fn a_rootless_oom_collect_reclaims_objects_the_mutator_still_holds() {
        let _guard = lock();
        let capacity = (ALIGN + 3 * (HEADER_SIZE + 4)) as usize;
        lamella_gc_init(capacity, vec![leaf()]);
        let live = lamella_gc_alloc(4, 0);
        assert_ne!(live, Ref::NULL.0, "the first allocation must succeed");
        let _ = lamella_gc_alloc(4, 0);
        let _ = lamella_gc_alloc(4, 0);

        let reused = lamella_gc_alloc(4, 0);

        assert_eq!(
            reused, live,
            "with no roots the OOM collect reclaims a LIVE object and the retry reuses its address"
        );
        lamella_gc_teardown();
    }

    #[cfg(feature = "gc-collect")]
    #[test]
    fn oom_collect_with_no_roots_reclaims_then_retry_succeeds() {
        let _guard = lock();
        let capacity = (ALIGN + 3 * (HEADER_SIZE + 4)) as usize;
        lamella_gc_init(capacity, vec![leaf()]);
        let _ = lamella_gc_alloc(4, 0);
        let _ = lamella_gc_alloc(4, 0);
        let _ = lamella_gc_alloc(4, 0);
        with_heap(|heap| assert_eq!(heap.top(), capacity as u32));

        let reused = lamella_gc_alloc(4, 0);
        assert_eq!(reused, ALIGN + HEADER_SIZE);
        with_heap(|heap| assert_eq!(heap.top(), ALIGN + HEADER_SIZE + 4));
        lamella_gc_teardown();
    }

    #[cfg(feature = "gc-collect")]
    #[test]
    fn collect_via_stack_relocates_frame_roots_and_reclaims_garbage() {
        let _guard = lock();
        lamella_gc_init(4096, vec![one_ref(), leaf()]);
        let a = Ref(lamella_gc_alloc(4, 0));
        let garbage = Ref(lamella_gc_alloc(4, 1));
        let b = Ref(lamella_gc_alloc(4, 1));
        let c = Ref(lamella_gc_alloc(4, 1));
        with_heap(|heap| heap.write_ref_field(a, 0, c));
        let _ = garbage;
        let top_before = with_heap(|heap| heap.top());

        let entry = StackMapEntry {
            return_pc: 0x100,
            frame_size: 32,
            saved_bytes: 4,
            ref_offsets: vec![4, 12],
            tagged_offsets: Vec::new(),
            pinned_offsets: vec![],
        };
        let maps = StackMapTable::from_entries(vec![entry]);
        let mut stack = vec![0u8; 32 + 4];
        put_ref(&mut stack, 4, a);
        put_ref(&mut stack, 12, b);
        put_ref(&mut stack, 32, Ref(0xDEAD));

        lamella_gc_collect(&mut stack, 0, 0x100, &maps);

        let a_new = get_ref(&stack, 4);
        let b_new = get_ref(&stack, 12);
        assert_eq!(a_new, Ref(ALIGN + HEADER_SIZE));
        with_heap(|heap| {
            assert_eq!(heap.type_id_of(a_new), 0);
            assert_eq!(heap.type_id_of(b_new), 1);
            let c_new = heap.read_ref_field(a_new, 0);
            assert_ne!(c_new, Ref::NULL);
            assert_eq!(heap.type_id_of(c_new), 1);
            assert!(heap.top() < top_before);
            assert_eq!(heap.top(), ALIGN + 3 * (HEADER_SIZE + 4));
        });
        lamella_gc_teardown();
    }

    #[cfg(feature = "gc-collect")]
    #[test]
    fn oom_roots_hook_keeps_live_objects_across_the_retry_collect() {
        let _guard = lock();
        let capacity = (ALIGN + 2 * (HEADER_SIZE + 4)) as usize;
        lamella_gc_init(capacity, vec![leaf()]);

        use core::cell::Cell;
        thread_local! {
            static LIVE_ROOT: Cell<u32> = const { Cell::new(0) };
        }
        fn hook(_heap: &mut Heap, visit: &mut dyn FnMut(&mut Ref)) {
            LIVE_ROOT.with(|cell| {
                let mut r = Ref(cell.get());
                visit(&mut r);
                cell.set(r.0);
            });
        }

        let keep = lamella_gc_alloc(4, 0);
        let _garbage = lamella_gc_alloc(4, 0);
        LIVE_ROOT.with(|c| c.set(keep));
        set_oom_roots_hook(hook);

        let fresh = lamella_gc_alloc(4, 0);
        let keep_new = LIVE_ROOT.with(Cell::get);
        assert_eq!(keep_new, ALIGN + HEADER_SIZE);
        assert_eq!(fresh, ALIGN + 2 * HEADER_SIZE + 4);
        assert_ne!(fresh, Ref::NULL.0);
        with_heap(|heap| assert_eq!(heap.top(), ALIGN + 2 * (HEADER_SIZE + 4)));
        lamella_gc_teardown();
    }

    #[test]
    fn teardown_then_reinit_gives_an_independent_fresh_heap() {
        let _guard = lock();
        lamella_gc_init(1024, vec![leaf()]);
        let _ = lamella_gc_alloc(4, 0);
        with_heap(|heap| assert!(heap.top() > ALIGN));
        lamella_gc_teardown();

        lamella_gc_init(1024, vec![leaf()]);
        let first = lamella_gc_alloc(4, 0);
        assert_eq!(first, ALIGN + HEADER_SIZE);
        lamella_gc_teardown();
    }

}

#[cfg(test)]
mod device_abi_tests {
    use super::*;
    use crate::device_heap::DeviceTypeDesc;
    use crate::heap::{ALIGN, HEADER_SIZE};
    use alloc::boxed::Box;
    use alloc::vec;
    use alloc::vec::Vec;
    use std::sync::Mutex;

    /// Serializes these tests on the global `DEVICE_GC` static (own lock, since they live
    /// in a separate module from the `GC` tests above).
    static SERIALIZE: Mutex<()> = Mutex::new(());

    /// A backend-shaped descriptor on the host, leaked so its address is stable (the header stores
    /// that address).
    ///
    /// The FOUR-word ratified header
    /// `[payload_size][nrefs][type_tag][base_ptr][ref_offsets...]`, mirroring `riscv32.rs`'s
    /// `DESC_HEADER_WORDS = 4`. This helper wrote a TWO-word header until the reader was corrected
    /// to @16 -- at which point it would have had `ref_offset` reading past the end of this very
    /// allocation, so it is not merely a stale comment. The sibling helper in `device_heap.rs`
    /// carries the same shape and the same note.
    fn make_desc(payload_size: u32, ref_offsets: &[u32]) -> *const DeviceTypeDesc {
        let mut words: Vec<u32> = Vec::with_capacity(4 + ref_offsets.len());
        words.push(payload_size);
        words.push(ref_offsets.len() as u32);
        words.push(0x811C_9DC5);
        words.push(0);
        words.extend_from_slice(ref_offsets);
        let leaked: &'static [u32] = Box::leak(words.into_boxed_slice());
        leaked.as_ptr().cast::<DeviceTypeDesc>()
    }

    /// A leaked raw region for the device heap, `'static` like the real `.heap` section.
    fn device_region(len: usize) -> (*mut u8, usize) {
        let buf: &'static mut [u8] = Box::leak(vec![0u8; len].into_boxed_slice());
        (buf.as_mut_ptr(), len)
    }

    #[test]
    fn device_init_region_then_alloc_impl_returns_a_real_payload_pointer() {
        let _guard = SERIALIZE.lock().unwrap_or_else(|p| p.into_inner());
        let leaf = make_desc(4, &[]);
        let (base, len) = device_region(64);
        unsafe { lamella_gc_init_region(base, len, StackMapTable::default()) };

        let p = unsafe { lamella_gc_alloc_impl(4, leaf, 0, 0) };
        assert!(!p.is_null());
        assert_eq!(p, unsafe { base.add((ALIGN + HEADER_SIZE) as usize) });
    }

    #[cfg(feature = "gc-collect")]
    #[test]
    fn device_alloc_impl_returns_null_on_hard_oom_after_a_collect() {
        let _guard = SERIALIZE.lock().unwrap_or_else(|p| p.into_inner());
        set_device_oom_roots_hook(no_roots);
        let big = make_desc(64, &[]);
        let (base, len) = device_region((ALIGN + HEADER_SIZE + 4) as usize);
        unsafe { lamella_gc_init_region(base, len, StackMapTable::default()) };
        let p = unsafe { lamella_gc_alloc_impl(64, big, 0, 0) };
        assert!(p.is_null());
    }

    /// A hook reporting NO roots: the explicit form of "this heap really is all garbage". It is a
    /// named function rather than an omission because that is the whole point of the hook -- an
    /// empty root set is a CLAIM the caller makes, not a default it falls into.
    #[cfg(feature = "gc-collect")]
    fn no_roots(_visit: &mut dyn FnMut(&mut Ref)) {}

    #[cfg(feature = "gc-collect")]
    #[test]
    fn device_oom_without_a_roots_hook_refuses_to_collect_rather_than_reclaiming_live_objects() {
        let _guard = SERIALIZE.lock().unwrap_or_else(|p| p.into_inner());
        clear_device_oom_roots_hook();
        let leaf = make_desc(4, &[]);
        let (base, len) = device_region((ALIGN + HEADER_SIZE + 4) as usize);
        unsafe { lamella_gc_init_region(base, len, StackMapTable::default()) };
        let first = unsafe { lamella_gc_alloc_impl(4, leaf, 0, 0) };
        assert!(!first.is_null(), "the region holds exactly one leaf");
        let second = unsafe { lamella_gc_alloc_impl(4, leaf, 0, 0) };
        assert!(
            second.is_null(),
            "with no roots hook the OOM path must refuse to collect, not reclaim a live object"
        );
    }

    #[cfg(feature = "gc-collect")]
    #[test]
    fn device_oom_drives_the_installed_hook_once_per_collection_pass() {
        let _guard = SERIALIZE.lock().unwrap_or_else(|p| p.into_inner());
        set_device_oom_roots_hook(count_the_passes);
        PASSES.store(0, core::sync::atomic::Ordering::SeqCst);
        let leaf = make_desc(4, &[]);
        let (base, len) = device_region((ALIGN + 2 * (HEADER_SIZE + 4)) as usize);
        unsafe { lamella_gc_init_region(base, len, StackMapTable::default()) };

        let first = unsafe { lamella_gc_alloc_impl(4, leaf, 0, 0) };
        let second = unsafe { lamella_gc_alloc_impl(4, leaf, 0, 0) };
        assert!(!first.is_null() && !second.is_null(), "two leaves fit");
        assert_eq!(
            PASSES.load(core::sync::atomic::Ordering::SeqCst),
            0,
            "a bump that fits must not collect, so the hook is not called on the fast path"
        );

        let third = unsafe { lamella_gc_alloc_impl(4, leaf, 0, 0) };
        assert!(
            !third.is_null(),
            "the collection reclaims the unreported objects and the retry fits"
        );
        assert_eq!(
            PASSES.load(core::sync::atomic::Ordering::SeqCst),
            2,
            "the hook is driven twice per collection -- once to mark, once to relocate"
        );
    }

    /// How many times [`count_the_passes`] has been driven since the last reset. A `static`
    /// rather than a closure capture because [`DeviceOomRootsHook`] is a plain `fn` pointer --
    /// which is what the device needs, since there is nothing to allocate a closure in.
    #[cfg(feature = "gc-collect")]
    static PASSES: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

    /// A hook that reports no roots and counts how often the collector asked it for them.
    #[cfg(feature = "gc-collect")]
    fn count_the_passes(_visit: &mut dyn FnMut(&mut Ref)) {
        PASSES.fetch_add(1, core::sync::atomic::Ordering::SeqCst);
    }

    #[cfg(feature = "gc-collect")]
    #[test]
    fn device_alloc_impl_oom_collects_unrooted_garbage_then_the_retry_succeeds() {
        let _guard = SERIALIZE.lock().unwrap_or_else(|p| p.into_inner());
        set_device_oom_roots_hook(no_roots);
        let leaf = make_desc(4, &[]);
        let (base, len) = device_region((ALIGN + 3 * (HEADER_SIZE + 4)) as usize);
        unsafe { lamella_gc_init_region(base, len, StackMapTable::default()) };
        for _ in 0..3 {
            assert!(!unsafe { lamella_gc_alloc_impl(4, leaf, 0, 0) }.is_null());
        }
        let reused = unsafe { lamella_gc_alloc_impl(4, leaf, 0, 0) };
        assert_eq!(reused, unsafe { base.add((ALIGN + HEADER_SIZE) as usize) });
        with_device_heap(|heap| assert_eq!(heap.top(), ALIGN + HEADER_SIZE + 4));
    }
}
