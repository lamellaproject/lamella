//! The STM32F4 GPIO port C register map (ST RM0090) -- a native-Rust BSP leaf.
//! Addresses are literal facts from the reference manual.

use crate::gpio::{PinRegs, RegOp};
use alloc::{vec, vec::Vec};

/// RCC AHB1 peripheral-clock-enable register.
const RCC_AHB1ENR: u32 = 0x4002_3830;
/// The AHB1ENR bit that ungates GPIO port C.
const RCC_GPIOC_EN: u32 = 1 << 2;
/// GPIO port C base.
const GPIOC_BASE: u32 = 0x4002_0800;
/// Mode register offset (2 bits per pin: 00 input, 01 output).
const GPIO_MODER: u32 = 0x00;
/// Input data register offset.
const GPIO_IDR: u32 = 0x10;
/// Bit set/reset register offset (write 1<<n to set high, 1<<(n+16) to drive low -- atomic).
const GPIO_BSRR: u32 = 0x18;

/// The highest valid pin number on this port (port C has pins 0..=15).
pub(crate) const MAX_PIN: u32 = 15;

/// The precomputed drive registers for `pin`: atomic BSRR set/reset, IDR for reads.
pub(crate) fn pin_regs(pin: u32) -> PinRegs {
    PinRegs {
        pin_id: pin,
        set_reg: GPIOC_BASE + GPIO_BSRR,
        set_val: 1 << pin,
        clr_reg: GPIOC_BASE + GPIO_BSRR,
        clr_val: 1 << (pin + 16),
        read_reg: GPIOC_BASE + GPIO_IDR,
        read_mask: 1 << pin,
    }
}

/// The direction ops for `pin`: one read-modify-write of its 2-bit MODER field (01 = output,
/// 00 = input).
pub(crate) fn direction_ops(pin: u32, output: bool) -> Vec<RegOp> {
    let clear_mask = 0b11u32 << (2 * pin);
    let set_value = if output { 0b01u32 << (2 * pin) } else { 0 };
    vec![RegOp::ClearAndSet { reg: GPIOC_BASE + GPIO_MODER, clear_mask, set_value }]
}

/// The open ops for `pin`: ungate the port clock (a bit in RCC_AHB1ENR), then set its direction.
pub(crate) fn open_ops(pin: u32, output: bool) -> Vec<RegOp> {
    let mut ops = vec![RegOp::SetBits { reg: RCC_AHB1ENR, set_mask: RCC_GPIOC_EN }];
    ops.extend(direction_ops(pin, output));
    ops
}

/// This board's named pins: `board.LED` (the demo LED) and `board.PC0`..`board.PC15`.
pub(crate) fn board_pin_id(name: &str) -> Option<u32> {
    match name {
        "LED" => Some(13),
        _ => name
            .strip_prefix("PC")
            .and_then(|digits| digits.parse::<u32>().ok())
            .filter(|&n| n <= MAX_PIN),
    }
}
