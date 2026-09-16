
use super::{ControlPipe, DfuError};
use std::collections::VecDeque;

/// One thing the pipe was asked to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Sent {
    Out { request: u8, value: u16, data: Vec<u8> },
    In { request: u8, value: u16, length: usize },
    Pause(u32),
}

/// A control pipe that records every request and answers each read from a script.
pub(crate) struct Scripted {
    pub(crate) sent: Vec<Sent>,
    replies: VecDeque<Result<Vec<u8>, DfuError>>,
}

impl Scripted {
    pub(crate) fn answering(replies: Vec<Vec<u8>>) -> Self {
        Self::answering_each(replies.into_iter().map(Ok).collect())
    }

    /// A pipe whose reads take `replies` in order, failing where an entry is a failure.
    pub(crate) fn answering_each(replies: Vec<Result<Vec<u8>, DfuError>>) -> Self {
        Self {
            sent: Vec::new(),
            replies: replies.into(),
        }
    }
}

impl ControlPipe for Scripted {
    fn class_out(&mut self, request: u8, value: u16, data: &[u8]) -> Result<(), DfuError> {
        self.sent.push(Sent::Out { request, value, data: data.to_vec() });
        Ok(())
    }

    fn class_in(&mut self, request: u8, value: u16, buffer: &mut [u8]) -> Result<usize, DfuError> {
        self.sent.push(Sent::In { request, value, length: buffer.len() });
        let reply = self
            .replies
            .pop_front()
            .ok_or_else(|| DfuError::Transport("the script has no reply left".to_owned()))??;
        let length = reply.len().min(buffer.len());
        buffer[..length].copy_from_slice(&reply[..length]);
        Ok(length)
    }

    fn pause(&mut self, milliseconds: u32) {
        self.sent.push(Sent::Pause(milliseconds));
    }
}

/// A `DFU_GETSTATUS` reply: the status, a three-byte poll timeout least significant byte first, the
/// state, and a string index of zero.
pub(crate) fn status(status: u8, poll_timeout_ms: u32, state: u8) -> Vec<u8> {
    let [low, middle, high, _] = poll_timeout_ms.to_le_bytes();
    vec![status, low, middle, high, state, 0]
}
