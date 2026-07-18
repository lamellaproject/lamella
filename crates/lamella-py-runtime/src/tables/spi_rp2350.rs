//! The Raspberry Pi RP2350 (Pico 2) SPI0 (an ARM PrimeCell PL022 SSP) register facts --
//! transcribed from the neutral peripheral-table SSOT (sources: the official RP2350 SVD; the
//! sequence is silicon-verified on a Pico 2 via the PL022 LBM loopback + a logic-analyzer decode
//! of 400 frames at 1 MHz mode-0). The SSPCLK is clk_peri, so the crystal/clk_peri bring-up is the
//! same first-seven steps as uart_rp2350 (SSPCLK = clk_peri = the 12 MHz crystal).

use crate::spi::{SpiConfig, SpiConfigError, SpiFacts, SpiOp};
use alloc::vec;
use alloc::vec::Vec;

/// The crystal + clk_peri bring-up (identical to uart_rp2350: SSPCLK is clk_peri).
const XOSC_STARTUP: u32 = 0x4004_800C;
const XOSC_CTRL: u32 = 0x4004_8000;
const XOSC_STATUS: u32 = 0x4004_8004;
const XOSC_ENABLE_1_15MHZ: u32 = 0x00FA_BAA0;
const XOSC_STARTUP_1MS: u32 = 0xC4;
const XOSC_STABLE: u32 = 1 << 31;
const CLK_PERI_CTRL: u32 = 0x4001_0048;
const CLK_PERI_DIV: u32 = 0x4001_004C;
const PERI_AUXSRC_XOSC: u32 = 4 << 5;
const PERI_ENABLE: u32 = 1 << 11;
const CLK_PERI_DIV_BY_1: u32 = 0x1_0000;

/// RESETS: the atomic-clear alias releases SPI0 (bit 18) + PADS_BANK0 + IO_BANK0.
const RESETS_RESET_CLR: u32 = 0x4002_3000;
const RESETS_RESET_DONE: u32 = 0x4002_0008;
const RESET_SPI0_PADS_IO: u32 = (1 << 18) | (1 << 9) | (1 << 6);

/// IO_BANK0: GP16 (MISO) / GP17 (hardware CS) / GP18 (SCLK) / GP19 (MOSI) at FUNCSEL 1 (spi0).
const IO_BANK0_GPIO16_CTRL: u32 = 0x4002_8084;
const IO_BANK0_GPIO17_CTRL: u32 = 0x4002_808C;
const IO_BANK0_GPIO18_CTRL: u32 = 0x4002_8094;
const IO_BANK0_GPIO19_CTRL: u32 = 0x4002_809C;
const FUNCSEL_SPI: u32 = 1;

/// PADS_BANK0: de-isolate each SPI pad (RP2350 pads reset isolated), input buffer on the RX pin.
const PADS_BANK0_GPIO16: u32 = 0x4003_8044;
const PADS_BANK0_GPIO17: u32 = 0x4003_8048;
const PADS_BANK0_GPIO18: u32 = 0x4003_804C;
const PADS_BANK0_GPIO19: u32 = 0x4003_8050;
const PAD_IE_DEISOLATE: u32 = 1 << 6;

/// The PL022 SSP block.
const SPI0: u32 = 0x4008_0000;
const SSPCR0: u32 = SPI0;
const SSPCR1: u32 = SPI0 + 0x4;
const SSPDR: u32 = SPI0 + 0x8;
const SSPSR: u32 = SPI0 + 0xC;
const SSPCPSR: u32 = SPI0 + 0x10;
/// SSPCR0: DSS 7 = 8-bit frames; SPO (bit 6) = CPOL; SPH (bit 7) = CPHA; SCR in bits 8..15.
const CR0_DSS_8BIT: u32 = 0x7;
const CR0_SPO: u32 = 1 << 6;
const CR0_SPH: u32 = 1 << 7;
/// SSPCR1: SSE (bit 1) enables the port; master mode is MS (bit 2) clear.
const CR1_SSE: u32 = 1 << 1;
/// SSPSR flags: TFE (TX FIFO empty), TNF (TX FIFO not full), RNE (RX FIFO not empty).
const SR_TFE: u32 = 1 << 0;
const SR_TNF: u32 = 1 << 1;
const SR_RNE: u32 = 1 << 2;

/// The SSP function clock (clk_peri = the 12 MHz crystal on this chip).
const SSPCLK_HZ: u64 = 12_000_000;

/// The pins the bring-up muxes to SPI0 (MISO/CS/SCLK/MOSI): a managed `cs=` naming one is rejected.
pub(crate) const FUNCTION_PINS: &[u32] = &[16, 17, 18, 19];

/// The `(sck, mosi, miso)` pins for the `busio.SPI(clock, MOSI, MISO)` pin check (GP18/GP19/GP16).
pub(crate) const SCK_MOSI_MISO: (u32, u32, u32) = (18, 19, 16);

pub(crate) fn facts() -> SpiFacts {
    SpiFacts {
        data_reg: SSPDR,
        status_reg: SSPSR,
        status_idle_flags: SR_TFE | SR_TNF,
        status_rx_ready: SR_RNE,
        sim_ready: &[(XOSC_STATUS, XOSC_STABLE)],
    }
}

/// The (CPSDVSR, SCR, realized-Hz) triple for `rate`, computed by the official pico-sdk
/// `spi_set_baudrate` algorithm so the C# and Python skins program IDENTICAL registers: the
/// smallest even prescaler that keeps the post-divide in range, then the largest post-divide whose
/// output does not exceed `rate` (bit rate = SSPCLK / (CPSDVSR * (1 + SCR)), the NEVER-EXCEED
/// contract). `None` when the rate is below the divider floor (~184 Hz at 12 MHz).
fn prescale(rate: u32) -> Option<(u32, u32, u32)> {
    let rate = u64::from(rate);
    if rate == 0 {
        return None;
    }
    let mut cpsdvsr = 2u64;
    while cpsdvsr <= 254 {
        if SSPCLK_HZ < (cpsdvsr + 2) * 256 * rate {
            break;
        }
        cpsdvsr += 2;
    }
    if cpsdvsr > 254 {
        return None;
    }
    let mut postdiv = 256u64;
    while postdiv > 1 {
        if SSPCLK_HZ / (cpsdvsr * (postdiv - 1)) > rate {
            break;
        }
        postdiv -= 1;
    }
    let realized = SSPCLK_HZ / (cpsdvsr * postdiv);
    Some((cpsdvsr as u32, (postdiv - 1) as u32, realized as u32))
}

/// The SSP-block programming for `config`: the (CPSDVSR, SSPCR0, realized-rate) triple. LSB-first
/// is rejected (the PL022's Motorola SPI mode is MSB-only -- never software-bit-reverse).
fn ssp_config(config: &SpiConfig) -> Result<(u32, u32, u32), SpiConfigError> {
    if config.bit_order == crate::spi::BIT_ORDER_LSB {
        return Err(SpiConfigError::BitOrderNotTabled);
    }
    let (cpsdvsr, scr, realized) =
        prescale(config.frequency).ok_or(SpiConfigError::BaudUnreachable)?;
    let cpol = (config.mode >> 1) & 1;
    let cpha = config.mode & 1;
    let cr0 = CR0_DSS_8BIT | (cpol * CR0_SPO) | (cpha * CR0_SPH) | (scr << 8);
    Ok((cpsdvsr, cr0, realized))
}

/// The SSP-block reprogram alone (`busio.SPI.configure`): reprogram CPSR/CR0 with SSE dropped, then
/// set last -- NO clock bring-up (that is board-shared, run once at open, never re-run mid-life).
pub(crate) fn reconfigure_ops(config: &SpiConfig) -> Result<(Vec<SpiOp>, u32), SpiConfigError> {
    let (cpsdvsr, cr0, realized) = ssp_config(config)?;
    Ok((
        vec![
            SpiOp::Write { reg: SSPCR1, value: 0 },
            SpiOp::Write { reg: SSPCPSR, value: cpsdvsr },
            SpiOp::Write { reg: SSPCR0, value: cr0 },
            SpiOp::Write { reg: SSPCR1, value: CR1_SSE },
        ],
        realized,
    ))
}

/// The bring-up opening the port with `config`, paired with the realized bit rate: the clk_peri
/// bring-up first, then the SPI resets/pads/pins, then CPSR/CR0 programmed DISABLED and SSE set last.
pub(crate) fn open_ops(config: &SpiConfig) -> Result<(Vec<SpiOp>, u32), SpiConfigError> {
    let (cpsdvsr, cr0, realized) = ssp_config(config)?;
    let ops = vec![
        SpiOp::Write { reg: XOSC_STARTUP, value: XOSC_STARTUP_1MS },
        SpiOp::Write { reg: XOSC_CTRL, value: XOSC_ENABLE_1_15MHZ },
        SpiOp::PollEq { reg: XOSC_STATUS, mask: XOSC_STABLE, want: XOSC_STABLE },
        SpiOp::Write { reg: CLK_PERI_CTRL, value: 0 },
        SpiOp::Write { reg: CLK_PERI_CTRL, value: PERI_AUXSRC_XOSC },
        SpiOp::Write { reg: CLK_PERI_DIV, value: CLK_PERI_DIV_BY_1 },
        SpiOp::Write { reg: CLK_PERI_CTRL, value: PERI_ENABLE | PERI_AUXSRC_XOSC },
        SpiOp::Write { reg: RESETS_RESET_CLR, value: RESET_SPI0_PADS_IO },
        SpiOp::PollEq {
            reg: RESETS_RESET_DONE,
            mask: RESET_SPI0_PADS_IO,
            want: RESET_SPI0_PADS_IO,
        },
        SpiOp::Write { reg: PADS_BANK0_GPIO16, value: PAD_IE_DEISOLATE },
        SpiOp::Write { reg: PADS_BANK0_GPIO17, value: PAD_IE_DEISOLATE },
        SpiOp::Write { reg: PADS_BANK0_GPIO18, value: PAD_IE_DEISOLATE },
        SpiOp::Write { reg: PADS_BANK0_GPIO19, value: PAD_IE_DEISOLATE },
        SpiOp::Write { reg: IO_BANK0_GPIO16_CTRL, value: FUNCSEL_SPI },
        SpiOp::Write { reg: IO_BANK0_GPIO17_CTRL, value: FUNCSEL_SPI },
        SpiOp::Write { reg: IO_BANK0_GPIO18_CTRL, value: FUNCSEL_SPI },
        SpiOp::Write { reg: IO_BANK0_GPIO19_CTRL, value: FUNCSEL_SPI },
        SpiOp::Write { reg: SSPCR1, value: 0 },
        SpiOp::Write { reg: SSPCPSR, value: cpsdvsr },
        SpiOp::Write { reg: SSPCR0, value: cr0 },
        SpiOp::Write { reg: SSPCR1, value: CR1_SSE },
    ];
    Ok((ops, realized))
}

/// One full-duplex byte: TNF (TX FIFO has room), the push, RNE (the reply arrived), the read.
pub(crate) fn transfer_byte_ops(byte: u8) -> Vec<SpiOp> {
    vec![
        SpiOp::PollEq { reg: SSPSR, mask: SR_TNF, want: SR_TNF },
        SpiOp::Write { reg: SSPDR, value: u32::from(byte) },
        SpiOp::PollEq { reg: SSPSR, mask: SR_RNE, want: SR_RNE },
        SpiOp::ReadInto { reg: SSPDR, mask: 0xFF },
    ]
}
