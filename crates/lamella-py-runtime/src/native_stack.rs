//! Bounds native interpreter reentry independently of the heap-backed Python frame stack.

use alloc::rc::Rc;
use core::cell::Cell;

use crate::trap::Trap;

#[derive(Debug, Default)]
pub(crate) struct NativeStack {
    active_floor: Rc<Cell<Option<usize>>>,
    pub(crate) floor: Option<usize>,
}

#[cfg(any(
    windows,
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple"
))]
fn outer_floor(address: usize, configured: Option<usize>) -> Result<usize, Trap> {
    let bounds = lamella_stack_host::current_bounds().ok_or(Trap::RecursionError)?;
    if !bounds.contains(&address) {
        return Err(Trap::RecursionError);
    }
    // Reserve up to half the thread's usable stack for unchecked helpers and error handling.
    // Unoptimized frames need a larger allowance on threads that have room for it.
    let reserve = if cfg!(debug_assertions) {
        256 * 1024
    } else {
        16 * 1024
    };
    Ok(bounds
        .start
        .saturating_add(reserve.min(bounds.len() / 2))
        .max(configured.unwrap_or(0)))
}

#[cfg(not(any(
    windows,
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple"
)))]
fn outer_floor(address: usize, configured: Option<usize>) -> Result<usize, Trap> {
    // An embedder without stack bounds uses a budget relative to the outermost native entry.
    let budget = if cfg!(debug_assertions) {
        512 * 1024
    } else {
        24 * 1024
    };
    Ok(configured.unwrap_or_else(|| address.saturating_sub(budget)))
}

impl NativeStack {
    // Keep the address in this function's own frame so every entry measures at the same boundary.
    #[inline(never)]
    pub(crate) fn enter(&self) -> Result<NativeCall, Trap> {
        let marker = 0_u8;
        let address = core::ptr::addr_of!(marker) as usize;
        let saved = self.active_floor.get();
        let floor = match saved {
            Some(floor) => floor,
            None => outer_floor(address, self.floor)?,
        };
        if address < floor {
            return Err(Trap::RecursionError);
        }
        self.active_floor.set(Some(floor));
        Ok(NativeCall {
            state: Rc::clone(&self.active_floor),
            saved,
        })
    }
}

pub(crate) struct NativeCall {
    state: Rc<Cell<Option<usize>>>,
    saved: Option<usize>,
}

impl Drop for NativeCall {
    fn drop(&mut self) {
        self.state.set(self.saved);
    }
}
