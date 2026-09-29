//! The monotonic millisecond clock a firmware gives the
//! interpreter, folded from Cortex-M SysTick. One implementation, shared by `#[path]` with every
//! firmware that includes it, because the thing that goes wrong here is having two.
//!
//! # Why a board needs this at all
//!
//! The interpreter reads this clock to compute a `Thread.Sleep` deadline. Without a clock, every
//! deadline has already passed, so `Thread.Sleep` returns at once and the program runs at the wrong
//! speed, with no error reported.
//!
//! # Exactly one fold per board
//!
//! SysTick is a single 24-bit down-counter, and a monotonic clock built on it works by folding the
//! ticks elapsed since its own last reading into a running total. That last reading is state, and it
//! is consumed by whoever reads it: two folds over one counter steal each other's ticks, and the
//! result still looks like a clock -- it counts up, it never goes backwards, and it drifts. This
//! module holds the only fold; its statics are per-linked-binary, so including it is what enforces
//! the invariant.
//!
//! It reads `SYST_CVR` (the current value) and never `SYST_CSR`, and that is also load-bearing:
//! reading CSR clears `COUNTFLAG`, and a firmware's UART transport consumes that flag as its
//! partial-frame idle clock. A clock that read CSR would silently take those wraps away and break
//! frame timeouts instead.
//!
//! # What it does not promise
//!
//! `elapsed` resolves at most one counter wrap between two readings, so the total is exact only if
//! something calls [`now_ms`] at least once per wrap period. [`sleep_ms`] spins on it, so a delay is
//! accurate while it is being waited out; a long stretch of computation between two reads loses
//! whole wraps and the absolute time base runs slow. Deadlines are relative, so this costs accuracy
//! in the wall clock rather than in a sleep.
//!
//! And it is only as good as the oscillator underneath: the scale handed to [`install`] is the
//! board's core-clock rate, so a firmware running on an untrimmed RC delivers delays with that
//! oscillator's tolerance. A board whose rate is not knowable to better than its own timing needs
//! should say so rather than install a precise-looking clock over an imprecise reference.

use core::sync::atomic::{AtomicU32, Ordering};

#[path = "idle_block.rs"]
pub mod idle_block;

/// Cortex-M SysTick, at its architectural addresses (the same on every profile that has it).
const SYST_RVR: usize = 0xe000_e014;
const SYST_CVR: usize = 0xe000_e018;
/// SysTick is a 24-bit counter; both registers ignore the top byte.
const COUNT_MASK: u32 = 0x00ff_ffff;

/// The board's core-clock ticks per millisecond, from [`install`]. One is the floor rather than
/// zero, so an uninstalled or mis-scaled clock runs fast instead of dividing by zero.
static TICKS_PER_MS: AtomicU32 = AtomicU32::new(1);
/// The last counter value this fold consumed -- the state that makes a second fold destructive.
static LAST: AtomicU32 = AtomicU32::new(0);
/// Ticks left over from the last fold, below one millisecond. Without it, every reading rounds a
/// sub-millisecond remainder away and the clock loses time in proportion to how often it is read.
static SPARE: AtomicU32 = AtomicU32::new(0);
/// The 64-bit millisecond total, as two halves: the M0/M0+ parts this also serves have no 64-bit
/// atomics, and there is a single consumer.
static MILLIS_LOW: AtomicU32 = AtomicU32::new(0);
static MILLIS_HIGH: AtomicU32 = AtomicU32::new(0);
/// Whether [`install`] observed the counter moving. False until it does, so a board that never
/// installed a clock, and a board whose counter is dead, both leave it clear -- the bit is earned
/// by a positive observation rather than assumed by the absence of a complaint.
static RUNNING: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

fn read_register(address: usize) -> u32 {
    unsafe { core::ptr::read_volatile(address as *const u32) }
}

/// What [`install`] found when it looked at the counter it was about to fold.
///
/// Three states and not two, because the two failures have different remedies at the call site and a
/// single "bad clock" would send the reader to look in the wrong place.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ClockSource {
    /// The counter was seen to move. The fold has a live source.
    Running,
    /// The counter never moved across the whole probe: SysTick is not counting. Remedy: enable it
    /// (write `SYST_CSR`) before calling [`install`].
    Stopped,
    /// The reload is zero, so the counter has no range to count over and stops at zero. Remedy: set
    /// `SYST_RVR` to the wrap period this board wants. A separate state from [`Stopped`](Self::Stopped)
    /// because a firmware that enabled SysTick and left the reload at its reset value hits exactly
    /// this, and "enable it" is not the fix.
    NoReload,
}

/// How many times [`probe`] reads the counter before concluding it is not moving.
///
/// One read pair would do on paper -- SysTick decrements once per core clock and two loads of a
/// peripheral register are several cycles apart, so a live counter cannot show the same value twice.
/// The loop is for the case that is not on paper: a debugger halting the core between the two reads,
/// or a part whose SysTick is clocked by a slow reference divider rather than by the core.
const PROBE_READS: u32 = 64;

/// Decide the verdict from what the probe saw, split out from the reading so the decision is
/// testable on a host that has no SysTick.
///
/// `reload` is `SYST_RVR`; `moved` is whether any two consecutive counter readings differed. The
/// reload is checked first because it explains the other observation: a zero reload produces a
/// counter that does not move, and reporting [`ClockSource::Stopped`] there would send someone to
/// enable a timer that is already enabled.
pub(crate) const fn verdict(reload: u32, moved: bool) -> ClockSource {
    if reload == 0 {
        ClockSource::NoReload
    } else if moved {
        ClockSource::Running
    } else {
        ClockSource::Stopped
    }
}

/// Watch the counter and report whether it is actually running.
///
/// It reads `SYST_CVR` and `SYST_RVR` and deliberately not `SYST_CSR`, for the reason the module
/// header gives (reading CSR clears `COUNTFLAG`, which the UART transport consumes as its
/// partial-frame idle clock) and for a second one: CSR's ENABLE bit is what a firmware asked for,
/// and the counter moving is what it got. A check written against the bit would pass on a part whose
/// SysTick has no clock source wired, which is the case it exists to catch.
fn probe() -> ClockSource {
    let reload = read_register(SYST_RVR) & COUNT_MASK;
    let mut previous = read_register(SYST_CVR) & COUNT_MASK;
    let mut moved = false;
    let mut reads = 0;
    while reads < PROBE_READS && !moved {
        let current = read_register(SYST_CVR) & COUNT_MASK;
        moved = current != previous;
        previous = current;
        reads += 1;
    }
    verdict(reload, moved)
}

/// Tell the clock this board's core-clock rate, in Hz, start the fold from the counter's current
/// position, and check that the counter is moving.
///
/// Call once at boot, after the clock tree is in its final state and SysTick is running -- a scale
/// captured before a PLL switch measures the wrong timebase for the rest of the board's life, which
/// is the same silent wrongness as having no clock at all.
///
/// # Why it checks, and why the answer leaves the board
///
/// The seam the interpreter gets is a plain `fn() -> u64`. A source that never advances returns a
/// perfectly well-formed answer, so nothing downstream can tell a stopped clock from a fast program:
/// a self-timing benchmark on such a board reports 0 ms for thousands of iterations of real work
/// while a checksum over that work passes. A capability that is present but dead is worse than one
/// that is absent, because absent can be reported. So the verdict is reported -- to the host through
/// `Capabilities::MONOTONIC_CLOCK`, which a firmware advertises only after this returns
/// [`ClockSource::Running`].
///
/// It does not refuse to install on a bad verdict. A board whose clock is dead is still a board a
/// host must be able to reach, and refusing here would replace a measurable wrongness with a silent
/// one. What it does do is stop [`sleep_ms`] spinning forever on a counter that will never reach a
/// deadline.
pub fn install(core_hz: u32) -> ClockSource {
    TICKS_PER_MS.store((core_hz / 1000).max(1), Ordering::Relaxed);
    LAST.store(read_register(SYST_CVR) & COUNT_MASK, Ordering::Relaxed);
    SPARE.store(0, Ordering::Relaxed);
    let source = probe();
    let running = matches!(source, ClockSource::Running);
    RUNNING.store(running, Ordering::Relaxed);
    #[cfg(all(target_os = "none", feature = "serve"))]
    lamella_runner::note_monotonic_clock(running);
    #[cfg(all(target_os = "none", not(feature = "serve")))]
    let _ = running;
    source
}

/// Ticks between two readings of a down counter that reloads from `reload`: the plain difference,
/// unless it passed zero in between. One wrap is resolved; more than one is not, and is lost.
///
/// `reload` is a parameter rather than a register read so this is a pure function -- it is the
/// arithmetic that silently drifts a clock if it is wrong, so it is worth being able to test on
/// the host. The caller pays one extra register read per call for that, in a loop that is already
/// reading one.
pub(crate) const fn elapsed(last: u32, now: u32, reload: u32) -> u32 {
    if last >= now { last - now } else { last + reload + 1 - now }
}

/// Split `ticks` (plus whatever was left over last time) into whole milliseconds and a new
/// remainder.
///
/// The remainder is the point: this is read at whatever rate the program happens to call it, and
/// discarding a sub-millisecond leftover each time would lose time in proportion to how often the
/// clock is read -- a clock that runs slower the more you look at it, which is the kind of wrong
/// that reads as jitter rather than as a bug.
pub(crate) const fn fold(ticks: u32, ticks_per_ms: u32, spare: u32) -> (u32, u32) {
    let total = spare.wrapping_add(ticks);
    (total / ticks_per_ms, total % ticks_per_ms)
}

/// Milliseconds since [`install`], monotonic.
#[must_use]
pub fn now_ms() -> u64 {
    let ticks_per_ms = TICKS_PER_MS.load(Ordering::Relaxed).max(1);
    let now = read_register(SYST_CVR) & COUNT_MASK;
    let last = LAST.load(Ordering::Relaxed);
    LAST.store(now, Ordering::Relaxed);
    let reload = read_register(SYST_RVR) & COUNT_MASK;
    let (millis, spare) =
        fold(elapsed(last, now, reload), ticks_per_ms, SPARE.load(Ordering::Relaxed));
    SPARE.store(spare, Ordering::Relaxed);
    let low = MILLIS_LOW.load(Ordering::Relaxed);
    let bumped = low.wrapping_add(millis);
    if bumped < low {
        MILLIS_HIGH.store(MILLIS_HIGH.load(Ordering::Relaxed).wrapping_add(1), Ordering::Relaxed);
    }
    MILLIS_LOW.store(bumped, Ordering::Relaxed);
    (u64::from(MILLIS_HIGH.load(Ordering::Relaxed)) << 32) | u64::from(bumped)
}

/// Microseconds from a millisecond total and the ticks the fold kept below one millisecond.
///
/// Monotonic as the tick total is: `millis * ticks_per_ms + spare` only grows, and this is that
/// total in microseconds, rounded down.
pub(crate) const fn micros(millis: u64, spare: u32, ticks_per_ms: u32) -> u64 {
    millis * 1000 + spare as u64 * 1000 / ticks_per_ms as u64
}

/// Microseconds since [`install`], monotonic, for a driver whose deadlines are finer than a
/// millisecond. It is the same fold, not a second one: it folds through [`now_ms`] and then reads
/// the sub-millisecond remainder that fold keeps, so the two readings never steal each other's ticks.
#[must_use]
#[allow(dead_code)] // Read only where a driver keeps microsecond deadlines; the other includers do not.
pub fn now_us() -> u64 {
    let millis = now_ms();
    micros(millis, SPARE.load(Ordering::Relaxed), TICKS_PER_MS.load(Ordering::Relaxed).max(1))
}

/// The longest single block this performs, whatever it is asked for.
///
/// The scheduler asks for the whole remaining sleep in one call, and a board that takes it
/// literally stops answering for that long: it services its carrier around the idle block, not
/// inside it, so a `Thread.Sleep(30000)` would leave a host unable to reach the board -- or take it
/// back from the app it deployed -- for thirty seconds. Uncapped, a HELLO sent to a board
/// sleeping 30 s goes unanswered.
///
/// Slicing costs nothing and changes no timing. The sleeping thread is parked on an absolute
/// deadline, so a short block simply wakes nobody and the scheduler blocks again -- servicing the
/// carrier each time round -- until the deadline actually passes. The sleep is neither shortened nor
/// lengthened; only the interval at which the board is reachable changes.
///
/// The value is also below one SysTick wrap on every board this serves, which keeps the fold in
/// [`now_ms`] exact through a long sleep -- the one condition it needs and the one a long
/// uninterrupted block would break.
const MAX_BLOCK_MS: u64 = 16;

/// Wait up to [`MAX_BLOCK_MS`] of `millis`, busy-spinning on [`now_ms`] -- which also keeps the fold
/// live across the wait, so the counter cannot wrap unobserved while nothing else is reading it.
/// The interpreter calls this only once every green thread is parked, so there is nothing else to
/// run; it calls it again if the sleeper's deadline has not arrived.
///
/// # The one thing it will not do is wait on a clock that cannot arrive
///
/// This spin's exit condition is the clock itself, so a counter [`install`] found not running would
/// hold the core here forever: the deadline is a fixed 16 ms ahead of a reading that never changes.
/// A `Thread.Sleep` anywhere in a deployed program would hang the board outright, and the board would
/// stop answering its host -- the loudest possible consequence of the quietest possible fault.
///
/// With no live clock it returns at once, which degrades the board to the no-clock-installed
/// behavior the runtime already defines: the scheduler computes a deadline that has already passed
/// and the sleep returns immediately. The program runs at the wrong speed, which is bad, and it does
/// so while remaining reachable, which is what makes it recoverable. `Capabilities::MONOTONIC_CLOCK`
/// is what says which of the two a host is talking to.
/// # Why it can stop the core rather than spin
///
/// This is `ReactorEnv::sleep_millis` on every Cortex-M board in the tree -- the device half of the
/// runtime's one block point, and the only place a board is ever idle on purpose. Spinning here is
/// what makes a chip run flat out with nothing to do, so each pass offers the core to
/// [`super::idle_block::block`] before falling back to the spin.
///
/// The offer is declined on a board that has not registered a
/// [`super::idle_block::WakeSource::Timer`], because a sleep with no way to end it would turn
/// `Thread.Sleep(100)` into "until something unrelated happens". Such a board spins exactly as it
/// would without the block point, and a board that registers a timer stops spinning with no change
/// here.
///
/// The spin is also load-bearing beyond burning time: it calls [`now_ms`] repeatedly, which keeps
/// the SysTick fold live so the counter cannot wrap unobserved. A sleep must therefore stay shorter
/// than one wrap, which [`MAX_BLOCK_MS`] already guarantees on every board this serves.
pub fn sleep_ms(millis: u64) {
    if !RUNNING.load(Ordering::Relaxed) {
        return;
    }
    let slice = millis.min(MAX_BLOCK_MS);
    let deadline = now_ms().saturating_add(slice);
    while now_ms() < deadline {
        let _ = idle_block::block(slice, true);
    }
}
