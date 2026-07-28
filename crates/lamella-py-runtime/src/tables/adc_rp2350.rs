//! The Raspberry Pi RP2350 (Pico 2) SAR ADC register facts + calibration -- transcribed from
//! the neutral peripheral-table SSOT (sources: the official RP2350 datasheet ch 12.4 + the
//! pico-sdk hardware_adc). 12-bit, one converter muxed over 5 channels;
//! the last channel is the internal temperature sensor. clk_adc runs on its own 48 MHz generator
//! raised from PLL_USB (a live native-USB wire proves the PLL is up).

use crate::adc::{AdcFacts, AdcOp};
use alloc::vec;
use alloc::vec::Vec;

/// The ADC clock generator (its own 48 MHz off PLL_USB) -- AUXSRC selected disabled, then enabled.
const CLK_ADC_CTRL: u32 = 0x4001_006C;
const CLK_ADC_AUXSRC_PLL_USB: u32 = 0x0;
const CLK_ADC_ENABLE: u32 = 0x800;
const CLK_ADC_ENABLED: u32 = 1 << 28;

/// RESETS: the atomic-clear alias releases the ADC (bit 0); done via the sim's reset accumulator.
const RESETS_RESET_CLR: u32 = 0x4002_3000;
const RESETS_RESET_DONE: u32 = 0x4002_0008;
const RESET_ADC: u32 = 1 << 0;

/// The SAR ADC block.
const ADC: u32 = 0x400a_0000;
const CS: u32 = ADC;
const RESULT: u32 = ADC + 0x4;
const CS_EN_TS_EN: u32 = 0x3;
const CS_START_ONCE: u32 = 1 << 2;
const CS_READY: u32 = 1 << 8;
const CS_ERR: u32 = 1 << 9;
const RESULT_MASK: u32 = 0xFFF;

/// External-channel pad prep: OD (output disable) high, IE (digital receiver) low, pulls off, then
/// funcsel NULL (no digital function claims the pin while the analogue mux taps the pad).
const IO_BANK0: u32 = 0x4002_8000;
const PADS_BANK0: u32 = 0x4003_8000;
const PAD_ANALOG: u32 = 0x80;
const FUNCSEL_NULL: u32 = 0x1F;

/// The reference as INTEGER MICROVOLTS (a BOARD fact: the Pico 2's filtered 3V3 rail) and the
/// 12-bit resolution -- both language skins derive conversions from HERE, neither hardcodes a volt.
const REFERENCE_UV: u32 = 3_300_000;
const RESOLUTION_BITS: u32 = 12;

/// The channel map (datasheet table 1118, QFN-60 / the Pico 2's package): channels 0-3 = GPIO26-29,
/// channel 4 = the temperature sensor (pinless). `board.A0`..`board.A3` + `board.TEMP_SENSOR`.
pub(crate) fn resource(name: &str) -> Option<(u32, u32)> {
    match name {
        "A0" => Some((0, 26)),
        "A1" => Some((1, 27)),
        "A2" => Some((2, 28)),
        "A3" => Some((3, 29)),
        "TEMP_SENSOR" => Some((4, crate::adc::NO_PIN)),
        _ => None,
    }
}

/// The backing pin for a channel number (channels 0-3 = GPIO26-29, channel 4 = pinless), or `None`.
pub(crate) fn channel_source(channel: u32) -> Option<u32> {
    match channel {
        0..=3 => Some(26 + channel),
        4 => Some(crate::adc::NO_PIN),
        _ => None,
    }
}

pub(crate) fn facts() -> AdcFacts {
    AdcFacts {
        cs: CS,
        result: RESULT,
        clk_ctrl: CLK_ADC_CTRL,
        cs_enabled: CS_EN_TS_EN,
        cs_start: CS_EN_TS_EN | CS_START_ONCE,
        cs_ready: CS_READY,
        cs_err: CS_ERR,
        clk_enabled: CLK_ADC_ENABLED,
        result_mask: RESULT_MASK,
        bits: RESOLUTION_BITS,
        reference_uv: REFERENCE_UV,
    }
}

/// The converter-block bring-up: clk_adc selected (disabled) then enabled + confirmed running, the
/// block out of reset, then the converter + temp bias powered and READY awaited.
pub(crate) fn block_init_ops() -> Vec<AdcOp> {
    vec![
        AdcOp::Write { reg: CLK_ADC_CTRL, value: CLK_ADC_AUXSRC_PLL_USB },
        AdcOp::Write { reg: CLK_ADC_CTRL, value: CLK_ADC_ENABLE },
        AdcOp::PollEq { reg: CLK_ADC_CTRL, mask: CLK_ADC_ENABLED, want: CLK_ADC_ENABLED },
        AdcOp::Write { reg: RESETS_RESET_CLR, value: RESET_ADC },
        AdcOp::PollEq { reg: RESETS_RESET_DONE, mask: RESET_ADC, want: RESET_ADC },
        AdcOp::Write { reg: CS, value: CS_EN_TS_EN },
        AdcOp::PollEq { reg: CS, mask: CS_READY, want: CS_READY },
    ]
}

/// The external-channel pad prep for `pin` (GPIO26-29); the temperature channel needs none.
pub(crate) fn pad_analog_ops(pin: u32) -> Vec<AdcOp> {
    vec![
        AdcOp::Write { reg: PADS_BANK0 + 4 + 4 * pin, value: PAD_ANALOG },
        AdcOp::Write { reg: IO_BANK0 + 4 + 8 * pin, value: FUNCSEL_NULL },
    ]
}
