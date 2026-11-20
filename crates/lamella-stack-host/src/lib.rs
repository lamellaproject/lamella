//! Native stack bounds of the calling thread.

#![no_std]

use core::ops::Range;

/// Returns the current thread's stack range, excluding Windows exception-guarantee space.
#[must_use]
pub fn current_bounds() -> Option<Range<usize>> {
    platform_bounds()
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn platform_bounds() -> Option<Range<usize>> {
    use windows_sys::Win32::System::Threading::{
        GetCurrentThreadStackLimits, SetThreadStackGuarantee,
    };

    let (mut low, mut high) = (0, 0);
    let mut guarantee = 0;
    // The limits are written to distinct machine words. A zero request reads the thread's
    // existing exception guarantee, which is unavailable during ordinary execution.
    unsafe {
        GetCurrentThreadStackLimits(&mut low, &mut high);
        if SetThreadStackGuarantee(&mut guarantee) == 0 {
            return None;
        }
    }
    let low = low.checked_add(guarantee as usize)?;
    (low < high).then_some(low..high)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
#[allow(unsafe_code)]
fn platform_bounds() -> Option<Range<usize>> {
    let mut attr = core::mem::MaybeUninit::uninit();
    let mut base = core::ptr::null_mut();
    let mut size = 0;
    // A successful query initializes the attributes. Release them after reading the stack.
    unsafe {
        if libc::pthread_getattr_np(libc::pthread_self(), attr.as_mut_ptr()) != 0 {
            return None;
        }
        let result = libc::pthread_attr_getstack(attr.as_ptr(), &mut base, &mut size);
        libc::pthread_attr_destroy(attr.as_mut_ptr());
        if result != 0 {
            return None;
        }
    }
    let low = base as usize;
    Some(low..low.checked_add(size)?)
}

#[cfg(target_vendor = "apple")]
#[allow(unsafe_code)]
fn platform_bounds() -> Option<Range<usize>> {
    // Both queries use the live thread's own identifier. The stack address is its upper end.
    let (high, size) = unsafe {
        let thread = libc::pthread_self();
        (
            libc::pthread_get_stackaddr_np(thread) as usize,
            libc::pthread_get_stacksize_np(thread),
        )
    };
    Some(high.checked_sub(size)?..high)
}

#[cfg(not(any(
    windows,
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple"
)))]
fn platform_bounds() -> Option<Range<usize>> {
    None
}
