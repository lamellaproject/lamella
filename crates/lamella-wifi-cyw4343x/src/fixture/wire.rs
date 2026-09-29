//! The wire fake: a script of frames, power and strap operations and idle
//! interrupt levels, replayed under the gSPI transport.

use crate::error::Refusal;
use crate::gspi::GspiWire;

/// One recorded wire operation.
#[derive(Clone, Copy, Debug)]
pub enum Row {
    /// The power-enable line driven to `on`.
    Power(bool),
    /// The mode strap held (`true`) or released (`false`).
    Strap(bool),
    /// One frame: the command word and data the driver must send, in wire
    /// order, and the bytes the chip answered. The row answers `times`
    /// identical frames before the script advances.
    Frame {
        /// The command word followed by the data, in wire order.
        tx: &'static [u8],
        /// The answer, in wire order.
        rx: &'static [u8],
        /// How many identical frames the row answers.
        times: u32,
        /// Where the row comes from.
        note: &'static str,
    },
    /// The interrupt line's level while idle.
    Irq(bool),
}

/// The stage name of a frame whose bytes differ from the script's row.
pub const STAGE_FRAME_MISMATCH: &str = "fixture: frame mismatch";
/// The stage name of a frame issued where the script expects another
/// operation, or past its end.
pub const STAGE_UNEXPECTED_FRAME: &str = "fixture: unexpected frame";
/// The stage name of a power or strap operation the script did not expect.
pub const STAGE_UNEXPECTED_OPERATION: &str = "fixture: unexpected wire operation";

const STALE_BYTES: usize = 64;

/// A wire that replays a script.
#[derive(Debug)]
pub struct FakeWire<'s> {
    script: &'s [Row],
    pos: usize,
    served: u32,
    fault: Option<Refusal>,
    stale: bool,
    last_rx: [u8; STALE_BYTES],
    last_len: usize,
}

impl<'s> FakeWire<'s> {
    /// A wire that replays `script` from its first row.
    pub const fn new(script: &'s [Row]) -> Self {
        FakeWire {
            script,
            pos: 0,
            served: 0,
            fault: None,
            stale: false,
            last_rx: [0; STALE_BYTES],
            last_len: 0,
        }
    }

    /// Answer every read one frame late: the first read gets zeros and each
    /// later read gets the previous read's bytes, while the driver's frames
    /// are still checked in order. A transport that answers out of order.
    pub fn stale_answers(mut self) -> Self {
        self.stale = true;
        self
    }

    /// Replay the script from its first row again, forgetting any fault.
    pub fn restart(&mut self) {
        self.pos = 0;
        self.served = 0;
        self.fault = None;
        self.last_len = 0;
    }

    /// The rows not yet consumed.
    pub fn remaining(&self) -> usize {
        self.script.len().saturating_sub(self.pos)
    }

    /// The first operation that did not match the script, if any.
    pub fn fault(&self) -> Option<Refusal> {
        self.fault
    }

    fn row_number(&self) -> u32 {
        (self.pos + 1) as u32
    }

    fn expect(&mut self, expected: Row) -> Result<(), Refusal> {
        let matched = match (self.script.get(self.pos), expected) {
            (Some(Row::Power(a)), Row::Power(b)) => *a == b,
            (Some(Row::Strap(a)), Row::Strap(b)) => *a == b,
            _ => false,
        };
        if !matched {
            let refusal = Refusal::new(STAGE_UNEXPECTED_OPERATION, self.row_number());
            self.fault.get_or_insert(refusal);
            return Err(refusal);
        }
        self.pos += 1;
        Ok(())
    }
}

impl GspiWire for FakeWire<'_> {
    fn set_power(&mut self, on: bool) {
        let _ = self.expect(Row::Power(on));
    }

    fn strap(&mut self, hold: bool) {
        let _ = self.expect(Row::Strap(hold));
    }

    fn frame(&mut self, cmd: [u8; 4], tx: &[u8], rx: &mut [u8]) -> Result<(), Refusal> {
        if let Some(fault) = self.fault {
            return Err(fault);
        }
        let Some(Row::Frame {
            tx: want,
            rx: answer,
            times,
            ..
        }) = self.script.get(self.pos).copied()
        else {
            let refusal = Refusal::new(STAGE_UNEXPECTED_FRAME, self.row_number());
            self.fault = Some(refusal);
            return Err(refusal);
        };
        let same = want.len() == 4 + tx.len()
            && want[..4] == cmd
            && want[4..] == *tx
            && answer.len() == rx.len();
        if !same {
            let refusal = Refusal::new(STAGE_FRAME_MISMATCH, self.row_number());
            self.fault = Some(refusal);
            return Err(refusal);
        }
        if !rx.is_empty() {
            if self.stale {
                for (i, b) in rx.iter_mut().enumerate() {
                    *b = if i < self.last_len {
                        self.last_rx[i]
                    } else {
                        0
                    };
                }
            } else {
                rx.copy_from_slice(answer);
            }
            let keep = answer.len().min(STALE_BYTES);
            self.last_rx[..keep].copy_from_slice(&answer[..keep]);
            self.last_len = keep;
        }
        self.served += 1;
        if self.served >= times {
            self.pos += 1;
            self.served = 0;
        }
        Ok(())
    }

    fn irq_asserted(&mut self) -> bool {
        match self.script.get(self.pos) {
            Some(Row::Irq(level)) => {
                self.pos += 1;
                *level
            }
            _ => false,
        }
    }
}
