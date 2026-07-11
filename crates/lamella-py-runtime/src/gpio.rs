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

/// The `digitalio.Direction` constant for `name` -- `OUTPUT`/`INPUT`, valued to match
/// the `Pin` mode words so `led.direction == Direction.OUTPUT` compares directly.
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

/// One register operation in a pin's setup, replayed over the MMIO seam by the caller. Boards differ
/// in HOW a pin is configured -- a single read-modify-write field (STM32F4's MODER) versus a sequence
/// of atomic-alias writes (the RP2350's RESETS / IO_BANK0 / SIO) -- so a board describes its setup as
/// an ordered list of these, and the setup stays board-neutral.
pub(crate) enum RegOp {
    /// Write `value` to `reg` outright -- an atomic set/clear alias, a function-select, a pad config
    /// (no read needed; the register's own semantics apply the effect).
    Write { reg: u32, value: u32 },
    /// Read-modify-write, setting bits: `reg = reg | set_mask` (ungate a clock bit, leaving the rest).
    SetBits { reg: u32, set_mask: u32 },
    /// Read-modify-write a bit field: `reg = (reg & !clear_mask) | set_value` (a MODER direction field).
    ClearAndSet { reg: u32, clear_mask: u32, set_value: u32 },
}


/// The target board whose register map the gpio layer drives. Selected per deployment (the entry sets
/// it via `ObjectModel::set_board`).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Board {
    /// STM32F4 GPIO port C (ST RM0090) -- the first blinky's target and the default.
    #[default]
    Stm32f4,
    /// Raspberry Pi RP2350 (Pico 2), driving GPIO through the single-cycle IO block (SIO).
    Rp2350,
    /// Espressif ESP32-C6 (DevKitC-1). UART-first: its gpio facts are not tabled yet, so the
    /// gpio arms are empty (a gpio claim fails loud as unsupported) while the UART arm is live.
    Esp32c6,
    /// Raspberry Pi RP2040 (Pico H). UART-first like the C6 -- its PL011 arm is live; the
    /// RP2040 gpio facts (distinct block bases from the RP2350's) wait on their own table.
    Rp2040,
}

impl Board {
    /// The pin number for a named board pin (`board.LED`, `board.PC13`), or `None`.
    pub(crate) fn pin_id(self, name: &str) -> Option<u32> {
        match self {
            Board::Stm32f4 => stm32f4::board_pin_id(name),
            Board::Rp2350 => rp2350::board_pin_id(name),
            Board::Esp32c6 | Board::Rp2040 => None,
        }
    }

    /// The precomputed drive registers for `pin` (atomic set/reset + the read register), stamped into
    /// a `Pin` so a per-op drive is one register write.
    pub(crate) fn pin_regs(self, pin: u32) -> PinRegs {
        match self {
            Board::Stm32f4 => stm32f4::pin_regs(pin),
            Board::Rp2350 => rp2350::pin_regs(pin),
            Board::Esp32c6 | Board::Rp2040 => PinRegs {
                pin_id: pin,
                set_reg: 0,
                set_val: 0,
                clr_reg: 0,
                clr_val: 0,
                read_reg: 0,
                read_mask: 0,
            },
        }
    }

    /// The register ops that set an already-open `pin`'s DIRECTION (output vs input) -- the in-place
    /// `pin.direction = ...` change, without re-running the one-time port bring-up.
    pub(crate) fn direction_ops(self, pin: u32, output: bool) -> alloc::vec::Vec<RegOp> {
        match self {
            Board::Stm32f4 => stm32f4::direction_ops(pin, output),
            Board::Rp2350 => rp2350::direction_ops(pin, output),
            Board::Esp32c6 | Board::Rp2040 => alloc::vec::Vec::new(),
        }
    }

    /// The register ops that OPEN `pin` in the given direction from reset: the one-time port bring-up
    /// (clock ungate / peripheral un-reset, pad, function-select) followed by the direction ops.
    pub(crate) fn open_ops(self, pin: u32, output: bool) -> alloc::vec::Vec<RegOp> {
        match self {
            Board::Stm32f4 => stm32f4::open_ops(pin, output),
            Board::Rp2350 => rp2350::open_ops(pin, output),
            Board::Esp32c6 | Board::Rp2040 => alloc::vec::Vec::new(),
        }
    }

    /// The highest valid pin number on the board's port.
    pub(crate) fn max_pin(self) -> u32 {
        match self {
            Board::Stm32f4 => stm32f4::MAX_PIN,
            Board::Rp2350 => rp2350::MAX_PIN,
            Board::Esp32c6 | Board::Rp2040 => 0,
        }
    }

    /// Whether this board's GPIO arm is populated (the ESP32-C6 arm is UART-first: gpio waits
    /// on its table).
    pub(crate) fn gpio_supported(self) -> bool {
        !matches!(self, Board::Esp32c6 | Board::Rp2040)
    }
}

/// The STM32F4 GPIO port C register map (ST RM0090) -- the native-Rust BSP for the first blinky.
/// Addresses are literal facts from the reference manual.
mod stm32f4 {
    use super::{PinRegs, RegOp};
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
    pub(super) const MAX_PIN: u32 = 15;

    /// The precomputed drive registers for `pin`: atomic BSRR set/reset, IDR for reads.
    pub(super) fn pin_regs(pin: u32) -> PinRegs {
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
    pub(super) fn direction_ops(pin: u32, output: bool) -> Vec<RegOp> {
        let clear_mask = 0b11u32 << (2 * pin);
        let set_value = if output { 0b01u32 << (2 * pin) } else { 0 };
        vec![RegOp::ClearAndSet { reg: GPIOC_BASE + GPIO_MODER, clear_mask, set_value }]
    }

    /// The open ops for `pin`: ungate the port clock (a bit in RCC_AHB1ENR), then set its direction.
    pub(super) fn open_ops(pin: u32, output: bool) -> Vec<RegOp> {
        let mut ops = vec![RegOp::SetBits { reg: RCC_AHB1ENR, set_mask: RCC_GPIOC_EN }];
        ops.extend(direction_ops(pin, output));
        ops
    }

    /// This board's named pins: `board.LED` (the demo LED) and `board.PC0`..`board.PC15`.
    pub(super) fn board_pin_id(name: &str) -> Option<u32> {
        match name {
            "LED" => Some(13),
            _ => name
                .strip_prefix("PC")
                .and_then(|digits| digits.parse::<u32>().ok())
                .filter(|&n| n <= MAX_PIN),
        }
    }
}

/// The Raspberry Pi RP2350 (Pico 2) GPIO map, driving pins through the single-cycle IO block (SIO).
/// Addresses are literal facts from the RP2350 datasheet (SIO, IO_BANK0, PADS_BANK0, RESETS).
mod rp2350 {
    use super::{PinRegs, RegOp};
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
    pub(super) const MAX_PIN: u32 = 29;

    /// The precomputed drive registers for `pin`: SIO atomic OUT_SET / OUT_CLR, IN for reads. Unlike
    /// the STM32's shared BSRR, high and low are DISTINCT registers, each written `1 << pin`.
    pub(super) fn pin_regs(pin: u32) -> PinRegs {
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
    pub(super) fn direction_ops(pin: u32, output: bool) -> Vec<RegOp> {
        let reg = if output { SIO_OE_SET } else { SIO_OE_CLR };
        vec![RegOp::Write { reg, value: 1 << pin }]
    }

    /// The open ops for `pin`: un-reset the GPIO mux + pads, de-isolate the pad, route the pin to the
    /// SIO, then set its direction. The interpreter path leaves ample settle between the un-reset and
    /// the dependent writes, so it does not busy-poll RESETS_DONE the way a tight bare-metal loop does.
    pub(super) fn open_ops(pin: u32, output: bool) -> Vec<RegOp> {
        let mut ops = vec![
            RegOp::Write { reg: RESETS_CLR, value: RESET_IO_BANK0 | RESET_PADS_BANK0 },
            RegOp::Write { reg: PADS_BANK0 + 4 + 4 * pin, value: PAD_IE },
            RegOp::Write { reg: IO_BANK0 + 4 + 8 * pin, value: FUNCSEL_SIO },
        ];
        ops.extend(direction_ops(pin, output));
        ops
    }

    /// This board's named pins: `board.LED` (GPIO 25 on the Pico 2) and `board.GP0`..`board.GP29`.
    pub(super) fn board_pin_id(name: &str) -> Option<u32> {
        match name {
            "LED" => Some(25),
            _ => name
                .strip_prefix("GP")
                .and_then(|digits| digits.parse::<u32>().ok())
                .filter(|&n| n <= MAX_PIN),
        }
    }
}
