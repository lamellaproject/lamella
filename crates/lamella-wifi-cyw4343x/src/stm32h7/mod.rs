//! The STM32H747's SDMMC1 controller as an SDIO host, for the Arduino GIGA
//! R1 WiFi and Portenta H7 boards, whose radio module sits on that
//! controller. Register facts per RM0399 Rev 4 (the STM32H745/755 and
//! STM32H747/757 reference manual): chapter 58 for the controller, section
//! 9.7 for the reset and clock control, section 12.4 for the GPIO ports and
//! the memory map for the addresses; pin facts per the STM32H747xI
//! datasheet's alternate-function tables and the boards' schematics. Data
//! moves by programmed I/O through the controller's FIFO under hardware
//! flow control: no DMA, and no buffer the caller must place.
//!
//! The controller's kernel clock is the caller's: the caller configures the
//! PLL that feeds it and names the source and its frequency here; the host
//! selects that source and derives the two bus dividers from the frequency.

#![allow(unsafe_code)]

mod sdmmc1;

pub use sdmmc1::Sdmmc1;

use crate::sdio::{Response, Width};

/// The kernel clock source of SDMMC1 (RM0399 section 9.7.18, `SDMMCSEL`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KernelClock {
    /// `pll1_q_ck`.
    Pll1Q,
    /// `pll2_r_ck`.
    Pll2R,
}

/// A GPIO pin: the port index (0 for port A through 10 for port K) and the
/// pin number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pin {
    /// The port index.
    pub port: u8,
    /// The pin number, 0 to 15.
    pub pin: u8,
}

/// The base of the GPIO port register blocks (the memory map).
pub const GPIO_BASE: usize = 0x5802_0000;

impl Pin {
    /// A pin by port index and number.
    pub const fn new(port: u8, pin: u8) -> Self {
        Pin { port, pin }
    }

    /// The port's register block.
    pub const fn port_base(self) -> usize {
        GPIO_BASE + 0x400 * self.port as usize
    }

    /// The alternate-function register's offset and the field's shift for
    /// this pin: pins 0 to 7 in the low register, 8 to 15 in the high one,
    /// four bits each (RM0399 sections 12.4.9 and 12.4.10).
    pub const fn afr(self) -> (usize, u32) {
        if self.pin < 8 {
            (0x20, 4 * self.pin as u32)
        } else {
            (0x24, 4 * (self.pin as u32 - 8))
        }
    }
}

/// A board's wiring of the radio module's control lines and its I/O rail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Board {
    /// The module's power-enable line.
    pub power: Pin,
    /// The module's host-wake output, where the board wires it.
    pub host_wake: Option<Pin>,
    /// The module's I/O rail, in millivolts.
    pub io_millivolts: u32,
}

/// The Arduino GIGA R1 WiFi (schematic ABX00063, the radio sheet): the
/// power line on PB10, the host wake on PI8, the module's I/O rail at 3.3 V.
pub const GIGA_R1_WIFI: Board = Board {
    power: Pin::new(1, 10),
    host_wake: Some(Pin::new(8, 8)),
    io_millivolts: 3300,
};

/// The Arduino Portenta H7 family (schematic ABX00046, the radio sheet):
/// the power line on PJ1, the host wake on PJ5, the module's I/O rail at
/// 3.1 V.
pub const PORTENTA_H7: Board = Board {
    power: Pin::new(9, 1),
    host_wake: Some(Pin::new(9, 5)),
    io_millivolts: 3100,
};

/// The SDMMC1 pins on the STM32H747: D0 to D3 on PC8 to PC11, the clock on
/// PC12, the command line on PD2, all alternate function 12 (the datasheet's
/// port C and port D alternate-function tables).
pub const SDMMC1_PINS: [Pin; 6] = [
    Pin::new(2, 8),
    Pin::new(2, 9),
    Pin::new(2, 10),
    Pin::new(2, 11),
    Pin::new(2, 12),
    Pin::new(3, 2),
];

/// The alternate function of the SDMMC1 pins.
pub const SDMMC1_AF: u32 = 12;

/// The identification-mode clock ceiling.
pub const IDENTIFICATION_HZ: u32 = 400_000;
/// The default-speed transfer clock ceiling.
pub const TRANSFER_HZ: u32 = 25_000_000;

/// The smallest clock divider that puts the bus clock strictly under
/// `ceiling_hz`: the bus clock is the kernel clock over twice the divider
/// (RM0399 section 58.10.2, `CLKDIV`); at least 1, at most the field's
/// 1023.
pub const fn divider(kernel_hz: u32, ceiling_hz: u32) -> u32 {
    let twice = 2 * ceiling_hz;
    let mut div = kernel_hz / twice;
    if !kernel_hz.is_multiple_of(twice) {
        div += 1;
    }
    if kernel_hz / (2 * (if div == 0 { 1 } else { div })) >= ceiling_hz {
        div += 1;
    }
    if div == 0 {
        div = 1;
    }
    if div > 0x3FF { 0x3FF } else { div }
}

/// Bits of the clock control register (RM0399 section 58.10.2).
pub mod clkcr {
    /// The 4-bit wide bus.
    pub const WIDBUS_4: u32 = 1 << 14;
    /// Hardware flow control: the bus clock stops rather than the FIFO
    /// overrunning or underrunning.
    pub const HWFC_EN: u32 = 1 << 17;
    /// The divider field.
    pub const DIV_MASK: u32 = 0x3FF;
}

/// Bits of the command register (RM0399 section 58.10.4).
pub mod cmdr {
    /// The command carries a data transfer.
    pub const CMDTRANS: u32 = 1 << 6;
    /// The command is a stop, signalling the abort to the data path.
    pub const CMDSTOP: u32 = 1 << 7;
    /// A short response with a CRC.
    pub const WAITRESP_SHORT: u32 = 1 << 8;
    /// A short response without a CRC.
    pub const WAITRESP_SHORT_NO_CRC: u32 = 2 << 8;
    /// The command path enable.
    pub const CPSMEN: u32 = 1 << 12;
}

/// Bits of the data control register (RM0399 section 58.10.9).
pub mod dctrl {
    /// The transfer direction: from the card.
    pub const DTDIR: u32 = 1 << 1;
    /// The SD I/O interrupt detection.
    pub const SDIOEN: u32 = 1 << 11;
    /// The FIFO reset.
    pub const FIFORST: u32 = 1 << 13;
}

/// Bits of the status register (RM0399 section 58.10.11).
pub mod star {
    /// A command response with a failed CRC.
    pub const CCRCFAIL: u32 = 1 << 0;
    /// A data block with a failed CRC.
    pub const DCRCFAIL: u32 = 1 << 1;
    /// No command response within 64 bus clocks.
    pub const CTIMEOUT: u32 = 1 << 2;
    /// The data timer expired.
    pub const DTIMEOUT: u32 = 1 << 3;
    /// The transmit FIFO ran empty.
    pub const TXUNDERR: u32 = 1 << 4;
    /// The receive FIFO ran full.
    pub const RXOVERR: u32 = 1 << 5;
    /// A command response received.
    pub const CMDREND: u32 = 1 << 6;
    /// A command without a response sent.
    pub const CMDSENT: u32 = 1 << 7;
    /// The data transfer ended correctly.
    pub const DATAEND: u32 = 1 << 8;
    /// The data transfer was aborted.
    pub const DABORT: u32 = 1 << 11;
    /// The data path state machine is active.
    pub const DPSMACT: u32 = 1 << 12;
    /// The command path state machine is active.
    pub const CPSMACT: u32 = 1 << 13;
    /// The transmit FIFO is full.
    pub const TXFIFOF: u32 = 1 << 16;
    /// The receive FIFO is empty.
    pub const RXFIFOE: u32 = 1 << 19;
    /// The card's interrupt was received.
    pub const SDIOIT: u32 = 1 << 22;
    /// Every flag the clear register clears: bits 0 to 11 and 21 to 28.
    pub const CLEARABLE: u32 = 0x1FE0_0FFF;
}

/// The direction of a data transfer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// From the host to the card.
    ToCard,
    /// From the card to the host.
    FromCard,
}

/// The clock control word for a divider and a bus width: hardware flow
/// control on, the clock never stopped at idle (the card's interrupt rides
/// a data line and needs it), data changing on the falling edge.
pub const fn clkcr_word(div: u32, width: Width) -> u32 {
    let mut word = (div & clkcr::DIV_MASK) | clkcr::HWFC_EN;
    if let Width::Four = width {
        word |= clkcr::WIDBUS_4;
    }
    word
}

/// The command word for an index, a response kind and whether the command
/// carries data.
pub const fn cmdr_word(index: u8, response: Response, data: bool) -> u32 {
    let mut word = (index as u32 & 0x3F) | cmdr::CPSMEN;
    word |= match response {
        Response::None => 0,
        Response::Short => cmdr::WAITRESP_SHORT,
        Response::ShortWithoutCrc => cmdr::WAITRESP_SHORT_NO_CRC,
    };
    if data {
        word |= cmdr::CMDTRANS;
    }
    word
}

/// The data control word for a direction and a block size (a power of two,
/// encoded as its logarithm): block mode ending on the count, the
/// interrupt detection kept on.
pub const fn dctrl_word(direction: Direction, block_len: usize) -> u32 {
    let mut word = (block_len.trailing_zeros() << 4) | dctrl::SDIOEN;
    if let Direction::FromCard = direction {
        word |= dctrl::DTDIR;
    }
    word
}

/// The data timeout in bus clocks: a quarter second at the bus rate.
pub const fn data_timeout(bus_hz: u32) -> u32 {
    bus_hz / 4
}

/// A FIFO word from four wire bytes: the first byte on the wire is the
/// least significant byte of the word.
pub const fn word_of_bytes(bytes: [u8; 4]) -> u32 {
    u32::from_le_bytes(bytes)
}
