//! The PRIMASK-based `critical_section` provider a Cortex-M firmware registers, in a file of
//! its own so that a binary can take it independently of which carrier it uses.
//!
//! # Who needs one
//!
//! Any firmware with state shared between an interrupt handler and the code it interrupts, where
//! the two are not separable into a single-producer/single-consumer pair. The USB bus driver
//! serializes its register access this way, and so does the pin-change event queue, whose handler
//! displaces the oldest entry when it is full and therefore moves an index the reader also moves.
//!
//! # Include it exactly once per binary
//!
//! `critical_section::set_impl!` registers the process-wide implementation, so a second one in the
//! same binary is a duplicate-symbol link error. No other file in this directory registers one, so
//! a firmware needs this include exactly when it needs a critical section at all.
//!
//! Acquire masks interrupts, remembering whether they were on; release unmasks only when the
//! (outermost) acquiring section was the one that masked them, so nesting is safe.


struct BareCs;
critical_section::set_impl!(BareCs);

unsafe impl critical_section::Impl for BareCs {
    unsafe fn acquire() -> critical_section::RawRestoreState {
        let primask: u32;
        unsafe {
            core::arch::asm!("mrs {}, PRIMASK", out(reg) primask, options(nomem, nostack, preserves_flags));
            core::arch::asm!("cpsid i", options(nomem, nostack, preserves_flags));
        }
        core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
        primask & 1 == 0 // interrupts WERE enabled -> this section re-enables them on release
    }

    unsafe fn release(was_enabled: critical_section::RawRestoreState) {
        core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
        if was_enabled {
            unsafe { core::arch::asm!("cpsie i", options(nomem, nostack, preserves_flags)) };
        }
    }
}
