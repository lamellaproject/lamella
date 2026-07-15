//! The clean, first-class Lamella Python UART layer -- and the per-chip facts behind it.

use crate::tables::{uart_esp32c6, uart_rp2040, uart_rp2350};
use alloc::vec::Vec;

/// Method ids for the `uart` module object (dispatched in `ObjectModel::call_uart_method`).
pub(crate) const UART_OPEN: u32 = 0;

/// The `uart` method id for `name`, or `None`.
pub(crate) fn uart_method_id(name: &str) -> Option<u32> {
    match name {
        "open" => Some(UART_OPEN),
        _ => None,
    }
}

/// Method ids for a `Port` object (dispatched in `ObjectModel::call_port_method`). `__enter__` /
/// `__exit__` ride the ordinary attribute path, so `with uart.open(...) as port:` needs no
/// interpreter support.
pub(crate) const PORT_ANY: u32 = 0;
pub(crate) const PORT_READ: u32 = 1;
pub(crate) const PORT_READ_EXACTLY: u32 = 2;
pub(crate) const PORT_READINTO: u32 = 3;
pub(crate) const PORT_READLINE: u32 = 4;
pub(crate) const PORT_WRITE: u32 = 5;
pub(crate) const PORT_FLUSH: u32 = 6;
pub(crate) const PORT_DISCARD_INPUT: u32 = 7;
pub(crate) const PORT_CLOSE: u32 = 8;
pub(crate) const PORT_ENTER: u32 = 9;
pub(crate) const PORT_EXIT: u32 = 10;

/// The `Port` method id for `name`, or `None`.
pub(crate) fn port_method_id(name: &str) -> Option<u32> {
    match name {
        "any" => Some(PORT_ANY),
        "read" => Some(PORT_READ),
        "read_exactly" => Some(PORT_READ_EXACTLY),
        "readinto" => Some(PORT_READINTO),
        "readline" => Some(PORT_READLINE),
        "write" => Some(PORT_WRITE),
        "flush" => Some(PORT_FLUSH),
        "discard_input" => Some(PORT_DISCARD_INPUT),
        "close" => Some(PORT_CLOSE),
        "__enter__" => Some(PORT_ENTER),
        "__exit__" => Some(PORT_EXIT),
        _ => None,
    }
}

pub(crate) const PORT_W_INSTANCE: u32 = 0;
pub(crate) const PORT_W_OPEN: u32 = 1;
pub(crate) const PORT_W_BAUDRATE: u32 = 2;
pub(crate) const PORT_W_DATA_BITS: u32 = 3;
pub(crate) const PORT_W_PARITY: u32 = 4;
pub(crate) const PORT_W_STOP_BITS: u32 = 5;
/// The number of u32 words in a `Port`'s payload.
pub(crate) const PORT_WORDS: u32 = 6;

/// Parity codes stored in a `Port` (and passed through the config).
pub(crate) const PARITY_NONE: u32 = 0;
pub(crate) const PARITY_EVEN: u32 = 1;
pub(crate) const PARITY_ODD: u32 = 2;

/// The parity code for its Python name, or `None` for an unknown name.
pub(crate) fn parity_code(name: &str) -> Option<u32> {
    match name {
        "none" => Some(PARITY_NONE),
        "even" => Some(PARITY_EVEN),
        "odd" => Some(PARITY_ODD),
        _ => None,
    }
}

/// The Python name of a stored parity code.
pub(crate) fn parity_name(code: u32) -> &'static str {
    match code {
        PARITY_EVEN => "even",
        PARITY_ODD => "odd",
        _ => "none",
    }
}

pub(crate) use crate::shims::uart::*;

/// A validated line configuration (the `uart.open` keywords, defaults 115200-8N1).
#[derive(Clone, Copy)]
pub(crate) struct UartConfig {
    pub baudrate: u32,
    pub data_bits: u32,
    pub parity: u32,
    pub stop_bits: u32,
}

impl Default for UartConfig {
    fn default() -> UartConfig {
        UartConfig { baudrate: 115_200, data_bits: 8, parity: PARITY_NONE, stop_bits: 1 }
    }
}

/// A parsed `timeout_ms`: block forever / never block / an integer-ms deadline.
#[derive(Clone, Copy)]
pub(crate) enum UartTimeout {
    Blocking,
    Poll,
    DeadlineMs(u32),
}

/// One step of a driver sequence, replayed over the MMIO seam -- the in-code form of a
/// peripheral table's `sequences` entries.
pub(crate) enum UartOp {
    /// Write `value` to `reg`.
    Write { reg: u32, value: u32 },
    /// Poll until `reg & mask == want` (a self-clearing latch, a drained FIFO).
    PollEq { reg: u32, mask: u32, want: u32 },
    /// Poll until `reg & mask < below` (FIFO room).
    PollBelow { reg: u32, mask: u32, below: u32 },
}

/// How a chip reports its FIFO state -- the two shapes the tables have produced so far.
pub(crate) enum UartStatus {
    /// One status register carrying byte COUNTS (ESP32-C6 style): `any()` is exact.
    Counts { status: u32, rx_shift: u32, rx_mask: u32, tx_shift: u32, tx_mask: u32 },
    /// FLAG bits only (PL011 style): an rx-empty bit, no count -- `any()` honestly reports
    /// 0 or 1 ("at least one byte is immediately readable"), matching what the silicon can say.
    Flags { flags: u32, rx_empty_mask: u32 },
}

/// A config-rejected-by-this-chip reason (validation the generic layer cannot do).
pub(crate) enum UartConfigError {
    /// The divisor cannot express the requested baudrate.
    BaudOutOfRange,
}

/// The per-instance registers every per-op path needs (FIFO push/pop, the status shape), plus
/// the sim's behavioral facts. The full bring-up lives in the board's `open_ops`.
pub(crate) struct UartFacts {
    /// The data register: a write pushes one TX byte; a read pops one RX byte.
    pub fifo: u32,
    /// How RX/TX readiness is reported.
    pub status: UartStatus,
    /// A config-latch register the sim must self-clear on read (0 = the chip has none).
    pub self_clear_reg: u32,
    /// Read-only READY registers the sim must present as already-true (a stable-crystal bit,
    /// reset-done bits) so the driver's real init polls terminate off-device.
    pub sim_ready: &'static [(u32, u32)],
    pub fifo_depth: u32,
}

/// The target boards' UART arms, keyed like the gpio arms on [`crate::gpio::Board`].
impl crate::gpio::Board {
    /// The UART instance number for a named board resource (`board.UART0`), or `None`.
    ///
    /// (Both tabled chips expose one instance so far; a second lands as another name here.)
    pub(crate) fn uart_instance(self, name: &str) -> Option<u32> {
        match self {
            crate::gpio::Board::Esp32c6
            | crate::gpio::Board::Rp2040
            | crate::gpio::Board::Rp2350 => match name {
                "UART0" => Some(0),
                _ => None,
            },
            _ => None,
        }
    }

    /// The per-op register facts for `instance`, or `None` when this board has no UART arm (or
    /// no such instance).
    pub(crate) fn uart_facts(self, instance: u32) -> Option<UartFacts> {
        match self {
            crate::gpio::Board::Esp32c6 if instance == 0 => Some(uart_esp32c6::facts()),
            crate::gpio::Board::Rp2040 if instance == 0 => Some(uart_rp2040::facts()),
            crate::gpio::Board::Rp2350 if instance == 0 => Some(uart_rp2350::facts()),
            _ => None,
        }
    }

    /// The ordered bring-up sequence opening `instance` with `config` (pre-validated), or `None`
    /// when the board/instance has no UART arm; a config this chip's table cannot express is the
    /// inner `Err`.
    pub(crate) fn uart_open_ops(
        self,
        instance: u32,
        config: &UartConfig,
    ) -> Option<Result<Vec<UartOp>, UartConfigError>> {
        match self {
            crate::gpio::Board::Esp32c6 if instance == 0 => Some(uart_esp32c6::open_ops(config)),
            crate::gpio::Board::Rp2040 if instance == 0 => Some(uart_rp2040::open_ops(config)),
            crate::gpio::Board::Rp2350 if instance == 0 => Some(uart_rp2350::open_ops(config)),
            _ => None,
        }
    }

    /// The per-byte transmit sequence (FIFO-room poll + the push).
    pub(crate) fn uart_tx_byte_ops(self, instance: u32, byte: u8) -> Vec<UartOp> {
        match self {
            crate::gpio::Board::Esp32c6 if instance == 0 => uart_esp32c6::tx_byte_ops(byte),
            crate::gpio::Board::Rp2040 if instance == 0 => uart_rp2040::tx_byte_ops(byte),
            crate::gpio::Board::Rp2350 if instance == 0 => uart_rp2350::tx_byte_ops(byte),
            _ => Vec::new(),
        }
    }

    /// The table-fixed console pins of `instance` (TX, RX) -- `board.TX`/`board.RX` for the
    /// CircuitPython idiom, and the values a shim's tx=/rx= override must MATCH (a different
    /// pin is rejected loudly; the table wires the pins, never a silent mismatch).
    pub(crate) fn uart_default_pins(self, instance: u32) -> Option<(u32, u32)> {
        match self {
            crate::gpio::Board::Esp32c6 if instance == 0 => Some(uart_esp32c6::DEFAULT_PINS),
            crate::gpio::Board::Rp2040 if instance == 0 => Some(uart_rp2040::DEFAULT_PINS),
            crate::gpio::Board::Rp2350 if instance == 0 => Some(uart_rp2350::DEFAULT_PINS),
            _ => None,
        }
    }

    /// The transmit-drained poll (`flush`).
    pub(crate) fn uart_flush_ops(self, instance: u32) -> Vec<UartOp> {
        match self {
            crate::gpio::Board::Esp32c6 if instance == 0 => uart_esp32c6::flush_ops(),
            crate::gpio::Board::Rp2040 if instance == 0 => uart_rp2040::flush_ops(),
            crate::gpio::Board::Rp2350 if instance == 0 => uart_rp2350::flush_ops(),
            _ => Vec::new(),
        }
    }
}
