//! The Espressif ESP32-C6 HP UART0 register facts -- transcribed from the
//! neutral peripheral-table SSOT (sources: the official ESP32-C6 SVD + esp-idf v5.3
//! low-level headers; the numbers below are that table's literals, silicon-verified by the C#
//! driver bring-up on the DevKitC-1).

use crate::uart::{UartConfig, UartConfigError, UartFacts, UartOp, UartStatus};
use crate::uart::{PARITY_EVEN, PARITY_NONE};
use alloc::vec;
use alloc::vec::Vec;

/// Power/clock/reset control block.
const PCR: u32 = 0x6009_6000;
const PCR_UART0_CONF: u32 = PCR;
const PCR_UART0_SCLK_CONF: u32 = PCR + 0x4;
/// Bus clock on + reset asserted / released (PCR_UART0_CONF values).
const PCR_CLK_EN_RST: u32 = 0x3;
const PCR_CLK_EN: u32 = 0x1;
/// SCLK_EN | SCLK_SEL = XTAL (40 MHz), coarse dividers 0.
const PCR_SCLK_XTAL: u32 = 0x0070_0000;

/// HP UART0 register block.
const UART0: u32 = 0x6000_0000;
const FIFO: u32 = UART0;
const CLKDIV: u32 = UART0 + 0x14;
const STATUS: u32 = UART0 + 0x1C;
const CONF0: u32 = UART0 + 0x20;
const REG_UPDATE: u32 = UART0 + 0x98;

/// IO_MUX: the DevKitC-1 console pins, kept on their NATIVE U0TXD/U0RXD functions.
const IO_MUX: u32 = 0x6009_0000;
const IO_MUX_GPIO16: u32 = IO_MUX + 0x44;
const IO_MUX_GPIO17: u32 = IO_MUX + 0x48;
/// MCU_SEL 0 (native function) + default drive strength (FUN_DRV = 2).
const TX_PIN_NATIVE: u32 = 0x800;
/// As above plus FUN_IE (the RX input buffer).
const RX_PIN_NATIVE: u32 = 0xA00;

/// CONF0 field pieces (BIT_NUM at [2,2] counts from 5 data bits; STOP_BIT_NUM 1 or 3;
/// MEM_CLK_EN keeps the FIFO memory clocked; the two FIFO resets are pulses).
const CONF0_MEM_CLK_EN: u32 = 1 << 20;
const CONF0_FIFO_RST: u32 = (1 << 22) | (1 << 23);

const XTAL_HZ: u64 = 40_000_000;
const FIFO_DEPTH: u32 = 128;

/// The table-fixed console pins of instance 0 (TX, RX): the DevKitC-1's GPIO16/GPIO17.
pub(crate) const DEFAULT_PINS: (u32, u32) = (16, 17);

pub(crate) fn facts() -> UartFacts {
    UartFacts {
        fifo: FIFO,
        status: UartStatus::Counts {
            status: STATUS,
            rx_shift: 0,
            rx_mask: 0xFF,
            tx_shift: 16,
            tx_mask: 0xFF,
        },
        self_clear_reg: REG_UPDATE,
        sim_ready: &[],
        fifo_depth: FIFO_DEPTH,
    }
}

/// The CONF0 line-configuration value for `config` (without the FIFO-reset pulse bits).
fn conf0(config: &UartConfig) -> u32 {
    let parity_bits = match config.parity {
        PARITY_NONE => 0,
        PARITY_EVEN => 0b10,
        _ => 0b11,
    };
    let bit_num = (config.data_bits - 5) << 2;
    let stop = if config.stop_bits == 2 { 3 << 4 } else { 1 << 4 };
    CONF0_MEM_CLK_EN | stop | bit_num | parity_bits
}

/// The CLKDIV value for `baudrate` over the 40 MHz crystal (divisor in 1/16ths:
/// `INT | FRAG << 20`), or `None` when the 12-bit integer part cannot express the rate.
fn clkdiv(baudrate: u32) -> Option<u32> {
    let divisor16 = (XTAL_HZ * 16 / u64::from(baudrate)) as u32;
    let int = divisor16 >> 4;
    let frag = divisor16 & 0xF;
    if int == 0 || int > 0xFFF {
        return None;
    }
    Some((frag << 20) | int)
}

/// The bring-up sequence: clock + reset, function clock off the crystal, the native console
/// pins, the line config (with the FIFO-reset pulse), the baud divisor, and the REG_UPDATE
/// latch handshake -- the table's `sequences.init` verbatim.
pub(crate) fn open_ops(config: &UartConfig) -> Result<Vec<UartOp>, UartConfigError> {
    let clkdiv = clkdiv(config.baudrate).ok_or(UartConfigError::BaudOutOfRange)?;
    let conf0 = conf0(config);
    Ok(vec![
        UartOp::Write { reg: PCR_UART0_CONF, value: PCR_CLK_EN_RST },
        UartOp::Write { reg: PCR_UART0_CONF, value: PCR_CLK_EN },
        UartOp::Write { reg: PCR_UART0_SCLK_CONF, value: PCR_SCLK_XTAL },
        UartOp::Write { reg: IO_MUX_GPIO16, value: TX_PIN_NATIVE },
        UartOp::Write { reg: IO_MUX_GPIO17, value: RX_PIN_NATIVE },
        UartOp::Write { reg: CONF0, value: conf0 | CONF0_FIFO_RST },
        UartOp::Write { reg: CONF0, value: conf0 },
        UartOp::Write { reg: CLKDIV, value: clkdiv },
        UartOp::Write { reg: REG_UPDATE, value: 1 },
        UartOp::PollEq { reg: REG_UPDATE, mask: 1, want: 0 },
    ])
}

/// One byte out: wait for FIFO room, then a full-word FIFO write (the byte in bits 7..0 --
/// a narrower access would read-modify-write the register and corrupt the FIFO).
pub(crate) fn tx_byte_ops(byte: u8) -> Vec<UartOp> {
    vec![
        UartOp::PollBelow { reg: STATUS, mask: 0xFF_0000, below: FIFO_DEPTH << 16 },
        UartOp::Write { reg: FIFO, value: u32::from(byte) },
    ]
}

/// Transmit drained: TXFIFO_CNT down to zero.
pub(crate) fn flush_ops() -> Vec<UartOp> {
    vec![UartOp::PollEq { reg: STATUS, mask: 0xFF_0000, want: 0 }]
}
