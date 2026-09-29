//! The chip's internal cores and their control: the cores' addresses on
//! the backplane, the agent registers every core exposes through its
//! wrapper, the running test, and the disable and reset sequences as a
//! resumable step machine, so that their one millisecond-scale wait is a
//! deadline handed back to the caller.
//!
//! The ARM and SOCSRAM cores are controlled through their wrapper register
//! blocks, which sit 0x100000 above the cores' own registers; the SOCSRAM
//! bank registers are the core's own. Every access here is a 32-bit
//! backplane register access through the function-1 window.

use crate::backplane::Window;
use crate::clock::{Clock, Micros};
use crate::error::Refusal;
use crate::transport::Transport;

/// The SDIO device core: the mailbox and interrupt registers.
pub const SDIOD: u32 = 0x1800_2000;
/// The SOCSRAM core's own registers: the bank registers.
pub const SOCSRAM: u32 = 0x1800_4000;
/// The wrapper register block of the WLAN ARM Cortex-M3 core.
pub const ARM_CM3_WRAPPER: u32 = 0x1810_3000;
/// The wrapper register block of the SOCSRAM core.
pub const SOCSRAM_WRAPPER: u32 = 0x1810_4000;

/// The agent registers every core exposes at its wrapper, and their bits.
pub mod agent {
    /// I/O control.
    pub const IOCTL: u32 = 0x0408;
    /// Reset control.
    pub const RESET_CTL: u32 = 0x0800;
    /// I/O control: the clock enable.
    pub const CLK: u32 = 0x0001;
    /// I/O control: force the gated clocks on.
    pub const FGC: u32 = 0x0002;
    /// Reset control: the core is held in reset.
    pub const IN_RESET: u32 = 0x0001;
}

/// Registers of the SDIO device core, as offsets from [`SDIOD`].
pub mod sdiod {
    /// The interrupt status, write-1-to-clear.
    pub const INTSTATUS: u32 = 0x20;
    /// The host interrupt mask: the status bits that raise the host
    /// interrupt.
    pub const HOSTINTMASK: u32 = 0x24;
    /// The function interrupt mask, one byte.
    pub const FUNCINTMASK: u32 = 0x34;
    /// Interrupt status: a frame is available to read.
    pub const HMB_FRAME_IND: u32 = 1 << 6;
    /// Interrupt status: the four host-mailbox bits.
    pub const HMB_SW_MASK: u32 = 0xF0;
    /// Interrupt status: the chip moved from doze to active, the positive
    /// sign of a firmware that came up.
    pub const CHIPACTIVE: u32 = 1 << 29;
    /// The host interrupt mask the driver arms: the mailbox bits and the
    /// chip-active bit.
    pub const HOST_INTERRUPTS: u32 = HMB_SW_MASK | CHIPACTIVE;
    /// The function interrupt mask: function 2.
    pub const FUNCTION_2_INTERRUPT: u8 = 0x02;
}

/// Registers of the SOCSRAM core, as offsets from [`SOCSRAM`].
pub mod socsram {
    /// The bank index register.
    pub const BANK_INDEX: u32 = 0x10;
    /// The indexed bank's power-down and assignment register.
    pub const BANK_PDA: u32 = 0x44;
}

/// Registers of the chip-common core, as offsets from
/// [`crate::backplane::CHIPCOMMON`].
pub mod chipcommon {
    /// The save/restore engine's control word: non-zero once a running
    /// firmware has initialized the engine.
    pub const SR_CONTROL1: u32 = 0x508;
}

/// Whether the core at `wrapper` is running: its clock enabled, its gated
/// clocks not forced, and its reset released. `Ok(Err(word))` carries the
/// register word that decided otherwise.
pub fn running<T: Transport>(
    window: &mut Window,
    bus: &mut T,
    wrapper: u32,
) -> Result<Result<(), u32>, Refusal> {
    let ioctl = window.read32(bus, wrapper + agent::IOCTL)?;
    if ioctl & (agent::FGC | agent::CLK) != agent::CLK {
        return Ok(Err(ioctl));
    }
    let reset = window.read32(bus, wrapper + agent::RESET_CTL)?;
    if reset & agent::IN_RESET != 0 {
        return Ok(Err(reset));
    }
    Ok(Ok(()))
}

/// What a step of a core sequence asks of the caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoreStep {
    /// More work is ready: step again at once.
    Again,
    /// Step again at or after this instant.
    At(Micros),
    /// The sequence is complete.
    Done,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Begin,
    Drain { until: Micros },
    Hold,
    Release,
    Done,
}

/// The settle that lets pending backplane operations drain before a
/// running core is put into reset.
const DRAIN_US: Micros = 10_000;

/// A core's disable, or its reset and restart, as a sequence of steps.
///
/// A disable puts the core into reset: with the core running, its clock
/// is forced on, the reset asserted, and the reset-state control word
/// written; a core already in reset takes the last write alone. A reset
/// is a disable followed by the release of the reset and the write of the
/// running control word, the clock enabled and the gated clocks no longer
/// forced. Each register write is read back before the next, and the
/// microsecond settles go through the caller's clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CoreReset {
    wrapper: u32,
    prereset: u32,
    reset: u32,
    postreset: Option<u32>,
    phase: Phase,
}

impl CoreReset {
    /// A disable of the core at `wrapper`, with the core-specific control
    /// bits to hold before and in reset.
    pub const fn disable(wrapper: u32, prereset: u32, reset: u32) -> Self {
        CoreReset {
            wrapper,
            prereset,
            reset,
            postreset: None,
            phase: Phase::Begin,
        }
    }

    /// A reset and restart of the core at `wrapper`, with the
    /// core-specific control bits to hold before, in and after reset.
    pub const fn reset(wrapper: u32, prereset: u32, reset: u32, postreset: u32) -> Self {
        CoreReset {
            wrapper,
            prereset,
            reset,
            postreset: Some(postreset),
            phase: Phase::Begin,
        }
    }

    /// Whether the sequence is complete.
    pub fn is_done(&self) -> bool {
        self.phase == Phase::Done
    }

    fn write_ioctl<T: Transport>(
        &self,
        window: &mut Window,
        bus: &mut T,
        value: u32,
    ) -> Result<(), Refusal> {
        window.write32(bus, self.wrapper + agent::IOCTL, value)?;
        let _ = window.read32(bus, self.wrapper + agent::IOCTL)?;
        Ok(())
    }

    fn write_reset<T: Transport>(
        &self,
        window: &mut Window,
        bus: &mut T,
        value: u32,
    ) -> Result<(), Refusal> {
        window.write32(bus, self.wrapper + agent::RESET_CTL, value)?;
        let _ = window.read32(bus, self.wrapper + agent::RESET_CTL)?;
        Ok(())
    }

    /// One step of the sequence.
    pub fn step<T: Transport, C: Clock>(
        &mut self,
        window: &mut Window,
        bus: &mut T,
        clock: &mut C,
        now: Micros,
    ) -> Result<CoreStep, Refusal> {
        match self.phase {
            Phase::Begin => {
                let reset = window.read32(bus, self.wrapper + agent::RESET_CTL)?;
                if reset & agent::IN_RESET != 0 {
                    self.phase = Phase::Hold;
                    return Ok(CoreStep::Again);
                }
                let until = now + DRAIN_US;
                self.phase = Phase::Drain { until };
                Ok(CoreStep::At(until))
            }
            Phase::Drain { until } => {
                if now < until {
                    return Ok(CoreStep::At(until));
                }
                self.write_ioctl(window, bus, self.prereset | agent::FGC | agent::CLK)?;
                window.write32(bus, self.wrapper + agent::RESET_CTL, agent::IN_RESET)?;
                clock.delay_us(1);
                self.phase = Phase::Hold;
                Ok(CoreStep::Again)
            }
            Phase::Hold => {
                self.write_ioctl(window, bus, self.reset | agent::FGC | agent::CLK)?;
                clock.delay_us(10);
                match self.postreset {
                    None => {
                        self.phase = Phase::Done;
                        Ok(CoreStep::Done)
                    }
                    Some(_) => {
                        self.phase = Phase::Release;
                        Ok(CoreStep::Again)
                    }
                }
            }
            Phase::Release => {
                let postreset = self.postreset.unwrap_or(0);
                self.write_reset(window, bus, 0)?;
                clock.delay_us(1);
                self.write_ioctl(window, bus, postreset | agent::CLK)?;
                clock.delay_us(1);
                self.phase = Phase::Done;
                Ok(CoreStep::Done)
            }
            Phase::Done => Ok(CoreStep::Done),
        }
    }
}
