//! The clean, first-class Lamella Python GPIO layer -- and the board-support facts behind it.

/// Method ids for the `gpio` module object (dispatched in `ObjectModel::call_gpio_method`).
pub(crate) const GPIO_OUTPUT: u32 = 0;
pub(crate) const GPIO_INPUT: u32 = 1;

/// The `gpio` method id for `name`, or `None`.
pub(crate) fn gpio_method_id(name: &str) -> Option<u32> {
    match name {
        "output" => Some(GPIO_OUTPUT),
        "input" => Some(GPIO_INPUT),
        _ => None,
    }
}

/// Method ids for a `Pin` object (dispatched in `ObjectModel::call_pin_method`). `on`/`off`
/// alias `high`/`low`; `value` reads with no argument or writes with one.
pub(crate) const PIN_HIGH: u32 = 0;
pub(crate) const PIN_LOW: u32 = 1;
pub(crate) const PIN_TOGGLE: u32 = 2;
pub(crate) const PIN_VALUE: u32 = 3;
pub(crate) const PIN_READ: u32 = 4;
pub(crate) const PIN_DEINIT: u32 = 5;

/// The `Pin` method id for `name`, or `None`.
pub(crate) fn pin_method_id(name: &str) -> Option<u32> {
    match name {
        "high" | "on" => Some(PIN_HIGH),
        "low" | "off" => Some(PIN_LOW),
        "toggle" => Some(PIN_TOGGLE),
        "value" => Some(PIN_VALUE),
        "read" => Some(PIN_READ),
        "deinit" => Some(PIN_DEINIT),
        _ => None,
    }
}


/// `machine.Pin.IN` -- configure the pin as an input.
pub(crate) const MACHINE_PIN_IN: u32 = 0;
/// `machine.Pin.OUT` -- configure the pin as an output.
pub(crate) const MACHINE_PIN_OUT: u32 = 1;

/// The `machine.Pin` mode constant for `name` (`OUT`/`IN`), or `None`.
pub(crate) fn machine_pin_const(name: &str) -> Option<u32> {
    match name {
        "IN" => Some(MACHINE_PIN_IN),
        "OUT" => Some(MACHINE_PIN_OUT),
        _ => None,
    }
}

pub(crate) fn direction_const(name: &str) -> Option<u32> {
    match name {
        "OUTPUT" => Some(PIN_MODE_OUTPUT),
        "INPUT" => Some(PIN_MODE_INPUT),
        _ => None,
    }
}

pub(crate) const PIN_W_ID: u32 = 0;
pub(crate) const PIN_W_SET_REG: u32 = 1;
pub(crate) const PIN_W_SET_VAL: u32 = 2;
pub(crate) const PIN_W_CLR_REG: u32 = 3;
pub(crate) const PIN_W_CLR_VAL: u32 = 4;
pub(crate) const PIN_W_READ_REG: u32 = 5;
pub(crate) const PIN_W_READ_MASK: u32 = 6;
pub(crate) const PIN_W_CUR: u32 = 7;
pub(crate) const PIN_W_MODE: u32 = 8;
/// The number of u32 words in a `Pin`'s payload.
pub(crate) const PIN_WORDS: u32 = 9;

pub(crate) const PIN_MODE_INPUT: u32 = 0;
pub(crate) const PIN_MODE_OUTPUT: u32 = 1;

/// The precomputed drive registers a `Pin` carries, so a per-op drive is one register write.
pub(crate) struct PinRegs {
    pub pin_id: u32,
    pub set_reg: u32,
    pub set_val: u32,
    pub clr_reg: u32,
    pub clr_val: u32,
    pub read_reg: u32,
    pub read_mask: u32,
}


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

/// The port's clock-enable register + bit, ungated once before use.
pub(crate) const CLOCK_ENABLE_REG: u32 = RCC_AHB1ENR;
pub(crate) const CLOCK_ENABLE_BIT: u32 = RCC_GPIOC_EN;
/// The direction (MODER) register whose 2-bit field selects a pin's direction.
pub(crate) const MODER_REG: u32 = GPIOC_BASE + GPIO_MODER;

/// The highest valid pin number on this board's port (port C has pins 0..=15).
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

/// The `(clear_mask, set_value)` for `pin`'s 2-bit MODER field: `output` selects 01, else 00.
/// The caller does `(moder & !clear_mask) | set_value`.
pub(crate) fn moder_bits(pin: u32, output: bool) -> (u32, u32) {
    let clear_mask = 0b11u32 << (2 * pin);
    let set_value = if output { 0b01u32 << (2 * pin) } else { 0 };
    (clear_mask, set_value)
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
