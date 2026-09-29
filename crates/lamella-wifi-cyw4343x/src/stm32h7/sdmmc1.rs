//! The SDMMC1 host: the register sequences of RM0399 chapter 58 as the
//! seven host operations, and the board's two control lines.

use core::ptr::{read_volatile, write_volatile};

use super::{
    Board, Direction, IDENTIFICATION_HZ, KernelClock, Pin, SDMMC1_AF, SDMMC1_PINS, TRANSFER_HZ,
    clkcr_word, cmdr, cmdr_word, data_timeout, dctrl, dctrl_word, divider, star, word_of_bytes,
};
use crate::clock::Micros;
use crate::error::Refusal;
use crate::sdio::{Rate, Response, SdioHost, Shape, Width};
use crate::transport::Attach;

/// The SDMMC1 register block (the memory map, Table 7).
const SDMMC1: usize = 0x5200_7000;
/// The power control register (58.10.1).
const POWER: usize = SDMMC1;
/// The clock control register (58.10.2).
const CLKCR: usize = SDMMC1 + 0x004;
/// The argument register (58.10.3).
const ARGR: usize = SDMMC1 + 0x008;
/// The command register (58.10.4).
const CMDR: usize = SDMMC1 + 0x00C;
/// The first response register: response bits 39:8 (58.10.6).
const RESP1R: usize = SDMMC1 + 0x014;
/// The data timer register (58.10.7).
const DTIMER: usize = SDMMC1 + 0x024;
/// The data length register (58.10.8).
const DLENR: usize = SDMMC1 + 0x028;
/// The data control register (58.10.9).
const DCTRL: usize = SDMMC1 + 0x02C;
/// The status register (58.10.11).
const STAR: usize = SDMMC1 + 0x034;
/// The interrupt clear register (58.10.12).
const ICR: usize = SDMMC1 + 0x038;
/// The mask register (58.10.13).
const MASKR: usize = SDMMC1 + 0x03C;
/// The internal DMA control register (58.10.16), kept disabled.
const IDMACTRLR: usize = SDMMC1 + 0x050;
/// The data FIFO (58.10.15), word access only.
const FIFOR: usize = SDMMC1 + 0x080;

/// The reset and clock control block (the memory map).
const RCC: usize = 0x5802_4400;
/// The domain-1 kernel clock configuration register (9.7.18).
const RCC_D1CCIPR: usize = RCC + 0x04C;
/// The AHB3 reset register (9.7.27).
const RCC_AHB3RSTR: usize = RCC + 0x07C;
/// The AHB3 clock enable register (9.7.39).
const RCC_AHB3ENR: usize = RCC + 0x0D4;
/// The AHB4 clock enable register (9.7.42): a bit per GPIO port.
const RCC_AHB4ENR: usize = RCC + 0x0E0;
/// `SDMMCSEL` of the kernel clock configuration register.
const SDMMCSEL: u32 = 1 << 16;
/// The SDMMC1 bit of the AHB3 reset and enable registers.
const SDMMC1_BIT: u32 = 1 << 16;

/// GPIO port register offsets (12.4).
const MODER: usize = 0x00;
const OTYPER: usize = 0x04;
const OSPEEDR: usize = 0x08;
const PUPDR: usize = 0x0C;
const IDR: usize = 0x10;
const BSRR: usize = 0x18;

/// The power control field: the lines driven low.
const PWRCTRL_CYCLE: u32 = 0b10;
/// The power control field: the lines driven high, the clock stopped.
const PWRCTRL_OFF: u32 = 0b00;
/// The power control field: the card clocked.
const PWRCTRL_ON: u32 = 0b11;

/// The flags a command exchange produces.
const COMMAND_FLAGS: u32 = star::CCRCFAIL | star::CTIMEOUT | star::CMDREND | star::CMDSENT;
/// The flags that end a data transfer early.
const DATA_ERRORS: u32 =
    star::DCRCFAIL | star::DTIMEOUT | star::TXUNDERR | star::RXOVERR | star::DABORT;

/// The bound on a spin waiting for a command flag; the controller's own
/// response timeout is 64 bus clocks.
const COMMAND_SPIN: u32 = 10_000_000;
/// The bound on a spin waiting for data; the data timer is the real bound.
const DATA_SPIN: u32 = 100_000_000;

/// The deadline between the power phases: the manual's minimum is 1 ms.
const POWER_STEP_US: Micros = 2_000;

/// The stage name of a state machine that never went idle.
pub const STAGE_BUSY: &str = "SDMMC busy";
/// The stage name of a command whose completion flag never appeared.
pub const STAGE_COMMAND_COMPLETION: &str = "SDMMC command completion";
/// The stage name of a command with no response in time.
pub const STAGE_COMMAND_TIMEOUT: &str = "SDMMC command timeout";
/// The stage name of a response with a bad CRC.
pub const STAGE_RESPONSE_CRC: &str = "SDMMC response CRC";
/// The stage name of a data transfer the data timer ended.
pub const STAGE_DATA_TIMEOUT: &str = "SDMMC data timeout";
/// The stage name of a data block with a bad CRC.
pub const STAGE_DATA_CRC: &str = "SDMMC data CRC";
/// The stage name of a receive FIFO overrun.
pub const STAGE_FIFO_OVERRUN: &str = "SDMMC FIFO overrun";
/// The stage name of a transmit FIFO underrun.
pub const STAGE_FIFO_UNDERRUN: &str = "SDMMC FIFO underrun";
/// The stage name of an aborted data transfer.
pub const STAGE_TRANSFER_ABORTED: &str = "SDMMC transfer aborted";
/// The stage name of a data transfer whose end never came.
pub const STAGE_DATA_COMPLETION: &str = "SDMMC data completion";

/// A volatile 32-bit read of a controller, clock or port register.
///
/// The address is one of this module's register addresses: a memory-mapped
/// register of the STM32H747 named by RM0399's memory map, word aligned,
/// aliased by no Rust reference; a volatile access is the only way the
/// hardware is reached.
fn rd(addr: usize) -> u32 {
    unsafe { read_volatile(addr as *const u32) }
}

/// A volatile 32-bit write; the contract of [`rd`].
fn wr(addr: usize, value: u32) {
    unsafe { write_volatile(addr as *mut u32, value) }
}

/// Write a two-bit field of a port register.
fn set_field2(addr: usize, pin: u8, value: u32) {
    let shift = 2 * u32::from(pin);
    let v = rd(addr) & !(0b11 << shift);
    wr(addr, v | ((value & 0b11) << shift));
}

/// The port's clock on (RM0399 9.7.42), read back as the write spacer.
fn port_enable(pin: Pin) {
    wr(RCC_AHB4ENR, rd(RCC_AHB4ENR) | (1 << pin.port));
    let _ = rd(RCC_AHB4ENR);
}

/// A pin to its alternate function: the function number, push-pull, very
/// high speed and the pull set first, the mode last, so the pin never
/// sits on alternate function 0 (12.4.1 to 12.4.4, 12.4.9, 12.4.10).
fn pin_alternate(pin: Pin, af: u32, pull_up: bool) {
    let base = pin.port_base();
    let (reg, shift) = pin.afr();
    let v = rd(base + reg) & !(0xF << shift);
    wr(base + reg, v | (af << shift));
    wr(base + OTYPER, rd(base + OTYPER) & !(1 << pin.pin));
    set_field2(base + OSPEEDR, pin.pin, 0b11);
    set_field2(base + PUPDR, pin.pin, if pull_up { 0b01 } else { 0b00 });
    set_field2(base + MODER, pin.pin, 0b10);
}

/// A pin to a push-pull output at `level` (the level set first through the
/// set/reset register, 12.4.7, so the pin never glitches high).
fn pin_output(pin: Pin, level: bool) {
    let base = pin.port_base();
    pin_write(pin, level);
    wr(base + OTYPER, rd(base + OTYPER) & !(1 << pin.pin));
    set_field2(base + PUPDR, pin.pin, 0b00);
    set_field2(base + MODER, pin.pin, 0b01);
}

/// An output pin's level.
fn pin_write(pin: Pin, level: bool) {
    let bit = 1u32 << pin.pin;
    wr(pin.port_base() + BSRR, if level { bit } else { bit << 16 });
}

/// A pin to a plain input with no pull.
fn pin_input(pin: Pin) {
    let base = pin.port_base();
    set_field2(base + MODER, pin.pin, 0b00);
    set_field2(base + PUPDR, pin.pin, 0b00);
}

/// An input pin's level (12.4.5).
fn pin_read(pin: Pin) -> bool {
    rd(pin.port_base() + IDR) & (1 << pin.pin) != 0
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Start,
    PowerCycle { until: Micros },
    PowerOff { until: Micros },
    PowerOn { until: Micros },
    Ready,
}

/// The SDMMC1 controller as an SDIO host.
#[derive(Debug)]
pub struct Sdmmc1 {
    source: KernelClock,
    kernel_hz: u32,
    board: Board,
    phase: Phase,
    bus_hz: u32,
}

impl Sdmmc1 {
    /// A host on SDMMC1 clocked from `source` at `kernel_hz`, on `board`.
    /// The caller has the source running; nothing is touched until the
    /// first `attach`.
    pub const fn new(source: KernelClock, kernel_hz: u32, board: Board) -> Self {
        Sdmmc1 {
            source,
            kernel_hz,
            board,
            phase: Phase::Start,
            bus_hz: 0,
        }
    }

    /// The board.
    pub fn board(&self) -> Board {
        self.board
    }

    /// The bus clock as last set, in hertz; 0 before the first attach.
    pub fn bus_hz(&self) -> u32 {
        self.bus_hz
    }

    /// Whether the module's host-wake line is high; `false` on a board
    /// that does not wire it.
    pub fn host_wake_asserted(&self) -> bool {
        match self.board.host_wake {
            Some(pin) => pin_read(pin),
            None => false,
        }
    }

    /// The pins, the kernel clock source, the controller's clock and reset,
    /// the flags cleared, the interrupts masked, the lines driven low.
    fn configure(&mut self) {
        for pin in SDMMC1_PINS {
            port_enable(pin);
        }
        port_enable(self.board.power);
        if let Some(wake) = self.board.host_wake {
            port_enable(wake);
        }
        for (i, pin) in SDMMC1_PINS.iter().enumerate() {
            let is_clock = i == 4;
            pin_alternate(*pin, SDMMC1_AF, !is_clock);
        }
        pin_output(self.board.power, false);
        if let Some(wake) = self.board.host_wake {
            pin_input(wake);
        }
        let mux = rd(RCC_D1CCIPR);
        wr(
            RCC_D1CCIPR,
            match self.source {
                KernelClock::Pll1Q => mux & !SDMMCSEL,
                KernelClock::Pll2R => mux | SDMMCSEL,
            },
        );
        wr(RCC_AHB3ENR, rd(RCC_AHB3ENR) | SDMMC1_BIT);
        let _ = rd(RCC_AHB3ENR);
        wr(RCC_AHB3RSTR, rd(RCC_AHB3RSTR) | SDMMC1_BIT);
        let _ = rd(RCC_AHB3RSTR);
        wr(RCC_AHB3RSTR, rd(RCC_AHB3RSTR) & !SDMMC1_BIT);
        let _ = rd(RCC_AHB3RSTR);
        wr(MASKR, 0);
        wr(ICR, star::CLEARABLE);
        wr(POWER, PWRCTRL_CYCLE);
    }

    fn set_clock(&mut self, rate: Rate, width: Width) {
        let ceiling = match rate {
            Rate::Identification => IDENTIFICATION_HZ,
            Rate::Transfer => TRANSFER_HZ,
        };
        let div = divider(self.kernel_hz, ceiling);
        wr(CLKCR, clkcr_word(div, width));
        let _ = rd(CLKCR);
        self.bus_hz = self.kernel_hz / (2 * div);
    }

    fn attach_step(&mut self, now: Micros) -> Result<Attach, Refusal> {
        match self.phase {
            Phase::Start => {
                self.configure();
                let until = now + POWER_STEP_US;
                self.phase = Phase::PowerCycle { until };
                Ok(Attach::Pending { until })
            }
            Phase::PowerCycle { until } => {
                if now < until {
                    return Ok(Attach::Pending { until });
                }
                wr(POWER, PWRCTRL_OFF);
                let until = now + POWER_STEP_US;
                self.phase = Phase::PowerOff { until };
                Ok(Attach::Pending { until })
            }
            Phase::PowerOff { until } => {
                if now < until {
                    return Ok(Attach::Pending { until });
                }
                self.set_clock(Rate::Identification, Width::One);
                wr(POWER, PWRCTRL_ON);
                let until = now + POWER_STEP_US;
                self.phase = Phase::PowerOn { until };
                Ok(Attach::Pending { until })
            }
            Phase::PowerOn { until } => {
                if now < until {
                    return Ok(Attach::Pending { until });
                }
                wr(DCTRL, dctrl::SDIOEN);
                self.phase = Phase::Ready;
                Ok(Attach::Ready)
            }
            Phase::Ready => {
                self.phase = Phase::Start;
                self.attach_step(now)
            }
        }
    }

    /// Wait for the named state machines to go idle.
    fn wait_idle(&mut self, mask: u32) -> Result<(), Refusal> {
        let mut spins = 0u32;
        loop {
            let sta = rd(STAR);
            if sta & mask == 0 {
                return Ok(());
            }
            spins += 1;
            if spins > COMMAND_SPIN {
                return Err(Refusal::new(STAGE_BUSY, sta));
            }
        }
    }

    /// One command through the command path: the argument, the flags
    /// cleared, the command word with `extra` bits, the completion flag
    /// awaited and cleared.
    fn issue(
        &mut self,
        index: u8,
        arg: u32,
        response: Response,
        extra: u32,
    ) -> Result<u32, Refusal> {
        self.wait_idle(star::CPSMACT)?;
        wr(ARGR, arg);
        wr(ICR, COMMAND_FLAGS);
        wr(CMDR, cmdr_word(index, response, false) | extra);
        let want = match response {
            Response::None => star::CMDSENT,
            _ => star::CMDREND | star::CTIMEOUT | star::CCRCFAIL,
        };
        let mut spins = 0u32;
        let sta = loop {
            let sta = rd(STAR);
            if sta & want != 0 {
                break sta;
            }
            spins += 1;
            if spins > COMMAND_SPIN {
                return Err(Refusal::new(STAGE_COMMAND_COMPLETION, sta));
            }
        };
        wr(ICR, COMMAND_FLAGS);
        if sta & star::CTIMEOUT != 0 {
            return Err(Refusal::new(STAGE_COMMAND_TIMEOUT, sta));
        }
        if sta & star::CCRCFAIL != 0 {
            return Err(Refusal::new(STAGE_RESPONSE_CRC, sta));
        }
        match response {
            Response::None => Ok(0),
            _ => Ok(rd(RESP1R)),
        }
    }

    /// The data path armed for one transfer: the DMA off, the timer, the
    /// length, the control word (58.5.4, the FIFO procedures).
    fn arm(&mut self, direction: Direction, shape: Shape) {
        wr(IDMACTRLR, 0);
        wr(DTIMER, data_timeout(self.bus_hz));
        wr(DLENR, shape.len as u32);
        wr(DCTRL, dctrl_word(direction, shape.block_len()));
    }

    /// The recovery after a data error, or a response that refused the
    /// transfer: a stop command (the card's I/O abort of the function, sent
    /// so the command path signals the abort the data path waits for), the
    /// FIFO reset, the state machines awaited idle, the flags cleared, the
    /// interrupt detection re-armed.
    fn recover(&mut self, func: u32) {
        let abort = (1 << 31) | (0x06 << 9) | (func & 7);
        let _ = self.issue(52, abort, Response::Short, cmdr::CMDSTOP);
        if rd(STAR) & star::DPSMACT != 0 {
            wr(DCTRL, rd(DCTRL) | dctrl::FIFORST);
        }
        let _ = self.wait_idle(star::DPSMACT | star::CPSMACT);
        wr(ICR, star::CLEARABLE);
        wr(DCTRL, dctrl::SDIOEN);
    }

    fn data_error(sta: u32) -> Refusal {
        let stage = if sta & star::DTIMEOUT != 0 {
            STAGE_DATA_TIMEOUT
        } else if sta & star::DCRCFAIL != 0 {
            STAGE_DATA_CRC
        } else if sta & star::RXOVERR != 0 {
            STAGE_FIFO_OVERRUN
        } else if sta & star::TXUNDERR != 0 {
            STAGE_FIFO_UNDERRUN
        } else {
            STAGE_TRANSFER_ABORTED
        };
        Refusal::new(stage, sta)
    }

    /// Wait for the end of the transfer, or an error.
    fn wait_end(&mut self, func: u32) -> Result<(), Refusal> {
        let mut spins = 0u32;
        loop {
            let sta = rd(STAR);
            if sta & DATA_ERRORS != 0 {
                self.recover(func);
                return Err(Self::data_error(sta));
            }
            if sta & star::DATAEND != 0 {
                wr(ICR, star::CLEARABLE);
                return Ok(());
            }
            spins += 1;
            if spins > DATA_SPIN {
                self.recover(func);
                return Err(Refusal::new(STAGE_DATA_COMPLETION, sta));
            }
        }
    }
}

/// The function a CMD53 argument names.
const fn func_of(arg: u32) -> u32 {
    (arg >> 28) & 7
}

impl SdioHost for Sdmmc1 {
    /// The controller's bring-up (RM0399 58.6.7), one phase per call: the
    /// configuration and the power-cycle state (the lines driven low); the
    /// power-off state (the lines driven high); the clock at the
    /// identification rate on one data line and the power-on state; the
    /// interrupt detection armed. The module's power line stays low
    /// throughout, so the module meets an idle-high bus when it is powered.
    fn attach(&mut self, now: Micros) -> Result<Attach, Refusal> {
        self.attach_step(now)
    }

    fn set_power(&mut self, on: bool) {
        pin_write(self.board.power, on);
    }

    fn set_bus(&mut self, rate: Rate, width: Width) -> Result<(), Refusal> {
        self.wait_idle(star::DPSMACT | star::CPSMACT)?;
        self.set_clock(rate, width);
        Ok(())
    }

    fn command(&mut self, index: u8, arg: u32, response: Response) -> Result<u32, Refusal> {
        self.issue(index, arg, response, 0)
    }

    /// A read through the FIFO: the data path armed, CMD53 issued as a data
    /// command, each word taken as the receive FIFO offers it, the end
    /// awaited.
    fn read_data(&mut self, arg: u32, shape: Shape, buf: &mut [u8]) -> Result<u32, Refusal> {
        let func = func_of(arg);
        self.wait_idle(star::DPSMACT | star::CPSMACT)?;
        self.arm(Direction::FromCard, shape);
        let r5 = match self.issue(53, arg, Response::Short, cmdr::CMDTRANS) {
            Ok(r5) => r5,
            Err(e) => {
                self.recover(func);
                return Err(e);
            }
        };
        if r5 & crate::sdio::r5::ERRORS != 0 {
            self.recover(func);
            return Ok(r5);
        }
        let mut words = shape.len.div_ceil(4);
        let mut at = 0usize;
        let mut spins = 0u32;
        while words > 0 {
            let sta = rd(STAR);
            if sta & DATA_ERRORS != 0 {
                self.recover(func);
                return Err(Self::data_error(sta));
            }
            if sta & star::RXFIFOE == 0 {
                for b in rd(FIFOR).to_le_bytes() {
                    if at < buf.len() {
                        buf[at] = b;
                    }
                    at += 1;
                }
                words -= 1;
                spins = 0;
                continue;
            }
            spins += 1;
            if spins > DATA_SPIN {
                self.recover(func);
                return Err(Refusal::new(STAGE_DATA_COMPLETION, sta));
            }
        }
        self.wait_end(func)?;
        Ok(r5)
    }

    /// A write through the FIFO: the data path armed, CMD53 issued as a data
    /// command, each word given as the transmit FIFO has room, the end
    /// awaited (the card's CRC status and busy included).
    fn write_data(&mut self, arg: u32, shape: Shape, data: &[u8]) -> Result<u32, Refusal> {
        let func = func_of(arg);
        self.wait_idle(star::DPSMACT | star::CPSMACT)?;
        self.arm(Direction::ToCard, shape);
        let r5 = match self.issue(53, arg, Response::Short, cmdr::CMDTRANS) {
            Ok(r5) => r5,
            Err(e) => {
                self.recover(func);
                return Err(e);
            }
        };
        if r5 & crate::sdio::r5::ERRORS != 0 {
            self.recover(func);
            return Ok(r5);
        }
        let mut words = shape.len.div_ceil(4);
        let mut at = 0usize;
        let mut spins = 0u32;
        while words > 0 {
            let sta = rd(STAR);
            if sta & DATA_ERRORS != 0 {
                self.recover(func);
                return Err(Self::data_error(sta));
            }
            if sta & star::TXFIFOF == 0 {
                let mut bytes = [0u8; 4];
                for (i, b) in bytes.iter_mut().enumerate() {
                    if at + i < data.len() {
                        *b = data[at + i];
                    }
                }
                wr(FIFOR, word_of_bytes(bytes));
                at += 4;
                words -= 1;
                spins = 0;
                continue;
            }
            spins += 1;
            if spins > DATA_SPIN {
                self.recover(func);
                return Err(Refusal::new(STAGE_DATA_COMPLETION, sta));
            }
        }
        self.wait_end(func)?;
        Ok(r5)
    }

    fn take_interrupt(&mut self) -> u16 {
        if rd(STAR) & star::SDIOIT != 0 {
            wr(ICR, star::SDIOIT);
            1
        } else {
            0
        }
    }
}
