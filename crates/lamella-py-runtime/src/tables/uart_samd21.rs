//! The Microchip SAMD21 SERCOM USART driver, per the family's USART bring-up (app note AT11626).

use crate::uart::{UartConfig, UartConfigError, UartFacts, UartOp, UartStatus};
use crate::uart::{PARITY_NONE};
use alloc::vec;
use alloc::vec::Vec;

/// The resolved per-role register FACTS a SERCOM-USART binding needs. Every field is a
/// generation-time literal read out of the board's `FACTS[<role>]`; the field names below and the
/// descriptor's keys are one contract, checked where they are read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Samd21UartFacts {
    /// The SERCOM block base (role-specific).
    pub sercom_base: u32,
    /// The GCLK.CLKCTRL value routing a generic clock to this SERCOM's core (ID | GEN | CLKEN).
    pub gclk_clkctrl_value: u32,
    /// The PM.APBCMASK bit that gates this SERCOM's APB (bus) clock.
    pub apbc_mask: u32,
    /// The absolute PORT PMUX byte for the TX/RX pin pair, and the pair value muxing them to SERCOM.
    pub pmux_reg: u32,
    pub pmux_pair: u32,
    /// The absolute PORT PINCFG bytes for the TX and RX pins.
    pub pincfg_tx_reg: u32,
    pub pincfg_rx_reg: u32,
    /// CTRLA.TXPO / RXPO (which SERCOM PAD carries TX / RX).
    pub txpo: u32,
    pub rxpo: u32,
    /// The resolved BAUD register value for 115200 under the board's default plan.
    pub baud_115200: u32,
}



/// GCLK: CLKCTRL selects a generator for a peripheral core clock (16-bit); STATUS.SYNCBUSY (bit 7)
/// covers the write.
const GCLK_CLKCTRL: u32 = 0x4000_0C02;
const GCLK_STATUS: u32 = 0x4000_0C01;
const GCLK_SYNCBUSY: u32 = 1 << 7;
/// PM.APBCMASK gates the SERCOM APB (bus) clocks.
const PM_APBCMASK: u32 = 0x4000_0420;
/// PORT PINCFG: PMUXEN routes the pin to its peripheral mux; INEN turns the input buffer on (the
/// classic SAMD21 gotcha -- a muxed RX pin with INEN off never raises RXC).
const PINCFG_PMUXEN: u32 = 1 << 0;
const PINCFG_INEN: u32 = 1 << 1;

/// SERCOM USART registers, offset from `sercom_base`.
const CTRLA: u32 = 0x00;
const CTRLB: u32 = 0x04;
const BAUD: u32 = 0x0C;
const INTFLAG: u32 = 0x18;
const SYNCBUSY: u32 = 0x1C;
const DATA: u32 = 0x28;

/// CTRLA: USART with internal clock, run-in-standby, TXPO/RXPO from FACTS, LSB-first, ENABLE.
const CTRLA_MODE_USART_INT: u32 = 1 << 2;
const CTRLA_RUNSTDBY: u32 = 1 << 7;
const CTRLA_DORD: u32 = 1 << 30;
const CTRLA_ENABLE: u32 = 1 << 1;
/// CTRLB: 8-bit char (CHSIZE 0), TX + RX enabled.
const CTRLB_TXEN: u32 = 1 << 16;
const CTRLB_RXEN: u32 = 1 << 17;
/// SYNCBUSY: the CTRLB write and the ENABLE write each synchronize.
const SYNCBUSY_CTRLB: u32 = 1 << 2;
const SYNCBUSY_ENABLE: u32 = 1 << 1;
/// INTFLAG: DRE = data register empty (TX ready), TXC = transmit complete (line idle), RXC = a byte
/// is received (readable).
const INTFLAG_DRE: u32 = 1 << 0;
const INTFLAG_TXC: u32 = 1 << 1;
const INTFLAG_RXC: u32 = 1 << 2;

/// The per-instance sim facts: DATA is the FIFO, INTFLAG carries both readiness bits, and the SERCOM
/// SYNCBUSY / GCLK STATUS registers read 0 (never written) so the init's sync polls terminate.
pub(crate) fn facts(f: &Samd21UartFacts) -> UartFacts {
    UartFacts {
        fifo: f.sercom_base + DATA,
        status: UartStatus::FlagsReady {
            flags: f.sercom_base + INTFLAG,
            tx_ready_mask: INTFLAG_DRE | INTFLAG_TXC,
            rx_ready_mask: INTFLAG_RXC,
        },
        self_clear_reg: 0,
        sim_ready: &[],
        fifo_depth: 1,
    }
}

/// The 8N1-at-115200 config this first slice supports (the serve binary's proven config + the one
/// resolved baud FACT). Anything else is refused rather than mis-programmed.
fn require_supported(config: &UartConfig) -> Result<(), UartConfigError> {
    if config.baudrate != 115_200 {
        return Err(UartConfigError::BaudOutOfRange);
    }
    if config.data_bits != 8 || config.parity != PARITY_NONE || config.stop_bits != 1 {
        return Err(UartConfigError::ParityNotTabled);
    }
    Ok(())
}

/// The bring-up: gate the SERCOM APB clock, route its core clock and wait for sync, mux the TX/RX
/// pins (RX with the input buffer on), then CTRLA (mode + TXPO/RXPO + LSB-first), CTRLB (TX/RX
/// enable) with its sync wait, the resolved BAUD, and ENABLE with its sync wait.
pub(crate) fn open_ops(
    f: &Samd21UartFacts,
    config: &UartConfig,
) -> Result<Vec<UartOp>, UartConfigError> {
    require_supported(config)?;
    let ctrla = CTRLA_MODE_USART_INT
        | CTRLA_RUNSTDBY
        | (f.txpo << 16)
        | (f.rxpo << 20)
        | CTRLA_DORD;
    Ok(vec![
        UartOp::Write { reg: PM_APBCMASK, value: f.apbc_mask },
        UartOp::Write { reg: GCLK_CLKCTRL, value: f.gclk_clkctrl_value },
        UartOp::PollEq { reg: GCLK_STATUS, mask: GCLK_SYNCBUSY, want: 0 },
        UartOp::Write { reg: f.pmux_reg, value: f.pmux_pair },
        UartOp::Write { reg: f.pincfg_tx_reg, value: PINCFG_PMUXEN },
        UartOp::Write { reg: f.pincfg_rx_reg, value: PINCFG_PMUXEN | PINCFG_INEN },
        UartOp::Write { reg: f.sercom_base + CTRLA, value: ctrla },
        UartOp::Write { reg: f.sercom_base + CTRLB, value: CTRLB_TXEN | CTRLB_RXEN },
        UartOp::PollEq { reg: f.sercom_base + SYNCBUSY, mask: SYNCBUSY_CTRLB, want: 0 },
        UartOp::Write { reg: f.sercom_base + BAUD, value: f.baud_115200 },
        UartOp::Write { reg: f.sercom_base + CTRLA, value: ctrla | CTRLA_ENABLE },
        UartOp::PollEq { reg: f.sercom_base + SYNCBUSY, mask: SYNCBUSY_ENABLE, want: 0 },
    ])
}

/// One byte out: DRE (data register empty) set, then the DATA write.
pub(crate) fn tx_byte_ops(f: &Samd21UartFacts, byte: u8) -> Vec<UartOp> {
    vec![
        UartOp::PollEq { reg: f.sercom_base + INTFLAG, mask: INTFLAG_DRE, want: INTFLAG_DRE },
        UartOp::Write { reg: f.sercom_base + DATA, value: u32::from(byte) },
    ]
}

/// Transmit drained: TXC (transmit complete) set means the shift register has emptied.
pub(crate) fn flush_ops(f: &Samd21UartFacts) -> Vec<UartOp> {
    vec![UartOp::PollEq { reg: f.sercom_base + INTFLAG, mask: INTFLAG_TXC, want: INTFLAG_TXC }]
}
