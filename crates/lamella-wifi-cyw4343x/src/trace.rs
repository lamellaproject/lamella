//! The trace: a wrapper wire that records every wire operation to a byte
//! sink, so an exchange with real silicon can be replayed through the same
//! tests as a recorded one.
//!
//! The records, with little-endian lengths: `01 on` for the power line,
//! `02 hold` for the strap, `03 nn nn cmd(4) tx.. mm mm rx..` for one frame
//! (the byte count `4 + tx.len()`, the command word and the data in wire
//! order, the byte count `rx.len()`, the answer in wire order as the wire
//! returned it), `04 level` for the interrupt line's level, and `05 index
//! phase` for a setting selected.

use crate::error::Refusal;
use crate::gspi::{GspiWire, Setting};

/// The record tags.
pub mod tag {
    /// The power line driven.
    pub const POWER: u8 = 0x01;
    /// The strap held or released.
    pub const STRAP: u8 = 0x02;
    /// One frame.
    pub const FRAME: u8 = 0x03;
    /// The interrupt line sampled.
    pub const IRQ: u8 = 0x04;
    /// A setting and phase selected.
    pub const SELECT: u8 = 0x05;
}

/// Where the records go: a RAM buffer a probe reads, a console, a vector
/// in a test.
pub trait TraceSink {
    /// Append bytes to the trace.
    fn write(&mut self, bytes: &[u8]);
}

/// A wire that passes every operation to another and records it.
#[derive(Debug)]
pub struct Traced<W, S> {
    wire: W,
    sink: S,
}

impl<W, S> Traced<W, S> {
    /// A traced wire around `wire`, recording into `sink`.
    pub const fn new(wire: W, sink: S) -> Self {
        Traced { wire, sink }
    }

    /// The wrapped wire.
    pub fn wire(&self) -> &W {
        &self.wire
    }

    /// The wrapped wire, mutably.
    pub fn wire_mut(&mut self) -> &mut W {
        &mut self.wire
    }

    /// The sink.
    pub fn sink(&self) -> &S {
        &self.sink
    }

    /// Give the wire and the sink back.
    pub fn into_parts(self) -> (W, S) {
        (self.wire, self.sink)
    }
}

impl<W: GspiWire, S: TraceSink> GspiWire for Traced<W, S> {
    fn set_power(&mut self, on: bool) {
        self.wire.set_power(on);
        self.sink.write(&[tag::POWER, u8::from(on)]);
    }

    fn strap(&mut self, hold: bool) {
        self.wire.strap(hold);
        self.sink.write(&[tag::STRAP, u8::from(hold)]);
    }

    fn frame(&mut self, cmd: [u8; 4], tx: &[u8], rx: &mut [u8]) -> Result<(), Refusal> {
        let result = self.wire.frame(cmd, tx, rx);
        let out = (4 + tx.len()) as u16;
        self.sink.write(&[tag::FRAME, out as u8, (out >> 8) as u8]);
        self.sink.write(&cmd);
        self.sink.write(tx);
        let back = rx.len() as u16;
        self.sink.write(&[back as u8, (back >> 8) as u8]);
        self.sink.write(rx);
        result
    }

    fn irq_asserted(&mut self) -> bool {
        let level = self.wire.irq_asserted();
        self.sink.write(&[tag::IRQ, u8::from(level)]);
        level
    }

    fn settings(&self) -> u8 {
        self.wire.settings()
    }

    fn setting(&self, index: u8) -> Setting {
        self.wire.setting(index)
    }

    fn select(&mut self, index: u8, phase: i8) {
        self.wire.select(index, phase);
        self.sink.write(&[tag::SELECT, index, phase as u8]);
    }

    fn selected(&self) -> (u8, i8) {
        self.wire.selected()
    }
}
