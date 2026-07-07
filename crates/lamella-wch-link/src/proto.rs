//! The WCH-Link vendor USB command protocol in RV (RISC-V) mode, transport-free: request/reply framing,
//! the command encoders, and the reply parsers. No I/O -- a [`crate::Transport`] carries the bytes.

use core::fmt;

/// The lead byte of every request packet; an error reply reuses it (with a reason in the command slot).
pub const REQUEST: u8 = 0x81;
/// The lead byte of a success reply.
pub const REPLY_OK: u8 = 0x82;

/// Control command: probe firmware/info and target attach, selected by a subcommand byte.
pub const CMD_CONTROL: u8 = 0x0D;
/// [`CMD_CONTROL`] subcommand: read the probe firmware version.
pub const CTRL_VERSION: u8 = 0x01;
/// [`CMD_CONTROL`] subcommand: connect/attach the target chip.
pub const CTRL_ATTACH: u8 = 0x02;
/// DMI operation: one read/write of a RISC-V Debug Module register over the wire (the run-control
/// primitive -- see [`crate::dm`]).
pub const CMD_DMI: u8 = 0x08;
/// Reset the target.
pub const CMD_RESET: u8 = 0x0B;
/// Leave debug mode (release the target).
pub const CMD_DISABLE: u8 = 0x0E;

/// DMI request `op`: no-op / read / write (the RISC-V debug DMI operation encoding).
pub const DMI_NOP: u8 = 0;
/// DMI request `op`: read.
pub const DMI_READ: u8 = 1;
/// DMI request `op`: write.
pub const DMI_WRITE: u8 = 2;
/// DMI reply status (the reply `op` slot): success.
pub const DMI_STATUS_OK: u8 = 0;
/// DMI reply status: the operation failed (retry or surface an error).
pub const DMI_STATUS_FAIL: u8 = 2;

/// Frames a request packet: `[0x81, cmd, len, payload...]` with `len = payload.len()`.
///
/// # Panics
/// If `payload` is longer than 255 bytes (`len` is a single byte); WCH command payloads are small.
pub fn frame(cmd: u8, payload: &[u8]) -> Vec<u8> {
    assert!(payload.len() <= u8::MAX as usize, "WCH payload exceeds one length byte");
    let mut packet = Vec::with_capacity(3 + payload.len());
    packet.push(REQUEST);
    packet.push(cmd);
    packet.push(payload.len() as u8);
    packet.extend_from_slice(payload);
    packet
}

/// Encodes a DMI operation payload: `[addr, data (big-endian u32), op]` -- the request body carried by
/// [`CMD_DMI`].
pub fn dmi_payload(addr: u8, data: u32, op: u8) -> [u8; 6] {
    let d = data.to_be_bytes();
    [addr, d[0], d[1], d[2], d[3], op]
}

/// Validates a reply's header (`[0x82, cmd, len]`) against the request `cmd` and returns its payload
/// slice. An error reply (lead byte `!= 0x82`) or a mismatched command is a [`WchError`].
pub fn parse_reply(reply: &[u8], cmd: u8) -> Result<&[u8], WchError> {
    if reply.len() < 3 {
        return Err(WchError::ShortReply(reply.len()));
    }
    if reply[0] != REPLY_OK {
        return Err(WchError::ProbeError { reason: reply[1] });
    }
    if reply[1] != cmd {
        return Err(WchError::CommandMismatch { sent: cmd, got: reply[1] });
    }
    let len = reply[2] as usize;
    reply.get(3..3 + len).ok_or(WchError::ShortReply(reply.len()))
}

/// Parses a DMI reply payload `[addr, data (big-endian u32), status]` into `(addr, data, status)`.
pub fn parse_dmi(payload: &[u8]) -> Result<(u8, u32, u8), WchError> {
    if payload.len() < 6 {
        return Err(WchError::ShortReply(payload.len()));
    }
    let data = u32::from_be_bytes([payload[1], payload[2], payload[3], payload[4]]);
    Ok((payload[0], data, payload[5]))
}

/// Parses a firmware-version reply payload into `(major, minor)` from its leading two bytes. A real
/// WCH-LinkE replies `[02, 07, 02, 00]` = v2.7, where byte 2 (`0x02`) is the probe variant (WCH-Link**E**)
/// and byte 3 is reserved -- confirmed live against a probe.
pub fn parse_version(payload: &[u8]) -> Result<(u8, u8), WchError> {
    match payload {
        [major, minor, ..] => Ok((*major, *minor)),
        _ => Err(WchError::ShortReply(payload.len())),
    }
}

/// A WCH-Link protocol error: a transport failure, a malformed/short reply, a probe-reported error, or
/// a failed DMI status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WchError {
    /// The underlying byte transport failed (its message).
    Transport(String),
    /// A reply was shorter than its header/payload required (the length seen).
    ShortReply(usize),
    /// The probe returned an error reply (its reason code in the command slot).
    ProbeError {
        /// The reason code the probe placed in the command slot.
        reason: u8,
    },
    /// A reply's command byte did not match the request.
    CommandMismatch {
        /// The command byte that was sent.
        sent: u8,
        /// The command byte the reply carried.
        got: u8,
    },
    /// A DMI operation reported a non-success status.
    DmiStatus(u8),
}

impl fmt::Display for WchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WchError::Transport(message) => write!(f, "WCH-Link transport error: {message}"),
            WchError::ShortReply(len) => write!(f, "WCH-Link reply too short ({len} bytes)"),
            WchError::ProbeError { reason } => {
                write!(f, "WCH-Link probe returned error reason 0x{reason:02x}")
            }
            WchError::CommandMismatch { sent, got } => {
                write!(f, "WCH-Link reply command 0x{got:02x} != request 0x{sent:02x}")
            }
            WchError::DmiStatus(status) => write!(f, "WCH-Link DMI status 0x{status:02x} (not ok)"),
        }
    }
}

impl std::error::Error for WchError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_a_request_with_the_payload_length() {
        assert_eq!(frame(CMD_CONTROL, &[CTRL_VERSION]), [0x81, 0x0D, 0x01, 0x01]);
        assert_eq!(frame(CMD_DMI, &[]), [0x81, 0x08, 0x00]);
    }

    #[test]
    fn encodes_a_dmi_payload_big_endian() {
        assert_eq!(dmi_payload(0x10, 0x8000_0001, DMI_WRITE), [0x10, 0x80, 0x00, 0x00, 0x01, 0x02]);
    }

    #[test]
    fn parses_a_success_reply_payload() {
        let reply = [REPLY_OK, CMD_CONTROL, 0x02, 0xAA, 0xBB];
        assert_eq!(parse_reply(&reply, CMD_CONTROL).unwrap(), &[0xAA, 0xBB]);
    }

    #[test]
    fn rejects_an_error_reply_and_a_command_mismatch() {
        assert_eq!(
            parse_reply(&[REQUEST, 0x55, 0x00], CMD_DMI),
            Err(WchError::ProbeError { reason: 0x55 })
        );
        assert_eq!(
            parse_reply(&[REPLY_OK, CMD_RESET, 0x00], CMD_DMI),
            Err(WchError::CommandMismatch { sent: CMD_DMI, got: CMD_RESET })
        );
    }

    #[test]
    fn round_trips_a_dmi_reply() {
        let payload = [0x11, 0x00, 0x0C, 0x03, 0x82, DMI_STATUS_OK];
        assert_eq!(parse_dmi(&payload).unwrap(), (0x11, 0x000C_0382, DMI_STATUS_OK));
    }

    #[test]
    fn reads_version_major_minor_from_the_payload_head() {
        assert_eq!(parse_version(&[0x02, 0x07, 0x02, 0x00]).unwrap(), (0x02, 0x07));
    }
}
