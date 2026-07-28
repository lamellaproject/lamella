//! The Raspberry Pi RP2350 (Pico 2) UART0 (an ARM PrimeCell PL011) register facts -- transcribed
//! from the neutral peripheral-table SSOT (sources: the official RP2350 SVD).
//! The same PL011 block as the RP2040, so the shapes match uart_rp2040; the RP2350 deltas are the
//! block bases, the RESETS bit positions, the explicit CLK_PERI_DIV, and the pads' reset ISOLATION
//! latch (each console pin's pad must be de-isolated before it works).

use crate::uart::{UartConfig, UartConfigError, UartFacts, UartOp, UartStatus};
use crate::uart::{PARITY_EVEN, PARITY_ODD};
use alloc::vec;
use alloc::vec::Vec;

/// The crystal oscillator: enable (magic | range), stable flag, startup delay.
const XOSC_STARTUP: u32 = 0x4004_800C;
const XOSC_CTRL: u32 = 0x4004_8000;
const XOSC_STATUS: u32 = 0x4004_8004;
const XOSC_ENABLE_1_15MHZ: u32 = 0x00FA_BAA0;
const XOSC_STARTUP_1MS: u32 = 0xC4;
const XOSC_STABLE: u32 = 1 << 31;

/// clk_peri: OFF at reset, non-glitchless aux mux -- disable, select the crystal, enable. The
/// RP2350 adds CLK_PERI_DIV (reset divide-by-1), written explicitly so the divisor never depends
/// on resident-firmware state.
const CLK_PERI_CTRL: u32 = 0x4001_0048;
const CLK_PERI_DIV: u32 = 0x4001_004C;
const PERI_AUXSRC_XOSC: u32 = 4 << 5;
const PERI_ENABLE: u32 = 1 << 11;
const CLK_PERI_DIV_BY_1: u32 = 0x1_0000;

/// RESETS: the atomic-clear alias releases UART0 (bit 26 on the RP2350) + PADS_BANK0 + IO_BANK0.
const RESETS_RESET_CLR: u32 = 0x4002_3000;
const RESETS_RESET_DONE: u32 = 0x4002_0008;
const RESET_UART0_PADS_IO: u32 = (1 << 26) | (1 << 9) | (1 << 6);

/// IO_BANK0: GP0 -> UART0 TX, GP1 -> UART0 RX, both at FUNCSEL 2.
const IO_BANK0_GPIO0_CTRL: u32 = 0x4002_8004;
const IO_BANK0_GPIO1_CTRL: u32 = 0x4002_800C;
const FUNCSEL_UART: u32 = 2;

/// PADS_BANK0: RP2350 pads reset ISOLATED (ISO, bit 8) -- write IE set + ISO clear per pin.
const PADS_BANK0_GPIO0: u32 = 0x4003_8004;
const PADS_BANK0_GPIO1: u32 = 0x4003_8008;
const PAD_IE_DEISOLATE: u32 = 1 << 6;

/// The PL011 block.
const UART0: u32 = 0x4007_0000;
const UARTDR: u32 = UART0;
const UARTFR: u32 = UART0 + 0x18;
const UARTIBRD: u32 = UART0 + 0x24;
const UARTFBRD: u32 = UART0 + 0x28;
const UARTLCR_H: u32 = UART0 + 0x2C;
const UARTCR: u32 = UART0 + 0x30;
const FR_BUSY: u32 = 1 << 3;
const FR_RXFE: u32 = 1 << 4;
const FR_TXFF: u32 = 1 << 5;
const LCR_FEN: u32 = 1 << 4;
const LCR_STP2: u32 = 1 << 3;
const LCR_EPS: u32 = 1 << 2;
const LCR_PEN: u32 = 1 << 1;
const CR_ENABLED_TX_RX: u32 = 0x301;

const XOSC_HZ: u64 = 12_000_000;
const FIFO_DEPTH: u32 = 32;

/// The table-fixed console pins of instance 0 (TX, RX): the Pico 2's GP0/GP1.
pub(crate) const DEFAULT_PINS: (u32, u32) = (0, 1);

pub(crate) fn facts() -> UartFacts {
    UartFacts {
        fifo: UARTDR,
        status: UartStatus::Flags { flags: UARTFR, rx_empty_mask: FR_RXFE },
        self_clear_reg: 0,
        sim_ready: &[(XOSC_STATUS, XOSC_STABLE)],
        fifo_depth: FIFO_DEPTH,
    }
}

/// The IBRD/FBRD pair for `baudrate` off the 12 MHz crystal (divisor = uartclk / (16 * baud);
/// FBRD = the fraction in 64ths, rounded), or `None` outside the 16-bit IBRD.
fn divisor(baudrate: u32) -> Option<(u32, u32)> {
    let denominator = 16 * u64::from(baudrate);
    let ibrd = XOSC_HZ / denominator;
    if ibrd == 0 || ibrd > 0xFFFF {
        return None;
    }
    let remainder = XOSC_HZ % denominator;
    let fbrd = (remainder * 64 + denominator / 2) / denominator;
    Some((ibrd as u32, fbrd as u32))
}

/// The UARTLCR_H line config. The RP2350 table carries the parity select (PEN enables, EPS 1 =
/// even / 0 = odd), so none/even/odd are all expressible -- no config is rejected for parity.
fn lcr_h(config: &UartConfig) -> u32 {
    let wlen = (config.data_bits - 5) << 5;
    let stop = if config.stop_bits == 2 { LCR_STP2 } else { 0 };
    let parity = match config.parity {
        PARITY_EVEN => LCR_PEN | LCR_EPS,
        PARITY_ODD => LCR_PEN,
        _ => 0,
    };
    LCR_FEN | wlen | stop | parity
}

/// The bring-up: crystal up, clk_peri switched to it (disable / select / DIV / enable -- the mux
/// is not glitchless), the three resets released, the console pads de-isolated, the pins routed,
/// then IBRD, FBRD, LCR_H (WHICH LATCHES THE DIVISOR -- the PL011 ordering rule), enable.
pub(crate) fn open_ops(config: &UartConfig) -> Result<Vec<UartOp>, UartConfigError> {
    let (ibrd, fbrd) = divisor(config.baudrate).ok_or(UartConfigError::BaudOutOfRange)?;
    Ok(vec![
        UartOp::Write { reg: XOSC_STARTUP, value: XOSC_STARTUP_1MS },
        UartOp::Write { reg: XOSC_CTRL, value: XOSC_ENABLE_1_15MHZ },
        UartOp::PollEq { reg: XOSC_STATUS, mask: XOSC_STABLE, want: XOSC_STABLE },
        UartOp::Write { reg: CLK_PERI_CTRL, value: 0 },
        UartOp::Write { reg: CLK_PERI_CTRL, value: PERI_AUXSRC_XOSC },
        UartOp::Write { reg: CLK_PERI_DIV, value: CLK_PERI_DIV_BY_1 },
        UartOp::Write { reg: CLK_PERI_CTRL, value: PERI_ENABLE | PERI_AUXSRC_XOSC },
        UartOp::Write { reg: RESETS_RESET_CLR, value: RESET_UART0_PADS_IO },
        UartOp::PollEq {
            reg: RESETS_RESET_DONE,
            mask: RESET_UART0_PADS_IO,
            want: RESET_UART0_PADS_IO,
        },
        UartOp::Write { reg: PADS_BANK0_GPIO0, value: PAD_IE_DEISOLATE },
        UartOp::Write { reg: PADS_BANK0_GPIO1, value: PAD_IE_DEISOLATE },
        UartOp::Write { reg: IO_BANK0_GPIO0_CTRL, value: FUNCSEL_UART },
        UartOp::Write { reg: IO_BANK0_GPIO1_CTRL, value: FUNCSEL_UART },
        UartOp::Write { reg: UARTIBRD, value: ibrd },
        UartOp::Write { reg: UARTFBRD, value: fbrd },
        UartOp::Write { reg: UARTLCR_H, value: lcr_h(config) },
        UartOp::Write { reg: UARTCR, value: CR_ENABLED_TX_RX },
    ])
}

/// One byte out: TXFF clear (FIFO room), then the data-register write.
pub(crate) fn tx_byte_ops(byte: u8) -> Vec<UartOp> {
    vec![
        UartOp::PollEq { reg: UARTFR, mask: FR_TXFF, want: 0 },
        UartOp::Write { reg: UARTDR, value: u32::from(byte) },
    ]
}

/// Transmit drained: BUSY covers the shift register, clearing only at a fully idle line.
pub(crate) fn flush_ops() -> Vec<UartOp> {
    vec![UartOp::PollEq { reg: UARTFR, mask: FR_BUSY, want: 0 }]
}
