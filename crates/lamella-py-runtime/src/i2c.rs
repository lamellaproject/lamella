//! The clean, first-class Lamella Python I2C layer -- and the per-chip facts behind it.

use crate::tables::i2c_rp2350;
use alloc::vec::Vec;

/// The 7-bit address range a scan walks: 0x00-0x07 and 0x78-0x7F are I2C-reserved.
pub(crate) const SCAN_START: u8 = 0x08;
pub(crate) const SCAN_END: u8 = 0x77;

/// Method ids for the `i2c` module object (dispatched in `ObjectModel::call_i2c_method`).
pub(crate) const I2C_OPEN: u32 = 0;

/// The `i2c` method id for `name`, or `None`.
pub(crate) fn i2c_method_id(name: &str) -> Option<u32> {
    match name {
        "open" => Some(I2C_OPEN),
        _ => None,
    }
}

/// Method ids for an `I2cBus` object (dispatched in `ObjectModel::call_i2c_bus_method`).
pub(crate) const BUS_SCAN: u32 = 0;
pub(crate) const BUS_PROBE: u32 = 1;
pub(crate) const BUS_READ: u32 = 2;
pub(crate) const BUS_WRITE: u32 = 3;
pub(crate) const BUS_WRITE_THEN_READ: u32 = 4;
pub(crate) const BUS_READ_REGISTER: u32 = 5;
pub(crate) const BUS_WRITE_REGISTER: u32 = 6;
pub(crate) const BUS_CLOSE: u32 = 7;
pub(crate) const BUS_ENTER: u32 = 8;
pub(crate) const BUS_EXIT: u32 = 9;

/// The `I2cBus` method id for `name`, or `None`.
pub(crate) fn i2c_bus_method_id(name: &str) -> Option<u32> {
    match name {
        "scan" => Some(BUS_SCAN),
        "probe" => Some(BUS_PROBE),
        "read" => Some(BUS_READ),
        "write" => Some(BUS_WRITE),
        "write_then_read" => Some(BUS_WRITE_THEN_READ),
        "read_register" => Some(BUS_READ_REGISTER),
        "write_register" => Some(BUS_WRITE_REGISTER),
        "close" => Some(BUS_CLOSE),
        "__enter__" => Some(BUS_ENTER),
        "__exit__" => Some(BUS_EXIT),
        _ => None,
    }
}

pub(crate) const BUS_W_INSTANCE: u32 = 0;
pub(crate) const BUS_W_OPEN: u32 = 1;
/// The REALIZED SCL rate (never-exceed), echoed by the read-only `frequency`.
pub(crate) const BUS_W_FREQUENCY: u32 = 2;
/// The number of u32 words in an `I2cBus`'s payload.
pub(crate) const BUS_WORDS: u32 = 3;

/// A validated I2C configuration (the `i2c.open` keyword, default 100 kHz standard mode).
#[derive(Clone, Copy)]
pub(crate) struct I2cConfig {
    /// The REQUESTED SCL rate; the realized (never-exceed) rate comes back from `i2c_open_ops`.
    pub frequency: u32,
}

impl Default for I2cConfig {
    fn default() -> I2cConfig {
        I2cConfig { frequency: 100_000 }
    }
}

/// One step of an I2C INIT sequence, replayed over the MMIO seam. (The transactions themselves are
/// procedural in the surface -- their byte counts and abort handling are data-dependent.)
pub(crate) enum I2cOp {
    /// Write `value` to `reg`.
    Write { reg: u32, value: u32 },
    /// Poll until `reg & mask == want` (a reset-done latch).
    PollEq { reg: u32, mask: u32, want: u32 },
}

/// A config-rejected-by-this-chip reason.
pub(crate) enum I2cConfigError {
    /// The requested SCL rate is outside the counters' expressible range.
    FrequencyUnreachable,
}

/// The DW_apb_i2c registers + framing bits the surface drives a transaction with, plus the sim's
/// ready facts. The full bring-up lives in the board's `i2c_open_ops`.
pub(crate) struct I2cFacts {
    pub enable: u32,
    pub tar: u32,
    pub data_cmd: u32,
    pub raw_intr_stat: u32,
    pub abort_source: u32,
    pub clr_tx_abrt: u32,
    pub clr_stop_det: u32,
    pub rxflr: u32,
    pub status: u32,
    /// IC_STATUS.TFNF -- TX FIFO has room.
    pub status_tfnf: u32,
    /// IC_RAW_INTR_STAT.TX_EMPTY -- the shift register finished (write completion).
    pub intr_tx_empty: u32,
    /// IC_RAW_INTR_STAT.TX_ABRT -- the transfer aborted (a NACK).
    pub intr_tx_abrt: u32,
    /// IC_RAW_INTR_STAT.STOP_DET -- the bus reached STOP.
    pub intr_stop_det: u32,
    /// IC_DATA_CMD.CMD -- clock a read (vs write the low byte).
    pub cmd_read: u32,
    /// IC_DATA_CMD.STOP -- end the transaction after this word.
    pub cmd_stop: u32,
    /// IC_DATA_CMD.RESTART -- issue a repeated START before this word (write_then_read).
    pub cmd_restart: u32,
    /// IC_TX_ABRT_SOURCE.ABRT_7B_ADDR_NOACK -- nobody answered the address.
    pub abrt_addr_nack: u32,
    /// IC_TX_ABRT_SOURCE.ABRT_TXDATA_NOACK -- a data byte drew no ack.
    pub abrt_data_nack: u32,
}

/// The target boards' I2C arms, keyed like the gpio/uart/spi arms on [`crate::gpio::Board`].
impl crate::gpio::Board {
    /// The I2C instance number for a named board resource (`board.I2C0`), or `None`.
    pub(crate) fn i2c_instance(self, name: &str) -> Option<u32> {
        match self {
            crate::gpio::Board::Rp2350 => match name {
                "I2C0" => Some(0),
                _ => None,
            },
            _ => None,
        }
    }

    /// The DW_apb_i2c register facts for `instance`, or `None` when this board has no I2C arm.
    pub(crate) fn i2c_facts(self, instance: u32) -> Option<I2cFacts> {
        match self {
            crate::gpio::Board::Rp2350 if instance == 0 => Some(i2c_rp2350::facts()),
            _ => None,
        }
    }

    /// The instance's `(scl, sda)` pins -- what a `busio.I2C(scl, sda)` shim's pin arguments must
    /// match (the table wires the pins; a differing pin fails loud).
    pub(crate) fn i2c_pins(self, instance: u32) -> Option<(u32, u32)> {
        match self {
            crate::gpio::Board::Rp2350 if instance == 0 => Some((5, 4)),
            _ => None,
        }
    }

    /// The ordered bring-up opening `instance` with `config`, paired with the REALIZED (never-
    /// exceed) SCL rate; `None` when the board/instance has no I2C arm, the inner `Err` for a
    /// frequency this chip's counters cannot express.
    pub(crate) fn i2c_open_ops(
        self,
        instance: u32,
        config: &I2cConfig,
    ) -> Option<Result<(Vec<I2cOp>, u32), I2cConfigError>> {
        match self {
            crate::gpio::Board::Rp2350 if instance == 0 => Some(i2c_rp2350::open_ops(config)),
            _ => None,
        }
    }
}
