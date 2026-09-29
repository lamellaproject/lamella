//! A reclaiming segregated-fit allocator for the constrained serve tiers: O(1) for every size
//! class, and a short best-fit list for the blocks beyond them.

#![no_std]
#![allow(unsafe_code)]
#![forbid(unsafe_op_in_unsafe_fn)]

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::ptr::NonNull;
#[cfg(target_has_atomic = "8")]
use core::sync::atomic::AtomicBool;
use core::sync::atomic::Ordering;

/// The number of size classes. The class table spans the interpreter's whole allocation
/// range with a fit tight enough that internal fragmentation stays small even for the
/// large TLS record buffers (~16 KiB), while keeping the class count low.
const CLASS_COUNT: usize = 44;

/// The minimum block size (and payload alignment): 16 bytes holds the free-list link with
/// room to spare and satisfies every alignment the interpreter's values need (<= 8).
const MIN_BLOCK: usize = 16;

/// The size (bytes) of class `index`. The schedule is fine where the churn lives (16-byte
/// steps to 256) and coarsens as sizes grow, so a 16 KiB record buffer still fits within
/// ~2 KiB of its class:
///   0..16   : 16, 32, ..., 256        (16-byte steps)   -- the tiny-churn range
///   16..28  : 320, 384, ..., 1024     (64-byte steps)
///   28..40  : 1536, 2048, ..., 7168   (512-byte steps)
///   40..44  : 10240, 12288, 16384, 20480
const fn class_size(index: usize) -> usize {
    if index < 16 {
        (index + 1) * 16
    } else if index < 28 {
        256 + (index - 15) * 64
    } else if index < 40 {
        1024 + (index - 27) * 512
    } else {
        match index {
            40 => 10240,
            41 => 12288,
            42 => 16384,
            _ => 20480,
        }
    }
}

/// The smallest class that fits `size` (already rounded to at least [`MIN_BLOCK`]), or
/// `None` when `size` exceeds every class.
///
/// Inverted in CLOSED FORM rather than by scanning [`class_size`], and the reason is interrupt
/// latency rather than throughput. On ARMv6-M this crate's lock is an interrupt-disable critical
/// section (see [`LockedHeap`]), the scan ran a call per class inside it, and 44 of those held
/// interrupts off for longer than a UART byte time on an 8 MHz part -- which is a LOST BYTE on the
/// receive path, measured on the NUCLEO-F091RC. It is the arithmetic inverse of the schedule above
/// and nothing else, held to that by `the_closed_form_agrees_with_the_schedule_over_the_whole_range`.
const fn class_for(size: usize) -> Option<usize> {
    if size <= MIN_BLOCK {
        Some(0)
    } else if size <= 256 {
        Some(size.div_ceil(16) - 1)
    } else if size <= 1024 {
        Some(15 + (size - 256).div_ceil(64))
    } else if size <= 7168 {
        Some(27 + (size - 1024).div_ceil(512))
    } else if size <= 10240 {
        Some(40)
    } else if size <= 12288 {
        Some(41)
    } else if size <= 16384 {
        Some(42)
    } else if size <= 20480 {
        Some(43)
    } else {
        None
    }
}

/// Everything an allocation or a free needs that is derivable from its [`Layout`] ALONE.
///
/// It exists to be computed OUTSIDE the lock. On ARMv6-M the lock is interrupts-off, so every
/// instruction inside it is interrupt latency paid by every peripheral on the part -- and the class
/// arithmetic needs no heap state at all, so there is no reason for it to be in there. Splitting it
/// out is what leaves the critical section holding only the pointer moves it exists to serialize.
struct Request {
    /// The class whose free list may satisfy this request, and to which its block returns.
    ///
    /// `None` covers BOTH cases no class serves: a request beyond every class, which the large free
    /// list serves ([`Request::large`]), and an over-aligned one, which an unreclaimed carve serves.
    /// Class blocks are only [`MIN_BLOCK`]-aligned, so for an over-aligned request neither the pop
    /// nor the push is sound -- and collapsing the two into one field is what makes `alloc` and
    /// `dealloc` agree about which blocks recycle by construction rather than by two predicates
    /// that must be kept in step.
    class: Option<usize>,
    /// The bytes to carve when no free list can serve it: the class's fixed size, or, beyond every
    /// class, the request's own size.
    carve_size: usize,
    /// The alignment the carve must honor.
    align: usize,
    /// Whether this is a LARGE block: beyond every class, at no more than the minimum alignment. A
    /// freed large block joins the large free list, where a later large request of its size or
    /// smaller takes it ([`Heap::take_large`]).
    large: bool,
}

impl Request {
    /// Reads `layout` into the class arithmetic. Pure -- no heap state, nothing to lock.
    fn of(layout: Layout) -> Request {
        let need = layout.size().max(MIN_BLOCK).max(layout.align());
        let align = layout.align().max(MIN_BLOCK);
        let sized = class_for(need);
        let large = sized.is_none() && align <= MIN_BLOCK;
        Request {
            class: if align <= MIN_BLOCK { sized } else { None },
            carve_size: match sized {
                Some(class) => class_size(class),
                None if large => need.next_multiple_of(MIN_BLOCK),
                None => need,
            },
            align,
            large,
        }
    }
}

/// The largest class's block size: a request beyond it is a large block (see [`Request::large`]).
const LARGEST_CLASS: usize = class_size(CLASS_COUNT - 1);

/// The largest class whose blocks fit in `size` bytes, or `None` below the smallest class.
const fn class_at_most(size: usize) -> Option<usize> {
    if size < MIN_BLOCK {
        return None;
    }
    match class_for(size) {
        Some(class) if class_size(class) == size => Some(class),
        Some(class) => Some(class - 1),
        None => Some(CLASS_COUNT - 1),
    }
}

/// A free LARGE block's header: its size and the next free large block (null at the tail). Written
/// into the block's own bytes while it is free; every large block is far bigger than this.
struct LargeNode {
    next: *mut LargeNode,
    size: usize,
}

/// A free block's intrusive header: the next free block in its class (null at the tail).
/// Written into the block's own bytes while it is free.
struct FreeNode {
    next: *mut FreeNode,
}

/// The segregated-fit heap over one contiguous region. Not `Sync` on its own; wrap in
/// [`LockedHeap`] for a global allocator.
pub struct Heap {
    /// The managed region.
    base: *mut u8,
    /// One past the region end (the bump frontier cannot pass it).
    end: *mut u8,
    /// The next never-yet-carved byte: a class with an empty free list carves here.
    frontier: *mut u8,
    /// Per-class free-list heads (null when the class has no reusable block).
    classes: [*mut FreeNode; CLASS_COUNT],
    /// The free LARGE blocks, taken best fit (null when there are none) -- see [`Request::large`].
    large: *mut LargeNode,
    /// Bytes the region has handed out and not taken back -- see [`Heap::live`].
    ///
    /// Maintained HERE rather than by a counting wrapper around the allocator, and on a CAS-less
    /// target that is the difference between possible and not: a wrapper's counter is read-modify-
    /// written outside any lock, and ARMv6-M has no atomic read-modify-write at all. Inside these
    /// methods the caller already holds the exclusion, so a plain `usize` needs none of its own.
    live: usize,
}

impl Heap {
    /// An empty heap; call [`Heap::init`] with the region before first use.
    pub const fn empty() -> Heap {
        Heap {
            base: core::ptr::null_mut(),
            end: core::ptr::null_mut(),
            frontier: core::ptr::null_mut(),
            classes: [core::ptr::null_mut(); CLASS_COUNT],
            large: core::ptr::null_mut(),
            live: 0,
        }
    }

    /// Points the heap at the `size`-byte region beginning at `base`. The region must stay
    /// valid and exclusively owned by this heap for its whole life.
    ///
    /// # Safety
    /// `base` must be non-null, aligned to at least [`MIN_BLOCK`], and reference `size`
    /// writable bytes not aliased elsewhere.
    pub unsafe fn init(&mut self, base: *mut u8, size: usize) {
        self.base = base;
        self.end = unsafe { base.add(size) };
        self.frontier = base;
        self.classes = [core::ptr::null_mut(); CLASS_COUNT];
        self.large = core::ptr::null_mut();
        self.live = 0;
    }

    /// Allocates for a prepared `request`, or null on exhaustion. A class hit pops its free
    /// list, a miss carves the class's fixed size from the bump frontier (aligned when the
    /// request over-aligns).
    ///
    /// Takes a [`Request`] rather than a `Layout` so the class arithmetic happens before the
    /// caller takes the lock: this body is what runs with interrupts off on ARMv6-M, and it is
    /// a few loads and stores with no loop and no call.
    fn alloc(&mut self, request: &Request) -> *mut u8 {
        if let Some(class) = request.class {
            let head = self.classes[class];
            if !head.is_null() {
                self.classes[class] = unsafe { (*head).next };
                self.live += request.carve_size;
                return head.cast();
            }
        }
        if request.large {
            let reused = self.take_large(request.carve_size);
            if !reused.is_null() {
                self.live += request.carve_size;
                return reused;
            }
        }
        let carved = self.carve(request.carve_size, request.align);
        if !carved.is_null() {
            self.live += request.carve_size;
        }
        carved
    }

    /// Carves `size` bytes from the bump frontier, advanced to `align`, or null if it would
    /// pass the region end.
    fn carve(&mut self, size: usize, align: usize) -> *mut u8 {
        let addr = self.frontier as usize;
        let aligned = (addr + align - 1) & !(align - 1);
        let next = aligned.saturating_add(size);
        if next > self.end as usize {
            return core::ptr::null_mut();
        }
        self.frontier = next as *mut u8;
        aligned as *mut u8
    }

    /// Takes the free large block that fits `size` bytes most closely, handing what it does not
    /// need to [`Heap::release`], or null when no free large block is big enough.
    ///
    /// BEST fit rather than first. First fit split a big free block for a smaller request, so a
    /// table growing from 24 to 40 KiB found no 40 KiB block on its second cycle; best fit gives
    /// each size its own block back.
    ///
    /// A walk inside the lock, and a bounded one: every node is a free block bigger than every
    /// class, so the list cannot hold more than the region's size over [`LARGEST_CLASS`] of them --
    /// twelve in a 256 KiB arena, each a load and a compare.
    fn take_large(&mut self, size: usize) -> *mut u8 {
        let mut best: *mut *mut LargeNode = core::ptr::null_mut();
        let mut best_size = usize::MAX;
        let mut link: *mut *mut LargeNode = &raw mut self.large;
        unsafe {
            while !(*link).is_null() {
                let node = *link;
                let node_size = (*node).size;
                if node_size >= size && node_size < best_size {
                    best = link;
                    best_size = node_size;
                    if node_size == size {
                        break;
                    }
                }
                link = &raw mut (*node).next;
            }
            if best.is_null() {
                return core::ptr::null_mut();
            }
            let node = *best;
            *best = (*node).next;
            if best_size > size {
                self.release(node.cast::<u8>().add(size), best_size - size);
            }
            node.cast()
        }
    }

    /// Makes the `size` free bytes at `ptr` reusable. Beyond every class they join the large free
    /// list; otherwise they become a block of the largest class they hold.
    ///
    /// The bytes past that class's size are LOST for good: the block is used and freed at its
    /// class's size from then on, and nothing tracks its tail again, because blocks never coalesce.
    /// The loss is bounded per split, by the step between two classes -- under 4 KiB at the top
    /// classes -- where dropping the whole remainder, as before, lost all of it.
    ///
    /// `ptr` is aligned to, and `size` is a multiple of, [`MIN_BLOCK`].
    fn release(&mut self, ptr: *mut u8, size: usize) {
        if size > LARGEST_CLASS {
            let node = ptr.cast::<LargeNode>();
            unsafe { node.write(LargeNode { next: self.large, size }) };
            self.large = node;
        } else if let Some(class) = class_at_most(size) {
            let node = ptr.cast::<FreeNode>();
            unsafe { (*node).next = self.classes[class] };
            self.classes[class] = node;
        }
    }

    /// Returns a block to a free list: a class block to its class's list (O(1)), a large block to
    /// the large free list ([`Request::large`]). An over-aligned carve is dropped: it is never
    /// reused, matching the bump behavior for that rare case.
    ///
    /// Takes a [`Request`] for the same reason [`Heap::alloc`] does: the class arithmetic is
    /// the caller's, computed before the lock.
    ///
    /// # Safety
    /// `ptr` must be a block this heap returned for a request prepared from the same
    /// `Layout`, not yet freed.
    unsafe fn dealloc(&mut self, ptr: *mut u8, request: &Request) {
        if let Some(class) = request.class {
            let node = ptr.cast::<FreeNode>();
            unsafe { (*node).next = self.classes[class] };
            self.classes[class] = node;
            self.live = self.live.saturating_sub(request.carve_size);
        } else if request.large {
            self.release(ptr, request.carve_size);
            self.live = self.live.saturating_sub(request.carve_size);
        }
    }

    /// Bytes never yet carved from the region -- the headroom remaining (does not count
    /// bytes sitting on free lists, which are already reusable).
    #[must_use]
    pub fn frontier_free(&self) -> usize {
        self.end as usize - self.frontier as usize
    }

    /// Bytes carved from the region so far (the high-water frontier). Monotonic -- the
    /// frontier never retreats (freed blocks recycle via their class list, they do not
    /// un-carve), so a lock-free mirror of this value is a sound diagnostic.
    #[must_use]
    fn carved(&self) -> usize {
        self.frontier as usize - self.base as usize
    }

    /// Bytes handed out and not yet returned: what a collector's pressure probe has to ask.
    ///
    /// A DIFFERENT question from [`Heap::carved`], and the reason the two exist side by side is
    /// that the high-water never falls -- so a trigger reading it finds itself over threshold
    /// forever however much a collection reclaims, and keeps raising its own bar until the region
    /// runs out. It is also a different question from [`Heap::frontier_free`], which cannot see
    /// the free lists at all.
    ///
    /// It counts BLOCK sizes, not requested sizes: a 20-byte request occupies a 32-byte class, and
    /// the 12 bytes of internal fragmentation are just as unavailable to the next caller as the 20
    /// asked for. A count of requested sizes under-reports by exactly that rounding.
    ///
    /// Constant time, which is a requirement rather than a nicety wherever a safe point reads it
    /// before every operation -- unlike [`Heap::free_list_bytes`], which walks every class list.
    #[must_use]
    pub fn live(&self) -> usize {
        self.live
    }

    /// Bytes sitting on the per-class free lists: already reclaimed and reusable, but invisible
    /// to [`Heap::frontier_free`] because the frontier never retreats.
    ///
    /// The distinction is not academic once the frontier is fully carved. At that point
    /// `frontier_free` is 0 FOREVER, however much has since been freed -- so a caller that asks
    /// only the frontier concludes the heap is permanently full while the whole of a dropped
    /// REPL session sits reusable on these lists.
    ///
    /// Reusability is per CLASS, not global: a class block serves only its own class, and a free
    /// large block serves only a large request of its size or smaller -- blocks never coalesce.
    /// Treat the total as an upper bound on what is actually available to any particular
    /// allocation pattern.
    ///
    /// O(free blocks) -- it walks every free list. Intended for a between-submissions probe or a
    /// diagnostic, never a hot path.
    #[must_use]
    pub fn free_list_bytes(&self) -> usize {
        let mut total = 0;
        let mut large = self.large;
        while !large.is_null() {
            unsafe {
                total += (*large).size;
                large = (*large).next;
            }
        }
        let mut index = 0;
        while index < CLASS_COUNT {
            let size = class_size(index);
            let mut node = self.classes[index];
            while !node.is_null() {
                total += size;
                node = unsafe { (*node).next };
            }
            index += 1;
        }
        total
    }
}

unsafe impl Send for Heap {}

/// A locked [`Heap`] usable as a `#[global_allocator]`.
///
/// # Two locks, chosen by what the target's atomics can do
///
/// Where the target has atomic compare-and-swap the lock is a plain spin (uncontended on the
/// single-core serve). **ARMv6-M -- Cortex-M0 and M0+ -- has atomic load/store but NO CAS**, so
/// `compare_exchange_weak` does not exist there and this crate did not compile for `thumbv6m` at
/// all. That is not a small gap in practice: it put the micro:bit v1, the RP2040, and both SAMD21
/// boards on the bump arena, whose `dealloc` is an empty body -- so the collector on those parts
/// ran, reclaimed, and released every dead object's payload onto the floor. A bounded live set
/// still exhausted the arena, which is exactly the property [the GC bar] forbids.
///
/// On a CAS-less target the lock is therefore an interrupt-disable critical section instead. On a
/// single-core MCU that is not a workaround, it is the stronger primitive: a spin lock on one core
/// DEADLOCKS outright if an interrupt handler allocates while the lock is held, where disabling
/// interrupts makes that case impossible rather than unlikely.
pub struct LockedHeap {
    /// The spin flag -- present only on targets whose atomics can CAS; the critical-section arm
    /// needs no flag, because interrupts-off IS the exclusion.
    #[cfg(target_has_atomic = "8")]
    locked: AtomicBool,
    heap: UnsafeCell<Heap>,
    /// A lock-free mirror of the heap's carved high-water (see [`Heap::carved`]), refreshed
    /// after each allocation. Read WITHOUT the lock -- for a diagnostic or a panic-path
    /// number that must never risk the lock (an alloc-error panic fires with the lock
    /// already released, but a panic from elsewhere could hold it).
    carved: core::sync::atomic::AtomicUsize,
    /// A lock-free mirror of [`Heap::live`], refreshed after each allocation and each free.
    ///
    /// Read WITHOUT the lock, for the same reason as `carved` and one more: a collector's arena
    /// probe reads it at every safe point, before every operation, so it must not be able to
    /// contend with the allocation it is about to authorize.
    live: core::sync::atomic::AtomicUsize,
}

impl LockedHeap {
    /// An empty locked heap; [`LockedHeap::init`] points it at a region before first use.
    #[must_use]
    pub const fn empty() -> LockedHeap {
        LockedHeap {
            #[cfg(target_has_atomic = "8")]
            locked: AtomicBool::new(false),
            heap: UnsafeCell::new(Heap::empty()),
            carved: core::sync::atomic::AtomicUsize::new(0),
            live: core::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Points the heap at the `size`-byte region at `base` (see [`Heap::init`]).
    ///
    /// # Safety
    /// As [`Heap::init`]: `base`/`size` describe a valid, exclusively-owned region.
    pub unsafe fn init(&self, base: *mut u8, size: usize) {
        self.with(|heap| unsafe { heap.init(base, size) });
    }

    /// Runs `body` holding the lock (the spin arm -- targets with atomic CAS).
    #[cfg(target_has_atomic = "8")]
    fn with<T>(&self, body: impl FnOnce(&mut Heap) -> T) -> T {
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        let result = body(unsafe { &mut *self.heap.get() });
        self.locked.store(false, Ordering::Release);
        result
    }

    /// Runs `body` holding the lock (the critical-section arm -- ARMv6-M, which has no CAS).
    ///
    /// Interrupts off IS the exclusion, so there is no flag to set and nothing to spin on.
    ///
    /// WHAT THE CALLER MUST DO: everything derivable from the `Layout` is computed BEFORE this is
    /// called (see [`Request`]), so `body` is only the pointer moves that need serializing. That
    /// is a caller obligation, not a property of this function, and it was not always met -- an
    /// earlier version passed the `Layout` through and searched the class table in here, holding
    /// interrupts off across a call per class. On an 8 MHz Cortex-M0 that exceeded a 115200 byte
    /// time from ~1 KiB upward, and the receive path LOST A BYTE. "O(1) allocation" was true of
    /// the free-list work and said nothing about the search that chose the list.
    #[cfg(not(target_has_atomic = "8"))]
    fn with<T>(&self, body: impl FnOnce(&mut Heap) -> T) -> T {
        critical_section(|| body(unsafe { &mut *self.heap.get() }))
    }

    /// Bytes never yet carved from the region (takes the lock).
    #[must_use]
    pub fn frontier_free(&self) -> usize {
        self.with(|heap| heap.frontier_free())
    }

    /// The carved high-water, read LOCK-FREE (the mirror refreshed after each alloc). Safe
    /// in a panic handler where taking the lock could deadlock.
    #[must_use]
    pub fn carved_lockfree(&self) -> usize {
        self.carved.load(Ordering::Relaxed)
    }

    /// Bytes handed out and not yet returned (see [`Heap::live`]), read LOCK-FREE.
    ///
    /// **This is the figure an arena probe wants**, and having it here rather than in a counting
    /// wrapper around the global allocator is not tidiness. A wrapper has to read-modify-write its
    /// own counter outside this lock, and **ARMv6-M has no atomic read-modify-write** -- so on
    /// Cortex-M0/M0+ such a wrapper either does not compile or needs a second interrupt-disable
    /// critical section of its own, beside the one this crate already takes for the same region.
    /// Maintained inside [`Heap`], the count needs no synchronization it does not already have.
    #[must_use]
    pub fn live_lockfree(&self) -> usize {
        self.live.load(Ordering::Relaxed)
    }

    /// Bytes on the per-class free lists (takes the lock). See [`Heap::free_list_bytes`] --
    /// including the caveat that these are reusable only within their own class.
    #[must_use]
    pub fn free_list_bytes(&self) -> usize {
        self.with(|heap| heap.free_list_bytes())
    }
}

unsafe impl Sync for LockedHeap {}

/// Runs `body` with interrupts disabled on a Cortex-M target, restoring the prior interrupt state
/// afterwards. Same shape as `lamella-alloc`'s, deliberately: the two crates guard different
/// structures for the same reason, and a second spelling of one primitive is a place for them to
/// drift.
///
/// The restore is CONDITIONAL on the entry state rather than an unconditional `cpsie i`, so a call
/// made with interrupts already off -- from a fault handler, or nested inside another section --
/// leaves them off instead of silently re-enabling them under its caller.
#[cfg(all(not(target_has_atomic = "8"), target_arch = "arm"))]
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

#[cfg(all(not(target_has_atomic = "8"), not(target_arch = "arm")))]
compile_error!(
    "lamella-heap needs either atomic CAS or a critical section for this target, and has neither: \
     add an interrupt-disable `critical_section` for this architecture beside the ARMv6-M one"
);

unsafe impl GlobalAlloc for LockedHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let request = Request::of(layout);
        let (ptr, carved, live) =
            self.with(|heap| (heap.alloc(&request), heap.carved(), heap.live()));
        self.carved.store(carved, Ordering::Relaxed);
        self.live.store(live, Ordering::Relaxed);
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if let Some(ptr) = NonNull::new(ptr) {
            let request = Request::of(layout);
            let live = self.with(|heap| {
                unsafe { heap.dealloc(ptr.as_ptr(), &request) };
                heap.live()
            });
            self.live.store(live, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::alloc::{alloc as sys_alloc, dealloc as sys_dealloc, Layout as SysLayout};
    use std::vec::Vec;

    /// A test heap over a leaked host region.
    fn heap(size: usize) -> (LockedHeap, *mut u8, SysLayout) {
        let layout = SysLayout::from_size_align(size, 16).unwrap();
        let region = unsafe { sys_alloc(layout) };
        let locked = LockedHeap::empty();
        unsafe { locked.init(region, size) };
        (locked, region, layout)
    }

    fn free_region(region: *mut u8, layout: SysLayout) {
        unsafe { sys_dealloc(region, layout) };
    }

    #[test]
    fn same_size_churn_reuses_one_block() {
        let (h, region, layout) = heap(64 * 1024);
        let l = Layout::from_size_align(48, 8).unwrap();
        let first = unsafe { h.alloc(l) };
        assert!(!first.is_null());
        let after_first = h.frontier_free();
        for _ in 0..10_000 {
            unsafe { h.dealloc(first, l) };
            let p = unsafe { h.alloc(l) };
            assert_eq!(p, first, "the freed block is reused in place");
        }
        assert_eq!(h.frontier_free(), after_first, "no frontier growth under churn");
        free_region(region, layout);
    }

    #[test]
    fn distinct_sizes_use_distinct_classes() {
        let (h, region, layout) = heap(64 * 1024);
        let small = Layout::from_size_align(16, 8).unwrap();
        let big = Layout::from_size_align(1000, 8).unwrap();
        let a = unsafe { h.alloc(small) };
        let b = unsafe { h.alloc(big) };
        assert!(!a.is_null() && !b.is_null());
        assert_ne!(a, b);
        unsafe { h.dealloc(a, small) };
        let c = unsafe { h.alloc(big) };
        assert_ne!(c, a, "a big request must not reuse a small freed block");
        let d = unsafe { h.alloc(small) };
        assert_eq!(d, a, "the small class reuses its freed block");
        free_region(region, layout);
    }

    #[test]
    fn large_record_buffers_fit_and_reuse() {
        let (h, region, layout) = heap(128 * 1024);
        let xfer = Layout::from_size_align(16_640, 8).unwrap();
        let a = unsafe { h.alloc(xfer) };
        assert!(!a.is_null());
        unsafe { h.dealloc(a, xfer) };
        let b = unsafe { h.alloc(xfer) };
        assert_eq!(a, b, "the record buffer's class reuses it across evaluations");
        free_region(region, layout);
    }

    #[test]
    fn exhaustion_returns_null_not_a_wild_pointer() {
        let (h, region, layout) = heap(8 * 1024);
        let big = Layout::from_size_align(4096, 8).unwrap();
        let mut live = Vec::new();
        loop {
            let p = unsafe { h.alloc(big) };
            if p.is_null() {
                break;
            }
            live.push(p);
        }
        assert!(!live.is_empty(), "some allocations succeeded before exhaustion");
        assert!(unsafe { h.alloc(big) }.is_null());
        free_region(region, layout);
    }

    #[test]
    fn realistic_mixed_churn_stays_bounded() {
        let (h, region, layout) = heap(64 * 1024);
        let hold = Layout::from_size_align(2048, 8).unwrap();
        let held = unsafe { h.alloc(hold) };
        assert!(!held.is_null());
        let small = Layout::from_size_align(96, 8).unwrap();
        let medium = Layout::from_size_align(512, 8).unwrap();
        let s = unsafe { h.alloc(small) };
        let m = unsafe { h.alloc(medium) };
        unsafe {
            h.dealloc(s, small);
            h.dealloc(m, medium);
        }
        let stable = h.frontier_free();
        for _ in 0..50_000 {
            let s = unsafe { h.alloc(small) };
            let m = unsafe { h.alloc(medium) };
            unsafe {
                h.dealloc(s, small);
                h.dealloc(m, medium);
            }
        }
        assert_eq!(h.frontier_free(), stable, "mixed churn does not grow the frontier");
        unsafe { h.dealloc(held, hold) };
        free_region(region, layout);
    }

    /// The schedule search `class_for` REPLACED: the smallest class whose size fits, found by
    /// walking [`class_size`] itself. Kept here as the reference the closed form is checked
    /// against -- it restates nothing, so it cannot drift from the table the way a second copy
    /// of the arithmetic would.
    fn class_for_by_scan(size: usize) -> Option<usize> {
        (0..CLASS_COUNT).find(|&index| class_size(index) >= size)
    }

    #[test]
    fn the_closed_form_agrees_with_the_schedule_over_the_whole_range() {
        let top = class_size(CLASS_COUNT - 1);
        for size in 0..=(top + 64) {
            assert_eq!(
                class_for(size),
                class_for_by_scan(size),
                "the closed form and the schedule disagree at size {size}"
            );
        }
    }

    #[test]
    fn class_schedule_is_monotonic_and_covers_the_range() {
        let mut previous = 0;
        for index in 0..CLASS_COUNT {
            let size = class_size(index);
            assert!(size > previous, "class sizes strictly increase");
            assert_eq!(size % 16, 0, "every class is 16-aligned");
            previous = size;
        }
        let top = class_size(CLASS_COUNT - 1);
        assert_eq!(top, 20480, "the top class covers the large record buffers");
        assert_eq!(class_for(1), Some(0));
        assert_eq!(class_for(16), Some(0));
        assert_eq!(class_for(17), Some(1));
        assert_eq!(class_for(top), Some(CLASS_COUNT - 1));
        assert_eq!(class_for(top + 1), None);
    }

    #[test]
    fn a_fully_carved_heap_still_reports_its_reclaimed_bytes() {
        let (h, region, layout) = heap(64 * 1024);
        let l = Layout::from_size_align(256, 8).unwrap();
        let mut live = Vec::new();
        loop {
            let p = unsafe { h.alloc(l) };
            if p.is_null() {
                break;
            }
            live.push(p);
        }
        assert!(live.len() > 1, "the region held many blocks");
        assert!(h.frontier_free() < 256, "the frontier is carved out");
        assert_eq!(h.free_list_bytes(), 0, "nothing freed yet");

        let freed = live.len();
        for p in live.drain(..) {
            unsafe { h.dealloc(p, l) };
        }

        assert_eq!(h.frontier_free(), 0, "the frontier NEVER retreats -- this is the trap");
        assert_eq!(
            h.free_list_bytes(),
            freed * 256,
            "every freed block is accounted for, exactly, at its class size"
        );

        let reused = unsafe { h.alloc(l) };
        assert!(!reused.is_null(), "a fully-carved heap still serves from its free lists");
        assert_eq!(h.free_list_bytes(), (freed - 1) * 256, "the reuse is reflected");
        free_region(region, layout);
    }

    #[test]
    fn free_list_bytes_counts_the_class_size_not_the_request() {
        let (h, region, layout) = heap(64 * 1024);
        let l = Layout::from_size_align(17, 8).unwrap();
        let p = unsafe { h.alloc(l) };
        assert!(!p.is_null());
        unsafe { h.dealloc(p, l) };
        assert_eq!(h.free_list_bytes(), class_size(class_for(17).unwrap()));
        free_region(region, layout);
    }

    #[test]
    fn live_falls_when_a_block_is_freed_and_the_high_water_does_not() {
        let (h, region, layout) = heap(64 * 1024);
        let l = Layout::from_size_align(48, 8).unwrap();
        assert_eq!(h.live_lockfree(), 0, "a fresh region has nothing live");
        let p = unsafe { h.alloc(l) };
        assert!(!p.is_null());
        let carved_after_alloc = h.carved_lockfree();
        assert_eq!(h.live_lockfree(), 48, "the 48-byte class block is live");
        unsafe { h.dealloc(p, l) };
        assert_eq!(h.live_lockfree(), 0, "freeing it returns the bytes to the count");
        assert_eq!(
            h.carved_lockfree(),
            carved_after_alloc,
            "and the high-water does NOT fall -- that is the whole distinction"
        );
        free_region(region, layout);
    }

    #[test]
    fn live_counts_the_block_and_not_the_request() {
        let (h, region, layout) = heap(64 * 1024);
        let l = Layout::from_size_align(20, 8).unwrap();
        let p = unsafe { h.alloc(l) };
        assert!(!p.is_null());
        assert_eq!(h.live_lockfree(), class_size(class_for(20).unwrap()));
        assert_eq!(h.live_lockfree(), 32, "and that class is 32, not the 20 requested");
        free_region(region, layout);
    }

    #[test]
    fn live_does_not_fall_for_a_block_the_allocator_cannot_reclaim() {
        let (h, region, layout) = heap(256 * 1024);
        let huge = Layout::from_size_align(64 * 1024, 64).unwrap();
        let p = unsafe { h.alloc(huge) };
        assert!(!p.is_null());
        assert_eq!(h.live_lockfree(), 64 * 1024);
        unsafe { h.dealloc(p, huge) };
        assert_eq!(
            h.live_lockfree(),
            64 * 1024,
            "an unreclaimable block stays counted, because it stays consumed"
        );
        assert_eq!(h.free_list_bytes(), 0, "and it really is on no free list");
        free_region(region, layout);
    }

    #[test]
    fn live_is_flat_across_churn_where_the_high_water_is_also_flat() {
        let (h, region, layout) = heap(64 * 1024);
        let l = Layout::from_size_align(48, 8).unwrap();
        let held = unsafe { h.alloc(l) };
        assert!(!held.is_null());
        let live_with_one = h.live_lockfree();
        for _ in 0..10_000 {
            let p = unsafe { h.alloc(l) };
            unsafe { h.dealloc(p, l) };
        }
        assert_eq!(h.live_lockfree(), live_with_one, "churn leaves one block live");
        free_region(region, layout);
    }

    #[test]
    fn a_freed_large_block_serves_a_same_size_or_smaller_request_without_carving() {
        let (h, region, layout) = heap(256 * 1024);
        let big = Layout::from_size_align(64 * 1024, 8).unwrap();
        let first = unsafe { h.alloc(big) };
        assert!(!first.is_null());
        let watermark = h.carved_lockfree();
        unsafe { h.dealloc(first, big) };
        assert_eq!(h.live_lockfree(), 0, "a freed large block is not live");
        assert_eq!(h.free_list_bytes(), 64 * 1024, "it waits on the large free list");

        let again = unsafe { h.alloc(big) };
        assert_eq!(again, first, "the same size gets the same block back");
        assert_eq!(h.carved_lockfree(), watermark, "and the region is not carved again");
        unsafe { h.dealloc(again, big) };

        let smaller = Layout::from_size_align(40 * 1024, 8).unwrap();
        let front = unsafe { h.alloc(smaller) };
        assert_eq!(front, first, "a smaller large request takes the block's front");
        assert_eq!(h.carved_lockfree(), watermark);
        assert_eq!(h.free_list_bytes(), 24 * 1024, "and the rest stays free");
        let rest = Layout::from_size_align(24 * 1024, 8).unwrap();
        let tail = unsafe { h.alloc(rest) };
        assert_eq!(tail as usize, first as usize + 40 * 1024, "the rest serves the next that fits");
        assert_eq!(h.carved_lockfree(), watermark);
        assert_eq!(h.free_list_bytes(), 0);
        unsafe { h.dealloc(front, smaller) };
        unsafe { h.dealloc(tail, rest) };
        assert_eq!(h.live_lockfree(), 0);
        free_region(region, layout);
    }

    #[test]
    fn a_remainder_below_the_top_class_becomes_a_block_of_the_largest_class_it_holds() {
        let (h, region, layout) = heap(256 * 1024);
        let big = Layout::from_size_align(64 * 1024, 8).unwrap();
        let block = unsafe { h.alloc(big) };
        unsafe { h.dealloc(block, big) };
        let most = Layout::from_size_align(60 * 1024, 8).unwrap();
        let front = unsafe { h.alloc(most) };
        assert_eq!(front, block);
        let watermark = h.carved_lockfree();
        let small = Layout::from_size_align(4000, 8).unwrap();
        let from_the_rest = unsafe { h.alloc(small) };
        assert_eq!(
            from_the_rest as usize,
            block as usize + 60 * 1024,
            "a request of that class takes the remainder rather than carving"
        );
        assert_eq!(h.carved_lockfree(), watermark);
        unsafe { h.dealloc(front, most) };
        unsafe { h.dealloc(from_the_rest, small) };
        assert_eq!(h.live_lockfree(), 0);
        free_region(region, layout);
    }

    #[test]
    fn a_table_that_grows_past_the_classes_survives_a_thousand_collections_on_a_96k_arena() {
        let (h, region, layout) = heap(96 * 1024);
        let table = Layout::from_size_align(24 * 1024, 8).unwrap();
        let grown = Layout::from_size_align(40 * 1024, 8).unwrap();
        let mut watermark = None;
        for cycle in 0..1_000 {
            let small = unsafe { h.alloc(table) };
            assert!(!small.is_null(), "cycle {cycle}: the table");
            let bigger = unsafe { h.alloc(grown) };
            assert!(!bigger.is_null(), "cycle {cycle}: the table, grown");
            unsafe { h.dealloc(small, table) };
            unsafe { h.dealloc(bigger, grown) };
            assert_eq!(h.live_lockfree(), 0, "cycle {cycle}: live returns to its steady value");
            let carved = h.carved_lockfree();
            assert_eq!(*watermark.get_or_insert(carved), carved, "cycle {cycle}: nothing more carved");
        }
        free_region(region, layout);
    }

    #[test]
    fn a_seeded_mix_either_side_of_the_classes_never_overlaps_or_corrupts_a_block() {
        let (h, region, layout) = heap(512 * 1024);
        let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let pattern = |tag: u32, offset: usize| (tag as usize).wrapping_mul(31).wrapping_add(offset) as u8;
        let checked = |size: usize| {
            (0..size).filter(move |offset| *offset < 64 || *offset + 64 >= size || offset % 61 == 0)
        };
        let mut live: Vec<(*mut u8, Layout, u32)> = Vec::new();
        let mut large_reused = 0usize;
        for step in 0..20_000u32 {
            let roll = next();
            if !live.is_empty() && (roll % 3 == 0 || live.len() > 40) {
                let index = (next() as usize) % live.len();
                let (ptr, block, tag) = live.swap_remove(index);
                for offset in checked(block.size()) {
                    let byte = unsafe { *ptr.add(offset) };
                    assert_eq!(byte, pattern(tag, offset), "step {step}: a live block was overwritten");
                }
                unsafe { h.dealloc(ptr, block) };
                continue;
            }
            let size = match roll % 4 {
                0 | 1 => 16 + (roll >> 8) as usize % 2_032,
                2 => 2_048 + (roll >> 8) as usize % 18_432,
                _ => 20_481 + (roll >> 8) as usize % 45_056,
            };
            let block = Layout::from_size_align(size, 8).unwrap();
            let carved_before = h.carved_lockfree();
            let ptr = unsafe { h.alloc(block) };
            if ptr.is_null() {
                continue;
            }
            if size > 20_480 && h.carved_lockfree() == carved_before {
                large_reused += 1;
            }
            let (start, end) = (ptr as usize, ptr as usize + size);
            for &(other, other_block, _) in &live {
                let (other_start, other_end) = (other as usize, other as usize + other_block.size());
                assert!(
                    end <= other_start || other_end <= start,
                    "step {step}: a new block overlaps a live one"
                );
            }
            for offset in 0..size {
                unsafe { *ptr.add(offset) = pattern(step, offset) };
            }
            live.push((ptr, block, step));
        }
        assert!(large_reused > 0, "the run reused freed large blocks, so it tested that path");
        for (ptr, block, tag) in live.drain(..) {
            for offset in checked(block.size()) {
                let byte = unsafe { *ptr.add(offset) };
                assert_eq!(byte, pattern(tag, offset), "at the end: a live block was overwritten");
            }
            unsafe { h.dealloc(ptr, block) };
        }
        assert_eq!(h.live_lockfree(), 0, "everything freed, nothing counted live");
        free_region(region, layout);
    }
}
