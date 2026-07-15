//! The clean, first-class Lamella Python ADC layer -- and the per-chip facts behind it.

use crate::tables::adc_rp2350;
use alloc::vec::Vec;

/// Method ids for the `adc` module object (dispatched in `ObjectModel::call_adc_method`).
pub(crate) const ADC_OPEN: u32 = 0;

/// The `adc` method id for `name`, or `None`.
pub(crate) fn adc_method_id(name: &str) -> Option<u32> {
    match name {
        "open" => Some(ADC_OPEN),
        _ => None,
    }
}

/// Method ids for a `Channel` object (dispatched in `ObjectModel::call_adc_channel_method`).
pub(crate) const CH_READ_U16: u32 = 0;
pub(crate) const CH_READ_RAW: u32 = 1;
pub(crate) const CH_READ_UV: u32 = 2;
pub(crate) const CH_CLOSE: u32 = 3;
pub(crate) const CH_ENTER: u32 = 4;
pub(crate) const CH_EXIT: u32 = 5;

/// The `Channel` method id for `name`, or `None`.
pub(crate) fn adc_channel_method_id(name: &str) -> Option<u32> {
    match name {
        "read_u16" => Some(CH_READ_U16),
        "read_raw" => Some(CH_READ_RAW),
        "read_uv" => Some(CH_READ_UV),
        "close" => Some(CH_CLOSE),
        "__enter__" => Some(CH_ENTER),
        "__exit__" => Some(CH_EXIT),
        _ => None,
    }
}

pub(crate) const CH_W_CHANNEL: u32 = 0;
pub(crate) const CH_W_OPEN: u32 = 1;
/// The backing pin, or [`NO_PIN`] for a pinless internal channel.
pub(crate) const CH_W_PIN: u32 = 2;
/// The hardware resolution (the `.bits` echo).
pub(crate) const CH_W_BITS: u32 = 3;
/// The reference in microvolts (the `.reference_uv` echo; the read_uv scale).
pub(crate) const CH_W_REFERENCE_UV: u32 = 4;
/// The number of u32 words in a `Channel`'s payload.
pub(crate) const CH_WORDS: u32 = 5;

/// The `CH_W_PIN` sentinel for a pinless internal channel (the temperature sensor).
pub(crate) const NO_PIN: u32 = u32::MAX;

/// Left-justify-and-replicate a `raw` count of `bits` resolution to a 16-bit value (MicroPython's
/// read_u16 contract): `(raw << (16-bits)) | (raw >> (2*bits-16))` for 8..16-bit chips. The
/// replication -- not a multiplicative rescale -- is the silicon-pinned, cross-skin bit-exact form
/// (12-bit: 899 -> 14387).
#[must_use]
pub(crate) fn normalize_u16(raw: u32, bits: u32) -> u32 {
    if bits >= 16 {
        raw & 0xFFFF
    } else {
        let shift_up = 16 - bits;
        let shift_down = 2 * bits - 16;
        ((raw << shift_up) | (raw >> shift_down)) & 0xFFFF
    }
}

/// Integer microvolts for a `raw` count: `raw * reference_uv / (1 << bits)`, truncating (the
/// family's pinned rounding). Uses u64 intermediate so the product never overflows.
#[must_use]
pub(crate) fn raw_to_microvolts(raw: u32, bits: u32, reference_uv: u32) -> u32 {
    let numerator = u64::from(raw) * u64::from(reference_uv);
    (numerator >> bits) as u32
}

/// One step of an ADC driver sequence, replayed over the MMIO seam.
pub(crate) enum AdcOp {
    /// Write `value` to `reg`.
    Write { reg: u32, value: u32 },
    /// Poll until `reg & mask == want` (clk-enabled, converter-ready).
    PollEq { reg: u32, mask: u32, want: u32 },
}

/// The per-chip ADC registers + facts the surface drives conversions with (the block bring-up and
/// the pad prep are the board's ops; the read loop is procedural in the surface).
pub(crate) struct AdcFacts {
    /// The control/status register (AINSEL + EN/START/READY/ERR).
    pub cs: u32,
    /// The conversion-result register.
    pub result: u32,
    /// The ADC clock-generator control (its ENABLED bit is polled at bring-up).
    pub clk_ctrl: u32,
    /// The CS base value keeping the converter + temp bias enabled (EN | TS_EN).
    pub cs_enabled: u32,
    /// The self-clearing one-shot start bit (added to `cs_enabled` for the START write).
    pub cs_start: u32,
    /// CS.READY -- the conversion finished / the converter is usable.
    pub cs_ready: u32,
    /// CS.ERR -- the most recent conversion errored (discard + re-run).
    pub cs_err: u32,
    /// CLK_ADC_CTRL.ENABLED (read-only) -- the generator reports running.
    pub clk_enabled: u32,
    /// The result field mask (the hardware count).
    pub result_mask: u32,
    /// The hardware resolution in bits.
    pub bits: u32,
    /// The reference in microvolts.
    pub reference_uv: u32,
}

/// The target boards' ADC arms, keyed like the other peripheral arms on [`crate::gpio::Board`].
impl crate::gpio::Board {
    /// The `(channel, pin)` a named ADC resource resolves to (`board.A0` -> a pin-backed channel,
    /// `board.TEMP_SENSOR` -> the pinless internal channel), or `None`. The pin is [`NO_PIN`] for a
    /// pinless channel.
    pub(crate) fn adc_resource(self, name: &str) -> Option<(u32, u32)> {
        match self {
            crate::gpio::Board::Rp2350 => adc_rp2350::resource(name),
            _ => None,
        }
    }

    /// The backing pin for a channel number ([`NO_PIN`] for a pinless channel), or `None` when the
    /// channel is out of range -- the `machine.ADC(channel)` / `analogio` int form.
    pub(crate) fn adc_channel_source(self, channel: u32) -> Option<u32> {
        match self {
            crate::gpio::Board::Rp2350 => adc_rp2350::channel_source(channel),
            _ => None,
        }
    }

    /// The ADC register facts, or `None` when this board has no ADC arm.
    pub(crate) fn adc_facts(self) -> Option<AdcFacts> {
        match self {
            crate::gpio::Board::Rp2350 => Some(adc_rp2350::facts()),
            _ => None,
        }
    }

    /// The converter-block bring-up (clk_adc up, block out of reset, converter + temp bias powered):
    /// run once, on the FIRST channel open.
    pub(crate) fn adc_block_init_ops(self) -> Vec<AdcOp> {
        match self {
            crate::gpio::Board::Rp2350 => adc_rp2350::block_init_ops(),
            _ => Vec::new(),
        }
    }

    /// The external-channel pad prep (disable the digital functions so the analogue mux taps the
    /// pad): run when opening a PIN-backed channel.
    pub(crate) fn adc_pad_analog_ops(self, pin: u32) -> Vec<AdcOp> {
        match self {
            crate::gpio::Board::Rp2350 => adc_rp2350::pad_analog_ops(pin),
            _ => Vec::new(),
        }
    }
}
