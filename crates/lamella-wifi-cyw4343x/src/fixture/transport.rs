//! The transport fake: a script of the trait's own operations, replayed
//! under the bus-agnostic core. One exchange at this level serves both
//! buses.

use crate::clock::Micros;
use crate::error::Refusal;
use crate::transport::{Attach, Func, Part, Transport, Tune};

/// One recorded transport operation.
#[derive(Clone, Copy, Debug)]
pub enum Op {
    /// The transport's attach: `Pending` one millisecond ahead `pending`
    /// times, then `Ready`.
    Attach {
        /// How many pending steps precede `Ready`.
        pending: u32,
    },
    /// One direct read: the function and address the core must name, and
    /// the byte answered. The row answers `times` identical reads before
    /// the script advances.
    ReadDirect {
        /// The function.
        func: Func,
        /// The address.
        addr: u32,
        /// The byte answered.
        value: u8,
        /// How many identical reads the row answers.
        times: u32,
        /// Where the row comes from.
        note: &'static str,
    },
    /// One direct write: the function, address and byte the core must
    /// write.
    WriteDirect {
        /// The function.
        func: Func,
        /// The address.
        addr: u32,
        /// The byte.
        value: u8,
        /// Where the row comes from.
        note: &'static str,
    },
    /// One extended read: the function, address and addressing the core
    /// must name, and the bytes answered (their count is the read's length).
    ReadExtended {
        /// The function.
        func: Func,
        /// The address.
        addr: u32,
        /// Incrementing addressing.
        incr: bool,
        /// The bytes answered.
        data: &'static [u8],
        /// Where the row comes from.
        note: &'static str,
    },
    /// One extended write: the function, address, addressing and bytes the
    /// core must write.
    WriteExtended {
        /// The function.
        func: Func,
        /// The address.
        addr: u32,
        /// Incrementing addressing.
        incr: bool,
        /// The bytes.
        data: &'static [u8],
        /// Where the row comes from.
        note: &'static str,
    },
    /// The status word answered.
    Status {
        /// The word.
        value: u32,
    },
    /// The interrupt latch answered.
    TakeInterrupt {
        /// The latch.
        value: u16,
    },
    /// The bus-level abort of function 2.
    AbortF2,
    /// The availability query's answer.
    F2Available {
        /// The waiting frame's length, if any.
        len: Option<usize>,
    },
    /// The frame read.
    F2Read {
        /// The frame.
        data: &'static [u8],
    },
    /// The frame write: the bytes the core must write and whether the chip
    /// accepted them.
    F2Write {
        /// The frame.
        data: &'static [u8],
        /// Whether the write was accepted.
        accepted: bool,
    },
    /// The packet channel's readiness query. The row answers `times`
    /// identical queries before the script advances.
    F2Ready {
        /// The answer.
        ready: bool,
        /// How many identical queries the row answers.
        times: u32,
    },
    /// The wake-on-command control.
    WakeOnCommand,
    /// The tuning hook: `Pending` one millisecond ahead `pending` times,
    /// then `Done`.
    Tune {
        /// How many pending steps precede `Done`.
        pending: u32,
    },
    /// The packet channel's interrupt setup, answering whether the chip's
    /// mailbox interrupt is the bus's frame indication.
    F2InterruptSetup {
        /// The answer.
        mailbox: bool,
    },
    /// A group of rows replayed in sequence `times` times; the group
    /// counts as one row of the script.
    Repeat {
        /// The rows.
        ops: &'static [Op],
        /// How many times the group is replayed.
        times: u32,
    },
}

/// The stage name of an operation whose arguments differ from the script's
/// row.
pub const STAGE_TRANSPORT_MISMATCH: &str = "fixture: transport mismatch";
/// The stage name of an operation the script did not expect, or one past
/// its end.
pub const STAGE_UNEXPECTED_TRANSPORT_OPERATION: &str = "fixture: unexpected transport operation";

const PENDING_STEP_US: Micros = 1_000;

/// A transport that replays a script.
#[derive(Debug)]
pub struct FakeTransport<'s> {
    script: &'s [Op],
    pos: usize,
    served: u32,
    fault: Option<Refusal>,
    part: Part,
    group: Option<(usize, u32)>,
}

impl<'s> FakeTransport<'s> {
    /// A transport to `part` that replays `script` from its first row.
    pub const fn new(script: &'s [Op], part: Part) -> Self {
        FakeTransport {
            script,
            pos: 0,
            served: 0,
            fault: None,
            part,
            group: None,
        }
    }

    /// Replay the script from its first row again, forgetting any fault.
    pub fn restart(&mut self) {
        self.pos = 0;
        self.served = 0;
        self.fault = None;
        self.group = None;
    }

    /// The rows not yet consumed; a repeated group counts as one.
    pub fn remaining(&self) -> usize {
        self.script.len().saturating_sub(self.pos)
    }

    /// The first operation that did not match the script, if any.
    pub fn fault(&self) -> Option<Refusal> {
        self.fault
    }

    /// The current row: inside a repeated group, the group's current row.
    fn row(&mut self) -> Result<Option<Op>, Refusal> {
        if let Some(fault) = self.fault {
            return Err(fault);
        }
        match self.script.get(self.pos).copied() {
            Some(Op::Repeat { ops, .. }) => {
                let (i, _) = *self.group.get_or_insert((0, 0));
                Ok(ops.get(i).copied())
            }
            other => Ok(other),
        }
    }

    fn fail(&mut self, stage: &'static str) -> Refusal {
        let refusal = Refusal::new(stage, (self.pos + 1) as u32);
        self.fault.get_or_insert(refusal);
        refusal
    }

    /// Move past the current row; inside a repeated group, to the group's
    /// next row, its next replay, or past the group.
    fn advance(&mut self) {
        self.served = 0;
        if let (Some(Op::Repeat { ops, times }), Some((i, done))) =
            (self.script.get(self.pos).copied(), self.group)
        {
            let i = i + 1;
            if i < ops.len() {
                self.group = Some((i, done));
                return;
            }
            let done = done + 1;
            if done < times {
                self.group = Some((0, done));
                return;
            }
            self.group = None;
        }
        self.pos += 1;
    }

    /// One step of a pumped row: `true` when the row completes.
    fn step(&mut self, pending: u32) -> bool {
        if self.served < pending {
            self.served += 1;
            false
        } else {
            self.advance();
            true
        }
    }
}

impl Transport for FakeTransport<'_> {
    const F1_CHUNK: usize = 64;

    fn chip_id(&self) -> u16 {
        self.part.chip_id()
    }

    fn attach(&mut self, now: Micros) -> Result<Attach, Refusal> {
        match self.row()? {
            Some(Op::Attach { pending }) => {
                if self.step(pending) {
                    Ok(Attach::Ready)
                } else {
                    Ok(Attach::Pending {
                        until: now + PENDING_STEP_US,
                    })
                }
            }
            _ => Err(self.fail(STAGE_UNEXPECTED_TRANSPORT_OPERATION)),
        }
    }

    fn read_direct(&mut self, func: Func, addr: u32) -> Result<u8, Refusal> {
        match self.row()? {
            Some(Op::ReadDirect {
                func: f,
                addr: a,
                value,
                times,
                ..
            }) => {
                if f != func || a != addr {
                    return Err(self.fail(STAGE_TRANSPORT_MISMATCH));
                }
                self.served += 1;
                if self.served >= times {
                    self.advance();
                }
                Ok(value)
            }
            _ => Err(self.fail(STAGE_UNEXPECTED_TRANSPORT_OPERATION)),
        }
    }

    fn write_direct(&mut self, func: Func, addr: u32, value: u8) -> Result<(), Refusal> {
        match self.row()? {
            Some(Op::WriteDirect {
                func: f,
                addr: a,
                value: v,
                ..
            }) => {
                if f != func || a != addr || v != value {
                    return Err(self.fail(STAGE_TRANSPORT_MISMATCH));
                }
                self.advance();
                Ok(())
            }
            _ => Err(self.fail(STAGE_UNEXPECTED_TRANSPORT_OPERATION)),
        }
    }

    fn read_extended(
        &mut self,
        func: Func,
        addr: u32,
        incr: bool,
        buf: &mut [u8],
    ) -> Result<(), Refusal> {
        match self.row()? {
            Some(Op::ReadExtended {
                func: f,
                addr: a,
                incr: i,
                data,
                ..
            }) => {
                if f != func || a != addr || i != incr || data.len() != buf.len() {
                    return Err(self.fail(STAGE_TRANSPORT_MISMATCH));
                }
                buf.copy_from_slice(data);
                self.advance();
                Ok(())
            }
            _ => Err(self.fail(STAGE_UNEXPECTED_TRANSPORT_OPERATION)),
        }
    }

    fn write_extended(
        &mut self,
        func: Func,
        addr: u32,
        incr: bool,
        data: &[u8],
    ) -> Result<(), Refusal> {
        match self.row()? {
            Some(Op::WriteExtended {
                func: f,
                addr: a,
                incr: i,
                data: want,
                ..
            }) => {
                if f != func || a != addr || i != incr || want != data {
                    return Err(self.fail(STAGE_TRANSPORT_MISMATCH));
                }
                self.advance();
                Ok(())
            }
            _ => Err(self.fail(STAGE_UNEXPECTED_TRANSPORT_OPERATION)),
        }
    }

    fn status(&mut self) -> Result<u32, Refusal> {
        match self.row()? {
            Some(Op::Status { value }) => {
                self.advance();
                Ok(value)
            }
            _ => Err(self.fail(STAGE_UNEXPECTED_TRANSPORT_OPERATION)),
        }
    }

    fn take_interrupt(&mut self) -> Result<u16, Refusal> {
        match self.row()? {
            Some(Op::TakeInterrupt { value }) => {
                self.advance();
                Ok(value)
            }
            _ => Err(self.fail(STAGE_UNEXPECTED_TRANSPORT_OPERATION)),
        }
    }

    fn abort_f2(&mut self) -> Result<(), Refusal> {
        match self.row()? {
            Some(Op::AbortF2) => {
                self.advance();
                Ok(())
            }
            _ => Err(self.fail(STAGE_UNEXPECTED_TRANSPORT_OPERATION)),
        }
    }

    fn f2_frame_available(&mut self) -> Result<Option<usize>, Refusal> {
        match self.row()? {
            Some(Op::F2Available { len }) => {
                self.advance();
                Ok(len)
            }
            _ => Err(self.fail(STAGE_UNEXPECTED_TRANSPORT_OPERATION)),
        }
    }

    fn f2_read(&mut self, len: usize, frame: &mut [u8]) -> Result<usize, Refusal> {
        match self.row()? {
            Some(Op::F2Read { data }) => {
                if data.len() != len || frame.len() < len {
                    return Err(self.fail(STAGE_TRANSPORT_MISMATCH));
                }
                frame[..len].copy_from_slice(data);
                self.advance();
                Ok(len)
            }
            _ => Err(self.fail(STAGE_UNEXPECTED_TRANSPORT_OPERATION)),
        }
    }

    fn f2_write(&mut self, frame: &[u8]) -> Result<bool, Refusal> {
        match self.row()? {
            Some(Op::F2Write { data, accepted }) => {
                if data != frame {
                    return Err(self.fail(STAGE_TRANSPORT_MISMATCH));
                }
                self.advance();
                Ok(accepted)
            }
            _ => Err(self.fail(STAGE_UNEXPECTED_TRANSPORT_OPERATION)),
        }
    }

    fn f2_ready(&mut self) -> Result<bool, Refusal> {
        match self.row()? {
            Some(Op::F2Ready { ready, times }) => {
                self.served += 1;
                if self.served >= times {
                    self.advance();
                }
                Ok(ready)
            }
            _ => Err(self.fail(STAGE_UNEXPECTED_TRANSPORT_OPERATION)),
        }
    }

    fn wake_on_command(&mut self) -> Result<(), Refusal> {
        match self.row()? {
            Some(Op::WakeOnCommand) => {
                self.advance();
                Ok(())
            }
            _ => Err(self.fail(STAGE_UNEXPECTED_TRANSPORT_OPERATION)),
        }
    }

    fn f2_interrupt_setup(&mut self) -> Result<bool, Refusal> {
        match self.row()? {
            Some(Op::F2InterruptSetup { mailbox }) => {
                self.advance();
                Ok(mailbox)
            }
            _ => Err(self.fail(STAGE_UNEXPECTED_TRANSPORT_OPERATION)),
        }
    }

    fn tune(&mut self, f1_scratch: u32, now: Micros) -> Result<Tune, Refusal> {
        let _ = f1_scratch;
        match self.row()? {
            Some(Op::Tune { pending }) => {
                if self.step(pending) {
                    Ok(Tune::Done)
                } else {
                    Ok(Tune::Pending {
                        until: now + PENDING_STEP_US,
                    })
                }
            }
            _ => Err(self.fail(STAGE_UNEXPECTED_TRANSPORT_OPERATION)),
        }
    }
}
