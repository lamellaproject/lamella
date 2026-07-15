//! The clean, first-class Lamella Python GPIO layer -- and the board-support facts behind it.

use crate::tables::{gpio_rp2350, gpio_stm32f4};

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
            Board::Stm32f4 => gpio_stm32f4::board_pin_id(name),
            Board::Rp2350 => gpio_rp2350::board_pin_id(name),
            Board::Esp32c6 | Board::Rp2040 => None,
        }
    }

    /// The precomputed drive registers for `pin` (atomic set/reset + the read register), stamped into
    /// a `Pin` so a per-op drive is one register write.
    pub(crate) fn pin_regs(self, pin: u32) -> PinRegs {
        match self {
            Board::Stm32f4 => gpio_stm32f4::pin_regs(pin),
            Board::Rp2350 => gpio_rp2350::pin_regs(pin),
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
            Board::Stm32f4 => gpio_stm32f4::direction_ops(pin, output),
            Board::Rp2350 => gpio_rp2350::direction_ops(pin, output),
            Board::Esp32c6 | Board::Rp2040 => alloc::vec::Vec::new(),
        }
    }

    /// The register ops that OPEN `pin` in the given direction from reset: the one-time port bring-up
    /// (clock ungate / peripheral un-reset, pad, function-select) followed by the direction ops.
    pub(crate) fn open_ops(self, pin: u32, output: bool) -> alloc::vec::Vec<RegOp> {
        match self {
            Board::Stm32f4 => gpio_stm32f4::open_ops(pin, output),
            Board::Rp2350 => gpio_rp2350::open_ops(pin, output),
            Board::Esp32c6 | Board::Rp2040 => alloc::vec::Vec::new(),
        }
    }

    /// The highest valid pin number on the board's port.
    pub(crate) fn max_pin(self) -> u32 {
        match self {
            Board::Stm32f4 => gpio_stm32f4::MAX_PIN,
            Board::Rp2350 => gpio_rp2350::MAX_PIN,
            Board::Esp32c6 | Board::Rp2040 => 0,
        }
    }

    /// Whether this board's GPIO arm is populated (the ESP32-C6 arm is UART-first: gpio waits
    /// on its table).
    pub(crate) fn gpio_supported(self) -> bool {
        !matches!(self, Board::Esp32c6 | Board::Rp2040)
    }

    /// The board's RESETS `(clear-alias, done)` register pair, or `None` when the board has no such
    /// block. The host sim accumulates writes to the clear-alias and reflects them in the done
    /// register, so peripherals sharing one done register each observe their own bit cleared. (The
    /// ESP32-C6 clocks/resets through PCR, not a RESETS block, so it has no pair.)
    pub(crate) fn reset_regs(self) -> Option<(u32, u32)> {
        match self {
            Board::Rp2350 => Some((0x4002_3000, 0x4002_0008)),
            Board::Rp2040 => Some((0x4000_F000, 0x4000_C008)),
            Board::Stm32f4 | Board::Esp32c6 => None,
        }
    }
}
