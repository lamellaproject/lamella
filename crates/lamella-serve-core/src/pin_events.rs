//! The board-facing half of the pin-change interrupt seam: the queue a BSP's interrupt handler
//! pushes into, and the `lamella_isr_notify` symbol it calls. One implementation, shared by `#[path]`
//! with every firmware that includes it, because its statics have to be per-linked-binary.
//!
//! # The split, and why the queue is not simply here
//!
//! The rule -- ordering, capacity, what overflow means, how many events one drain takes -- is
//! `lamella-pin-events`, and only the storage is here. That is not tidiness: an ahead-of-time
//! compiled image links an archive with no writable-data segment, where a `static` resolves into
//! `.text` and writing it corrupts code, so that tier keeps the same queue in its caller-provided
//! RAM region instead. Two places to put it, one implementation of the arithmetic. The live debug
//! window takes the same split for the same reason.
//!
//! # What a BSP does with it
//!
//! Include this module, install [`source`] on the `Vm` at boot, and call `lamella_isr_notify` from
//! the interrupt handler once per bound line found pending. The interpreter drains the queue between
//! scheduler quanta and calls the managed dispatcher, which turns the token back into the pin and
//! the delegates a driver registered for it.
//!
//! # A critical section, and why a lock-free ring does not express this
//!
//! The queue drops its oldest entry when full, so the producer -- the interrupt handler -- moves the
//! index the consumer reads. That is exactly what a single-producer/single-consumer ring is defined
//! not to do, so the mutual exclusion is real rather than avoidable. On these parts it is an
//! interrupt disable spanning a handful of instructions.
//!
//! Including this file therefore requires the binary to register a `critical_section` implementation,
//! and a binary that does not fails to link rather than producing an image that appears to serve
//! interrupts and quietly corrupts a queue. Same deliberate choice as the board-supplied Link
//! carrier symbols.

use lamella_cil_runtime::{PinEvent, PinEventSource};
use lamella_pin_events::PinEventQueue;

/// How many events the board may fall behind by before the oldest is dropped.
///
/// Sized for a burst rather than for a backlog: a mechanical switch bounces for tens of
/// milliseconds and the drain runs every scheduler quantum, so this holds one contact bounce
/// comfortably. A deeper queue would not help a program that is not draining -- it would only make
/// the loss report arrive later, and the loss is the thing worth reporting promptly.
const CAPACITY: usize = 16;

static PIN_EVENT_QUEUE: critical_section::Mutex<core::cell::RefCell<PinEventQueue<CAPACITY>>> =
    critical_section::Mutex::new(core::cell::RefCell::new(PinEventQueue::new()));

/// Records a pin-change interrupt: `token` is the board's own identifier for the line it armed, and
/// `level` is the pad as the handler read it, non-zero for high.
///
/// # What this requires of its caller
///
/// Clear the line's pending flag, then read the pad. Whichever step comes first, a second edge
/// can land between the two, and the orders differ in what that costs. Read first, and the second
/// edge finds the flag still raised from the first, so the clear erases it: nothing re-enters, and
/// the last level reported is one the pad has already left, until some later edge happens to
/// correct it. Clear first, and the second edge raises the flag again, so the read already sees the
/// level after it and the handler is entered again to report that level a second time. The worst
/// case is a duplicate event, and once the pad stops moving the last event reported matches it, as
/// long as the level reading is never staler than the flag.
///
/// And the level must come from something that is actually sampling the pad. Reading the port's
/// input register requires that pad's input buffer to be enabled, which on some parts is a bit in a
/// different register from everything else a GPIO driver touches, and clearing the direction bit
/// does not set it. A driver that arms a line and leaves it off calls this with `level` 0 on every
/// edge, forever, and the application sees a button that is pressed and never released. It fails
/// as data rather than as an error: nothing traps, nothing is missing, and the event stream is well
/// formed and describes a world that does not exist.
///
/// Some interrupt controllers answer the pin state themselves, and reading the level from one of
/// those avoids the obligation entirely rather than satisfying it. Where that register describes a
/// debounced state, a handler not using the debouncer should satisfy itself that is the reading it
/// wants.
///
/// # Why it answers nothing
///
/// A handler cannot act on a refusal: it has no caller to report to, nowhere to retry and no room
/// to block. So loss is recorded in the queue and read once at drain time, where somebody can do
/// something about it.
///
/// # It may be called several times from one entry
///
/// One vector commonly serves many lines, so a handler is entered without knowing which pad fired
/// and answers every bound line it finds pending.
///
/// # Safety
/// Nothing is required of the caller beyond a context in which a critical section may be taken. It
/// is `unsafe` only because it is an `extern "C"` symbol reached from a vector table.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lamella_isr_notify(token: u32, level: u8) {
    critical_section::with(|cs| {
        PIN_EVENT_QUEUE
            .borrow_ref_mut(cs)
            .push(PinEvent { token, level: level != 0 });
    });
}

fn next_pin_event() -> Option<PinEvent> {
    critical_section::with(|cs| PIN_EVENT_QUEUE.borrow_ref_mut(cs).pop())
}

fn pin_events_overflowed() -> bool {
    critical_section::with(|cs| PIN_EVENT_QUEUE.borrow_ref_mut(cs).drain_overflowed())
}

/// The queue above in the shape the interpreter takes it. Install it on the `Vm` at boot, before
/// any program runs: until it is installed the interpreter attempts no drain at all, and queued events
/// simply accumulate until one overflows.
#[must_use]
pub fn source() -> PinEventSource {
    PinEventSource { next: next_pin_event, drain_overflowed: pin_events_overflowed }
}
