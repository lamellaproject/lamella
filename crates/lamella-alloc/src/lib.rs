#![cfg_attr(not(test), no_std)]
#![allow(unsafe_code)]

//! A bump allocator for the no-GC device profile.

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::ptr;

/// A bump allocator over an `N`-byte buffer: it allocates by advancing a cursor and
/// never frees an individual block (`dealloc` is a no-op), matching the no-GC arena.
///
/// Install one as the `#[global_allocator]`; pick `N` to fit the target's RAM budget.
pub struct BumpAllocator<const N: usize> {
    /// The backing storage the cursor advances through.
    heap: UnsafeCell<[u8; N]>,
    /// The offset of the next free byte within `heap`.
    next: UnsafeCell<usize>,
}

unsafe impl<const N: usize> Sync for BumpAllocator<N> {}

impl<const N: usize> BumpAllocator<N> {
    /// Creates an empty allocator with a zeroed `N`-byte buffer.
    #[must_use]
    pub const fn new() -> BumpAllocator<N> {
        BumpAllocator {
            heap: UnsafeCell::new([0; N]),
            next: UnsafeCell::new(0),
        }
    }

    /// The number of bytes handed out so far (including alignment padding).
    #[must_use]
    pub fn used(&self) -> usize {
        critical_section(|| unsafe { *self.next.get() })
    }

    /// The buffer's total size in bytes (`N`).
    #[must_use]
    pub const fn capacity(&self) -> usize {
        N
    }

    /// Frees *all* allocations at once by rewinding the cursor.
    ///
    /// # Safety
    /// The caller must ensure no allocation handed out by this allocator is still in
    /// use -- every outstanding pointer is invalidated.
    pub unsafe fn reset(&self) {
        critical_section(|| unsafe { *self.next.get() = 0 });
    }
}

impl<const N: usize> Default for BumpAllocator<N> {
    fn default() -> BumpAllocator<N> {
        BumpAllocator::new()
    }
}

unsafe impl<const N: usize> GlobalAlloc for BumpAllocator<N> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        critical_section(|| {
            let base = self.heap.get().cast::<u8>() as usize;
            let next = self.next.get();
            let cursor = base + unsafe { *next };
            let aligned = align_up(cursor, layout.align());
            match aligned.checked_add(layout.size()) {
                Some(end) if end <= base + N => {
                    unsafe { *next = end - base };
                    aligned as *mut u8
                }
                _ => ptr::null_mut(),
            }
        })
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {
    }
}

/// Rounds `address` up to the next multiple of `align` (a power of two, per `Layout`).
fn align_up(address: usize, align: usize) -> usize {
    (address + align - 1) & !(align - 1)
}

/// Runs `body` with interrupts disabled on a Cortex-M target, restoring the prior
/// interrupt state afterward (Cortex-M0 has no atomic CAS, so this is how a bump stays
/// atomic against interrupt handlers that allocate).
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

/// Off-target (the host, for tests) execution is single-threaded, so no actual
/// critical section is needed.
#[cfg(not(target_arch = "arm"))]
fn critical_section<R>(body: impl FnOnce() -> R) -> R {
    body()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bumps_aligned_blocks_within_the_buffer() {
        let allocator = BumpAllocator::<256>::new();
        assert_eq!(allocator.capacity(), 256);
        assert_eq!(allocator.used(), 0);

        let layout = Layout::from_size_align(8, 8).unwrap();
        let a = unsafe { allocator.alloc(layout) };
        let b = unsafe { allocator.alloc(layout) };
        assert!(!a.is_null() && !b.is_null());
        assert_ne!(a, b);
        assert_eq!(a as usize % 8, 0);
        assert_eq!(b as usize, a as usize + 8);
        assert_eq!(allocator.used(), 16);
    }

    #[test]
    fn honors_larger_alignment_with_padding() {
        let allocator = BumpAllocator::<256>::new();
        let one = unsafe { allocator.alloc(Layout::from_size_align(1, 1).unwrap()) };
        let aligned = unsafe { allocator.alloc(Layout::from_size_align(4, 64).unwrap()) };
        assert!(!one.is_null() && !aligned.is_null());
        assert_eq!(aligned as usize % 64, 0);
    }

    #[test]
    fn returns_null_when_the_buffer_is_exhausted() {
        let allocator = BumpAllocator::<64>::new();
        let big = unsafe { allocator.alloc(Layout::from_size_align(48, 1).unwrap()) };
        assert!(!big.is_null());
        let over = unsafe { allocator.alloc(Layout::from_size_align(32, 1).unwrap()) };
        assert!(over.is_null());
        assert_eq!(allocator.used(), 48);
    }

    #[test]
    fn reset_rewinds_the_cursor() {
        let allocator = BumpAllocator::<64>::new();
        let first = unsafe { allocator.alloc(Layout::from_size_align(16, 1).unwrap()) };
        assert_eq!(allocator.used(), 16);
        unsafe { allocator.reset() };
        assert_eq!(allocator.used(), 0);
        let again = unsafe { allocator.alloc(Layout::from_size_align(16, 1).unwrap()) };
        assert_eq!(first, again);
    }
}
