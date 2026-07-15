//! The Raspberry Pi RP2350 (Pico 2) GPIO map, driving pins through the single-cycle IO block (SIO).
//! Addresses are literal facts from the RP2350 datasheet (SIO, IO_BANK0, PADS_BANK0, RESETS).

use crate::gpio::{PinRegs, RegOp};
use alloc::{vec, vec::Vec};

/// Single-cycle IO block: per-pin drive + read via atomic set/clear aliases.
const SIO: u32 = 0xd000_0000;
const SIO_IN: u32 = SIO + 0x004;
const SIO_OUT_SET: u32 = SIO + 0x018;
const SIO_OUT_CLR: u32 = SIO + 0x020;
const SIO_OE_SET: u32 = SIO + 0x038;
const SIO_OE_CLR: u32 = SIO + 0x040;

/// IO_BANK0: GPIOn_CTRL at 8n + 4; FUNCSEL 5 routes the pin to the SIO (software GPIO).
const IO_BANK0: u32 = 0x4002_8000;
const FUNCSEL_SIO: u32 = 5;

/// PADS_BANK0: GPIOn at 4n + 4. A written value with ISO (bit 8) clear de-isolates the pad
/// (RP2350 pads reset isolated); IE (bit 6) enables the input buffer so SIO_IN reads the level.
const PADS_BANK0: u32 = 0x4003_8000;
const PAD_IE: u32 = 1 << 6;

/// RESETS: the atomic-clear alias (+0x3000) brings a peripheral out of reset by writing its bit.
const RESETS_CLR: u32 = 0x4002_0000 + 0x3000;
const RESET_IO_BANK0: u32 = 1 << 6;
const RESET_PADS_BANK0: u32 = 1 << 9;

/// The Pico 2 exposes GPIO 0..=29 (RP2350A); SIO bank 0 covers them in one 32-bit word.
pub(crate) const MAX_PIN: u32 = 29;

/// The precomputed drive registers for `pin`: SIO atomic OUT_SET / OUT_CLR, IN for reads. Unlike
/// the STM32's shared BSRR, high and low are DISTINCT registers, each written `1 << pin`.
pub(crate) fn pin_regs(pin: u32) -> PinRegs {
    PinRegs {
        pin_id: pin,
        set_reg: SIO_OUT_SET,
        set_val: 1 << pin,
        clr_reg: SIO_OUT_CLR,
        clr_val: 1 << pin,
        read_reg: SIO_IN,
        read_mask: 1 << pin,
    }
}

/// The direction ops for `pin`: one atomic SIO output-enable set (output) or clear (input).
pub(crate) fn direction_ops(pin: u32, output: bool) -> Vec<RegOp> {
    let reg = if output { SIO_OE_SET } else { SIO_OE_CLR };
    vec![RegOp::Write { reg, value: 1 << pin }]
}

/// The open ops for `pin`: un-reset the GPIO mux + pads, de-isolate the pad, route the pin to the
/// SIO, then set its direction. The interpreter path leaves ample settle between the un-reset and
/// the dependent writes, so it does not busy-poll RESETS_DONE the way a tight bare-metal loop does.
pub(crate) fn open_ops(pin: u32, output: bool) -> Vec<RegOp> {
    let mut ops = vec![
        RegOp::Write { reg: RESETS_CLR, value: RESET_IO_BANK0 | RESET_PADS_BANK0 },
        RegOp::Write { reg: PADS_BANK0 + 4 + 4 * pin, value: PAD_IE },
        RegOp::Write { reg: IO_BANK0 + 4 + 8 * pin, value: FUNCSEL_SIO },
    ];
    ops.extend(direction_ops(pin, output));
    ops
}

/// This board's named pins: `board.LED` (GPIO 25 on the Pico 2) and `board.GP0`..`board.GP29`.
pub(crate) fn board_pin_id(name: &str) -> Option<u32> {
    match name {
        "LED" => Some(25),
        _ => name
            .strip_prefix("GP")
            .and_then(|digits| digits.parse::<u32>().ok())
            .filter(|&n| n <= MAX_PIN),
    }
}
