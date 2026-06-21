//! The device GC link: the process/device-global C-ABI surface the AOT backend
//! (`lamella_aot::arm32`) links its emitted `newobj` / `box` / array-alloc and
//! safepoint-collect calls against. The mark-compact engine itself is [`crate::heap`];
//! this module owns only the *global heap* and the *entry points*, reusing
//! [`Heap::alloc`] / [`Heap::collect`] / [`Heap::collect_stack`] unchanged.

extern crate alloc;

use alloc::vec::Vec;
use core::cell::UnsafeCell;

use crate::device_heap::{DeviceHeap, DeviceTypeDesc};
use crate::heap::{Heap, Ref, StackMapTable, TypeDesc};

/// The signature of the out-of-memory roots hook: given the live heap, report every
/// root slot to `visit` so the subsequent compaction relocates them. This is the seam
/// the device build fills with "decode the AOT frames at the captured SP/return_pc";
/// see [`set_oom_roots_hook`].
pub type OomRootsHook = fn(&mut Heap, visit: &mut dyn FnMut(&mut Ref));

/// The process/device-global garbage-collected heap and its OOM roots hook, behind a
/// single-threaded critical-section cell.
///
/// Modelled on [`lamella_alloc::BumpAllocator`]: an `UnsafeCell` made `Sync` by the
/// promise that *every* access happens inside [`critical_section`], which is mutually
/// exclusive on the single core the device profile targets (interrupts off on
/// Cortex-M; a no-op on the host). `None` means "not yet initialised".
struct GcCell {
    /// The global heap; `None` until [`lamella_gc_init`] installs one.
    heap: UnsafeCell<Option<Heap>>,
    /// The roots reported on an OOM collection; `None` means "collect with no roots"
    /// (the conservative default until backend installs the SP/PC frame walk).
    oom_roots: UnsafeCell<Option<OomRootsHook>>,
}

unsafe impl Sync for GcCell {}

/// The one global heap the AOT-emitted allocator and collector operate on.
static GC: GcCell = GcCell {
    heap: UnsafeCell::new(None),
    oom_roots: UnsafeCell::new(None),
};

/// One-time GC setup: install the global heap over a `capacity`-byte region with the
/// given TypeDesc table (an object's header word indexes it). Replaces any previously
/// installed heap, so it doubles as the per-test reset.
///
/// On device this hands the GC its fixed raw heap region instead of a `Vec`-backed
/// [`Heap`]; see the module header. The TypeDesc table is moved in once and lives for
/// the program's lifetime.
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
pub fn lamella_gc_teardown() {
    critical_section(|| unsafe {
        *GC.heap.get() = None;
        *GC.oom_roots.get() = None;
    });
}

/// Installs the hook that reports the live roots on an out-of-memory collection (see
/// [`lamella_gc_alloc`]). This is the backend seam: on device it is set to "walk the AOT
/// frames at the safepoint-captured SP/return_pc"; until then it is unset and OOM
/// collects with no roots. Exposed for that wiring and for the host tests that prove the
/// retry-after-collect path.
pub fn set_oom_roots_hook(hook: OomRootsHook) {
    critical_section(|| unsafe {
        *GC.oom_roots.get() = Some(hook);
    });
}

/// Runs `body` with exclusive `&mut Heap` access to the global heap inside a critical
/// section. Panics if the heap is uninitialised (a `newobj` before `lamella_gc_init` is
/// a backend bug, never a recoverable runtime state).
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
        let hook = unsafe { *GC.oom_roots.get() };
        match hook {
            Some(hook) => collect_via_hook(heap, hook),
            None => heap.collect(|_visit| {}),
        }
        heap.alloc(type_desc_id).map_or(Ref::NULL.0, |r| r.0)
    })
}

/// Runs one collection whose roots are reported by `hook`. Split out so the borrow of
/// `heap` by the hook and by `collect` is expressed in one place: the hook is handed the
/// heap to read its roots from (it may inspect object layouts) and the `visit` sink that
/// `Heap::collect` drives twice (mark, then relocate).
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
    /// The decoded stack maps for the lowered program, installed once at startup so the
    /// OOM-triggered [`DeviceHeap::collect_stack`] can resolve each safepoint's roots
    /// from the SP/return_pc the alloc shim captured. `None` until installed.
    stack_maps: UnsafeCell<Option<StackMapTable>>,
}

unsafe impl Sync for DeviceGcCell {}

/// The one global device heap the AOT-emitted allocator and collector operate on.
static DEVICE_GC: DeviceGcCell = DeviceGcCell {
    heap: UnsafeCell::new(None),
    stack_maps: UnsafeCell::new(None),
};

/// One-time device GC setup: install the global heap over the raw region `[base, base +
/// len)` -- the backend's linker `.heap` section -- and the program's decoded stack maps.
/// The region and maps live for the program's lifetime (the device heap is never torn
/// down). Call once, before the first allocation.
///
/// # Safety
/// `base`/`len` must name `len` bytes of memory exclusively owned by the GC for the
/// program's lifetime and not aliased elsewhere (the `.heap` section); `len >= ALIGN`.
/// See [`DeviceHeap::from_raw`].
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
/// returns the real *payload* pointer (`region_base + offset`); on out-of-memory it drives
/// one stack-walking collection from the captured `(sp, return_pc)` and retries, returning
/// null (`0`) only if the object still does not fit.
///
/// The object header holds the `type_desc` *pointer* (so the collector reads the
/// `payload_size` and `ref_offsets` by dereferencing it -- the device representation),
/// where the host [`lamella_gc_alloc`] entry uses a table index. `sp` and `return_pc` are
/// the mutator's SP-at-the-call and the safepoint return address, captured for free by the
/// shim from `r2`/`r3`; the fast path ignores them, the OOM path walks the stack from them.
///
/// # Safety
/// `type_desc` must be a valid [`DeviceTypeDesc`] address the backend emitted (its
/// `payload_size`/`nrefs`/`ref_offsets` are read on alloc and on every trace). `sp` and
/// `return_pc`, on the OOM path, must be the real mutator SP-at-the-call and safepoint
/// return address so the frame walk reads live roots and not arbitrary memory.
///
/// # The on-device stack slice (a documented seam to backend's harness)
///
/// The OOM collection walks the live AOT call stack via [`DeviceHeap::collect_stack`],
/// which needs that stack as a `&mut [u8]` whose index `sp` is SP-at-the-call. Forming the
/// real on-device stack slice from the captured `sp` (its extent down to the bottom frame)
/// is backend's harness side; until that lands, the OOM path here collects with **no
/// roots** (conservative: it never spuriously keeps an object, and on a host call -- where
/// `sp`/`return_pc` are not a real stack -- it must not interpret arbitrary memory as
/// frames). The structure ("capture SP/PC at the safepoint, then `collect_stack`") is the
/// exact shape the real stack slice drops into.
#[cfg_attr(target_arch = "arm", unsafe(no_mangle))]
pub unsafe extern "C" fn lamella_gc_alloc_impl(
    payload_size: u32,
    type_desc: *const DeviceTypeDesc,
    sp: u32,
    return_pc: u32,
) -> *mut u8 {
    let _ = payload_size;
    let _ = (sp, return_pc);
    with_device_heap(|heap| {
        debug_assert_eq!(
            unsafe { (*type_desc).payload_size },
            payload_size,
            "lamella_gc_alloc payload_size disagrees with the TypeDesc",
        );
        if let Some(reference) = unsafe { heap.alloc(type_desc) } {
            return heap.payload_ptr(reference);
        }
        heap.collect(|_visit| {});
        unsafe { heap.alloc(type_desc) }.map_or(core::ptr::null_mut(), |r| heap.payload_ptr(r))
    })
}

/// The device safepoint-collect entry: walks the live AOT call stack from the captured
/// `(sp, return_pc)` against the installed stack maps and relocates the survivors,
/// rewriting the roots in `stack`. The pointer-ABI counterpart of [`lamella_gc_collect`],
/// over the global [`DeviceHeap`].
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

#[cfg(target_arch = "arm")]
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
    use crate::heap::{StackMapEntry, ALIGN, HEADER_SIZE};
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
        }
    }

    /// A type with a single reference field at payload offset 0.
    fn one_ref() -> TypeDesc {
        TypeDesc {
            payload_size: 4,
            ref_offsets: vec![0],
        }
    }

    /// Reads a [`Ref`] back from a stack image at `at`.
    fn get_ref(image: &[u8], at: usize) -> Ref {
        Ref(u32::from_le_bytes([
            image[at],
            image[at + 1],
            image[at + 2],
            image[at + 3],
        ]))
    }

    /// Writes a [`Ref`] as 4 little-endian bytes into a stack image at `at`.
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
        assert_eq!(c, ALIGN + HEADER_SIZE, "OOM collect freed unrooted a,b; retry reuses front");

        lamella_gc_teardown();
    }

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
                },
            ],
        );
        assert_eq!(lamella_gc_alloc(64, 1), Ref::NULL.0);
        lamella_gc_teardown();
    }

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
            ref_offsets: vec![4, 12],
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

    /// A backend-shaped descriptor on the host: the wire words `[payload_size, nrefs,
    /// ref_offsets...]`, leaked so its address is stable (the header stores that address).
    fn make_desc(payload_size: u32, ref_offsets: &[u32]) -> *const DeviceTypeDesc {
        let mut words: Vec<u32> = Vec::with_capacity(2 + ref_offsets.len());
        words.push(payload_size);
        words.push(ref_offsets.len() as u32);
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

    #[test]
    fn device_alloc_impl_returns_null_on_hard_oom_after_a_collect() {
        let _guard = SERIALIZE.lock().unwrap_or_else(|p| p.into_inner());
        let big = make_desc(64, &[]);
        let (base, len) = device_region((ALIGN + HEADER_SIZE + 4) as usize);
        unsafe { lamella_gc_init_region(base, len, StackMapTable::default()) };
        let p = unsafe { lamella_gc_alloc_impl(64, big, 0, 0) };
        assert!(p.is_null());
    }

    #[test]
    fn device_alloc_impl_oom_collects_unrooted_garbage_then_the_retry_succeeds() {
        let _guard = SERIALIZE.lock().unwrap_or_else(|p| p.into_inner());
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
