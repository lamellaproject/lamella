//! The Lamella wireline debug + REPL protocol -- the carrier-agnostic core shared by the host
//! front-ends (the DAP adapter + the gdb/lldb-style CLI) and the on-device runner.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::vec::Vec;

/// The current protocol version this build implements. A peer advertises a [`ProtocolRange`] around it.
pub const PROTOCOL_VERSION: u16 = 1;

/// Identity + descriptors for the native driverless-WinUSB wireline carrier (the fast
/// interpreter-flash path): the shared VID/PID + WinUSB interface GUID + bulk endpoints, plus the
/// BOS / Microsoft OS 2.0 / WebUSB descriptor bytes a firmware embeds so Windows auto-loads
/// `winusb.sys` (no INF) and a browser can claim it. One home so firmware, host, and browser cannot
/// drift.
pub mod usb;

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
    /// Run a host-BAKED flash image (`RUN_IMAGE`) -- a PE-less constrained target sets this
    /// instead of [`Capabilities::REPL_RUN`]; the host bakes each submission and ships the image.
    pub const BAKED_IMAGE: u32 = 1 << 8;
    /// Debug the PERSISTENTLY DEPLOYED image in place (a 0-byte debug attach instead of
    /// re-sending the image over the wire) -- deploy-capable targets only.
    pub const DEBUG_ATTACH: u32 = 1 << 9;

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

/// The RESIDENT-PROFILE identity a target may append to its `HELLO_ACK` -- the board telling the
/// IDE what it is (docs/deployment-tiers.md), so a host scopes completion/validation to exactly
/// the surface the target carries and keys a cached manifest without a second round-trip.
///
/// `abi` is the intrinsic-ABI LEVEL (bumped only when an existing intrinsic's semantics change
/// incompatibly); `hash` is the CONTENT hash of the resident surface -- the intrinsic-registry
/// fingerprint, which already differs per profile build, folded with a resident corlib's content
/// hash once Tier-2 targets carry one. `name` is a short display / manifest-cache hint
/// ("netmf-v4_4", "kernel-floor"), capped at [`Self::NAME_CAP`] bytes so the identity stays a
/// fixed-size `Copy` value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProfileIdentity {
    /// The intrinsic-ABI level.
    pub abi: u16,
    /// The content hash of the resident surface.
    pub hash: u64,
    name_len: u8,
    name: [u8; Self::NAME_CAP],
}

impl ProfileIdentity {
    /// Maximum profile-name length on the wire (bytes; names are ASCII by convention).
    pub const NAME_CAP: usize = 16;
    /// Encoded size: `abi(2) | hash(8) | name_len(1)` before the name bytes.
    const FIXED_LEN: usize = 11;

    /// Build an identity; a `name` past [`Self::NAME_CAP`] bytes is truncated at a char boundary.
    #[must_use]
    pub fn new(abi: u16, hash: u64, name: &str) -> Self {
        let mut take = name.len().min(Self::NAME_CAP);
        while !name.is_char_boundary(take) {
            take -= 1;
        }
        let mut buf = [0u8; Self::NAME_CAP];
        buf[..take].copy_from_slice(&name.as_bytes()[..take]);
        Self { abi, hash, name_len: take as u8, name: buf }
    }

    /// The profile name.
    #[must_use]
    pub fn name(&self) -> &str {
        core::str::from_utf8(&self.name[..self.name_len as usize]).unwrap_or("")
    }

    fn encode_into(&self, payload: &mut Vec<u8>) {
        payload.extend_from_slice(&self.abi.to_le_bytes());
        payload.extend_from_slice(&self.hash.to_le_bytes());
        payload.push(self.name_len);
        payload.extend_from_slice(&self.name[..self.name_len as usize]);
    }

    /// Decode from `bytes`; `None` if the fixed head or the declared name is not fully present.
    fn decode_from(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < Self::FIXED_LEN {
            return None;
        }
        let abi = u16::from_le_bytes([bytes[0], bytes[1]]);
        let hash = u64::from_le_bytes(bytes[2..10].try_into().ok()?);
        let name_len = (bytes[10] as usize).min(Self::NAME_CAP);
        let name_bytes = bytes.get(Self::FIXED_LEN..Self::FIXED_LEN + name_len)?;
        let mut name = [0u8; Self::NAME_CAP];
        name[..name_len].copy_from_slice(name_bytes);
        Some(Self { abi, hash, name_len: name_len as u8, name })
    }
}

/// The target's `HELLO_ACK`: the negotiated version + the target's capabilities, optionally
/// followed by its resident-profile identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HelloAck {
    /// The negotiated protocol version (the top of the overlapping range).
    pub chosen: u16,
    /// The capabilities the target offers.
    pub caps: Capabilities,
    /// The target's resident-profile identity, when it advertises one (a pre-identity target, or
    /// a truncated tail, decodes as `None` -- the handshake itself never depends on it).
    pub profile: Option<ProfileIdentity>,
}

impl HelloAck {
    /// `chosen(2) | caps(4)`, little-endian, then (when present) the profile identity
    /// `abi(2) | hash(8) | name_len(1) | name`. An old host's decode skips the tail.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut payload = Vec::with_capacity(6 + ProfileIdentity::FIXED_LEN + ProfileIdentity::NAME_CAP);
        payload.extend_from_slice(&self.chosen.to_le_bytes());
        payload.extend_from_slice(&self.caps.0.to_le_bytes());
        if let Some(profile) = &self.profile {
            profile.encode_into(&mut payload);
        }
        payload
    }

    /// Decode, tolerating a longer payload (a newer peer's trailing fields are skipped) and an
    /// absent/short identity tail (`profile` = `None`).
    #[must_use]
    pub fn decode(payload: &[u8]) -> Option<Self> {
        if payload.len() < 6 {
            return None;
        }
        Some(Self {
            chosen: u16::from_le_bytes([payload[0], payload[1]]),
            caps: Capabilities(u32::from_le_bytes([payload[2], payload[3], payload[4], payload[5]])),
            profile: ProfileIdentity::decode_from(&payload[6..]),
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
    /// The target's resident-profile identity, when it advertised one.
    pub profile: Option<ProfileIdentity>,
}

/// The target's reply to a `HELLO`: accept with the negotiated version + the target's capabilities, or
/// reject with the target's range. The target attaches its [`ProfileIdentity`] afterwards (the
/// negotiation itself never depends on it).
pub fn target_respond(host: &Hello, target_range: ProtocolRange, target_caps: Capabilities) -> Result<HelloAck, Nak> {
    match negotiate(host.range, target_range) {
        Some(chosen) => Ok(HelloAck { chosen, caps: target_caps, profile: None }),
        None => Err(Nak { target_range }),
    }
}

/// The host's session parameters from the target's `HELLO_ACK`: the chosen version + the capability
/// INTERSECTION (only what both sides offer) + the target's profile identity as advertised.
#[must_use]
pub fn host_finish(ack: &HelloAck, host_caps: Capabilities) -> Negotiated {
    Negotiated { version: ack.chosen, caps: host_caps.intersect(ack.caps), profile: ack.profile }
}

/// The full resident-profile MANIFEST a target returns for a `GET_PROFILE` request (the message
/// ids live beside the serve loop in `lamella-runner`): the identity plus the complete resident
/// surface -- today the intrinsic-id list; a Tier-2 target grows a resident-assembly section in a
/// later manifest version. A host asks only when [`ProfileIdentity::hash`] misses its cache.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileManifest {
    /// The same identity the `HELLO_ACK` advertises.
    pub identity: ProfileIdentity,
    /// Every intrinsic id this target registers, in registry order.
    pub intrinsic_ids: Vec<u32>,
}

impl ProfileManifest {
    /// Manifest layout version.
    pub const VERSION: u8 = 1;

    /// `version(1) | abi(2) | hash(8) | name_len(1) | name | count(2) | count x id(4)`, LE.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut payload =
            Vec::with_capacity(1 + ProfileIdentity::FIXED_LEN + ProfileIdentity::NAME_CAP + 2 + self.intrinsic_ids.len() * 4);
        payload.push(Self::VERSION);
        self.identity.encode_into(&mut payload);
        let count = self.intrinsic_ids.len().min(u16::MAX as usize);
        payload.extend_from_slice(&(count as u16).to_le_bytes());
        for id in &self.intrinsic_ids[..count] {
            payload.extend_from_slice(&id.to_le_bytes());
        }
        payload
    }

    /// Decode, tolerating a longer payload; `None` on an unknown version or a truncated list.
    #[must_use]
    pub fn decode(payload: &[u8]) -> Option<Self> {
        if payload.first() != Some(&Self::VERSION) {
            return None;
        }
        let identity = ProfileIdentity::decode_from(payload.get(1..)?)?;
        let ids_at = 1 + ProfileIdentity::FIXED_LEN + identity.name_len as usize;
        let count_bytes = payload.get(ids_at..ids_at + 2)?;
        let count = u16::from_le_bytes([count_bytes[0], count_bytes[1]]) as usize;
        let mut intrinsic_ids = Vec::with_capacity(count);
        for index in 0..count {
            let at = ids_at + 2 + index * 4;
            let bytes = payload.get(at..at + 4)?;
            intrinsic_ids.push(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]));
        }
        Some(Self { identity, intrinsic_ids })
    }
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
    fn hello_ack_profile_identity_round_trips_and_stays_optional() {
        let bare = HelloAck { chosen: 1, caps: Capabilities(0x107), profile: None };
        let bytes = bare.encode();
        assert_eq!(bytes.len(), 6, "no identity -> the pre-identity 6-byte ack");
        assert_eq!(HelloAck::decode(&bytes), Some(bare));

        let identity = ProfileIdentity::new(1, 0xDEAD_BEEF_0BAD_F00D, "netmf-v4_4");
        let ack = HelloAck { profile: Some(identity), ..bare };
        let bytes = ack.encode();
        let back = HelloAck::decode(&bytes).expect("an extended ack decodes");
        assert_eq!(back.profile, Some(identity));
        assert_eq!(back.profile.expect("present").name(), "netmf-v4_4");
        assert_eq!(u16::from_le_bytes([bytes[0], bytes[1]]), 1);
        assert_eq!(bytes[2..6], 0x107u32.to_le_bytes());
        let truncated = HelloAck::decode(&bytes[..8]).expect("still an ack");
        assert_eq!(truncated.profile, None);
    }

    #[test]
    fn profile_identity_name_truncates_at_the_cap() {
        let identity = ProfileIdentity::new(1, 7, "a-very-long-profile-name-indeed");
        assert_eq!(identity.name().len(), ProfileIdentity::NAME_CAP);
        assert!("a-very-long-profile-name-indeed".starts_with(identity.name()));
    }

    #[test]
    fn profile_manifest_round_trips_and_fails_loud_on_damage() {
        let manifest = ProfileManifest {
            identity: ProfileIdentity::new(1, 42, "kernel-floor"),
            intrinsic_ids: vec![0x811c_9dc5, 1, 2, 3],
        };
        let bytes = manifest.encode();
        assert_eq!(ProfileManifest::decode(&bytes), Some(manifest.clone()));
        assert_eq!(
            ProfileManifest::decode(&bytes[..bytes.len() - 1]),
            None,
            "a truncated id list is a decode failure, not a short list"
        );
        assert_eq!(ProfileManifest::decode(&[9]), None, "an unknown version is rejected");
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
