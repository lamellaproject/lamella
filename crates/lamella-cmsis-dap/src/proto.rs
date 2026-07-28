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

/// Parses the reply to a read `DAP_TransferBlock` into `out`, returning the completed count and
/// the last acknowledge; values are appended for however many transfers completed.
pub fn parse_block_read(reply: &[u8], out: &mut Vec<u32>) -> Result<(u16, Ack), ProtoError> {
    let (count, ack) = parse_block_write(reply)?;
    let expected = 4 + count as usize * 4;
    if reply.len() < expected {
        return Err(ProtoError::Truncated);
    }
    for word in reply[4..expected].chunks_exact(4) {
        out.push(u32::from_le_bytes([word[0], word[1], word[2], word[3]]));
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
