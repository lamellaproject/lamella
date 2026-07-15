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
/// Switch the PROBE's own operating mode (ARM/DAP <-> RISC-V), a firmware command that resets the probe
/// into the requested mode -- the software equivalent of the ModeS button, so no case/button access is
/// needed. Issued to a probe found in the OTHER mode; the probe then re-enumerates under the target
/// mode's product id. The payload is a single [`MODE_RISCV`]/[`MODE_ARM`] selector byte.
pub const CMD_MODE_SWITCH: u8 = 0xFF;

/// [`CMD_MODE_SWITCH`] selector: switch the probe into RISC-V mode (ASCII `'R'`); it re-enumerates as
/// [`crate::WCH_PID_RV`] (`1a86:8010`). Required before debugging/flashing a RISC-V target (a CH32V003).
pub const MODE_RISCV: u8 = b'R';
/// [`CMD_MODE_SWITCH`] selector: switch the probe into ARM/DAP mode (ASCII `'A'`); it re-enumerates as
/// the CMSIS-DAP product id (`1a86:8012`).
pub const MODE_ARM: u8 = b'A';

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


/// [`CMD_CONTROL`] subcommand: detach/release the target, ending a program session.
pub const CTRL_DETACH: u8 = 0xFF;
/// [`CMD_CONTROL`] subcommand: erase code flash by cycling the target's power (WCH-LinkE, which supplies
/// that power). Recovers a chip too locked to attach normally. Payload `[CTRL_ERASE_POWER_OFF, riscvchip]`.
pub const CTRL_ERASE_POWER_OFF: u8 = 0x0F;
/// [`CMD_CONTROL`] subcommand: erase code flash by driving the target's RST pin (needs a RST wire).
/// Payload `[CTRL_ERASE_RST_PIN, riscvchip]`.
pub const CTRL_ERASE_RST_PIN: u8 = 0x08;

/// Set the address+length window a following flash program/read applies to (`[start(BE), len(BE)]`).
pub const CMD_SET_ADDRESS: u8 = 0x01;
/// Flash/memory program operation, selected by a `PROG_*` subcommand byte.
pub const CMD_PROGRAM: u8 = 0x02;
/// Query/set flash protection, selected by a `CONFIG_*` subcommand byte.
pub const CMD_CONFIG: u8 = 0x06;
/// Set the probe<->target clock speed for a chip family (`[riscvchip, speed]`).
pub const CMD_SET_SPEED: u8 = 0x0C;

/// [`CMD_PROGRAM`] subcommand: erase the target code flash.
pub const PROG_ERASE_FLASH: u8 = 0x01;
/// [`CMD_PROGRAM`] subcommand: begin the fast-program image stream.
pub const PROG_WRITE_FLASH: u8 = 0x02;
/// [`CMD_PROGRAM`] subcommand: prepare to receive the RAM flash-loader (streamed on the data endpoint).
pub const PROG_WRITE_FLASH_OP: u8 = 0x05;
/// [`CMD_PROGRAM`] subcommand: commit the just-written flash-loader; the reply echoes this byte on success.
pub const PROG_COMMIT_FLASH_OP: u8 = 0x07;
/// [`CMD_PROGRAM`] subcommand: end the program session.
pub const PROG_END: u8 = 0x08;

/// [`CMD_RESET`] subcommand: reset the target and run, so it boots the freshly flashed image.
pub const RESET_RUN: u8 = 0x01;

/// [`CMD_SET_SPEED`] selector: the high (default) clock speed.
pub const SPEED_HIGH: u8 = 0x01;
/// [`CMD_SET_SPEED`] selector: the medium clock speed.
pub const SPEED_MEDIUM: u8 = 0x02;
/// [`CMD_SET_SPEED`] selector: the low clock speed.
pub const SPEED_LOW: u8 = 0x03;

/// [`CMD_CONFIG`] subcommand: query read-protection ([`FLAG_READ_PROTECTED`]/[`FLAG_READ_UNPROTECTED`]).
pub const CONFIG_CHECK_READ_PROTECT: u8 = 0x01;
/// [`CMD_CONFIG`] subcommand: query write-protection ([`FLAG_WRITE_PROTECTED`] when locked).
pub const CONFIG_CHECK_WRITE_PROTECT: u8 = 0x04;
/// [`CMD_CONFIG`] subcommand: remove flash protection. Sent alone (`[CONFIG_UNPROTECT]`) it lifts
/// read-protection; with the write-protect key appended (see [`unprotect_ex_payload`]) it lifts
/// write-protection. Lifting read-protection makes the probe firmware mass-erase the option-byte page
/// and code flash -- the security contract: protection cannot be removed while keeping the firmware.
pub const CONFIG_UNPROTECT: u8 = 0x02;

/// Read-protect query reply: flash is read-protected (debug/flash access is locked).
pub const FLAG_READ_PROTECTED: u8 = 0x01;
/// Read-protect query reply: flash is not read-protected.
pub const FLAG_READ_UNPROTECTED: u8 = 0x02;
/// Write-protect query reply: flash is write-protected.
pub const FLAG_WRITE_PROTECTED: u8 = 0x11;
/// Write-protect query reply: flash is not write-protected.
pub const FLAG_WRITE_UNPROTECTED: u8 = 0x00;

/// The status byte (last of four) of a fast-program pack acknowledgement that reports the pack programmed.
pub const FASTPROGRAM_ACK: u8 = 0x04;

/// Encodes the [`CMD_SET_ADDRESS`] payload `[start(big-endian u32), len(big-endian u32)]` -- the address
/// window a following flash program/read applies to.
pub fn set_address_payload(start: u32, len: u32) -> [u8; 8] {
    let (s, l) = (start.to_be_bytes(), len.to_be_bytes());
    [s[0], s[1], s[2], s[3], l[0], l[1], l[2], l[3]]
}

/// Encodes the [`CMD_CONFIG`] write-protection-removal payload `[CONFIG_UNPROTECT, key, 0xff x6]`: the
/// unprotect subcommand, the write-protect key (`0xff`), and six all-set write-protect bytes (clear WRP).
pub fn unprotect_ex_payload() -> [u8; 8] {
    [CONFIG_UNPROTECT, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]
}

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

/// Encodes a probe mode-switch request: `frame(CMD_MODE_SWITCH, [mode])` = `[0x81, 0xFF, 0x01, mode]`
/// with `mode` = [`MODE_RISCV`] or [`MODE_ARM`]. Sent to a probe found in the OTHER mode; the probe
/// resets into `mode` and re-enumerates under that mode's product id. Fire-and-forget: the probe drops
/// the USB link as it resets, so the caller does not read a reply (it opens the re-enumerated device).
pub fn mode_switch(mode: u8) -> Vec<u8> {
    frame(CMD_MODE_SWITCH, &[mode])
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
    /// Flash is protected, so programming was refused (unprotect the chip first).
    FlashProtected,
    /// A fast-program pack was not acknowledged (its status byte was not [`FASTPROGRAM_ACK`]).
    FastProgram(u8),
    /// A program subcommand returned an unexpected status/echo byte.
    ProgramReply {
        /// The byte the subcommand should have echoed/returned.
        expected: u8,
        /// The byte the reply actually carried.
        got: u8,
    },
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
            WchError::FlashProtected => {
                write!(f, "target flash is protected; unprotect the chip before flashing")
            }
            WchError::FastProgram(status) => {
                write!(f, "WCH-Link fast-program pack not acknowledged (status 0x{status:02x})")
            }
            WchError::ProgramReply { expected, got } => write!(
                f,
                "WCH-Link program reply 0x{got:02x} != expected 0x{expected:02x}"
            ),
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
    fn encodes_the_probe_mode_switch_request() {
        assert_eq!(mode_switch(MODE_RISCV), [0x81, 0xFF, 0x01, b'R']);
        assert_eq!(mode_switch(MODE_ARM), [0x81, 0xFF, 0x01, b'A']);
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
    fn encodes_the_set_address_window_big_endian() {
        assert_eq!(
            set_address_payload(0x0800_0000, 1372),
            [0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x05, 0x5c]
        );
    }

    #[test]
    fn encodes_the_write_unprotect_payload() {
        assert_eq!(unprotect_ex_payload(), [0x02, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]);
    }

    #[test]
    fn reads_version_major_minor_from_the_payload_head() {
        assert_eq!(parse_version(&[0x02, 0x07, 0x02, 0x00]).unwrap(), (0x02, 0x07));
    }
}
