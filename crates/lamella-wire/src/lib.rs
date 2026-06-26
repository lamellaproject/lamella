//! The Lamella wireline debug + REPL protocol -- the carrier-agnostic core shared by the host
//! front-ends (the DAP adapter + the gdb/lldb-style CLI) and the on-device runner.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::vec::Vec;

/// The current protocol version this build implements. A peer advertises a [`ProtocolRange`] around it.
pub const PROTOCOL_VERSION: u16 = 1;

/// The frame's leading sync magic ("LW" -- Lamella Wire). A receiver scans for it to find a frame
/// boundary after attaching mid-stream or recovering from line noise.
const SYNC: [u8; 2] = [0x4C, 0x57];
/// Bytes before the payload: `SYNC(2) | LEN(2) | TYPE(1) | SEQ(2)`.
const HEADER_LEN: usize = 7;
/// Trailing CRC-16 width.
const CRC_LEN: usize = 2;

/// Message type bytes. The Debug (`0x10+`) and REPL (`0x20+`) ranges are reserved.
pub mod msg {
    /// Host -> target: a [`super::Hello`] (version range + capabilities).
    pub const HELLO: u8 = 0x01;
    /// Target -> host: a [`super::HelloAck`] (the chosen version + the target's capabilities).
    pub const HELLO_ACK: u8 = 0x02;
    /// Target -> host: a [`super::Nak`] (no compatible version).
    pub const NAK: u8 = 0x03;
    /// Either way: an error response (e.g. an unknown command), payload = a reason byte + text.
    pub const ERROR: u8 = 0x04;
    /// Liveness probe.
    pub const PING: u8 = 0x05;
    /// Liveness reply.
    pub const PONG: u8 = 0x06;
}

/// A decoded protocol frame: its message type, sequence number, and payload bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    /// The message type byte (see [`msg`]).
    pub msg_type: u8,
    /// The sequence number -- matches a response to its request; async events use a distinct space.
    pub seq: u16,
    /// The message payload.
    pub payload: Vec<u8>,
}

/// CRC-16/CCITT-FALSE (poly 0x1021, init 0xFFFF) over the framed bytes, for frame integrity.
fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &byte in data {
        crc ^= (byte as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x1021 } else { crc << 1 };
        }
    }
    crc
}

/// Encode one frame: `SYNC | LEN | TYPE | SEQ | PAYLOAD | CRC`, the CRC covering `LEN..=PAYLOAD` (so a
/// corrupted length is also caught). `payload` must fit in a `u16` length.
#[must_use]
pub fn encode_frame(msg_type: u8, seq: u16, payload: &[u8]) -> Vec<u8> {
    let len = payload.len().min(u16::MAX as usize);
    let mut frame = Vec::with_capacity(HEADER_LEN + len + CRC_LEN);
    frame.extend_from_slice(&SYNC);
    frame.extend_from_slice(&(len as u16).to_le_bytes());
    frame.push(msg_type);
    frame.extend_from_slice(&seq.to_le_bytes());
    frame.extend_from_slice(&payload[..len]);
    let crc = crc16(&frame[2..]);
    frame.extend_from_slice(&crc.to_le_bytes());
    frame
}

/// Accumulates carrier bytes and yields whole frames, resynchronizing on the SYNC magic after garbage
/// or a CRC failure. A byte-stream transport (USB-CDC / UART) pushes received bytes here.
#[derive(Default)]
pub struct FrameReader {
    buf: Vec<u8>,
}

impl FrameReader {
    /// A new, empty reader.
    #[must_use]
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// Append received carrier bytes.
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Pull the next complete, CRC-valid frame, or `None` if more bytes are needed. Leading garbage and
    /// a CRC-failed frame are discarded (resync on the next SYNC).
    pub fn next_frame(&mut self) -> Option<Frame> {
        loop {
            match find_sync(&self.buf) {
                Some(0) => {}
                Some(pos) => {
                    self.buf.drain(0..pos);
                }
                None => {
                    let keep = usize::from(self.buf.last() == Some(&SYNC[0]));
                    let drop = self.buf.len() - keep;
                    self.buf.drain(0..drop);
                    return None;
                }
            }
            if self.buf.len() < HEADER_LEN {
                return None;
            }
            let len = u16::from_le_bytes([self.buf[2], self.buf[3]]) as usize;
            let frame_len = HEADER_LEN + len + CRC_LEN;
            if self.buf.len() < frame_len {
                return None;
            }
            let computed = crc16(&self.buf[2..HEADER_LEN + len]);
            let stored = u16::from_le_bytes([self.buf[HEADER_LEN + len], self.buf[HEADER_LEN + len + 1]]);
            if computed != stored {
                self.buf.drain(0..1);
                continue;
            }
            let frame = Frame {
                msg_type: self.buf[4],
                seq: u16::from_le_bytes([self.buf[5], self.buf[6]]),
                payload: self.buf[HEADER_LEN..HEADER_LEN + len].to_vec(),
            };
            self.buf.drain(0..frame_len);
            return Some(frame);
        }
    }
}

/// The index of the first `SYNC` magic in `buf`, if any.
fn find_sync(buf: &[u8]) -> Option<usize> {
    if buf.len() < 2 {
        return None;
    }
    (0..=buf.len() - 2).find(|&i| buf[i] == SYNC[0] && buf[i + 1] == SYNC[1])
}

/// A supported protocol version range `[min, max]`. Advertising a RANGE (not a single number) lets a
/// new host talk to an old target (negotiate down) and an old host talk to a new target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProtocolRange {
    /// The lowest protocol version this peer supports.
    pub min: u16,
    /// The highest protocol version this peer supports.
    pub max: u16,
}

impl ProtocolRange {
    /// A range supporting exactly one version.
    #[must_use]
    pub fn single(version: u16) -> Self {
        Self { min: version, max: version }
    }
}

impl Default for ProtocolRange {
    /// The range this build supports (currently just [`PROTOCOL_VERSION`]).
    fn default() -> Self {
        Self::single(PROTOCOL_VERSION)
    }
}

/// Optional protocol features, advertised independently of the version so a feature is a new bit rather
/// than a version bump. A session uses the INTERSECTION of the host's and target's capabilities.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Capabilities(pub u32);

impl Capabilities {
    /// Halt / resume / read memory.
    pub const DEBUG_BASIC: u32 = 1 << 0;
    /// Set / clear line breakpoints.
    pub const BREAKPOINTS: u32 = 1 << 1;
    /// Single-step (in / over / out).
    pub const STEPPING: u32 = 1 << 2;
    /// Write target memory.
    pub const MEM_WRITE: u32 = 1 << 3;
    /// Inspect managed locals / frames.
    pub const LOCALS: u32 = 1 << 4;
    /// Run a host-compiled program (or delta).
    pub const REPL_RUN: u32 = 1 << 5;
    /// Parse and interpret source on-device.
    pub const REPL_SOURCE: u32 = 1 << 6;
    /// Evaluate against an AOT-deployed target.
    pub const AOT_ATTACH: u32 = 1 << 7;

    /// Whether this set includes `flag`.
    #[must_use]
    pub fn has(self, flag: u32) -> bool {
        self.0 & flag == flag
    }

    /// The capabilities present in BOTH sets (what a session can use).
    #[must_use]
    pub fn intersect(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }
}

/// The host's opening `HELLO`: the version range + capabilities it supports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hello {
    /// The version range the host supports.
    pub range: ProtocolRange,
    /// The capabilities the host offers.
    pub caps: Capabilities,
}

impl Hello {
    /// `min(2) | max(2) | caps(4)`, little-endian.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut payload = Vec::with_capacity(8);
        payload.extend_from_slice(&self.range.min.to_le_bytes());
        payload.extend_from_slice(&self.range.max.to_le_bytes());
        payload.extend_from_slice(&self.caps.0.to_le_bytes());
        payload
    }

    /// Decode, tolerating a longer payload (a newer peer's trailing fields are skipped).
    #[must_use]
    pub fn decode(payload: &[u8]) -> Option<Self> {
        if payload.len() < 8 {
            return None;
        }
        Some(Self {
            range: ProtocolRange {
                min: u16::from_le_bytes([payload[0], payload[1]]),
                max: u16::from_le_bytes([payload[2], payload[3]]),
            },
            caps: Capabilities(u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]])),
        })
    }
}

/// The target's `HELLO_ACK`: the negotiated version + the target's capabilities.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HelloAck {
    /// The negotiated protocol version (the top of the overlapping range).
    pub chosen: u16,
    /// The capabilities the target offers.
    pub caps: Capabilities,
}

impl HelloAck {
    /// `chosen(2) | caps(4)`, little-endian.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut payload = Vec::with_capacity(6);
        payload.extend_from_slice(&self.chosen.to_le_bytes());
        payload.extend_from_slice(&self.caps.0.to_le_bytes());
        payload
    }

    /// Decode, tolerating a longer payload (a newer peer's trailing fields are skipped).
    #[must_use]
    pub fn decode(payload: &[u8]) -> Option<Self> {
        if payload.len() < 6 {
            return None;
        }
        Some(Self {
            chosen: u16::from_le_bytes([payload[0], payload[1]]),
            caps: Capabilities(u32::from_le_bytes([payload[2], payload[3], payload[4], payload[5]])),
        })
    }
}

/// The target's `NAK`: no version overlap; reports the target's own range so the host can diagnose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Nak {
    /// The target's own supported range (so the host can diagnose the mismatch).
    pub target_range: ProtocolRange,
}

impl Nak {
    /// `min(2) | max(2)`, little-endian.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut payload = Vec::with_capacity(4);
        payload.extend_from_slice(&self.target_range.min.to_le_bytes());
        payload.extend_from_slice(&self.target_range.max.to_le_bytes());
        payload
    }

    /// Decode, tolerating a longer payload.
    #[must_use]
    pub fn decode(payload: &[u8]) -> Option<Self> {
        if payload.len() < 4 {
            return None;
        }
        Some(Self {
            target_range: ProtocolRange {
                min: u16::from_le_bytes([payload[0], payload[1]]),
                max: u16::from_le_bytes([payload[2], payload[3]]),
            },
        })
    }
}

/// The highest version both ranges support, or `None` if the ranges are disjoint (-> a `NAK`).
pub fn negotiate(host: ProtocolRange, target: ProtocolRange) -> Option<u16> {
    let lo = host.min.max(target.min);
    let hi = host.max.min(target.max);
    (lo <= hi).then_some(hi)
}

/// The negotiated session parameters the host uses after a successful handshake.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Negotiated {
    /// The negotiated protocol version.
    pub version: u16,
    /// The capabilities both sides offer.
    pub caps: Capabilities,
}

/// The target's reply to a `HELLO`: accept with the negotiated version + the target's capabilities, or
/// reject with the target's range.
pub fn target_respond(host: &Hello, target_range: ProtocolRange, target_caps: Capabilities) -> Result<HelloAck, Nak> {
    match negotiate(host.range, target_range) {
        Some(chosen) => Ok(HelloAck { chosen, caps: target_caps }),
        None => Err(Nak { target_range }),
    }
}

/// The host's session parameters from the target's `HELLO_ACK`: the chosen version + the capability
/// INTERSECTION (only what both sides offer).
#[must_use]
pub fn host_finish(ack: &HelloAck, host_caps: Capabilities) -> Negotiated {
    Negotiated { version: ack.chosen, caps: host_caps.intersect(ack.caps) }
}

/// A carrier error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportError {
    /// The link is closed / disconnected.
    Closed,
    /// A carrier-level failure (I/O, USB).
    Carrier,
}

/// The carrier seam, at the FRAME level: a byte carrier (USB-CDC / UART) implements it over the
/// [`encode_frame`] / [`FrameReader`] framing; a packet carrier (HID / WinUSB) wraps frames into its
/// reports / bulk transfers. Non-blocking: [`Transport::poll`] returns `None` when no frame is ready.
pub trait Transport {
    /// Send one logical frame.
    fn send(&mut self, msg_type: u8, seq: u16, payload: &[u8]) -> Result<(), TransportError>;
    /// Return the next received frame, or `None` if none is ready yet.
    fn poll(&mut self) -> Result<Option<Frame>, TransportError>;
}

/// An in-memory [`Transport`] for tests / a host-side loopback: `send` encodes into `sent` (which a test
/// hands to the peer via [`MemTransport::feed`]), `poll` decodes fed bytes. No carrier, never errors.
#[derive(Default)]
pub struct MemTransport {
    reader: FrameReader,
    /// Encoded bytes this side has sent (a test feeds them to the peer).
    pub sent: Vec<u8>,
}

impl MemTransport {
    /// A new, empty in-memory transport.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Deliver bytes the peer sent.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.reader.push(bytes);
    }

    /// Take + clear the bytes this side has sent.
    pub fn take_sent(&mut self) -> Vec<u8> {
        core::mem::take(&mut self.sent)
    }
}

impl Transport for MemTransport {
    fn send(&mut self, msg_type: u8, seq: u16, payload: &[u8]) -> Result<(), TransportError> {
        self.sent.extend_from_slice(&encode_frame(msg_type, seq, payload));
        Ok(())
    }

    fn poll(&mut self) -> Result<Option<Frame>, TransportError> {
        Ok(self.reader.next_frame())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn frame_round_trips() {
        let bytes = encode_frame(msg::HELLO, 7, &[1, 2, 3, 4]);
        let mut reader = FrameReader::new();
        reader.push(&bytes);
        let frame = reader.next_frame().expect("a complete frame");
        assert_eq!(frame.msg_type, msg::HELLO);
        assert_eq!(frame.seq, 7);
        assert_eq!(frame.payload, vec![1, 2, 3, 4]);
        assert!(reader.next_frame().is_none());
    }

    #[test]
    fn reader_reassembles_across_chunks() {
        let bytes = encode_frame(0x42, 0xBEEF, &[9, 9, 9]);
        let mut reader = FrameReader::new();
        for (i, b) in bytes.iter().enumerate() {
            reader.push(&[*b]);
            let frame = reader.next_frame();
            if i + 1 < bytes.len() {
                assert!(frame.is_none(), "no frame until the last byte");
            } else {
                let frame = frame.expect("the final byte completes the frame");
                assert_eq!(frame.msg_type, 0x42);
                assert_eq!(frame.seq, 0xBEEF);
                assert_eq!(frame.payload, vec![9, 9, 9]);
            }
        }
    }

    #[test]
    fn reader_resyncs_past_leading_garbage_and_a_corrupt_frame() {
        let good = encode_frame(msg::PING, 1, &[0xAB]);
        let mut corrupt = encode_frame(msg::PING, 2, &[0xCD]);
        let last = corrupt.len() - 1;
        corrupt[last] ^= 0xFF;

        let mut reader = FrameReader::new();
        reader.push(&[0x00, 0xFF, 0x4C, 0x11]);
        reader.push(&corrupt);
        reader.push(&good);
        let frame = reader.next_frame().expect("the good frame survives the garbage + corruption");
        assert_eq!(frame.msg_type, msg::PING);
        assert_eq!(frame.seq, 1);
        assert_eq!(frame.payload, vec![0xAB]);
    }

    #[test]
    fn version_ranges_negotiate_to_the_highest_common() {
        assert_eq!(negotiate(ProtocolRange { min: 1, max: 3 }, ProtocolRange { min: 2, max: 5 }), Some(3));
        assert_eq!(negotiate(ProtocolRange::single(1), ProtocolRange::single(1)), Some(1));
        assert_eq!(negotiate(ProtocolRange { min: 1, max: 2 }, ProtocolRange { min: 3, max: 4 }), None);
    }

    #[test]
    fn hello_payloads_round_trip_and_tolerate_trailing_fields() {
        let hello = Hello {
            range: ProtocolRange { min: 1, max: 4 },
            caps: Capabilities(Capabilities::DEBUG_BASIC | Capabilities::REPL_RUN),
        };
        let mut payload = hello.encode();
        payload.extend_from_slice(&[0xDE, 0xAD]);
        assert_eq!(Hello::decode(&payload), Some(hello));
    }

    #[test]
    fn full_handshake_over_loopback() {
        let host_caps = Capabilities(Capabilities::DEBUG_BASIC | Capabilities::REPL_RUN);
        let target_range = ProtocolRange::single(1);
        let target_caps = Capabilities(Capabilities::DEBUG_BASIC | Capabilities::BREAKPOINTS);

        let mut host = MemTransport::new();
        let mut target = MemTransport::new();

        let hello = Hello { range: ProtocolRange { min: 1, max: 2 }, caps: host_caps };
        host.send(msg::HELLO, 0, &hello.encode()).unwrap();
        target.feed(&host.take_sent());

        let frame = target.poll().unwrap().expect("HELLO arrived");
        assert_eq!(frame.msg_type, msg::HELLO);
        let received = Hello::decode(&frame.payload).unwrap();
        let ack = target_respond(&received, target_range, target_caps).expect("a compatible version");
        target.send(msg::HELLO_ACK, frame.seq, &ack.encode()).unwrap();
        host.feed(&target.take_sent());

        let frame = host.poll().unwrap().expect("HELLO_ACK arrived");
        assert_eq!(frame.msg_type, msg::HELLO_ACK);
        let ack = HelloAck::decode(&frame.payload).unwrap();
        let session = host_finish(&ack, host_caps);

        assert_eq!(session.version, 1);
        assert!(session.caps.has(Capabilities::DEBUG_BASIC));
        assert!(!session.caps.has(Capabilities::REPL_RUN));
        assert!(!session.caps.has(Capabilities::BREAKPOINTS));
    }

    #[test]
    fn incompatible_versions_nak() {
        let host = Hello { range: ProtocolRange { min: 5, max: 6 }, caps: Capabilities::default() };
        let err = target_respond(&host, ProtocolRange::single(1), Capabilities::default());
        assert_eq!(err, Err(Nak { target_range: ProtocolRange::single(1) }));
    }
}
