//! A bring-up for a QSPI PSRAM part behind a microcontroller's memory-mapped
//! flash interface, written on `core` alone. Such a part (the AP Memory
//! APS6404L family and its compatible parts) shares the flash's clock and
//! four data lines, sits on the interface's second chip select, and appears
//! as ordinary RAM at a fixed address once three things are done: the pin
//! that carries the chip select is told so, the part is told to speak its
//! four-line mode through a short command sequence sent by hand, and the
//! interface's second memory window is told the part's read and write
//! commands and its timing limits.
//!
//! The crate has three pure parts and one host:
//!
//! - [`part`]: a part's datasheet facts as a value -- its commands, the wait
//!   cycles of its fast quad read, its frequency bounds, its chip-select
//!   limits in nanoseconds and the byte its Read ID answers for a good die.
//! - [`timing`]: the memory window's timing fields and the four format and
//!   command words, derived from a part and the system clock; a posture the
//!   part's limits cannot be met at is refused, never approximated.
//! - [`proof`]: a read-back test over the window, written against a small
//!   memory trait so a host test runs it over a buffer and over doctored
//!   buffers that must be refused.
//! - [`rp2350`] (the `rp2350-qmi` feature): the RP2350's QSPI memory
//!   interface as the host.
//!
//! The crate never masks interrupts, never sleeps, never prints and never
//! owns memory: every wait is a bounded poll of the host's own busy flag,
//! and the caller places the window in its heap or map as it sees fit.
#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

pub mod part;
pub mod proof;
pub mod timing;

#[cfg(feature = "rp2350-qmi")]
pub mod rp2350;

pub use part::{Grade, Part};
pub use timing::{TimingError, WindowTiming, window_timing};

/// The part's identification bytes as its Read ID frame answers them: the
/// vendor byte, the known-good-die byte and the first byte of the extended
/// identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Id {
    pub vendor: u8,
    pub kgd: u8,
    pub eid: u8,
}

impl Id {
    /// No bytes read.
    pub const NONE: Id = Id { vendor: 0, kgd: 0, eid: 0 };
}

/// A memory window a bring-up established: the cached view the program
/// uses, the uncached view the proof writes through, the window's size as
/// the caller's facts state it, the part's identification and the timing
/// the window runs at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    pub base: usize,
    pub bytes: usize,
    pub uncached_base: usize,
    pub id: Id,
    pub timing: WindowTiming,
}

/// The step of the direct-mode sequence a bring-up was in when the host's
/// busy flag never cleared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Releasing the pad and function blocks from reset.
    Resets,
    /// Entering direct mode: the memory-mapped transfer in flight never ended.
    Enter,
    /// The exit-quad command that returns a warm part to its serial mode.
    Nudge,
    /// The reset-enable command.
    ResetEnable,
    /// The reset command.
    Reset,
    /// The Read ID frame.
    ReadId,
    /// The enter-quad command.
    EnterQuad,
}

/// Why a bring-up refused. Nothing of the window is programmed on a refusal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The part's limits cannot be met at this system clock.
    Timing(TimingError),
    /// The chip-select line is not one the host can route its second chip
    /// select to.
    SelectLine(u8),
    /// The window's size is zero, not a multiple of four, or more than the
    /// chip select serves.
    WindowSize(usize),
    /// The host's busy flag never cleared during the named step.
    Stuck(Step),
    /// The part answered its Read ID with a known-good-die byte other than
    /// the pass value: no part, a part of another family, or a bad die.
    NotPresent { vendor: u8, kgd: u8 },
}
