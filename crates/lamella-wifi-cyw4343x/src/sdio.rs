//! The SDIO transport: the chip's SD I/O host interface, as wired on the
//! Arduino GIGA R1 WiFi and Portenta H7 boards, above a seven-operation
//! host that a controller drives.
//!
//! The bus is the SD Association's SDIO Simplified Specification Version
//! 2.00: the card is enumerated with CMD0, CMD5, CMD3 and CMD7 (section
//! 3.1's initialization flow); its registers are reached one byte at a time
//! with CMD52 (section 5.1) and in runs with CMD53 (section 5.3); the card
//! common control registers (section 6.9, Tables 6-1 and 6-2) and the
//! per-function block sizes (section 6.10, Table 6-3) are set before
//! function 1, the window onto the chip's address space, is enabled. The
//! chip's own facts (its function-1 registers, its transfer sizing, the
//! four-byte length tag at the head of every function-2 frame) are stated
//! where they are used. The host below this transport moves commands,
//! responses and data blocks, sets the bus clock and reports the card's
//! interrupt; it knows nothing of the protocol.

use crate::clock::Micros;
use crate::error::Refusal;
use crate::transport::{Attach, Func, Part, Transport};

/// The bus clock rate the transport asks the host for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rate {
    /// Identification: under 400 kHz, from power-on until the card is
    /// addressed and configured.
    Identification,
    /// Transfer: the default-speed ceiling of 25 MHz.
    Transfer,
}

/// The bus width.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Width {
    /// One data line.
    One,
    /// Four data lines.
    Four,
}

/// The response a command expects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Response {
    /// No response (CMD0).
    None,
    /// A 48-bit response carrying a CRC (R1, R5, R6).
    Short,
    /// A 48-bit response whose CRC field is fixed at ones (R4, the answer
    /// to CMD5).
    ShortWithoutCrc,
}

/// The shape of one CMD53 transfer on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shape {
    /// Bytes on the wire.
    pub len: usize,
    /// The block size in block mode; 0 in byte mode, where the transfer is
    /// one block of `len` bytes.
    pub block: usize,
}

impl Shape {
    /// A byte-mode transfer: one block of `len` bytes.
    pub const fn bytes(len: usize) -> Self {
        Shape { len, block: 0 }
    }

    /// A block-mode transfer of `count` blocks of `block` bytes.
    pub const fn blocks(count: usize, block: usize) -> Self {
        Shape {
            len: count * block,
            block,
        }
    }

    /// The number of blocks on the wire: 1 in byte mode.
    pub const fn count(&self) -> usize {
        match self.len.checked_div(self.block) {
            Some(count) => count,
            None => 1,
        }
    }

    /// The size of each block on the wire.
    pub const fn block_len(&self) -> usize {
        if self.block == 0 {
            self.len
        } else {
            self.block
        }
    }
}

/// The host: an SD I/O controller, or a fake replaying a recorded
/// exchange. Every operation performs a bounded amount of work; the
/// controller's own response and data timeouts are its bounds.
pub trait SdioHost {
    /// One step of the controller's bring-up, from reset to "powered, at the
    /// identification rate, one data line, the first 74 clocks delivered".
    /// A host that reported `Ready` and is called again starts over.
    fn attach(&mut self, now: Micros) -> Result<Attach, Refusal>;

    /// Drive the module's power-enable line.
    fn set_power(&mut self, on: bool);

    /// Set the bus clock rate and width.
    fn set_bus(&mut self, rate: Rate, width: Width) -> Result<(), Refusal>;

    /// One command/response exchange: the command index, its 32-bit
    /// argument and the response kind; returns response bits 39:8, or 0
    /// when no response is expected.
    fn command(&mut self, index: u8, arg: u32, response: Response) -> Result<u32, Refusal>;

    /// CMD53 with `arg`, reading `shape.len` bytes from the card: the first
    /// `buf.len()` land in `buf`, the rest are discarded. Returns the R5
    /// word.
    fn read_data(&mut self, arg: u32, shape: Shape, buf: &mut [u8]) -> Result<u32, Refusal>;

    /// CMD53 with `arg`, writing `data` then zero bytes up to `shape.len`.
    /// Returns the R5 word.
    fn write_data(&mut self, arg: u32, shape: Shape, data: &[u8]) -> Result<u32, Refusal>;

    /// Read and clear the controller's card-interrupt latch; 0 when none.
    fn take_interrupt(&mut self) -> u16;
}

/// The operating-voltage windows the host offers the card at the second
/// CMD5 (section 3.2, Table 3-1): bit 8 is the 2.0-2.1 V window, bit 23
/// the 3.5-3.6 V window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OcrWindow(u32);

impl OcrWindow {
    /// A 3.3 V rail: the 3.2-3.3 V and 3.3-3.4 V windows, bits 20 and 21.
    pub const V3_3: OcrWindow = OcrWindow(0x0030_0000);
    /// A 3.1 V rail: the 3.0-3.1 V and 3.1-3.2 V windows, bits 18 and 19.
    pub const V3_1: OcrWindow = OcrWindow(0x000C_0000);

    /// The windows touching a rail voltage in millivolts: the window
    /// containing it and, when it lies on a boundary, the window below.
    /// `None` under 2.7 V or over 3.6 V: a version 2.00 host does not use
    /// the 2.0-2.7 V range for basic communication (section 3.2).
    pub const fn for_millivolts(mv: u32) -> Option<OcrWindow> {
        if mv < 2700 || mv > 3600 {
            return None;
        }
        let bit = 8 + (mv - 2000) / 100;
        let mut mask = if bit <= 23 { 1 << bit } else { 0 };
        if mv.is_multiple_of(100) && bit > 15 {
            mask |= 1 << (bit - 1);
        }
        Some(OcrWindow(mask))
    }

    /// The window bits, positioned as in the OCR.
    pub const fn bits(self) -> u32 {
        self.0
    }
}

/// Fields of the R4 response to CMD5 (section 3.3), as the host returns
/// them: response bits 39:8.
pub mod r4 {
    /// The card is ready after initialization (the C bit).
    pub const READY: u32 = 1 << 31;

    /// The number of I/O functions the card carries.
    pub const fn functions(r4: u32) -> u32 {
        (r4 >> 28) & 7
    }

    /// The 24-bit operating conditions register.
    pub const fn ocr(r4: u32) -> u32 {
        r4 & 0x00FF_FFFF
    }
}

/// Fields of the R5 response to CMD52 and CMD53 (section 5.2.1, Table 5-1),
/// as the host returns them: the flags in bits 15:8, the data in bits 7:0.
pub mod r5 {
    /// The CRC check of the previous command failed.
    pub const COM_CRC_ERROR: u32 = 1 << 15;
    /// The command was not legal for the card state.
    pub const ILLEGAL_COMMAND: u32 = 1 << 14;
    /// A general or unknown error.
    pub const ERROR: u32 = 1 << 11;
    /// An invalid function number was requested.
    pub const FUNCTION_NUMBER: u32 = 1 << 9;
    /// The argument was out of range.
    pub const OUT_OF_RANGE: u32 = 1 << 8;
    /// The error flags together; the state field (bits 13:12) is not one.
    pub const ERRORS: u32 =
        COM_CRC_ERROR | ILLEGAL_COMMAND | ERROR | FUNCTION_NUMBER | OUT_OF_RANGE;

    /// The data byte.
    pub const fn data(r5: u32) -> u8 {
        r5 as u8
    }

    /// The flags byte.
    pub const fn flags(r5: u32) -> u8 {
        (r5 >> 8) as u8
    }
}

/// Fields of the R6 response to CMD3 (section 4.3, Tables 4-2 and 4-3), as
/// the host returns them: the new relative card address in bits 31:16 and
/// the I/O-only card's status bits in bits 15:0.
pub mod r6 {
    /// COM_CRC_ERROR, ILLEGAL_COMMAND and ERROR.
    pub const ERRORS: u32 = 0xE000;

    /// The relative card address the card published.
    pub const fn rca(r6: u32) -> u16 {
        (r6 >> 16) as u16
    }
}

/// The R1 response to CMD7 from an I/O-only card (section 4.10.8, Table
/// 4-7): the state field reads 15 and only the error bits below are used.
pub mod r1 {
    /// OUT_OF_RANGE, COM_CRC_ERROR, ILLEGAL_COMMAND and ERROR.
    pub const ERRORS: u32 = (1 << 31) | (1 << 23) | (1 << 22) | (1 << 19);
}

/// Card common control registers used here (section 6.9, Table 6-1), and
/// the block-size registers of the function basic registers (section
/// 6.10, Table 6-3).
pub mod cccr {
    /// I/O enable: bit n enables function n.
    pub const IO_ENABLE: u32 = 0x02;
    /// I/O ready: bit n reports function n initialized.
    pub const IO_READY: u32 = 0x03;
    /// Interrupt enable: bit 0 the master enable, bit n function n.
    pub const INT_ENABLE: u32 = 0x04;
    /// I/O abort: bits 2:0 name the function whose transfer to stop.
    pub const IO_ABORT: u32 = 0x06;
    /// Bus interface control: bits 1:0 the bus width.
    pub const BUS_INTERFACE: u32 = 0x07;
    /// The bus-width field of the bus interface control byte.
    pub const BUS_WIDTH_MASK: u8 = 0x03;
    /// The 4-bit bus width.
    pub const BUS_WIDTH_4: u8 = 0x02;
    /// The bit of function 1 in the enable and ready bytes.
    pub const FUNCTION_1: u8 = 0x02;
    /// The bit of function 2 in the enable and ready bytes.
    pub const FUNCTION_2: u8 = 0x04;
    /// The chip's card capability register, in the area reserved for
    /// vendor-unique registers (Table 6-1, 0xF0 to 0xFF).
    pub const CARD_CAPABILITY: u32 = 0xF0;
    /// Card capability: any command from the host wakes the chip, even one
    /// it does not decode.
    pub const CMD_NODEC: u8 = 0x08;
    /// The master enable with functions 1 and 2.
    pub const INT_ENABLE_ALL: u8 = 0x07;
    /// The abort of function 2.
    pub const ABORT_F2: u8 = 0x02;

    /// The low byte of a function's 16-bit block size register: 0x10 in the
    /// CCCR for function 0, 0x10 into the function's basic registers at
    /// 0x100 times the function number otherwise; the high byte follows.
    pub const fn block_size(func: crate::transport::Func) -> u32 {
        ((func as u8 as u32) << 8) | 0x10
    }
}

/// Function-1 registers of the chip's SDIO device core used by this
/// transport.
pub mod f1 {
    /// The chip's pull-ups on the command and data lines: 0 turns them off.
    pub const SDIO_PULLUP: u32 = 0x1000F;
    /// The function-2 receive watermark.
    pub const WATERMARK: u32 = 0x10008;
}

/// The function-2 receive watermark once the firmware runs: low, so a
/// transfer does not stall when the bus clock stops.
pub const F2_WATERMARK: u8 = 8;

/// The command argument of CMD52 (section 5.1): the read/write flag in bit
/// 31, the function in bits 30:28, the register address in bits 25:9 and
/// the data byte in bits 7:0. The read-after-write flag (bit 27) is never
/// set, so the R5 of a write carries the byte written.
pub const fn cmd52_arg(write: bool, func: Func, addr: u32, data: u8) -> u32 {
    (if write { 1 << 31 } else { 0 })
        | ((func as u8 as u32) << 28)
        | ((addr & 0x1_FFFF) << 9)
        | data as u32
}

/// The command argument of CMD53 (section 5.3): the read/write flag in bit
/// 31, the function in bits 30:28, block mode in bit 27, the incrementing
/// address flag in bit 26, the address in bits 25:9 and the byte or block
/// count in bits 8:0.
pub const fn cmd53_arg(write: bool, func: Func, addr: u32, incr: bool, shape: Shape) -> u32 {
    let count = if shape.block == 0 {
        shape.len
    } else {
        shape.count()
    };
    (if write { 1 << 31 } else { 0 })
        | ((func as u8 as u32) << 28)
        | (if shape.block != 0 { 1 << 27 } else { 0 })
        | (if incr { 1 << 26 } else { 0 })
        | ((addr & 0x1_FFFF) << 9)
        | (count as u32 & 0x1FF)
}

/// The CRC7 of the command line (the SD Physical Layer Specification's
/// polynomial x^7 + x^3 + 1) over `bytes`, the 40 bits before the CRC.
pub fn crc7(bytes: &[u8]) -> u8 {
    let mut crc: u8 = 0;
    for &b in bytes {
        for i in (0..8).rev() {
            let bit = ((b >> i) & 1) ^ (crc >> 6);
            crc = (crc << 1) & 0x7F;
            if bit != 0 {
                crc ^= 0x09;
            }
        }
    }
    crc
}

/// The six bytes of a command as it travels: the transmission bit and the
/// index, the argument most significant byte first, the CRC7 and the end
/// bit.
pub fn command_frame(index: u8, arg: u32) -> [u8; 6] {
    let a = arg.to_be_bytes();
    let mut frame = [0x40 | (index & 0x3F), a[0], a[1], a[2], a[3], 0];
    frame[5] = (crc7(&frame[..5]) << 1) | 1;
    frame
}

/// The block size on functions 1 and 2, the largest function 1 accepts.
pub const BLOCK: usize = 64;
/// The largest transfer on function 0, its block size.
pub const F0_MAX: usize = 32;
/// The largest transfer on function 1: thirty-two blocks, inside the
/// window.
pub const F1_MAX: usize = 2048;
/// The largest function-2 frame the transport moves.
pub const F2_MAX: usize = 2048;
/// The shortest function-2 frame: the twelve-byte header alone.
pub const FRAME_MIN: usize = 12;

/// The longest transfer each function accepts.
pub const fn ceiling(func: Func) -> usize {
    match func {
        Func::F0 => F0_MAX,
        Func::F1 => F1_MAX,
        Func::F2 => F2_MAX,
    }
}

/// The wire shape of a transfer of `len` bytes on `func`: under 64 bytes,
/// byte mode rounded up to a power of two; from 64 bytes, 64-byte blocks
/// rounded up to whole blocks; a length the function cannot carry is
/// refused.
pub fn wire_shape(func: Func, len: usize) -> Result<Shape, Refusal> {
    let limit = ceiling(func);
    if len == 0 || len > limit {
        return Err(Refusal::new(STAGE_TRANSFER, len as u32));
    }
    if len < BLOCK {
        let rounded = len.next_power_of_two();
        if rounded > limit {
            return Err(Refusal::new(STAGE_TRANSFER, len as u32));
        }
        Ok(Shape::bytes(rounded))
    } else {
        Ok(Shape::blocks(len.div_ceil(BLOCK), BLOCK))
    }
}

/// The stage name of a first R4 whose operating conditions lack the
/// offered window.
pub const STAGE_VOLTAGE_WINDOW: &str = "SDIO voltage window";
/// The stage name of a card reporting fewer than two I/O functions.
pub const STAGE_FUNCTION_COUNT: &str = "SDIO function count";
/// The stage name of a card that never reported ready to CMD5.
pub const STAGE_CARD_READY: &str = "SDIO card ready";
/// The stage name of an R6 carrying error bits.
pub const STAGE_RELATIVE_ADDRESS: &str = "SDIO relative address";
/// The stage name of a CMD7 response carrying error bits.
pub const STAGE_CARD_SELECT: &str = "SDIO card select";
/// The stage name of an R5 carrying error flags.
pub const STAGE_RESPONSE_FLAGS: &str = "SDIO response flags";
/// The stage name of a bus-width byte that did not read back as 4-bit.
pub const STAGE_BUS_WIDTH: &str = "SDIO bus width";
/// The stage name of an interrupt-enable byte that did not read back.
pub const STAGE_INTERRUPT_ENABLE: &str = "SDIO interrupt enable";
/// The stage name of function 1 never reporting ready.
pub const STAGE_FUNCTION_READY: &str = "SDIO function 1 ready";
/// The stage name of a transfer the transport refused before the host.
pub const STAGE_TRANSFER: &str = "SDIO transfer";
/// The stage name of a function-2 length tag that does not check.
pub const STAGE_FRAME_TAG: &str = "SDIO frame tag";

/// The reset hold with the power line low, and the settle after it rises.
const RESET_US: Micros = 10_000;
/// The wait after CMD0 before CMD5.
const IDLE_US: Micros = 50_000;
/// The settle after the bus clock rises to the transfer rate.
const RATE_SETTLE_US: Micros = 500_000;
/// Ready polls, of the card and of function 1: one hundred tries, ten
/// milliseconds apart.
const POLL_STEP_US: Micros = 10_000;
const POLL_TRIES: u8 = 100;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Start,
    HostAttach,
    ResetLow { until: Micros },
    ResetHigh { until: Micros },
    Idle { until: Micros },
    Negotiate { tries: u8, next: Micros },
    Address,
    Select,
    BusWidth,
    BlockSizes,
    Interrupts,
    TransferRate { until: Micros },
    ReadyPoll { tries: u8, next: Micros },
    PullUps,
    Ready,
}

/// The SDIO transport above a host.
#[derive(Debug)]
pub struct Sdio<H: SdioHost> {
    host: H,
    part: Part,
    window: OcrWindow,
    phase: Phase,
    rca: u16,
    tag: Option<[u8; 4]>,
    f2_enabled: bool,
}

impl<H: SdioHost> Sdio<H> {
    /// A transport above `host` to `part`, offering `window` at the second
    /// CMD5; in the power-up state.
    pub const fn new(host: H, part: Part, window: OcrWindow) -> Self {
        Sdio {
            host,
            part,
            window,
            phase: Phase::Start,
            rca: 0,
            tag: None,
            f2_enabled: false,
        }
    }

    /// The host.
    pub fn host(&self) -> &H {
        &self.host
    }

    /// The host, mutably.
    pub fn host_mut(&mut self) -> &mut H {
        &mut self.host
    }

    /// Give the host back.
    pub fn into_host(self) -> H {
        self.host
    }

    /// The part this transport is wired to.
    pub fn part(&self) -> Part {
        self.part
    }

    /// The relative card address the card published at the last attach;
    /// 0 before one.
    pub fn rca(&self) -> u16 {
        self.rca
    }

    fn check_r5(r5: u32) -> Result<(), Refusal> {
        if r5 & r5::ERRORS != 0 {
            return Err(Refusal::new(STAGE_RESPONSE_FLAGS, r5));
        }
        Ok(())
    }

    /// One CMD52: the byte read, or the byte written as the card echoes it.
    fn direct(&mut self, write: bool, func: Func, addr: u32, value: u8) -> Result<u8, Refusal> {
        let r5 = self
            .host
            .command(52, cmd52_arg(write, func, addr, value), Response::Short)?;
        Self::check_r5(r5)?;
        Ok(r5::data(r5))
    }

    fn extended_read(
        &mut self,
        func: Func,
        addr: u32,
        incr: bool,
        buf: &mut [u8],
    ) -> Result<(), Refusal> {
        let shape = wire_shape(func, buf.len())?;
        let r5 = self
            .host
            .read_data(cmd53_arg(false, func, addr, incr, shape), shape, buf)?;
        Self::check_r5(r5)
    }

    fn extended_write(
        &mut self,
        func: Func,
        addr: u32,
        incr: bool,
        data: &[u8],
    ) -> Result<(), Refusal> {
        let shape = wire_shape(func, data.len())?;
        let r5 = self
            .host
            .write_data(cmd53_arg(true, func, addr, incr, shape), shape, data)?;
        Self::check_r5(r5)
    }

    fn attach_step(&mut self, now: Micros) -> Result<Attach, Refusal> {
        match self.phase {
            Phase::Start => {
                self.rca = 0;
                self.tag = None;
                self.f2_enabled = false;
                self.phase = Phase::HostAttach;
                self.attach_step(now)
            }
            Phase::HostAttach => match self.host.attach(now)? {
                Attach::Pending { until } => Ok(Attach::Pending { until }),
                Attach::Ready => {
                    self.host.set_power(false);
                    let until = now + RESET_US;
                    self.phase = Phase::ResetLow { until };
                    Ok(Attach::Pending { until })
                }
            },
            Phase::ResetLow { until } => {
                if now < until {
                    return Ok(Attach::Pending { until });
                }
                self.host.set_power(true);
                let until = now + RESET_US;
                self.phase = Phase::ResetHigh { until };
                Ok(Attach::Pending { until })
            }
            Phase::ResetHigh { until } => {
                if now < until {
                    return Ok(Attach::Pending { until });
                }
                self.host.command(0, 0, Response::None)?;
                let until = now + IDLE_US;
                self.phase = Phase::Idle { until };
                Ok(Attach::Pending { until })
            }
            Phase::Idle { until } => {
                if now < until {
                    return Ok(Attach::Pending { until });
                }
                let r4 = self.host.command(5, 0, Response::ShortWithoutCrc)?;
                let window = self.window.bits();
                if r4::ocr(r4) & window != window {
                    return Err(Refusal::new(STAGE_VOLTAGE_WINDOW, r4));
                }
                if r4::functions(r4) < 2 {
                    return Err(Refusal::new(STAGE_FUNCTION_COUNT, r4));
                }
                self.phase = Phase::Negotiate {
                    tries: 0,
                    next: now,
                };
                Ok(Attach::Pending { until: now })
            }
            Phase::Negotiate { tries, next } => {
                if now < next {
                    return Ok(Attach::Pending { until: next });
                }
                let r4 = self
                    .host
                    .command(5, self.window.bits(), Response::ShortWithoutCrc)?;
                if r4 & r4::READY != 0 {
                    self.phase = Phase::Address;
                    return Ok(Attach::Pending { until: now });
                }
                let tries = tries + 1;
                if tries >= POLL_TRIES {
                    return Err(Refusal::new(STAGE_CARD_READY, r4));
                }
                let next = now + POLL_STEP_US;
                self.phase = Phase::Negotiate { tries, next };
                Ok(Attach::Pending { until: next })
            }
            Phase::Address => {
                let r6 = self.host.command(3, 0, Response::Short)?;
                if r6 & r6::ERRORS != 0 {
                    return Err(Refusal::new(STAGE_RELATIVE_ADDRESS, r6));
                }
                self.rca = r6::rca(r6);
                self.phase = Phase::Select;
                Ok(Attach::Pending { until: now })
            }
            Phase::Select => {
                let r1 = self
                    .host
                    .command(7, u32::from(self.rca) << 16, Response::Short)?;
                if r1 & r1::ERRORS != 0 {
                    return Err(Refusal::new(STAGE_CARD_SELECT, r1));
                }
                self.phase = Phase::BusWidth;
                Ok(Attach::Pending { until: now })
            }
            Phase::BusWidth => {
                let control = self.direct(false, Func::F0, cccr::BUS_INTERFACE, 0)?;
                let wide = (control & !cccr::BUS_WIDTH_MASK) | cccr::BUS_WIDTH_4;
                self.direct(true, Func::F0, cccr::BUS_INTERFACE, wide)?;
                self.host.set_bus(Rate::Identification, Width::Four)?;
                let back = self.direct(false, Func::F0, cccr::BUS_INTERFACE, 0)?;
                if back & cccr::BUS_WIDTH_MASK != cccr::BUS_WIDTH_4 {
                    return Err(Refusal::new(STAGE_BUS_WIDTH, u32::from(back)));
                }
                self.phase = Phase::BlockSizes;
                Ok(Attach::Pending { until: now })
            }
            Phase::BlockSizes => {
                for (func, size) in [(Func::F0, F0_MAX), (Func::F1, BLOCK), (Func::F2, BLOCK)] {
                    let reg = cccr::block_size(func);
                    self.direct(true, Func::F0, reg, size as u8)?;
                    self.direct(true, Func::F0, reg + 1, (size >> 8) as u8)?;
                }
                self.phase = Phase::Interrupts;
                Ok(Attach::Pending { until: now })
            }
            Phase::Interrupts => {
                self.direct(true, Func::F0, cccr::INT_ENABLE, cccr::INT_ENABLE_ALL)?;
                let back = self.direct(false, Func::F0, cccr::INT_ENABLE, 0)?;
                if back != cccr::INT_ENABLE_ALL {
                    return Err(Refusal::new(STAGE_INTERRUPT_ENABLE, u32::from(back)));
                }
                self.host.set_bus(Rate::Transfer, Width::Four)?;
                let until = now + RATE_SETTLE_US;
                self.phase = Phase::TransferRate { until };
                Ok(Attach::Pending { until })
            }
            Phase::TransferRate { until } => {
                if now < until {
                    return Ok(Attach::Pending { until });
                }
                let enabled = self.direct(false, Func::F0, cccr::IO_ENABLE, 0)?;
                self.direct(true, Func::F0, cccr::IO_ENABLE, enabled | cccr::FUNCTION_1)?;
                let next = now + POLL_STEP_US;
                self.phase = Phase::ReadyPoll { tries: 0, next };
                Ok(Attach::Pending { until: next })
            }
            Phase::ReadyPoll { tries, next } => {
                if now < next {
                    return Ok(Attach::Pending { until: next });
                }
                let ready = self.direct(false, Func::F0, cccr::IO_READY, 0)?;
                if ready & cccr::FUNCTION_1 != 0 {
                    self.phase = Phase::PullUps;
                    return Ok(Attach::Pending { until: now });
                }
                let tries = tries + 1;
                if tries >= POLL_TRIES {
                    return Err(Refusal::new(STAGE_FUNCTION_READY, u32::from(ready)));
                }
                let next = now + POLL_STEP_US;
                self.phase = Phase::ReadyPoll { tries, next };
                Ok(Attach::Pending { until: next })
            }
            Phase::PullUps => {
                self.direct(true, Func::F1, f1::SDIO_PULLUP, 0)?;
                self.phase = Phase::Ready;
                Ok(Attach::Ready)
            }
            Phase::Ready => {
                self.phase = Phase::Start;
                self.attach_step(now)
            }
        }
    }
}

impl<H: SdioHost> Transport for Sdio<H> {
    const F1_CHUNK: usize = F1_MAX;

    fn chip_id(&self) -> u16 {
        self.part.chip_id()
    }

    /// The attach, one phase per call: the host's own bring-up; the power
    /// line low, a hold, high, a settle; CMD0 and a wait; CMD5 to read the
    /// operating conditions, CMD5 with the offered window until the card
    /// reports ready; CMD3 for the address and CMD7 to select; the 4-bit
    /// width written and read back; the three block sizes; the interrupt
    /// enables written and read back; the transfer rate and its settle;
    /// function 1 enabled and polled ready; the chip's pull-ups off. A
    /// refusal returns the phase to the start, and a call after `Ready`
    /// starts over, so either way a further attach powers the module
    /// again.
    fn attach(&mut self, now: Micros) -> Result<Attach, Refusal> {
        let step = self.attach_step(now);
        if step.is_err() {
            self.phase = Phase::Start;
            self.rca = 0;
        }
        step
    }

    fn read_direct(&mut self, func: Func, addr: u32) -> Result<u8, Refusal> {
        self.direct(false, func, addr, 0)
    }

    fn write_direct(&mut self, func: Func, addr: u32, value: u8) -> Result<(), Refusal> {
        self.direct(true, func, addr, value).map(|_| ())
    }

    fn read_extended(
        &mut self,
        func: Func,
        addr: u32,
        incr: bool,
        buf: &mut [u8],
    ) -> Result<(), Refusal> {
        if buf.len() == 1 {
            buf[0] = self.direct(false, func, addr, 0)?;
            return Ok(());
        }
        self.extended_read(func, addr, incr, buf)
    }

    fn write_extended(
        &mut self,
        func: Func,
        addr: u32,
        incr: bool,
        data: &[u8],
    ) -> Result<(), Refusal> {
        if data.len() == 1 {
            return self.direct(true, func, addr, data[0]).map(|_| ());
        }
        self.extended_write(func, addr, incr, data)
    }

    /// Nothing: the bus has no status word of its own.
    fn status(&mut self) -> Result<u32, Refusal> {
        Ok(0)
    }

    fn take_interrupt(&mut self) -> Result<u16, Refusal> {
        Ok(self.host.take_interrupt())
    }

    /// The abort of function 2 through the I/O abort register.
    fn abort_f2(&mut self) -> Result<(), Refusal> {
        self.tag = None;
        self.direct(true, Func::F0, cccr::IO_ABORT, cccr::ABORT_F2)
            .map(|_| ())
    }

    /// The four-byte length tag at the head of the waiting frame: the frame
    /// size, little-endian, and its complement. A size of 0 means no frame.
    /// The tag is kept for the read that follows.
    fn f2_frame_available(&mut self) -> Result<Option<usize>, Refusal> {
        let mut tag = [0u8; 4];
        self.extended_read(Func::F2, 0, true, &mut tag)?;
        let size = u16::from_le_bytes([tag[0], tag[1]]);
        let check = u16::from_le_bytes([tag[2], tag[3]]);
        if size == 0 {
            self.tag = None;
            return Ok(None);
        }
        let len = usize::from(size);
        if size ^ check != 0xFFFF || !(FRAME_MIN..=F2_MAX).contains(&len) {
            self.tag = None;
            return Err(Refusal::new(
                STAGE_FRAME_TAG,
                u32::from(size) | (u32::from(check) << 16),
            ));
        }
        self.tag = Some(tag);
        Ok(Some(len))
    }

    /// The rest of the announced frame, read behind the kept tag, which is
    /// put back at the head.
    fn f2_read(&mut self, len: usize, frame: &mut [u8]) -> Result<usize, Refusal> {
        let Some(tag) = self.tag else {
            return Err(Refusal::new(STAGE_TRANSFER, len as u32));
        };
        let size = usize::from(u16::from_le_bytes([tag[0], tag[1]]));
        if len != size || frame.len() < size {
            return Err(Refusal::new(STAGE_TRANSFER, len as u32));
        }
        self.tag = None;
        self.extended_read(Func::F2, 0, true, &mut frame[4..size])?;
        frame[..4].copy_from_slice(&tag);
        Ok(size)
    }

    /// One function-2 write at address 0; the bus has no receive-ready gate
    /// of its own.
    fn f2_write(&mut self, frame: &[u8]) -> Result<bool, Refusal> {
        if frame.is_empty() || frame.len() > F2_MAX {
            return Err(Refusal::new(STAGE_TRANSFER, frame.len() as u32));
        }
        self.extended_write(Func::F2, 0, true, frame)?;
        Ok(true)
    }

    /// Function 2 enabled on the first call after the attach (the I/O
    /// Enable byte's bit 2), then the I/O Ready byte's bit 2 read on every
    /// call (section 6.9, Tables 6-1 and 6-2: the enable starts the
    /// function's initialization, the ready bit reports its completion).
    fn f2_ready(&mut self) -> Result<bool, Refusal> {
        if !self.f2_enabled {
            let enabled = self.direct(false, Func::F0, cccr::IO_ENABLE, 0)?;
            self.direct(true, Func::F0, cccr::IO_ENABLE, enabled | cccr::FUNCTION_2)?;
            self.f2_enabled = true;
        }
        let ready = self.direct(false, Func::F0, cccr::IO_READY, 0)?;
        Ok(ready & cccr::FUNCTION_2 != 0)
    }

    /// The card capability register's command-no-decode bit.
    fn wake_on_command(&mut self) -> Result<(), Refusal> {
        self.direct(true, Func::F0, cccr::CARD_CAPABILITY, cccr::CMD_NODEC)
            .map(|_| ())
    }

    /// The function-2 receive watermark lowered, and `true`: the chip's
    /// mailbox interrupt is this bus's frame indication, delivered as the
    /// card's in-band interrupt through the enables written at the attach.
    fn f2_interrupt_setup(&mut self) -> Result<bool, Refusal> {
        self.direct(true, Func::F1, f1::WATERMARK, F2_WATERMARK)?;
        Ok(true)
    }
}
