//! The monotonic millisecond clock an nRF51 firmware gives the
//! interpreter, folded from the chip's TIMER0. The second counterpart of `systick_clock.rs` for a
//! part that has no SysTick at all -- `systimer_clock.rs` is the first.
//!
//! # Why a board needs this at all
//!
//! The interpreter reads a clock to compute a `Thread.Sleep` deadline. Without one it computes a
//! deadline that has already passed and returns at once: nothing errors, no seam reports missing,
//! and the program simply runs at the wrong speed -- which, for a delay, is the whole of its
//! behavior. It is the failure mode of the API a beginner reaches for first.
//!
//! # Why not the shared SysTick fold: the counter is not there
//!
//! ARMv6-M makes SysTick optional and the nRF51822 does not implement it: its `SYST_CSR`, `SYST_RVR`
//! and `SYST_CVR` read zero and ignore writes, so a firmware that enables SysTick has its writes
//! dropped -- not racing, not mistimed, dropped. Without this module the part has no monotonic time
//! at all, and every duration, timeout and `Thread.Sleep` on it is unbacked.
//!
//! `systick_clock::install` behaves correctly on such a part rather than being fooled by it: it
//! advertises `MONOTONIC_CLOCK` only after seeing the counter move, and it reads CVR rather than
//! CSR's ENABLE bit precisely so a part with no counter cannot pass. The capability is absent and
//! reported absent, which is the outcome that file is designed for.
//!
//! This is a per-part fact, not an ISA one. A SAM D21 XPro is also Cortex-M0+, takes the same
//! shared `systick_clock`, and does advertise a running counter. Not "ARMv6-M has no SysTick" and
//! not "Cortex-M0 has none" -- this part has none. Do not assume SysTick on a part nobody has read.
//!
//! # What TIMER0 is, and why the fold is smaller than SysTick's but bigger than SYSTIMER's
//!
//! TIMER0 in `BITMODE = 32Bit` with `PRESCALER = 4` is a 32-bit up counter at `16 MHz / 2^4` = 1 MHz
//! exactly, so one tick is one microsecond (nRF51 Series RM v3.0 sec 19.1, `fTIMER = 16 MHz /
//! 2^PRESCALER`, and Table 146's `32Bit = 3`). It is read by writing its `CAPTURE[0]` task and
//! reading `CC[0]`, which latches the count -- there is no torn read to defend against, unlike the
//! 52-bit SYSTIMER behind a 32-bit bus.
//!
//! So this file sits between its two siblings. `systimer_clock` needs no fold at all: 52 bits at
//! 16 MHz runs about nine years, so every reading is an absolute division of a free-running counter.
//! 32 bits at 1 MHz wraps every 71.6 minutes, which is inside an ordinary uptime, so a wrap has
//! to be resolved and this module does keep the running total that makes it possible.
//!
//! It is still simpler than the SysTick fold in the way that matters. That one folds a 24-bit down
//! counter and has to carry a sub-millisecond remainder, because discarding one per reading would
//! lose time in proportion to how often the clock is read. Here the total is kept in microseconds
//! and divided only on the way out, so the truncation happens once at the boundary, never
//! accumulates, and there is no remainder to lose.
//!
//! One wrap is resolved between two readings; more than one is not, and is lost. `sleep_ms` spins
//! on `now_ms`, and the firmware's main loop reads it besides, so the only way to lose a wrap is to
//! leave the board computing for 71 minutes without ever asking the time. Deadlines are relative,
//! so that costs accuracy in the absolute wall clock rather than in any individual sleep.
//!
//! # Exactly one owner of TIMER0
//!
//! The SysTick fold's documentation is mostly about a hazard this module has too: the last reading
//! is state, and two folds over one counter steal each other's ticks, producing something that
//! still counts up, still never goes backwards, and drifts. This module holds the only fold, and its
//! statics are per-linked-binary, so including it is what enforces that.
//!
//! It also has to be the only thing that configures the timer: a `CLEAR` issued after this clock has
//! installed rewinds the counter underneath the fold, and the difference reads as a single
//! 71-minute jump forward.

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// TIMER0's base, and the registers this clock touches (nRF51 Series RM v3.0, sec 19.1's register
/// table: `MODE 0x504`, `BITMODE 0x508`, `PRESCALER 0x510`, `CC[0] 0x540`, `CAPTURE[0] 0x040`).
const TIMER0: usize = 0x4000_8000;
const TIMER0_START: usize = TIMER0 + 0x000;
const TIMER0_CLEAR: usize = TIMER0 + 0x00c;
const TIMER0_CAPTURE0: usize = TIMER0 + 0x040;
const TIMER0_MODE: usize = TIMER0 + 0x504;
const TIMER0_BITMODE: usize = TIMER0 + 0x508;
const TIMER0_PRESCALER: usize = TIMER0 + 0x510;
const TIMER0_CC0: usize = TIMER0 + 0x540;

/// `MODE = Timer` (count the peripheral clock, not an external event), `BITMODE = 32Bit`,
/// `PRESCALER = 4` for 16 MHz / 16 = 1 MHz. RM v3.0 Table 146 and sec 19.1.
const MODE_TIMER: u32 = 0;
const BITMODE_32: u32 = 3;
const PRESCALER_1MHZ: u32 = 4;

/// Ticks per millisecond at 1 MHz. A constant rather than an [`install`] parameter, and that is the
/// interesting difference from the SysTick fold: that scale is the board's core-clock rate, so a
/// scale captured before a PLL switch measures the wrong timebase for the rest of the board's life.
/// TIMER0's rate is set by this module's own PRESCALER write against a 16 MHz peripheral clock the
/// firmware does not change, so there is no number for a caller to get wrong.
const TICKS_PER_MS: u64 = 1_000;

/// The last counter value this fold consumed -- the state that makes a second fold destructive.
static LAST: AtomicU32 = AtomicU32::new(0);
/// The microsecond total, as two halves: this part is Cortex-M0 and has no 64-bit atomics, and
/// there is a single consumer.
static MICROS_LOW: AtomicU32 = AtomicU32::new(0);
static MICROS_HIGH: AtomicU32 = AtomicU32::new(0);
/// Whether [`install`] observed the counter moving. False until it does, so a part whose TIMER0 is
/// unclocked leaves it clear -- the bit is earned by a positive observation rather than assumed from
/// the absence of a complaint, the same rule the SysTick fold follows.
static RUNNING: AtomicBool = AtomicBool::new(false);

fn read_register(address: usize) -> u32 {
    unsafe { core::ptr::read_volatile(address as *const u32) }
}

fn write_register(address: usize, value: u32) {
    unsafe { core::ptr::write_volatile(address as *mut u32, value) }
}

/// Latch the counter and read it. One microsecond per tick.
fn counter() -> u32 {
    write_register(TIMER0_CAPTURE0, 1);
    read_register(TIMER0_CC0)
}

/// How many times [`install`] reads the counter before concluding it is not moving.
///
/// One read pair would do on paper -- the counter advances every microsecond and a capture plus a
/// load is several cycles. The loop is for the case that is not on paper: a debugger halting the
/// core between two reads, or a peripheral clock that is not running at all.
const PROBE_READS: u32 = 64;

/// Decide the verdict from what the probe saw. Pure, so it is testable on a host with no TIMER0.
///
/// Two states and not the SysTick fold's three. That one distinguishes `Stopped` from
/// `NoReload` because a firmware that enabled SysTick and left `SYST_RVR` at its reset value hits
/// exactly the second, and "enable it" is not the fix for it. A 32-bit up counter has no reload to
/// get wrong: it either counts or it does not, and there is only one remedy.
pub(crate) const fn running_from(moved: bool) -> bool {
    moved
}

/// Configure TIMER0, start it, begin the fold from its current position, and check that it moves.
///
/// Call once at boot, and before anything else wants the time. Every register write here is made
/// rather than assumed: this part's reset values would do, and a clock that is correct only because
/// nothing upstream disturbed it is a clock that breaks with no line of this code changing.
///
/// # Why it checks, and why the answer leaves the board
///
/// The seam the interpreter gets is a plain `fn() -> u64`. A source that never advances returns a
/// perfectly well-formed answer, so nothing downstream can tell a stopped clock from a fast program:
/// a self-timing benchmark on such a board reports 0 ms for thousands of iterations of real work
/// while a checksum over that work passes. A capability that is present but dead is worse than one
/// that is absent, because absent can be reported. So the verdict is reported -- to the host
/// through `Capabilities::MONOTONIC_CLOCK`, which is advertised only when this returns true.
///
/// It does not refuse to install on a bad verdict. A board whose clock is dead is still a board a
/// host must be able to reach, and refusing here would replace a measurable wrongness with a silent
/// one. What it does do is stop [`sleep_ms`] spinning forever on a counter that will never arrive.
pub fn install() -> bool {
    write_register(TIMER0_MODE, MODE_TIMER);
    write_register(TIMER0_BITMODE, BITMODE_32);
    write_register(TIMER0_PRESCALER, PRESCALER_1MHZ);
    write_register(TIMER0_CLEAR, 1);
    write_register(TIMER0_START, 1);

    LAST.store(counter(), Ordering::Relaxed);
    MICROS_LOW.store(0, Ordering::Relaxed);
    MICROS_HIGH.store(0, Ordering::Relaxed);

    let mut previous = counter();
    let mut moved = false;
    let mut reads = 0;
    while reads < PROBE_READS && !moved {
        let current = counter();
        moved = current != previous;
        previous = current;
        reads += 1;
    }
    let running = running_from(moved);
    RUNNING.store(running, Ordering::Relaxed);
    #[cfg(all(target_os = "none", feature = "serve"))]
    lamella_runner::note_monotonic_clock(running);
    running
}

/// Microseconds between two readings of a 32-bit up counter: the plain difference, with one wrap
/// resolved by the wrapping subtraction itself.
///
/// Pure so the wrap can be proven on a host. It is the arithmetic that silently drifts a clock if it
/// is wrong.
pub(crate) const fn elapsed(last: u32, now: u32) -> u32 {
    now.wrapping_sub(last)
}

/// Milliseconds since [`install`], monotonic.
#[must_use]
pub fn now_ms() -> u64 {
    let now = counter();
    let last = LAST.load(Ordering::Relaxed);
    LAST.store(now, Ordering::Relaxed);

    let low = MICROS_LOW.load(Ordering::Relaxed);
    let bumped = low.wrapping_add(elapsed(last, now));
    if bumped < low {
        MICROS_HIGH.store(MICROS_HIGH.load(Ordering::Relaxed).wrapping_add(1), Ordering::Relaxed);
    }
    MICROS_LOW.store(bumped, Ordering::Relaxed);
    let micros = (u64::from(MICROS_HIGH.load(Ordering::Relaxed)) << 32) | u64::from(bumped);
    micros / TICKS_PER_MS
}

/// The longest single block [`sleep_ms`] will take.
///
/// The interpreter's scheduler services the carrier around an idle block, not inside it, so an
/// uncapped `Thread.Sleep(30000)` would leave a host unable to reach the board -- or take it back
/// from the app it deployed -- for thirty seconds. Uncapped, a HELLO sent to a board sleeping 30 s
/// goes unanswered.
///
/// Slicing costs nothing and changes no timing: the sleeping thread is parked on an absolute
/// deadline, so a short block simply wakes nobody and the scheduler blocks again, servicing the
/// carrier each time round, until the deadline actually passes. The sleep is neither shortened nor
/// lengthened; only the interval at which the board is reachable changes.
const MAX_BLOCK_MS: u64 = 16;

/// Wait up to [`MAX_BLOCK_MS`] of `millis`, busy-spinning on [`now_ms`] -- which also keeps the fold
/// live across the wait, so the counter cannot wrap unobserved while nothing else is reading it.
///
/// # The one thing it will not do is wait on a clock that cannot arrive
///
/// This spin's exit condition is the clock itself, so a counter [`install`] found not running would
/// hold the core here forever and the board would stop answering its host -- the loudest possible
/// consequence of the quietest possible fault. With no live clock it returns at once, degrading to
/// the no-clock-installed behavior the runtime already defines: the deadline has already passed and
/// the sleep returns immediately. The program runs at the wrong speed, which is bad, and it does so
/// while remaining reachable, which is what makes it recoverable. `Capabilities::MONOTONIC_CLOCK` is
/// what tells a host which of the two it is talking to.
///
/// It spins rather than stopping the core, matching `systimer_clock` and unlike the SysTick fold,
/// which offers each pass to a registered `idle_block::WakeSource`.
pub fn sleep_ms(millis: u64) {
    if !RUNNING.load(Ordering::Relaxed) {
        return;
    }
    let deadline = now_ms().saturating_add(millis.min(MAX_BLOCK_MS));
    while now_ms() < deadline {}
}
