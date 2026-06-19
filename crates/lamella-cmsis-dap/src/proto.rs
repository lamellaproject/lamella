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
    pub const RESET_TARGET: u8 = 0x0A;
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
