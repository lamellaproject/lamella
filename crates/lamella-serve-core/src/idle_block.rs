//! The device side of the runtime's block point: where a board idles the processor while no
//! thread is runnable.
//!
//! `lamella-reactor::block_point` is the single block point for both tiers, and it already computes
//! the nearest deadline over sleepers, socket waits, timed `Monitor.Wait`s and receive timeouts. What
//! it does at the end of that arithmetic is call `ReactorEnv::sleep_millis`. A board that does
//! nothing more there waits in a busy loop, keeping the processor at full power with no work to do.
//!
//! This module is what a board calls instead. One implementation, shared by `#[path]` like
//! `systick_clock.rs` next to it, for the same reason: the thing that goes wrong here is having two.
//!
//! # The lost-wakeup race, which is the whole difficulty
//!
//! Nothing a green thread does can make another one runnable while the scheduler is inside the block
//! point -- there is one core and nothing else is running. Only an interrupt can. So the sequence
//! that looks obvious is wrong:
//!
//! ```text
//!   is there work?  -- no
//!                              <- the UART IRQ fires here. Its handler runs, puts a byte in the
//!                                 ring, and returns -- so the interrupt is no longer pending.
//!   WFI            -- nothing pending, so the core sleeps
//!                              <- and the host is waiting for a reply that will never come.
//! ```
//!
//! The board is now asleep with work in hand, and it stays asleep until some unrelated interrupt
//! happens to arrive. If the host is waiting on this board, none will. The failure is a hang, it
//! needs an interrupt to land inside a window a few instructions wide, and it therefore reproduces
//! about once a week -- which is exactly the kind of defect that ships.
//!
//! The order that closes it masks first:
//!
//! ```text
//!   mask interrupts (cpsid i)
//!   is there work?  -- if yes, unmask and return       <- catches a handler that already ran
//!   WFI                                                <- catches one that fires from here on
//!   unmask (cpsie i)
//! ```
//!
//! Both sides of the window are covered, and by different mechanisms. A handler that ran before the
//! mask left evidence in software, and the re-check finds it. One that fires after the mask cannot
//! run -- but it does become pending, and ARM `WFI` wakes on a pending interrupt regardless of
//! PRIMASK (ARMv7-M B1.5.17, ARMv6-M B1.5.16). That is the hardware property the whole idiom rests
//! on: the mask stops the handler from running without stopping it from waking the core, so there is
//! no instant at which an arriving interrupt can be lost.
//!
//! The re-check is therefore not belt-and-braces on top of the hardware guarantee. Each covers a
//! window the other does not, and [`Decision::for_idle`] is the half that can be proved on a host.
//!
//! # What this does not do
//!
//! It does not program a wake timer, and it refuses to sleep when nothing can wake it by the
//! deadline (see [`Decision::Spin`]). The per-chip wake timer and the sleep-mode ladder belong to
//! each board's own support crate; a board that has not registered one spins exactly as it would
//! without this module, which is the only safe default -- sleeping with no way back is a worse
//! failure than spinning.

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// How deeply the core is asked to sleep, as a value the chip defines rather than a `bool`.
///
/// An open value and never a boolean, so that deeper modes stay possible. The modes past this
/// one -- stop, standby, anything that loses RAM -- are per-part ladders with per-part wake
/// sources, and this type's job is to leave them reachable rather than to implement them. A `bool`
/// here would have to be widened at every implementor at once, and if it ever reached the AOT
/// scheduler's C ABI, widening it would be a breaking change.
///
/// Exactly one mode is defined; the deeper ones belong to each part's own support code.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
#[non_exhaustive]
pub enum SleepMode {
    /// The core stops; every clock, every peripheral and all RAM keep running. Any enabled interrupt
    /// wakes it, and execution resumes at the instruction after the `WFI`. This is plain ARM `WFI`
    /// with `SLEEPDEEP` clear, and it is the only mode whose behavior is architectural rather than
    /// per-part -- which is why it is the one defined here.
    CoreOnly = 0,
}

/// What kind of event a registered wake source can produce, because the difference decides whether a
/// deadline can be honored.
///
/// A timer source is a distinct kind and not a flavor of interrupt, and that distinction is what
/// makes a watchdog expressible: a poll-only peripheral that must be serviced every N ms registers a
/// timer source, and the block point may then not sleep past that interval. A registry that only
/// understood "some interrupt might arrive" could not represent "something must happen by time T",
/// and the watchdog could not be described in it at all.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum WakeSource {
    /// An interrupt that fires when something external happens -- a carrier byte, a pin edge. It can
    /// end a sleep, but it cannot promise to end one by any particular time.
    Interrupt,
    /// A timer that will fire by a deadline the board can program. This is the only kind that lets a
    /// sleep honor a `Thread.Sleep` deadline, and the only kind a periodic obligation (a watchdog
    /// kick, a poll-only sensor) can be expressed as.
    Timer,
}

/// What the block point should do with an idle scheduler.
///
/// Split into three rather than "sleep or do not", because the two non-sleeping answers have
/// different causes and different remedies: [`Runnable`](Self::Runnable) is the ordinary hot path and
/// [`Spin`](Self::Spin) is a board that cannot be woken by a deadline and is burning power because of
/// it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Decision {
    /// Work is already pending -- an interrupt handler left something behind. Do not sleep; the
    /// scheduler has something to do.
    Runnable,
    /// Nothing is pending and a timer can wake the core by the deadline: sleep the core.
    Sleep(SleepMode),
    /// Nothing is pending, but no registered source can wake the core by the deadline. Spin, because
    /// a `Thread.Sleep(100)` that returned after an hour -- or never -- is a worse failure than a
    /// warm chip.
    ///
    /// A board reaches this by having no timer wake source registered, and it is the same busy
    /// spin such a board performs without this module, so it adds no new hazard.
    Spin,
}

impl Decision {
    /// The block point's decision, as a pure function of what the board knows.
    ///
    /// Split out from the instruction sequence so it can be proved on a host with no chip, which
    /// matters more here than usual: the sequence below is four instructions and the difficulty is
    /// entirely in when each of them runs relative to an interrupt.
    ///
    /// - `work_pending` -- whether an interrupt handler has already left something for the scheduler.
    ///   Read after interrupts are masked, and the whole race is in that word.
    /// - `deadline_wake` -- whether a [`WakeSource::Timer`] is registered that can fire by the
    ///   deadline. `false` forbids the sleep rather than shortening it: the block point cannot
    ///   shorten a wait it has no way to end.
    /// - `has_deadline` -- whether there is a deadline at all. With none (every parked thread is
    ///   waiting on an interrupt, not on time) a plain interrupt source is enough, because there is
    ///   no time the sleep has to end by.
    #[must_use]
    pub const fn for_idle(work_pending: bool, deadline_wake: bool, has_deadline: bool) -> Self {
        if work_pending {
            Self::Runnable
        } else if has_deadline && !deadline_wake {
            Self::Spin
        } else {
            Self::Sleep(SleepMode::CoreOnly)
        }
    }
}

/// The board's answer to "has an interrupt left work for the scheduler?".
///
/// `None` until a board registers one, and that default is why an unregistered board never sleeps:
/// [`work_pending`] answers `true`, which is the conservative reading. A board that has not said
/// how it would know must not be assumed to have nothing to do -- getting that backwards produces
/// exactly the hang this module exists to prevent, on every board that had not opted in.
static WORK_PENDING: AtomicU32 = AtomicU32::new(0);
/// Whether a registered wake source can end a sleep at a deadline ([`WakeSource::Timer`]).
static HAS_TIMER_WAKE: AtomicBool = AtomicBool::new(false);
/// Total milliseconds this board has spent with the core stopped.
///
/// Recorded because automatic load-based scaling needs it and nothing else would ever produce it.
/// Two halves rather than an `AtomicU64`, which thumbv6m lacks; this file is host-and-device, and
/// the device statics below are `u32` for that reason. See [`slept_millis`].
static SLEPT_LOW: AtomicU32 = AtomicU32::new(0);
static SLEPT_HIGH: AtomicU32 = AtomicU32::new(0);

/// Register how this board knows an interrupt has left work for the scheduler.
///
/// For a firmware that takes programs from a host, that is "the carrier has buffered bytes" -- an
/// ISR-filled ring the transport drains. The predicate is read with interrupts masked, so it must
/// not itself block, allocate or wait; reading one atomic or one ring index is the whole of what
/// belongs here.
///
/// Until a board calls this the block point never sleeps. That is deliberate: see [`WORK_PENDING`].
pub fn set_work_predicate(predicate: fn() -> bool) {
    WORK_PENDING.store(predicate as usize as u32, Ordering::Relaxed);
}

/// Declare that this board can be woken by `source`.
///
/// A [`WakeSource::Timer`] is what lets the block point honor a deadline; an interrupt source alone
/// permits sleeping only when nothing is waiting on time. A board registers a timer source once it
/// programs a wake timer of its own.
pub fn add_wake_source(source: WakeSource) {
    if matches!(source, WakeSource::Timer) {
        HAS_TIMER_WAKE.store(true, Ordering::Relaxed);
    }
}

/// Whether an interrupt has left work for the scheduler, per the board's registered predicate.
///
/// `true` when no predicate is registered -- a board that has not said how it would know is treated
/// as having something to do, which costs power and cannot hang.
#[must_use]
pub fn work_pending() -> bool {
    let raw = WORK_PENDING.load(Ordering::Relaxed);
    if raw == 0 {
        return true;
    }
    let predicate: fn() -> bool = unsafe { core::mem::transmute(raw as usize) };
    predicate()
}

/// Milliseconds this board has spent with the core stopped, since boot.
#[must_use]
pub fn slept_millis() -> u64 {
    (u64::from(SLEPT_HIGH.load(Ordering::Relaxed)) << 32) | u64::from(SLEPT_LOW.load(Ordering::Relaxed))
}

/// Add `millis` to the slept total.
fn account(millis: u64) {
    let low = SLEPT_LOW.load(Ordering::Relaxed);
    let bumped = low.wrapping_add(millis as u32);
    if bumped < low {
        SLEPT_HIGH.store(SLEPT_HIGH.load(Ordering::Relaxed).wrapping_add(1), Ordering::Relaxed);
    }
    SLEPT_LOW.store(bumped, Ordering::Relaxed);
}

/// Stop the core until an interrupt arrives, or return at once if there is already work.
///
/// `millis` is how long the scheduler wanted to wait and is used to account the sleep; the wake
/// itself comes from an interrupt. `has_deadline` says whether that wait is a real deadline (a
/// `Thread.Sleep`, a timed wait) or merely an upper bound on an interrupt wait -- it is what decides
/// whether a board with no timer wake source may sleep at all.
///
/// Returns what it decided, so a caller can spin the remainder itself when the answer was
/// [`Decision::Spin`]. The four instructions are ordered as the module header describes and the
/// order is the entire correctness argument: mask, then re-check, then `WFI`, then unmask.
pub fn block(millis: u64, has_deadline: bool) -> Decision {
    #[cfg(target_os = "none")]
    {
        let primask: u32;
        unsafe {
            core::arch::asm!("mrs {}, PRIMASK", out(reg) primask, options(nomem, nostack, preserves_flags));
            core::arch::asm!("cpsid i", options(nomem, nostack, preserves_flags));
        }
        let decision = Decision::for_idle(
            work_pending(),
            HAS_TIMER_WAKE.load(Ordering::Relaxed),
            has_deadline,
        );
        if let Decision::Sleep(_) = decision {
            unsafe { core::arch::asm!("wfi", options(nomem, nostack, preserves_flags)) };
            account(millis);
        }
        if primask & 1 == 0 {
            unsafe { core::arch::asm!("cpsie i", options(nomem, nostack, preserves_flags)) };
        }
        decision
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = millis;
        Decision::for_idle(work_pending(), HAS_TIMER_WAKE.load(Ordering::Relaxed), has_deadline)
    }
}
