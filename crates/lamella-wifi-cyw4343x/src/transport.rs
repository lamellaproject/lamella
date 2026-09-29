//! The transport: the bus between the host and the chip, behind eight
//! operations that the gSPI and SDIO transports both implement.
//!
//! The bus-agnostic core reaches the chip only through this trait: it names
//! a function, an address and bytes, and never a block size, a bus width, a
//! clock divider or a sampling point. Everything a bus needs beyond that is
//! the transport's private state. Three facts of the running firmware are
//! answered by registers of the bus itself and so are the transport's too:
//! whether the packet channel is ready, the control that lets a command
//! wake a sleeping chip, and the bus-level half of the packet channel's
//! interrupt setup, whose answer says whether the chip's mailbox interrupt
//! is the bus's frame indication.

use crate::clock::Micros;
use crate::error::Refusal;

/// A bus function of the chip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Func {
    /// Function 0: the bus's own registers.
    F0 = 0,
    /// Function 1: the window onto the chip's internal address space.
    F1 = 1,
    /// Function 2: the packet channel.
    F2 = 2,
}

/// The radio part a transport is wired to. Both parts implement both
/// buses, so the part is a fact of the board, not of the bus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Part {
    /// The CYW43439, as fitted on the Raspberry Pi Pico W, Pico 2 W and
    /// Pimoroni Pico Plus 2 W boards.
    Cyw43439,
    /// The CYW4343W, the radio of the Murata Type 1DX module fitted on the
    /// Arduino GIGA R1 WiFi and Portenta H7 boards.
    Cyw4343w,
}

impl Part {
    /// The part's identity: the low 16 bits of the 32-bit word at internal
    /// address 0x18000000.
    pub const fn chip_id(self) -> u16 {
        match self {
            Part::Cyw43439 => 0xA9AF,
            Part::Cyw4343w => 0xA9A6,
        }
    }
}

/// One step of a transport's attach.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Attach {
    /// Call `attach` again at or after this instant; an instant at or before
    /// the current one means "again at once".
    Pending {
        /// The instant to call again at.
        until: Micros,
    },
    /// Function-1 register access works; the core continues.
    Ready,
}

/// One step of a transport's tuning.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tune {
    /// Call `tune` again at or after this instant; an instant at or before
    /// the current one means "again at once".
    Pending {
        /// The instant to call again at.
        until: Micros,
    },
    /// Tuning is complete.
    Done,
}

/// The bus between the host and the chip.
///
/// Every operation performs a bounded amount of bus work and returns; the
/// attach and tuning operations are resumable steps the core calls until
/// they report ready or done.
pub trait Transport {
    /// Bytes per bulk transfer on function 1.
    const F1_CHUNK: usize;

    /// The identity of the part this transport is wired to: the low 16 bits
    /// of the 32-bit word at internal address 0x18000000 (see [`Part`]).
    fn chip_id(&self) -> u16;

    /// One step of the bus-level attach: from power-on to working
    /// function-1 register access. The chip identity and the clock request
    /// that follow are the core's. A transport that reported `Ready` and is
    /// asked to attach again starts over from power-on.
    fn attach(&mut self, now: Micros) -> Result<Attach, Refusal>;

    /// Read one byte at a function and address.
    fn read_direct(&mut self, func: Func, addr: u32) -> Result<u8, Refusal>;

    /// Write one byte at a function and address.
    fn write_direct(&mut self, func: Func, addr: u32, value: u8) -> Result<(), Refusal>;

    /// Read `buf.len()` bytes at a function and address into the caller's
    /// slice; `incr` selects an incrementing address. Function 1 transfers
    /// are at most `F1_CHUNK` bytes.
    fn read_extended(
        &mut self,
        func: Func,
        addr: u32,
        incr: bool,
        buf: &mut [u8],
    ) -> Result<(), Refusal>;

    /// Write the caller's slice at a function and address; `incr` selects an
    /// incrementing address. Function 1 transfers are at most `F1_CHUNK`
    /// bytes.
    fn write_extended(
        &mut self,
        func: Func,
        addr: u32,
        incr: bool,
        data: &[u8],
    ) -> Result<(), Refusal>;

    /// The bus's own status word, where the bus has one; a bus without one
    /// returns 0.
    fn status(&mut self) -> Result<u32, Refusal>;

    /// Read and clear the bus-level host interrupt, returning the raw latch.
    fn take_interrupt(&mut self) -> Result<u16, Refusal>;

    /// The bus-level half of aborting a packet-channel transfer. The
    /// protocol half (the frame-terminate register on function 1) is the
    /// core's.
    fn abort_f2(&mut self) -> Result<(), Refusal>;

    /// Whether a packet-channel frame waits for the host, and its length in
    /// bytes.
    fn f2_frame_available(&mut self) -> Result<Option<usize>, Refusal>;

    /// Read the waiting packet-channel frame of `len` bytes (the length
    /// `f2_frame_available` announced) into the caller's slice; returns the
    /// bytes read.
    fn f2_read(&mut self, len: usize, frame: &mut [u8]) -> Result<usize, Refusal>;

    /// Write one packet-channel frame. `Ok(false)` means the chip's receive
    /// side has no room now and the frame was not written; the caller keeps
    /// it and tries again later.
    fn f2_write(&mut self, frame: &[u8]) -> Result<bool, Refusal>;

    /// Whether function 2, the packet channel, is ready for data transfer,
    /// as the bus reports it. Over SDIO the first call after the attach
    /// reported `Ready` enables the function and every call reads its
    /// ready bit; over gSPI every call reads the function-2 information
    /// register's ready bit. The core polls this once the firmware runs.
    fn f2_ready(&mut self) -> Result<bool, Refusal>;

    /// Let any command from the host wake a chip whose bus function has
    /// gone to sleep, where the bus has such a control. A bus without one
    /// keeps this default.
    fn wake_on_command(&mut self) -> Result<(), Refusal> {
        Ok(())
    }

    /// The bus-level half of the packet channel's interrupt setup once the
    /// firmware runs, and whether the chip's mailbox interrupt is this
    /// bus's frame indication. Over SDIO the function-2 receive watermark
    /// is lowered (a transfer must not stall when the bus clock stops) and
    /// the answer is `true`: the core then arms the chip's host and
    /// function interrupt masks and acknowledges the mailbox status in
    /// service. Over gSPI the frame indication is the bus's own interrupt
    /// register, enabled at the attach, so the default does nothing and
    /// answers `false`.
    fn f2_interrupt_setup(&mut self) -> Result<bool, Refusal> {
        Ok(false)
    }

    /// One step of the transport's tuning, called by the core once the chip
    /// is in upload mode with `f1_scratch` naming 64 writable scratch bytes
    /// on function 1. A transport with nothing to tune keeps this default.
    fn tune(&mut self, f1_scratch: u32, now: Micros) -> Result<Tune, Refusal> {
        let _ = (f1_scratch, now);
        Ok(Tune::Done)
    }
}
