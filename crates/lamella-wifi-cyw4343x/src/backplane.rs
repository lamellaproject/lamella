//! The backplane window: function 1 as a 32 KB window onto the chip's
//! internal 32-bit address space, selected by three window registers.
//!
//! A 32-bit register access sets bit 15 of the in-window address (the
//! 32-bit access flag); bulk data transfers never do. Backplane values are
//! little-endian on the wire.

use crate::error::Refusal;
use crate::transport::{Func, Transport};

/// Function-1 registers the core uses.
pub mod f1 {
    /// Window register: address bits 15:8 (only bit 15 is significant).
    pub const SBADDRLOW: u32 = 0x1000A;
    /// Window register: address bits 23:16.
    pub const SBADDRMID: u32 = 0x1000B;
    /// Window register: address bits 31:24.
    pub const SBADDRHIGH: u32 = 0x1000C;
    /// The chip clock control and status register.
    pub const CHIPCLKCSR: u32 = 0x1000E;
    /// The wake-up control register.
    pub const WAKEUP_CTRL: u32 = 0x1001E;
    /// The sleep control and status register.
    pub const SLEEP_CSR: u32 = 0x1001F;
    /// The packet channel's frame control register.
    pub const FRAME_CONTROL: u32 = 0x1000D;
    /// The window: address bits 31:15.
    pub const WINDOW_MASK: u32 = 0xFFFF_8000;
    /// The offset inside the window: address bits 14:0.
    pub const OFFSET_MASK: u32 = 0x7FFF;
    /// The 32-bit access flag, set in the in-window address of a four-byte
    /// register access.
    pub const ACCESS_32: u32 = 0x8000;
}

/// Bits of the chip clock control and status register.
pub mod clkcsr {
    /// Force the ALP clock request.
    pub const FORCE_ALP: u8 = 0x01;
    /// Force the HT clock request.
    pub const FORCE_HT: u8 = 0x02;
    /// Force the ILP clock request.
    pub const FORCE_ILP: u8 = 0x04;
    /// Request the ALP clock.
    pub const ALP_AVAIL_REQ: u8 = 0x08;
    /// Request the HT clock.
    pub const HT_AVAIL_REQ: u8 = 0x10;
    /// Squelch hardware clock requests.
    pub const FORCE_HW_CLKREQ_OFF: u8 = 0x20;
    /// Status: the ALP clock is ready.
    pub const ALP_AVAIL: u8 = 0x40;
    /// Status: the HT clock is ready.
    pub const HT_AVAIL: u8 = 0x80;
}

/// Bits of the wake-up control register.
pub mod wakeup {
    /// The chip raises the HT clock request itself when its bus function
    /// powers on.
    pub const HT_WAIT: u8 = 0x02;
}

/// Bits of the sleep control and status register.
pub mod sleep {
    /// Keep the bus function on through the firmware's own sleep.
    pub const KSO: u8 = 0x01;
    /// The device is on.
    pub const DEVON: u8 = 0x02;
}

/// Bits of the packet channel's frame control register.
pub mod frame_control {
    /// Terminate the frame being read.
    pub const RF_TERM: u8 = 0x01;
}

/// The chip-common core's base, the enumeration base; the chip identity is
/// the low 16 bits of the word at this address.
pub const CHIPCOMMON: u32 = 0x1800_0000;

/// The function-1 address of a 32-bit register access at backplane address
/// `addr`: the offset inside the window with the 32-bit access flag.
pub const fn register_offset(addr: u32) -> u32 {
    (addr & f1::OFFSET_MASK) | f1::ACCESS_32
}

/// The cached window selection: the three window registers are written only
/// when the byte they hold changes.
#[derive(Clone, Copy, Debug)]
pub struct Window {
    current: u32,
}

const UNPROGRAMMED: u32 = u32::MAX;

impl Window {
    /// A window with nothing programmed: the first selection writes all
    /// three registers.
    pub const fn new() -> Self {
        Window {
            current: UNPROGRAMMED,
        }
    }

    /// Select the window holding `addr`, writing only the register bytes
    /// that change. A failed write leaves the cache unprogrammed.
    pub fn select<T: Transport>(&mut self, bus: &mut T, addr: u32) -> Result<(), Refusal> {
        let window = addr & f1::WINDOW_MASK;
        if window == self.current {
            return Ok(());
        }
        let regs = [f1::SBADDRLOW, f1::SBADDRMID, f1::SBADDRHIGH];
        for (i, reg) in regs.iter().enumerate() {
            let shift = 8 * (i as u32 + 1);
            let byte = (window >> shift) as u8;
            let held = (self.current >> shift) as u8;
            let changed = self.current == UNPROGRAMMED || held != byte;
            if changed && let Err(e) = bus.write_direct(Func::F1, *reg, byte) {
                self.current = UNPROGRAMMED;
                return Err(e);
            }
        }
        self.current = window;
        Ok(())
    }

    /// Read a 32-bit backplane register at `addr`.
    pub fn read32<T: Transport>(&mut self, bus: &mut T, addr: u32) -> Result<u32, Refusal> {
        self.select(bus, addr)?;
        let mut bytes = [0u8; 4];
        bus.read_extended(Func::F1, register_offset(addr), true, &mut bytes)?;
        Ok(u32::from_le_bytes(bytes))
    }

    /// Write a 32-bit backplane register at `addr`.
    pub fn write32<T: Transport>(
        &mut self,
        bus: &mut T,
        addr: u32,
        value: u32,
    ) -> Result<(), Refusal> {
        self.select(bus, addr)?;
        bus.write_extended(Func::F1, register_offset(addr), true, &value.to_le_bytes())
    }

    /// Write one byte at backplane address `addr`: a one-byte function-1
    /// write at the window offset, without the 32-bit access flag.
    pub fn write8<T: Transport>(
        &mut self,
        bus: &mut T,
        addr: u32,
        value: u8,
    ) -> Result<(), Refusal> {
        self.select(bus, addr)?;
        bus.write_direct(Func::F1, addr & f1::OFFSET_MASK, value)
    }
}

impl Default for Window {
    fn default() -> Self {
        Self::new()
    }
}
