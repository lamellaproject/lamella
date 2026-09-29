//! The monotonic millisecond clock a RISC-V firmware gives the
//! interpreter, read from the chip's own SYSTIMER. The counterpart of `systick_clock.rs` for a core
//! that has no SysTick at all.
//!
//! # Why a board needs this at all
//!
//! The interpreter reads a clock to compute a `Thread.Sleep` deadline. Without one it computes a
//! deadline that has already passed and returns at once: nothing errors, no seam reports missing,
//! and the program simply runs at the wrong speed -- which, for a delay, is the whole of its
//! behavior. It is the failure mode of the API a beginner reaches for first.
//!
//! # Why this is not the shared SysTick fold, and why it is simpler rather than merely different
//!
//! SysTick is a 24-bit down counter, so a clock built on it must fold the ticks since its own last
//! reading into a running total -- and that last reading is state one reader consumes from another,
//! which is why there may be exactly one fold per board. SYSTIMER is a 52-bit up counter. There is
//! no wrap to resolve within any plausible uptime and no consumed state at all, so this module
//! holds none: reading it is a pure function of the hardware, every reader sees the same answer, and
//! a second reader cannot steal anything. The hazard the fold's documentation is mostly about does
//! not exist here.
//!
//! It also needs no `install(core_hz)` scale, and that is the interesting part rather than a
//! convenience. The fold's scale is the board's core-clock rate, so a scale captured before a PLL
//! switch measures the wrong timebase for the rest of the board's life. SYSTIMER's rate is a chip
//! constant: its counting clock is the crystal, divided to a documented average, and the chip
//! cannot run without that crystal. There is no number for a caller to get wrong.
//!
//! # The rate, and where it comes from
//!
//! The counter's clock source is XTAL_CLK or RC_FAST_CLK, and XTAL_CLK is the reset default. Scaled
//! by a fractional divider it yields an average 16 MHz counting clock, and the counter is documented
//! to increment by 1/16 us per cycle -- so a millisecond is 16,000 ticks, exactly. The part's
//! datasheet requires a 40 MHz crystal and states the chip cannot operate without it; 40 / 2.5 = 16
//! is the same 16 MHz from the other direction, which is why this rate can be a constant rather than
//! a measurement.
//!
//! At 16 MHz a 52-bit counter runs for about nine years before it wraps, so nothing here handles a
//! wrap. That is a deliberate omission with a stated reason, not an oversight.
//!
//! # The two ways to get this silently wrong
//!
//! **A stall-gated counter.** The counter can be configured to stop while the CPU is stalled. Left
//! that way it still counts, still rises monotonically, and still looks like a clock -- but a delay
//! measured against it is short by however long the core was stalled. [`install`] clears that bit
//! rather than trusting its reset value, because a boot chain runs before this code does.
//!
//! **A torn read.** The count is 52 bits behind a 32-bit bus, so it is not one load: a write asks
//! the timer to latch its value into two shadow registers, a status bit says the latch is done, and
//! only then are the halves readable. Reading the halves without the handshake can catch a carry
//! between them and yield a time that never happened.

/// The system timer, and the power/clock/reset block that gates it.
const SYSTIMER: usize = 0x6000_A000;
const PCR: usize = 0x6009_6000;

/// Enables UNIT0 and (crucially) whether UNIT0 stops while a core is stalled.
const SYSTIMER_CONF: usize = SYSTIMER + 0x0000;
/// Asks UNIT0 to latch its count, and reports when the latch is readable.
const SYSTIMER_UNIT0_OP: usize = SYSTIMER + 0x0004;
/// The latched count: high 20 bits, then low 32.
const SYSTIMER_UNIT0_VALUE_HI: usize = SYSTIMER + 0x0040;
const SYSTIMER_UNIT0_VALUE_LO: usize = SYSTIMER + 0x0044;

/// `SYSTIMER_CONF_REG`: run UNIT0.
const UNIT0_WORK_EN: u32 = 1 << 30;
/// `SYSTIMER_CONF_REG`: stop UNIT0 while CORE0 is stalled. Must be clear; see the module docs.
const UNIT0_CORE0_STALL_EN: u32 = 1 << 28;
/// `SYSTIMER_UNIT0_OP_REG`: write to latch the count (write-triggered; it does not stay set).
const UNIT0_UPDATE: u32 = 1 << 30;
/// `SYSTIMER_UNIT0_OP_REG`: the latched halves are synchronized and readable.
const UNIT0_VALUE_VALID: u32 = 1 << 29;

/// `PCR_SYSTIMER_CONF_REG`: the timer's APB register clock, and its module reset.
const PCR_SYSTIMER_CONF: usize = PCR + 0x0054;
const PCR_SYSTIMER_CLK_EN: u32 = 1 << 0;
const PCR_SYSTIMER_RST_EN: u32 = 1 << 1;
/// `PCR_SYSTIMER_FUNC_CLK_CONF_REG`: the counting clock's enable and source select.
const PCR_SYSTIMER_FUNC_CLK_CONF: usize = PCR + 0x0058;
const PCR_SYSTIMER_FUNC_CLK_EN: u32 = 1 << 22;
/// 0 selects XTAL_CLK (the crystal), 1 selects the internal RC oscillator. Clear it: an RC source
/// would give a clock whose rate is not knowable to better than the delays built on it.
const PCR_SYSTIMER_FUNC_CLK_SEL: u32 = 1 << 21;

/// Counting-clock ticks per millisecond: the average 16 MHz counting clock, i.e. 1/16 us per tick.
const TICKS_PER_MS: u64 = 16_000;

/// The counter's width. Present as a named constant because it is the reason nothing here resolves
/// a wrap: at [`TICKS_PER_MS`] this many ticks is roughly nine years of uptime.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const COUNTER_BITS: u32 = 52;

fn rd(address: usize) -> u32 {
    unsafe { core::ptr::read_volatile(address as *const u32) }
}

fn wr(address: usize, value: u32) {
    unsafe { core::ptr::write_volatile(address as *mut u32, value) }
}

/// Put the timer in the state this clock reads, asserting every bit it depends on rather than
/// trusting a reset value. Call once at boot.
///
/// Every one of these is already the reset default, and the writes are still here on purpose: a ROM
/// and a second-stage bootloader run before this code, both of them use timers, and a clock that is
/// correct only because nothing upstream disturbed it is a clock that breaks on a toolchain update
/// with no line of this code changing. The cost is four register writes, once.
pub fn install() {
    let conf = rd(PCR_SYSTIMER_CONF);
    wr(PCR_SYSTIMER_CONF, (conf | PCR_SYSTIMER_CLK_EN) & !PCR_SYSTIMER_RST_EN);
    let func = rd(PCR_SYSTIMER_FUNC_CLK_CONF);
    wr(
        PCR_SYSTIMER_FUNC_CLK_CONF,
        (func | PCR_SYSTIMER_FUNC_CLK_EN) & !PCR_SYSTIMER_FUNC_CLK_SEL,
    );
    let unit = rd(SYSTIMER_CONF);
    wr(SYSTIMER_CONF, (unit | UNIT0_WORK_EN) & !UNIT0_CORE0_STALL_EN);
}

/// Assemble a count from its latched halves: the high 20 bits above the low 32.
///
/// Pure so the shift can be proven on the host. Getting it wrong by one bit is a clock that runs at
/// half or double speed only once the counter passes 2^32 -- about four and a half minutes in, which
/// is long enough to look fine in a short test and wrong in a long one.
pub(crate) const fn counter_from(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | lo as u64
}

/// Counting-clock ticks to whole milliseconds.
///
/// Pure, and it keeps no remainder -- unlike the SysTick fold, which must, because that fold is fed
/// by differences it computes itself and discarding a sub-millisecond leftover each time would lose
/// time in proportion to how often the clock is read. Here every reading is an absolute division of
/// the same free-running counter, so the truncation is at most one millisecond in total and never
/// accumulates. Stated because the difference between the two files is easy to read as an omission.
pub(crate) const fn ticks_to_ms(ticks: u64) -> u64 {
    ticks / TICKS_PER_MS
}

/// Milliseconds since the counter started, monotonic.
pub fn now_ms() -> u64 {
    wr(SYSTIMER_UNIT0_OP, UNIT0_UPDATE);
    let mut guard = 0u32;
    while rd(SYSTIMER_UNIT0_OP) & UNIT0_VALUE_VALID == 0 && guard < 10_000 {
        guard = guard.wrapping_add(1);
    }
    let hi = rd(SYSTIMER_UNIT0_VALUE_HI);
    let lo = rd(SYSTIMER_UNIT0_VALUE_LO);
    ticks_to_ms(counter_from(hi, lo))
}

/// The longest single block [`sleep_ms`] will take.
///
/// The interpreter's scheduler services the carrier around an idle block, not inside it, so an
/// uncapped `Thread.Sleep(30000)` would leave a host unable to reach the board -- or take it back
/// from the app it deployed -- for thirty seconds.
///
/// Slicing costs nothing and changes no timing: the sleeping thread is parked on an absolute
/// deadline, so a short block simply wakes nobody and the scheduler blocks again, servicing the
/// carrier each time round, until the deadline actually passes. The sleep is neither shortened nor
/// lengthened; only the interval at which the board is reachable changes.
const MAX_BLOCK_MS: u64 = 16;

/// Wait up to [`MAX_BLOCK_MS`] of `millis`, busy-spinning on [`now_ms`]. The interpreter calls this
/// only once every green thread is parked, so there is nothing else to run; it calls it again if the
/// sleeper's deadline has not arrived.
pub fn sleep_ms(millis: u64) {
    let deadline = now_ms().saturating_add(millis.min(MAX_BLOCK_MS));
    while now_ms() < deadline {}
}
