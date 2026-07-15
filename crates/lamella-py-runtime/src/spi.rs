//! The clean, first-class Lamella Python SPI layer -- and the per-chip facts behind it.

use crate::tables::spi_rp2350;
use alloc::vec::Vec;

/// Method ids for the `spi` module object (dispatched in `ObjectModel::call_spi_method`).
pub(crate) const SPI_OPEN: u32 = 0;

/// The `spi` method id for `name`, or `None`.
pub(crate) fn spi_method_id(name: &str) -> Option<u32> {
    match name {
        "open" => Some(SPI_OPEN),
        _ => None,
    }
}

/// Method ids for a `SpiBus` object (dispatched in `ObjectModel::call_spi_bus_method`).
/// `__enter__` / `__exit__` ride the ordinary attribute path.
pub(crate) const BUS_TRANSFER: u32 = 0;
pub(crate) const BUS_WRITE: u32 = 1;
pub(crate) const BUS_READ: u32 = 2;
pub(crate) const BUS_TRANSFER_INTO: u32 = 3;
pub(crate) const BUS_CLOSE: u32 = 4;
pub(crate) const BUS_ENTER: u32 = 5;
pub(crate) const BUS_EXIT: u32 = 6;

/// The `SpiBus` method id for `name`, or `None`.
pub(crate) fn spi_bus_method_id(name: &str) -> Option<u32> {
    match name {
        "transfer" => Some(BUS_TRANSFER),
        "write" => Some(BUS_WRITE),
        "read" => Some(BUS_READ),
        "transfer_into" => Some(BUS_TRANSFER_INTO),
        "close" => Some(BUS_CLOSE),
        "__enter__" => Some(BUS_ENTER),
        "__exit__" => Some(BUS_EXIT),
        _ => None,
    }
}

pub(crate) const BUS_W_INSTANCE: u32 = 0;
pub(crate) const BUS_W_OPEN: u32 = 1;
/// The REALIZED clock rate (never-exceed), echoed by the read-only `frequency` (the Lamella
/// standard unifies SPI + I2C on `frequency=` -- the .NET IoT `ClockFrequency` cross-skin term;
/// the machine/busio shims keep their `baudrate=` idiom via translation).
pub(crate) const BUS_W_FREQUENCY: u32 = 2;
pub(crate) const BUS_W_MODE: u32 = 3;
pub(crate) const BUS_W_BIT_ORDER: u32 = 4;
/// The managed chip-select pin, or [`NO_CS`] for a raw bus.
pub(crate) const BUS_W_CS_PIN: u32 = 5;
/// The number of u32 words in a `SpiBus`'s payload.
pub(crate) const BUS_WORDS: u32 = 6;

/// The `BUS_W_CS_PIN` sentinel for a raw bus (no managed chip-select).
pub(crate) const NO_CS: u32 = u32::MAX;

/// Bit-order codes stored in a bus (and passed through the config).
pub(crate) const BIT_ORDER_MSB: u32 = 0;
pub(crate) const BIT_ORDER_LSB: u32 = 1;

/// The bit-order code for its Python name, or `None` for an unknown name.
pub(crate) fn bit_order_code(name: &str) -> Option<u32> {
    match name {
        "msb" => Some(BIT_ORDER_MSB),
        "lsb" => Some(BIT_ORDER_LSB),
        _ => None,
    }
}

/// The Python name of a stored bit-order code.
pub(crate) fn bit_order_name(code: u32) -> &'static str {
    if code == BIT_ORDER_LSB { "lsb" } else { "msb" }
}

/// A validated SPI configuration (the `spi.open` keywords, defaults 1 MHz / mode 0 / MSB).
#[derive(Clone, Copy)]
pub(crate) struct SpiConfig {
    /// The REQUESTED clock rate; the realized (never-exceed) rate comes back from `spi_open_ops`.
    pub frequency: u32,
    /// CPOL<<1 | CPHA (0..3).
    pub mode: u32,
    /// The stored bit-order code ([`BIT_ORDER_MSB`] / [`BIT_ORDER_LSB`]).
    pub bit_order: u32,
}

impl Default for SpiConfig {
    fn default() -> SpiConfig {
        SpiConfig { frequency: 1_000_000, mode: 0, bit_order: BIT_ORDER_MSB }
    }
}

/// One step of an SPI driver sequence, replayed over the MMIO seam.
pub(crate) enum SpiOp {
    /// Write `value` to `reg`.
    Write { reg: u32, value: u32 },
    /// Poll until `reg & mask == want` (a FIFO-room / reply-ready flag).
    PollEq { reg: u32, mask: u32, want: u32 },
    /// Read `reg & mask` and CAPTURE it as one inbound (full-duplex MISO) byte.
    ReadInto { reg: u32, mask: u32 },
}

/// A config-rejected-by-this-chip reason (validation the generic layer cannot do).
pub(crate) enum SpiConfigError {
    /// The prescaler cannot reach the requested rate (below the divider floor).
    BaudUnreachable,
    /// LSB-first requested where the table has no bit-order field (the PL022 is MSB-only).
    BitOrderNotTabled,
}

/// The per-instance registers the sim needs to model a transfer (the data register, the status
/// shape), plus the sim's ready facts. The full bring-up lives in the board's `spi_open_ops`.
pub(crate) struct SpiFacts {
    /// The data register: a write pushes one TX byte; a read pops the full-duplex reply.
    pub data_reg: u32,
    /// The status register the transfer polls.
    pub status_reg: u32,
    /// Status bits the sim reads as constantly set when idle (TX empty + TX-FIFO room).
    pub status_idle_flags: u32,
    /// The status bit set when a full-duplex reply is waiting to be read (RX not empty).
    pub status_rx_ready: u32,
    /// Read-only READY registers the sim presents as already-true (reset-done bits) so the
    /// driver's real init polls terminate off-device.
    pub sim_ready: &'static [(u32, u32)],
}

/// The target boards' SPI arms, keyed like the gpio/uart arms on [`crate::gpio::Board`].
impl crate::gpio::Board {
    /// The SPI instance number for a named board resource (`board.SPI0`), or `None`.
    pub(crate) fn spi_instance(self, name: &str) -> Option<u32> {
        match self {
            crate::gpio::Board::Rp2350 => match name {
                "SPI0" => Some(0),
                _ => None,
            },
            _ => None,
        }
    }

    /// The per-op register facts for `instance`, or `None` when this board has no SPI arm.
    pub(crate) fn spi_facts(self, instance: u32) -> Option<SpiFacts> {
        match self {
            crate::gpio::Board::Rp2350 if instance == 0 => Some(spi_rp2350::facts()),
            _ => None,
        }
    }

    /// The ordered bring-up opening `instance` with `config`, paired with the REALIZED (never-
    /// exceed) bit rate; `None` when the board/instance has no SPI arm, the inner `Err` when the
    /// config this chip's table cannot express.
    pub(crate) fn spi_open_ops(
        self,
        instance: u32,
        config: &SpiConfig,
    ) -> Option<Result<(Vec<SpiOp>, u32), SpiConfigError>> {
        match self {
            crate::gpio::Board::Rp2350 if instance == 0 => Some(spi_rp2350::open_ops(config)),
            _ => None,
        }
    }

    /// The SSP-block reprogram alone for `busio.SPI.configure` (no clock bring-up), paired with the
    /// realized rate; `None`/inner `Err` as [`Board::spi_open_ops`].
    pub(crate) fn spi_reconfigure_ops(
        self,
        instance: u32,
        config: &SpiConfig,
    ) -> Option<Result<(Vec<SpiOp>, u32), SpiConfigError>> {
        match self {
            crate::gpio::Board::Rp2350 if instance == 0 => Some(spi_rp2350::reconfigure_ops(config)),
            _ => None,
        }
    }

    /// The full-duplex transfer of one `byte` (FIFO-room poll, the push, the reply-ready poll, the
    /// inbound read).
    pub(crate) fn spi_transfer_byte_ops(self, instance: u32, byte: u8) -> Vec<SpiOp> {
        match self {
            crate::gpio::Board::Rp2350 if instance == 0 => spi_rp2350::transfer_byte_ops(byte),
            _ => Vec::new(),
        }
    }

    /// The pins this instance's bring-up already muxes to the SPI function -- a `cs=` naming one of
    /// them is rejected (it would fight the hardware chip-select / clock / data lines).
    pub(crate) fn spi_function_pins(self, instance: u32) -> &'static [u32] {
        match self {
            crate::gpio::Board::Rp2350 if instance == 0 => spi_rp2350::FUNCTION_PINS,
            _ => &[],
        }
    }

    /// The instance's `(sck, mosi, miso)` pins -- what a `busio.SPI(clock, MOSI, MISO)` shim's pin
    /// arguments must match (the table wires the pins; a differing pin fails loud).
    pub(crate) fn spi_pins(self, instance: u32) -> Option<(u32, u32, u32)> {
        match self {
            crate::gpio::Board::Rp2350 if instance == 0 => Some(spi_rp2350::SCK_MOSI_MISO),
            _ => None,
        }
    }
}
