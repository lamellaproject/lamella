//! The host fake: a script of commands, transfers, bus settings and power
//! operations, replayed under the SDIO transport.

use crate::clock::Micros;
use crate::error::Refusal;
use crate::sdio::{Rate, Response, SdioHost, Shape, Width};
use crate::transport::Attach;

/// One recorded host operation.
#[derive(Clone, Copy, Debug)]
pub enum HostRow {
    /// The controller's own attach: `Pending` two milliseconds ahead
    /// `pending` times, then `Ready`.
    Attach {
        /// How many pending steps precede `Ready`.
        pending: u32,
    },
    /// The power-enable line driven to `on`.
    Power(bool),
    /// The bus set to a rate and width.
    Bus {
        /// The rate.
        rate: Rate,
        /// The width.
        width: Width,
    },
    /// One command: the index, argument and response kind the transport
    /// must issue, and the reply. The row answers `times` identical
    /// commands before the script advances.
    Command {
        /// The command index.
        index: u8,
        /// The argument.
        arg: u32,
        /// The response kind.
        response: Response,
        /// The reply: response bits 39:8.
        reply: u32,
        /// How many identical commands the row answers.
        times: u32,
        /// Where the row comes from.
        note: &'static str,
    },
    /// One CMD53 read: the argument and shape the transport must issue, the
    /// bytes on the wire (`shape.len` of them) and the R5 reply.
    Read {
        /// The argument.
        arg: u32,
        /// The wire shape.
        shape: Shape,
        /// The wire bytes.
        data: &'static [u8],
        /// The R5 reply.
        reply: u32,
        /// Where the row comes from.
        note: &'static str,
    },
    /// One CMD53 write: the argument and shape the transport must issue,
    /// the bytes expected on the wire (`shape.len` of them, the padding
    /// included) and the R5 reply.
    Write {
        /// The argument.
        arg: u32,
        /// The wire shape.
        shape: Shape,
        /// The wire bytes, the padding included.
        data: &'static [u8],
        /// The R5 reply.
        reply: u32,
        /// Where the row comes from.
        note: &'static str,
    },
    /// The interrupt latch's answer.
    Irq(u16),
}

/// The stage name of a command whose index, argument or response kind
/// differs from the script's row.
pub const STAGE_COMMAND_MISMATCH: &str = "fixture: command mismatch";
/// The stage name of a transfer whose argument, shape or bytes differ from
/// the script's row.
pub const STAGE_TRANSFER_MISMATCH: &str = "fixture: transfer mismatch";
/// The stage name of a host operation the script did not expect, or one
/// past its end.
pub const STAGE_UNEXPECTED_HOST_OPERATION: &str = "fixture: unexpected host operation";

const PENDING_STEP_US: Micros = 2_000;

/// A host that replays a script.
#[derive(Debug)]
pub struct FakeHost<'s> {
    script: &'s [HostRow],
    pos: usize,
    served: u32,
    fault: Option<Refusal>,
}

impl<'s> FakeHost<'s> {
    /// A host that replays `script` from its first row.
    pub const fn new(script: &'s [HostRow]) -> Self {
        FakeHost {
            script,
            pos: 0,
            served: 0,
            fault: None,
        }
    }

    /// Replay the script from its first row again, forgetting any fault.
    pub fn restart(&mut self) {
        self.pos = 0;
        self.served = 0;
        self.fault = None;
    }

    /// The rows not yet consumed.
    pub fn remaining(&self) -> usize {
        self.script.len().saturating_sub(self.pos)
    }

    /// The first operation that did not match the script, if any.
    pub fn fault(&self) -> Option<Refusal> {
        self.fault
    }

    fn row(&self) -> Option<HostRow> {
        self.script.get(self.pos).copied()
    }

    fn fail(&mut self, stage: &'static str) -> Refusal {
        let refusal = Refusal::new(stage, (self.pos + 1) as u32);
        self.fault.get_or_insert(refusal);
        refusal
    }
}

impl SdioHost for FakeHost<'_> {
    fn attach(&mut self, now: Micros) -> Result<Attach, Refusal> {
        if let Some(fault) = self.fault {
            return Err(fault);
        }
        match self.row() {
            Some(HostRow::Attach { pending }) => {
                if self.served < pending {
                    self.served += 1;
                    Ok(Attach::Pending {
                        until: now + PENDING_STEP_US,
                    })
                } else {
                    self.served = 0;
                    self.pos += 1;
                    Ok(Attach::Ready)
                }
            }
            _ => Err(self.fail(STAGE_UNEXPECTED_HOST_OPERATION)),
        }
    }

    fn set_power(&mut self, on: bool) {
        match self.row() {
            Some(HostRow::Power(want)) if want == on => self.pos += 1,
            _ => {
                let _ = self.fail(STAGE_UNEXPECTED_HOST_OPERATION);
            }
        }
    }

    fn set_bus(&mut self, rate: Rate, width: Width) -> Result<(), Refusal> {
        if let Some(fault) = self.fault {
            return Err(fault);
        }
        match self.row() {
            Some(HostRow::Bus {
                rate: want_rate,
                width: want_width,
            }) if want_rate == rate && want_width == width => {
                self.pos += 1;
                Ok(())
            }
            _ => Err(self.fail(STAGE_UNEXPECTED_HOST_OPERATION)),
        }
    }

    fn command(&mut self, index: u8, arg: u32, response: Response) -> Result<u32, Refusal> {
        if let Some(fault) = self.fault {
            return Err(fault);
        }
        match self.row() {
            Some(HostRow::Command {
                index: want_index,
                arg: want_arg,
                response: want_response,
                reply,
                times,
                ..
            }) => {
                if want_index != index || want_arg != arg || want_response != response {
                    return Err(self.fail(STAGE_COMMAND_MISMATCH));
                }
                self.served += 1;
                if self.served >= times {
                    self.pos += 1;
                    self.served = 0;
                }
                Ok(reply)
            }
            _ => Err(self.fail(STAGE_UNEXPECTED_HOST_OPERATION)),
        }
    }

    fn read_data(&mut self, arg: u32, shape: Shape, buf: &mut [u8]) -> Result<u32, Refusal> {
        if let Some(fault) = self.fault {
            return Err(fault);
        }
        match self.row() {
            Some(HostRow::Read {
                arg: want_arg,
                shape: want_shape,
                data,
                reply,
                ..
            }) => {
                let same = want_arg == arg
                    && want_shape == shape
                    && data.len() == shape.len
                    && buf.len() <= data.len();
                if !same {
                    return Err(self.fail(STAGE_TRANSFER_MISMATCH));
                }
                buf.copy_from_slice(&data[..buf.len()]);
                self.pos += 1;
                Ok(reply)
            }
            _ => Err(self.fail(STAGE_UNEXPECTED_HOST_OPERATION)),
        }
    }

    fn write_data(&mut self, arg: u32, shape: Shape, data: &[u8]) -> Result<u32, Refusal> {
        if let Some(fault) = self.fault {
            return Err(fault);
        }
        match self.row() {
            Some(HostRow::Write {
                arg: want_arg,
                shape: want_shape,
                data: want,
                reply,
                ..
            }) => {
                let same = want_arg == arg
                    && want_shape == shape
                    && want.len() == shape.len
                    && data.len() <= want.len()
                    && want[..data.len()] == *data
                    && want[data.len()..].iter().all(|&b| b == 0);
                if !same {
                    return Err(self.fail(STAGE_TRANSFER_MISMATCH));
                }
                self.pos += 1;
                Ok(reply)
            }
            _ => Err(self.fail(STAGE_UNEXPECTED_HOST_OPERATION)),
        }
    }

    fn take_interrupt(&mut self) -> u16 {
        match self.row() {
            Some(HostRow::Irq(latch)) => {
                self.pos += 1;
                latch
            }
            _ => 0,
        }
    }
}
