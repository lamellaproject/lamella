//! The STM32 boards' millisecond clock: a 32-bit general-purpose timer counting once a millisecond
//! from the startup, read without an interrupt and widened to 64 bits as it is read.
//!
//! An STM32 board's image applies no clock plan, so the part runs on the clock it leaves reset on,
//! and the timer's prescaler divides the clock that reset leaves an APB1 timer, as the family's
//! generated facts state it, down to one count a millisecond. The timer counts 32 bits, so its count
//! wraps after 2^32 milliseconds, about 49.7 days, and [`now_ms`] carries each wrap into the
//! reading's high word.
//!
//! A wrap is seen by the first read after it, so a reading is exact only while the clock is read at
//! least once in every 2^32 milliseconds; a program that leaves it unread for longer loses whole
//! periods of 49.7 days from every later reading. A sleep reads it until the sleep ends, so a sleep
//! is exact however long it is.
//!
//! The clock is only as good as the oscillator the part runs on, which on these boards is an
//! internal RC oscillator, factory trimmed, rather than a crystal.

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::registers::Registers;

/// The counter's rate: one count a millisecond.
const COUNTS_PER_SECOND: u32 = 1000;

/// A general-purpose timer as a board's generated facts state it, and the clock it counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timer {
    /// The RCC enable register that gates the timer's clock.
    pub clock_register: u32,
    /// The timer's bit in that register.
    pub clock_mask: u32,
    /// The timer's control register, TIMx_CR1.
    pub control: u32,
    /// CR1's counter enable bit, CEN.
    pub counter_enable: u32,
    /// The timer's event generation register, TIMx_EGR.
    pub event_generation: u32,
    /// EGR's update generation bit, UG.
    pub update_generation: u32,
    /// The timer's prescaler, TIMx_PSC.
    pub prescaler: u32,
    /// The bits of PSC that hold the prescaler's value.
    pub prescaler_mask: u32,
    /// The timer's count, TIMx_CNT.
    pub counter: u32,
    /// The auto-reload value the timer leaves reset with, which its count wraps after.
    pub auto_reload_reset: u32,
    /// The clock the timer counts, in hertz, as the part leaves reset.
    pub clock_hz: u32,
}

impl Timer {
    /// The prescaler value that divides the timer's clock to one count a millisecond: the counter
    /// advances once every PSC + 1 cycles of its clock.
    #[must_use]
    pub const fn millisecond_prescaler(&self) -> u32 {
        self.clock_hz / COUNTS_PER_SECOND - 1
    }

    /// The timer, checked where it is declared: its clock divides to a whole count a millisecond,
    /// the prescaler holds the divisor, and the count wraps after 2^32, which [`now_ms`] carries.
    #[must_use]
    pub const fn checked(self) -> Self {
        assert!(
            self.clock_hz % COUNTS_PER_SECOND == 0 && self.clock_hz >= COUNTS_PER_SECOND,
            "the timer's clock divides to a whole count a millisecond"
        );
        assert!(
            self.millisecond_prescaler() <= self.prescaler_mask,
            "the timer's prescaler holds the divisor that makes a count a millisecond"
        );
        assert!(self.auto_reload_reset == u32::MAX, "the timer's count wraps after 2^32");
        self
    }
}

/// What the clock remembers between reads. All zero means "not started", which is what the RAM a
/// startup clears holds.
#[derive(Debug)]
pub struct Clock {
    started: AtomicBool,
    /// The count the latest read saw.
    last: AtomicU32,
    /// The wraps the reads have seen: the reading's high 32 bits.
    wraps: AtomicU32,
}

impl Clock {
    /// The clock before the startup has run.
    #[must_use]
    pub const fn new() -> Self {
        Clock { started: AtomicBool::new(false), last: AtomicU32::new(0), wraps: AtomicU32::new(0) }
    }
}

impl Default for Clock {
    fn default() -> Self {
        Self::new()
    }
}

/// Starts `timer` counting milliseconds from 0, once at reset, before any program code runs.
///
/// It opens the timer's clock gate and reads the gate back, sets the prescaler, raises an update,
/// which loads the prescaler and clears the count, and starts the counter. Every other register of
/// the timer is as the system reset before the startup left it: counting up, in no slave mode, to an
/// auto-reload value of 0xFFFF_FFFF (RM0385 Rev 9 section 5.1.1, RM0410 Rev 5 section 5.1.1 and
/// RM0456 Rev 7 section 11.3.2: a system reset sets every register to its reset value).
///
/// The read-back is the wait the manuals ask for, two cycles of the peripheral's clock after its
/// enable is set before its registers are reached (RM0385 and RM0410 section 5.2.12, RM0456 section
/// 11.4.24): the read cannot complete before the write it follows has reached the RCC, and the
/// timer's first register is written after it.
pub fn start<R: Registers>(timer: &Timer, regs: &mut R, clock: &Clock) {
    let gate = regs.read(timer.clock_register);
    regs.write(timer.clock_register, gate | timer.clock_mask);
    let _ = regs.read(timer.clock_register);
    regs.write(timer.prescaler, timer.millisecond_prescaler());
    regs.write(timer.event_generation, timer.update_generation);
    regs.write(timer.control, timer.counter_enable);
    clock.last.store(0, Ordering::Relaxed);
    clock.wraps.store(0, Ordering::Relaxed);
    clock.started.store(true, Ordering::Relaxed);
}

/// The clock in milliseconds, counted from 1 at the startup, or 0 before [`start`] has run.
///
/// A reading in the first millisecond is 1 rather than 0, because the runtime reads 0 as "no
/// clock". A count lower than the one the latest read saw is a wrap, which is carried into the high
/// word. The caller makes each read alone: a read that interrupted another could count one wrap
/// twice or not at all.
pub fn now_ms<R: Registers>(timer: &Timer, regs: &mut R, clock: &Clock) -> u64 {
    if !clock.started.load(Ordering::Relaxed) {
        return 0;
    }
    let count = regs.read(timer.counter);
    let mut wraps = clock.wraps.load(Ordering::Relaxed);
    if count < clock.last.load(Ordering::Relaxed) {
        wraps = wraps.wrapping_add(1);
        clock.wraps.store(wraps, Ordering::Relaxed);
    }
    clock.last.store(count, Ordering::Relaxed);
    ((u64::from(wraps) << 32) | u64::from(count)).wrapping_add(1)
}
