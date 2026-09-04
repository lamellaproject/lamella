//! The CMSIS-DAP command protocol: request encoders and response parsers, implemented
//! from the Arm CMSIS-DAP specification.

/// CMSIS-DAP command ids (the subset this host issues).
#[allow(missing_docs)]
pub mod cmd {
    pub const INFO: u8 = 0x00;
    pub const CONNECT: u8 = 0x02;
    pub const DISCONNECT: u8 = 0x03;
    pub const TRANSFER_CONFIGURE: u8 = 0x04;
    pub const TRANSFER: u8 = 0x05;
    pub const TRANSFER_BLOCK: u8 = 0x06;
    pub const RESET_TARGET: u8 = 0x0A;
    pub const SWJ_PINS: u8 = 0x10;
    pub const SWJ_CLOCK: u8 = 0x11;
    pub const SWJ_SEQUENCE: u8 = 0x12;
    pub const SWD_CONFIGURE: u8 = 0x13;
    pub const SWD_SEQUENCE: u8 = 0x1D;
}

/// The reply's first byte when the probe does NOT implement the command it was sent: "commands
/// that are not implemented reply with `0xFF` instead of repeating the command byte".
///
/// It is the only refusal a probe can express before it has looked at the arguments, and it is
/// why [`has_status_byte`] is not the whole story -- an unimplemented command never gets as far
/// as producing a status.
pub const INVALID_COMMAND: u8 = 0xFF;

/// A [`has_status_byte`] command succeeded.
pub const DAP_OK: u8 = 0x00;

/// A [`has_status_byte`] command failed.
pub const DAP_ERROR: u8 = 0xFF;

/// The `DAP_Connect` reply's port field when the probe could not initialize the mode asked for:
/// "0 = initialization failed; no mode pre-configured". `DAP_Connect` answers with the port it
/// actually selected rather than a status, so this value is its failure report.
pub const CONNECT_FAILED: u8 = 0x00;

/// Whether the byte AFTER the echoed command id is a status byte ([`DAP_OK`] / [`DAP_ERROR`]).
///
/// **IT IS NOT A STATUS FOR EVERY COMMAND, AND CHECKING IT BLINDLY WOULD BREAK THE ONES IT IS
/// NOT.** The reply layouts differ per command and the byte in that position carries whatever
/// each one puts there -- `DAP_Info` a LENGTH, `DAP_Connect` the PORT it selected, `DAP_Transfer`
/// and `DAP_TransferBlock` a transfer COUNT, `DAP_SWJ_Pins` the pin INPUT levels. A blanket
/// "byte 1 must be zero" rejects a perfectly good `DAP_Info` the moment it returns any data at
/// all, so the distinction has to be per command, from the documented layout of each.
pub fn has_status_byte(command: u8) -> bool {
    matches!(
        command,
        cmd::DISCONNECT
            | cmd::TRANSFER_CONFIGURE
            | cmd::RESET_TARGET
            | cmd::SWJ_CLOCK
            | cmd::SWJ_SEQUENCE
            | cmd::SWD_CONFIGURE
            | cmd::SWD_SEQUENCE
    )
}

/// One phase of a [`swd_sequence`]: a run of SWCLK cycles during which the host either DRIVES
/// SWDIO or RELEASES it.
///
/// The distinction is the whole reason this command exists next to `DAP_SWJ_Sequence`, which can
/// only ever drive. A SWD transaction hands the line over to the target for the turnaround and
/// acknowledge cycles, and a host that keeps driving through them is contending with the target --
/// or, where the target drives nothing (the multi-drop `TARGETSEL` write), is still leaving the
/// DP's state machine to sample a line the protocol says should be released.
pub enum SwdPhase<'a> {
    /// Host drives `cycles` bits from `data`, least-significant bit first.
    Out {
        /// How many SWCLK cycles to drive: 1-64, encoded as 6 bits where 0 means 64.
        cycles: u8,
        /// The bits to shift out, least-significant bit of the first byte first.
        data: &'a [u8],
    },
    /// Host releases SWDIO for `cycles` clocks; the probe captures and returns what it saw.
    In {
        /// How many SWCLK cycles to release for: 1-64, encoded as 6 bits where 0 means 64.
        cycles: u8,
    },
}

/// Encodes `DAP_SWD_Sequence`: a list of drive/release phases clocked back to back.
///
/// `cycles` is 1-64 per phase, encoded as 6 bits where 0 means 64. Bit 7 of the info byte marks a
/// release (input) phase.
pub fn swd_sequence(phases: &[SwdPhase]) -> Vec<u8> {
    let mut out = vec![cmd::SWD_SEQUENCE, phases.len() as u8];
    for phase in phases {
        match phase {
            SwdPhase::Out { cycles, data } => {
                out.push(cycles & 0x3f);
                let bytes = (usize::from(if *cycles == 0 { 64 } else { *cycles }) + 7) / 8;
                out.extend_from_slice(&data[..bytes.min(data.len())]);
            }
            SwdPhase::In { cycles } => out.push(0x80 | (cycles & 0x3f)),
        }
    }
    out
}

/// The wire protocol selected by `DAP_Connect`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Port {
    /// Serial Wire Debug.
    Swd = 1,
    /// JTAG.
    Jtag = 2,
}

/// The acknowledge field of an ADIv5 transfer response.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ack {
    /// Transfer completed (`OK`).
    Ok,
    /// Target asked to retry (`WAIT`).
    Wait,
    /// Target signalled a fault (`FAULT`).
    Fault,
    /// No acknowledge -- usually a wiring or protocol fault.
    NoAck,
    /// An acknowledge value the spec does not define.
    Unknown(u8),
}

impl Ack {
    fn from_bits(bits: u8) -> Ack {
        match bits & 0b111 {
            0b001 => Ack::Ok,
            0b010 => Ack::Wait,
            0b100 => Ack::Fault,
            0b111 => Ack::NoAck,
            other => Ack::Unknown(other),
        }
    }
}


/// A request byte that reads a Debug Port register.
pub const fn dp_read(reg: u8) -> u8 {
    0b0000_0010 | (reg & 0x0C)
}
/// A request byte that writes a Debug Port register.
pub const fn dp_write(reg: u8) -> u8 {
    reg & 0x0C
}
/// A request byte that reads an Access Port register.
pub const fn ap_read(reg: u8) -> u8 {
    0b0000_0011 | (reg & 0x0C)
}
/// A request byte that writes an Access Port register.
pub const fn ap_write(reg: u8) -> u8 {
    0b0000_0001 | (reg & 0x0C)
}

/// Encodes `DAP_Info` for the given info id.
pub fn info(info_id: u8) -> [u8; 2] {
    [cmd::INFO, info_id]
}

/// Encodes `DAP_Connect` selecting `port`.
pub fn connect(port: Port) -> [u8; 2] {
    [cmd::CONNECT, port as u8]
}

/// Encodes `DAP_Disconnect`.
pub fn disconnect() -> [u8; 1] {
    [cmd::DISCONNECT]
}

/// Encodes `DAP_SWJ_Clock` requesting `hz` (little-endian).
pub fn swj_clock(hz: u32) -> [u8; 5] {
    let b = hz.to_le_bytes();
    [cmd::SWJ_CLOCK, b[0], b[1], b[2], b[3]]
}


/// The `DAP_SWJ_Pins` bit for the target reset line (nRESET). Bit 7 in the CMSIS-DAP pin mask;
/// driving it low asserts reset (holds the core), high releases it.
pub const PIN_NRESET: u8 = 1 << 7;

/// The `DAP_SWJ_Pins` bit for SWCLK (TCK on a JTAG wire). Bit 0 in the CMSIS-DAP pin mask.
pub const PIN_SWCLK: u8 = 1 << 0;

/// The `DAP_SWJ_Pins` bit for SWDIO (TMS on a JTAG wire). Bit 1 in the CMSIS-DAP pin mask.
///
/// Useful in diagnostics because SWDIO is the only SWD signal that is BIDIRECTIONAL: driving it
/// against a target's pull-up and reading the result back tests whether the probe's level shifters
/// are actually driving, which no amount of reading alone can establish.
pub const PIN_SWDIO: u8 = 1 << 1;

/// Encodes `DAP_ResetTarget`: reset this target by whatever means THIS PROBE has for it.
///
/// **THE VENDOR-DEFINED RESET, AND IT EXISTS BECAUSE THE GENERIC PIN ROUTE DOES NOT REACH EVERY
/// BOARD.** [`swj_pins`] drives named SWJ pins directly, which works only where the probe wires
/// nRESET to one of them and is willing to drive it. A debugger whose reset reaches the target by
/// any other path -- a dedicated line, a device-specific sequence, a companion MCU -- implements
/// that path here instead, and answers `DAP_SWJ_Pins` for reset with nothing at all.
///
/// The reply's second byte is a STATUS and its third is `Execute`. **BIT 0 of `Execute` is the
/// probe saying it ran a device-specific sequence**; clear means it has none and the caller
/// should fall back to [`swj_pins`]. Test the BIT rather than comparing the byte to 1 -- a probe
/// setting any other bit would otherwise be read as having done nothing. **So a `DAP_OK` here
/// does not by itself mean a target was reset**, which is why the helper that sends this reports
/// the flag rather than swallowing it.
///
/// NOTE: it has NO capability bit, so it cannot be feature-probed before use. A probe that does
/// not implement it answers [`INVALID_COMMAND`], which surfaces as `DapError::Unsupported`.
pub fn reset_target() -> [u8; 1] {
    [cmd::RESET_TARGET]
}

/// Encodes `DAP_SWJ_Pins`: drive the pins named in `select` to the levels in `output`, then wait up
/// to `wait_us` microseconds for them to settle (the reply reports the pins' resulting input levels).
/// Used to release the nRESET a probe may assert on connect, so the core can run / be halted.
pub fn swj_pins(output: u8, select: u8, wait_us: u32) -> [u8; 7] {
    let w = wait_us.to_le_bytes();
    [cmd::SWJ_PINS, output, select, w[0], w[1], w[2], w[3]]
}

/// Encodes `DAP_SWJ_Sequence`: `bit_count` clocks shifting `bits` out on SWDIO,
/// least-significant bit first. A `bit_count` of 0 means 256, per the spec.
pub fn swj_sequence(bit_count: u8, bits: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + bits.len());
    out.push(cmd::SWJ_SEQUENCE);
    out.push(bit_count);
    out.extend_from_slice(bits);
    out
}

/// Encodes a single-access `DAP_Transfer` on DAP index 0: one `request` byte, plus the
/// 32-bit `write_data` when the request is a write.
pub fn transfer_one(request: u8, write_data: Option<u32>) -> Vec<u8> {
    let mut out = vec![cmd::TRANSFER, 0x00, 0x01, request];
    if let Some(data) = write_data {
        out.extend_from_slice(&data.to_le_bytes());
    }
    out
}

/// Encodes `DAP_TransferConfigure`: idle cycles appended after each transfer, and the probe's
/// retry budgets for `WAIT` acknowledges and value-match reads (little-endian u16s each).
pub fn transfer_configure(idle_cycles: u8, wait_retry: u16, match_retry: u16) -> [u8; 6] {
    let w = wait_retry.to_le_bytes();
    let m = match_retry.to_le_bytes();
    [cmd::TRANSFER_CONFIGURE, idle_cycles, w[0], w[1], m[0], m[1]]
}

/// Encodes a write `DAP_TransferBlock` on DAP index 0: one `request` byte repeated by the
/// probe for every 32-bit value in `values` -- the bulk sibling of [`transfer_one`], used to
/// stream a buffer through an auto-incrementing MEM-AP `DRW`.
pub fn transfer_block_write(request: u8, values: &[u32]) -> Vec<u8> {
    let count = values.len() as u16;
    let mut out = Vec::with_capacity(5 + values.len() * 4);
    out.push(cmd::TRANSFER_BLOCK);
    out.push(0x00);
    out.extend_from_slice(&count.to_le_bytes());
    out.push(request);
    for value in values {
        out.extend_from_slice(&value.to_le_bytes());
    }
    out
}

/// Parses the reply to a write `DAP_TransferBlock`: the completed-transfer count and the last
/// acknowledge.
pub fn parse_block_write(reply: &[u8]) -> Result<(u16, Ack), ProtoError> {
    if reply.len() < 4 {
        return Err(ProtoError::Truncated);
    }
    if reply[0] != cmd::TRANSFER_BLOCK {
        return Err(ProtoError::WrongCommand { expected: cmd::TRANSFER_BLOCK, got: reply[0] });
    }
    Ok((u16::from_le_bytes([reply[1], reply[2]]), Ack::from_bits(reply[3])))
}

/// Encodes a read `DAP_TransferBlock` on DAP index 0: `count` transfers of the one `request`
/// byte -- the bulk sibling of a single read, for streaming target memory out through an
/// auto-incrementing MEM-AP `DRW`.
pub fn transfer_block_read(request: u8, count: u16) -> [u8; 5] {
    let c = count.to_le_bytes();
    [cmd::TRANSFER_BLOCK, 0x00, c[0], c[1], request]
}

/// Parses the reply to a read `DAP_TransferBlock` into the caller's `out`, returning the completed
/// count and the last acknowledge; the first `count` slots are filled.
///
/// A reply carrying more words than `out` holds is [`ProtoError::Truncated`] rather than a partial
/// fill: the caller sized the buffer from the count it asked for, so a longer reply means the probe
/// and the caller disagree about the transfer, and silently keeping the prefix would hand back a
/// buffer that looks complete.
pub fn parse_block_read(reply: &[u8], out: &mut [u32]) -> Result<(u16, Ack), ProtoError> {
    let (count, ack) = parse_block_write(reply)?;
    let expected = 4 + count as usize * 4;
    if reply.len() < expected || out.len() < count as usize {
        return Err(ProtoError::Truncated);
    }
    for (slot, word) in out.iter_mut().zip(reply[4..expected].chunks_exact(4)) {
        *slot = u32::from_le_bytes([word[0], word[1], word[2], word[3]]);
    }
    Ok((count, ack))
}

/// The parsed reply to a single-access read transfer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadReply {
    /// How many of the requested transfers the probe completed.
    pub count: u8,
    /// The last acknowledge.
    pub ack: Ack,
    /// The 32-bit value read; present when `ack` is `Ok`.
    pub data: Option<u32>,
}

/// An error decoding a probe reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProtoError {
    /// The reply was shorter than its header requires.
    Truncated,
    /// The reply's command id did not echo the request.
    WrongCommand {
        /// The command id sent.
        expected: u8,
        /// The command id received.
        got: u8,
    },
}

/// Parses the reply to a single-access read `DAP_Transfer`.
pub fn parse_read(reply: &[u8]) -> Result<ReadReply, ProtoError> {
    if reply.len() < 3 {
        return Err(ProtoError::Truncated);
    }
    if reply[0] != cmd::TRANSFER {
        return Err(ProtoError::WrongCommand {
            expected: cmd::TRANSFER,
            got: reply[0],
        });
    }
    let count = reply[1];
    let ack = Ack::from_bits(reply[2]);
    let data = if ack == Ack::Ok && reply.len() >= 7 {
        Some(u32::from_le_bytes([reply[3], reply[4], reply[5], reply[6]]))
    } else {
        None
    };
    Ok(ReadReply { count, ack, data })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfer_request_bytes() {
        assert_eq!(dp_read(0x0), 0x02);
        assert_eq!(dp_read(0x4), 0x06);
        assert_eq!(dp_write(0x4), 0x04);
        assert_eq!(ap_read(0xC), 0x0F);
        assert_eq!(ap_write(0x0), 0x01);
    }

    #[test]
    fn encodes_swj_clock() {
        assert_eq!(swj_clock(1_000_000), [0x11, 0x40, 0x42, 0x0f, 0x00]);
    }

    #[test]
    fn encodes_single_read_and_write() {
        assert_eq!(
            transfer_one(dp_read(0x0), None),
            vec![0x05, 0x00, 0x01, 0x02]
        );
        assert_eq!(
            transfer_one(dp_write(0x4), Some(0x1234_5678)),
            vec![0x05, 0x00, 0x01, 0x04, 0x78, 0x56, 0x34, 0x12]
        );
    }

    #[test]
    fn parses_idcode_reply() {
        let reply = [0x05, 0x01, 0x01, 0x77, 0x14, 0xb1, 0x0b];
        let r = parse_read(&reply).unwrap();
        assert_eq!(r.count, 1);
        assert_eq!(r.ack, Ack::Ok);
        assert_eq!(r.data, Some(0x0bb1_1477));
    }

    #[test]
    fn rejects_wrong_command() {
        assert_eq!(
            parse_read(&[0x06, 0, 0]),
            Err(ProtoError::WrongCommand {
                expected: 0x05,
                got: 0x06
            })
        );
    }
}
