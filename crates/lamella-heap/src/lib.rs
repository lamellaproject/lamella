//! A reclaiming O(1) segregated-fit allocator for the constrained serve tiers.

#![no_std]
#![allow(unsafe_code)]
#![forbid(unsafe_op_in_unsafe_fn)]

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, Ordering};

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
///   28..40  : 1536, 2048, ..., 8192   (512-byte steps... widening)
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
const fn class_for(size: usize) -> Option<usize> {
    let mut index = 0;
    while index < CLASS_COUNT {
        if class_size(index) >= size {
            return Some(index);
        }
        index += 1;
    }
    None
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
}

impl Heap {
    /// An empty heap; call [`Heap::init`] with the region before first use.
    pub const fn empty() -> Heap {
        Heap {
            base: core::ptr::null_mut(),
            end: core::ptr::null_mut(),
            frontier: core::ptr::null_mut(),
            classes: [core::ptr::null_mut(); CLASS_COUNT],
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
    }

    /// The block size a request of `layout` takes: rounded to the class granularity, and to
    /// the alignment when it exceeds [`MIN_BLOCK`] (an over-aligned request bump-carves
    /// aligned, so its class rounding also honors the alignment).
    fn block_size(layout: Layout) -> usize {
        layout.size().max(MIN_BLOCK).max(layout.align())
    }

    /// Allocates for `layout`, or null on exhaustion. O(1): a class hit pops its free list,
    /// a miss carves the class's fixed size from the bump frontier (aligned when the
    /// request over-aligns).
    fn alloc(&mut self, layout: Layout) -> *mut u8 {
        let need = Self::block_size(layout);
        let align = layout.align().max(MIN_BLOCK);
        match class_for(need) {
            Some(class) => {
                if align <= MIN_BLOCK {
                    let head = self.classes[class];
                    if !head.is_null() {
                        self.classes[class] = unsafe { (*head).next };
                        return head.cast();
                    }
                }
                self.carve(class_size(class), align)
            }
            None => self.carve(need, align),
        }
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

    /// Returns a block to its size class's free list (O(1)). A block beyond every class
    /// (an unreclaimed large carve) is dropped -- it is never reused, matching the bump
    /// behavior for that rare case.
    ///
    /// # Safety
    /// `ptr` must be a block this heap returned for a `layout`-compatible request, not yet
    /// freed.
    unsafe fn dealloc(&mut self, ptr: *mut u8, layout: Layout) {
        let need = Self::block_size(layout);
        let align = layout.align().max(MIN_BLOCK);
        if align > MIN_BLOCK {
            return;
        }
        if let Some(class) = class_for(need) {
            let node = ptr.cast::<FreeNode>();
            unsafe { (*node).next = self.classes[class] };
            self.classes[class] = node;
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
}

unsafe impl Send for Heap {}

/// A spin-locked [`Heap`] usable as a `#[global_allocator]`. The lock is a plain spin
/// (uncontended on the single-core serve; no allocation happens in interrupt context).
pub struct LockedHeap {
    locked: AtomicBool,
    heap: UnsafeCell<Heap>,
    /// A lock-free mirror of the heap's carved high-water (see [`Heap::carved`]), refreshed
    /// after each allocation. Read WITHOUT the lock -- for a diagnostic or a panic-path
    /// number that must never risk the lock (an alloc-error panic fires with the lock
    /// already released, but a panic from elsewhere could hold it).
    carved: core::sync::atomic::AtomicUsize,
}

impl LockedHeap {
    /// An empty locked heap; [`LockedHeap::init`] points it at a region before first use.
    #[must_use]
    pub const fn empty() -> LockedHeap {
        LockedHeap {
            locked: AtomicBool::new(false),
            heap: UnsafeCell::new(Heap::empty()),
            carved: core::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Points the heap at the `size`-byte region at `base` (see [`Heap::init`]).
    ///
    /// # Safety
    /// As [`Heap::init`]: `base`/`size` describe a valid, exclusively-owned region.
    pub unsafe fn init(&self, base: *mut u8, size: usize) {
        self.with(|heap| unsafe { heap.init(base, size) });
    }

    /// Runs `body` holding the lock.
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
}

unsafe impl Sync for LockedHeap {}

unsafe impl GlobalAlloc for LockedHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let (ptr, carved) = self.with(|heap| (heap.alloc(layout), heap.carved()));
        self.carved.store(carved, Ordering::Relaxed);
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if let Some(ptr) = NonNull::new(ptr) {
            self.with(|heap| unsafe { heap.dealloc(ptr.as_ptr(), layout) });
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
}
