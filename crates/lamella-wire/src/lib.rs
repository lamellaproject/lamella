//! The Lamella Link debug + REPL protocol -- the carrier-agnostic core shared by the host
//! front-ends (the DAP adapter + the gdb/lldb-style CLI) and the on-device runner.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::vec::Vec;

/// The current protocol version this build implements. A peer advertises a [`ProtocolRange`] around it.
pub const PROTOCOL_VERSION: u16 = 1;

/// Identity + descriptors for the native driverless-WinUSB Lamella Link carrier (the fast
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

/// What an [`msg::ERROR`] carries: why a frame was refused.
///
/// # Why a refusal is a message rather than a silence
///
/// [`msg::ERROR`] is the answer to a message type a target does not implement. Without one, such a
/// message is simply dropped and the host waits out its timeout -- and a timeout cannot be told apart from a board that
/// has stopped answering, which is the single most expensive ambiguity in bringing a target up. The
/// three explanations for silence are "I do not implement that", "I crashed", and "the cable is bad",
/// and they are three different repairs.
///
/// # The payload shape, and a note on its history
///
/// Byte 0 is the reason. **What follows depends on the reason** rather than being free text: for
/// [`error::UNKNOWN_MESSAGE_TYPE`] it is the one byte that was not understood, which is the whole of the
/// useful information and needs no strings on a target counting flash.
///
/// # A refusal is NOT a [`Nak`]
///
/// [`msg::NAK`] answers a [`Hello`] whose version range does not overlap: the session cannot begin at
/// all. A refusal happens inside a session that negotiated fine, about one frame. Keeping them apart
/// matters because the remedies are opposite -- one says use a different protocol version, the other
/// says this target does not do that thing and the rest of the session is unaffected.
pub mod error {
    /// The message type is not one this target implements. Byte 1 is that type.
    pub const UNKNOWN_MESSAGE_TYPE: u8 = 0x01;

    /// The payload refusing `msg_type` as unimplemented.
    #[must_use]
    pub fn unknown_message_type(msg_type: u8) -> [u8; 2] {
        [UNKNOWN_MESSAGE_TYPE, msg_type]
    }

    /// The message type an [`unknown_message_type`] payload names, or `None` when the payload is some
    /// other refusal.
    ///
    /// Both sides go through this pair rather than slicing the bytes at each site, so the layout has one
    /// definition and a host cannot read a field a target never wrote.
    #[must_use]
    pub fn refused_message_type(payload: &[u8]) -> Option<u8> {
        match payload {
            [UNKNOWN_MESSAGE_TYPE, msg_type, ..] => Some(*msg_type),
            _ => None,
        }
    }
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

    /// Append received carrier bytes. Growth is RESERVE-EXACT to the frame length the
    /// header declares (once enough of it has arrived), not amplify-by-doubling: on a
    /// bump-allocator target each stale doubling of a multi-KB frame's buffer is permanent
    /// arena spend within the request, roughly doubling the assembly's high-water.
    pub fn push(&mut self, bytes: &[u8]) {
        let needed = self.buf.len() + bytes.len();
        if needed > self.buf.capacity() {
            let target = needed.max(self.expected_frame_end());
            self.buf.reserve_exact(target - self.buf.len());
        }
        self.buf.extend_from_slice(bytes);
    }

    /// Where the frame currently being assembled ends in `buf` (its sync offset plus the
    /// header-declared full frame length), or `0` when no header is readable yet -- the
    /// exact capacity [`FrameReader::push`] reserves toward.
    fn expected_frame_end(&self) -> usize {
        let Some(sync) = find_sync(&self.buf) else {
            return 0;
        };
        if self.buf.len() < sync + HEADER_LEN {
            return 0;
        }
        let len = u16::from_le_bytes([self.buf[sync + 2], self.buf[sync + 3]]) as usize;
        sync + HEADER_LEN + len + CRC_LEN
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
    /// The `HELLO_ACK` profile identity carries the STRUCTURED board/chip fields
    /// ([`ProfileIdentity::board_model`] / `chip_idcode` / `chip_devid`), so a host can
    /// identify the hardware without parsing a display name. The fields may still read
    /// `0` = unknown (a custom board reports no model; a firmware may not know its ids).
    pub const PROFILE_CHIPID: u32 = 1 << 10;
    /// On-device TELEMETRY / live-signal SCOPE (the `lamella_runner::telemetry` 0x40 message
    /// range). RESERVED: no firmware advertises this bit and no host may rely on it. First of
    /// the reserved 11-31 capability band.
    pub const TELEMETRY: u32 = 1 << 11;
    /// The target holds a RESIDENT corlib in flash, so it accepts a bare program assembly and
    /// resolves that program's corlib references out of the resident copy -- a program reaches the
    /// board as its own kilobytes instead of as an image carrying a corlib with it.
    ///
    /// # Presence is only half the question, and the other half is why this bit is not enough alone
    ///
    /// A host must also know it is the corlib the program was compiled against. Getting that wrong is
    /// SILENT: a corlib declaring a seam the firmware compiled out still loads, and the method keeps a
    /// placeholder body that returns zero. **So [`ProfileIdentity::hash`] covers the resident corlib's
    /// contents**, and a host that recorded that hash for a firmware it trusts can compare and be sure.
    /// This bit says the path exists; the hash says it is the right one.
    pub const RESIDENT_CORLIB: u32 = 1 << 13;
    /// The target implements the BUNDLE message types (`lamella_runner::bundle`) -- an artifact it
    /// loads through a different front end than a baked image. RESERVED: no firmware advertises this
    /// bit yet.
    ///
    /// **This is what a host should gate on, in preference to sending the op and reading the answer.**
    /// Both work -- an unimplemented type is refused with [`msg::ERROR`] naming it (see [`error`]) rather
    /// than dropped -- but this bit arrives in the `HELLO_ACK` a session already exchanges, so gating on
    /// it costs no round-trip and no wait, and it says what a target CAN do rather than one thing at a
    /// time.
    pub const BUNDLE: u32 = 1 << 12;
    /// The target answers a memory READ and WRITE (`lamella_runner::live`) **while a deployed app
    /// is still running** -- the on-target half of a host-side REPL evaluating against a live
    /// program, rather than against a stopped one.
    ///
    /// # Why this is a separate bit from [`DEBUG_BASIC`] and [`MEM_WRITE`]
    ///
    /// Those two describe the same two verbs on the HALTED debug channel, where the program is
    /// stopped at a known point and its state is at rest. This bit says the verbs are answered with
    /// the program in motion, which is a different promise about a different situation: what a host
    /// reads may be mid-update, and what it writes lands in a program that did not expect it.
    ///
    /// The distinction is not academic on a controller. Halting a live one is an EVENT -- a motor
    /// keeps turning, a valve stays where it was -- so "inspect a running system" and "poke a
    /// stopped one" are separate products, and a host must be able to tell which it is talking to.
    /// A target that only sets [`DEBUG_BASIC`] can still be inspected; it just has to be stopped
    /// first, and the host has to say so.
    pub const LIVE_MEMORY: u32 = 1 << 14;

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
/// IDE what it is, so a host scopes completion/validation to exactly
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
    /// The content hash of the resident surface: the intrinsic registry, **and the resident corlib's
    /// bytes when the target holds one** ([`Capabilities::RESIDENT_CORLIB`]).
    ///
    /// The corlib belongs in here because it IS part of the resident surface, and leaving it out made
    /// this field unable to tell apart two firmwares that differ only in the corlib they carry -- which
    /// is exactly the difference a host must not get wrong, since a mismatched corlib does not fail to
    /// load, it returns zero from a seam the firmware compiled out. A target with no resident corlib
    /// reports the registry's fingerprint alone, unchanged.
    pub hash: u64,
    /// The BOARD model code from [`board_model`] -- the product the firmware was built for,
    /// so a host names it without parsing a string. `0` = unknown (a custom board).
    pub board_model: u16,
    /// The chip's debug-port IDCODE (the SW-DP `DPIDR` the silicon answers to a probe) --
    /// the same value a probe-side identify reads, so the host's chip registry serves both
    /// paths. Known per chip family at firmware build time. `0` = unknown.
    pub chip_idcode: u32,
    /// The vendor DEVICE-ID register value (the family's DSU DID / DBGMCU_IDCODE / CHIP_ID
    /// style die id), read by the firmware from its own silicon at boot where the read is
    /// implemented. Distinguishes SKUs a shared `DPIDR` cannot. `0` = unknown.
    pub chip_devid: u32,
    name_len: u8,
    name: [u8; Self::NAME_CAP],
}

impl ProfileIdentity {
    /// Maximum profile-name length on the wire (bytes; names are ASCII by convention).
    pub const NAME_CAP: usize = 16;
    /// Encoded size: `abi(2) | hash(8) | name_len(1)` before the name bytes.
    const FIXED_LEN: usize = 11;
    /// Encoded size of the board/chip identity tail: `board_model(2) | chip_idcode(4) |
    /// chip_devid(4)`, after the name bytes ([`Capabilities::PROFILE_CHIPID`]).
    const CHIP_LEN: usize = 10;

    /// Build an identity; a `name` past [`Self::NAME_CAP`] bytes is truncated at a char boundary.
    /// The board/chip fields start `0` = unknown; [`Self::with_chip`] fills them.
    #[must_use]
    pub fn new(abi: u16, hash: u64, name: &str) -> Self {
        let mut take = name.len().min(Self::NAME_CAP);
        while !name.is_char_boundary(take) {
            take -= 1;
        }
        let mut buf = [0u8; Self::NAME_CAP];
        buf[..take].copy_from_slice(&name.as_bytes()[..take]);
        Self {
            abi,
            hash,
            board_model: 0,
            chip_idcode: 0,
            chip_devid: 0,
            name_len: take as u8,
            name: buf,
        }
    }

    /// The identity with its board/chip fields filled (each `0` = unknown stays honest --
    /// a custom board passes `board_model::UNKNOWN` and still names its silicon).
    #[must_use]
    pub fn with_chip(mut self, board_model: u16, chip_idcode: u32, chip_devid: u32) -> Self {
        self.board_model = board_model;
        self.chip_idcode = chip_idcode;
        self.chip_devid = chip_devid;
        self
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
        payload.extend_from_slice(&self.board_model.to_le_bytes());
        payload.extend_from_slice(&self.chip_idcode.to_le_bytes());
        payload.extend_from_slice(&self.chip_devid.to_le_bytes());
    }

    /// Decode from `bytes` as a message TAIL (the `HELLO_ACK` shape): the board/chip fields
    /// are read when present and stay `0` = unknown when a pre-chip-id target sent only the
    /// shorter identity. `None` if the fixed head or the declared name is not fully present.
    fn decode_from(bytes: &[u8]) -> Option<Self> {
        let (identity, consumed) = Self::decode_head(bytes, false)?;
        match bytes.get(consumed..consumed + Self::CHIP_LEN) {
            Some(_) => Some(Self::decode_head(bytes, true)?.0),
            None => Some(identity),
        }
    }

    /// Decode the identity head, `with_chip` selecting whether the board/chip tail is part of
    /// the layout (a versioned container like [`ProfileManifest`] knows; a message tail
    /// detects by length). Returns the identity and the bytes consumed.
    fn decode_head(bytes: &[u8], with_chip: bool) -> Option<(Self, usize)> {
        if bytes.len() < Self::FIXED_LEN {
            return None;
        }
        let abi = u16::from_le_bytes([bytes[0], bytes[1]]);
        let hash = u64::from_le_bytes(bytes[2..10].try_into().ok()?);
        let name_len = (bytes[10] as usize).min(Self::NAME_CAP);
        let name_bytes = bytes.get(Self::FIXED_LEN..Self::FIXED_LEN + name_len)?;
        let mut name = [0u8; Self::NAME_CAP];
        name[..name_len].copy_from_slice(name_bytes);
        let mut consumed = Self::FIXED_LEN + name_len;
        let (board_model, chip_idcode, chip_devid) = if with_chip {
            let tail = bytes.get(consumed..consumed + Self::CHIP_LEN)?;
            consumed += Self::CHIP_LEN;
            (
                u16::from_le_bytes([tail[0], tail[1]]),
                u32::from_le_bytes([tail[2], tail[3], tail[4], tail[5]]),
                u32::from_le_bytes([tail[6], tail[7], tail[8], tail[9]]),
            )
        } else {
            (0, 0, 0)
        };
        Some((
            Self {
                abi,
                hash,
                board_model,
                chip_idcode,
                chip_devid,
                name_len: name_len as u8,
                name,
            },
            consumed,
        ))
    }
}

/// Known BOARD model codes for [`ProfileIdentity::board_model`] -- the products the in-tree
/// serve firmwares are built for. The host registries (Studio's `boards.js`, Code's device UI)
/// mirror these codes to display names; `0` stays "unknown" so a custom board never lies.
/// Codes are wire values: append-only, never renumber.
pub mod board_model {
    /// Unknown / custom board (the chip fields may still identify the silicon).
    pub const UNKNOWN: u16 = 0;
    /// BBC micro:bit v1 (nRF51822).
    pub const MICROBIT_V1: u16 = 1;
    /// Raspberry Pi Pico 2 (RP2350).
    pub const PICO2: u16 = 2;
    /// SAM4S Xplained Pro (ATSAM4SD32C).
    pub const SAM4S_XPLAINED_PRO: u16 = 3;
    /// SAM E54 Xplained Pro (ATSAME54P20A).
    pub const SAME54_XPLAINED_PRO: u16 = 4;
    /// SAM D21 Xplained Pro (ATSAMD21J18A).
    pub const SAMD21_XPLAINED_PRO: u16 = 5;
    /// SAM W25 Xplained Pro (ATSAMW25: a SAMD21G18A host MCU + WINC1500 WiFi module).
    pub const SAMW25_XPLAINED_PRO: u16 = 6;
    /// STM32F091 Nucleo-64 (STM32F091RC).
    pub const STM32F091: u16 = 7;
    /// The STM32L476 bench board.
    pub const STM32L476: u16 = 8;
    /// Arduino MKR1000 (SAMD21G18A host MCU + WINC1500).
    pub const MKR1000: u16 = 9;
    /// Raspberry Pi Pico 2 W (RP2350 + CYW43439 WiFi).
    pub const PICO2_W: u16 = 10;
    /// BBC micro:bit v2 / v2.1 (Nordic nRF52833).
    pub const MICROBIT_V2: u16 = 11;
    /// Arduino Zero (ATSAMD21G18A + on-board EDBG).
    pub const ARDUINO_ZERO: u16 = 12;
    /// Raspberry Pi Pico / Pico H (RP2040, no wireless). The Pico W / WH add the CYW43439.
    pub const PICO: u16 = 13;
    /// Espressif ESP32-C6 (RISC-V RV32IMAC HP core) -- a second-source RISC-V beyond the RP2350.
    pub const ESP32_C6: u16 = 14;
    /// Arduino Due (ATSAM3X8E, Cortex-M3, 2x256 KiB EEFC planes) -- the first `sam3x`-family board.
    pub const ARDUINO_DUE: u16 = 15;
    /// Adafruit Feather M0 Express (ATSAMD21G18A, Cortex-M0+) -- a `samd21`-family board with no
    /// on-board debugger and no bridge UART: it is programmed through its own USB bootloader.
    pub const FEATHER_M0_EXPRESS: u16 = 16;
    /// Adafruit Feather M0 WiFi (ATSAMD21G18A + an ATWINC1500 on the board) -- the same host MCU
    /// and bootloader-only deploy path as the Express, wired to the WiFi part the ATSAMW25 module
    /// integrates. Distinct board: the WINC reaches a different SERCOM on different pads.
    pub const FEATHER_M0_WIFI: u16 = 17;
    /// Adafruit Feather RP2040 Adalogger (RP2040, 8 MB flash, microSD, STEMMA QT) -- the first
    /// `rp2040`-family board that is not a Raspberry Pi one, and the first with an on-board
    /// microSD slot.
    pub const FEATHER_RP2040_ADALOGGER: u16 = 18;
    /// Adafruit Feather M0 Adalogger (ATSAMD21G18A, Cortex-M0+, microSD, no SPI flash) -- the same
    /// host MCU and bootloader-only deploy path as the Express and the WiFi, distinguished by what
    /// is soldered beside it: a card slot on SERCOM4 where the Express has 2 MB of SPI flash.
    pub const FEATHER_M0_ADALOGGER: u16 = 19;
    /// ST 32F746GDISCOVERY (STM32F746NG, Cortex-M7) -- the first `stm32f7`-family board, and the
    /// first whose UART crosses two GPIO ports (PA9 transmits, PB7 receives).
    pub const STM32F746G_DISCO: u16 = 20;

    /// ST STM32F429I-DISC1 (STM32F429ZI, Cortex-M4F).
    pub const STM32F429I_DISCO: u16 = 21;

    /// The display name for a `board_model` wire value, or `None` for an unrecognized code. This is the one
    /// canonical value -> name map: every surface that displays a board name derives from it rather than
    /// keeping a table of its own. Add a board => one `const` above plus one arm here, and each of those
    /// surfaces picks it up.
    #[must_use]
    pub fn name(model: u16) -> Option<&'static str> {
        Some(match model {
            UNKNOWN => "custom board",
            MICROBIT_V1 => "BBC micro:bit v1",
            PICO2 => "Raspberry Pi Pico 2",
            SAM4S_XPLAINED_PRO => "SAM4S Xplained Pro",
            SAME54_XPLAINED_PRO => "SAM E54 Xplained Pro",
            SAMD21_XPLAINED_PRO => "SAMD21 Xplained Pro",
            SAMW25_XPLAINED_PRO => "ATSAMW25 Xplained Pro",
            STM32F091 => "STM32F091 Nucleo-64",
            STM32L476 => "STM32L476 Nucleo",
            MKR1000 => "Arduino MKR1000",
            PICO2_W => "Raspberry Pi Pico 2 W",
            MICROBIT_V2 => "BBC micro:bit v2",
            ARDUINO_ZERO => "Arduino Zero",
            PICO => "Raspberry Pi Pico",
            ESP32_C6 => "Espressif ESP32-C6",
            ARDUINO_DUE => "Arduino Due",
            FEATHER_M0_EXPRESS => "Adafruit Feather M0 Express",
            FEATHER_M0_WIFI => "Adafruit Feather M0 WiFi",
            FEATHER_RP2040_ADALOGGER => "Adafruit Feather RP2040 Adalogger",
            FEATHER_M0_ADALOGGER => "Adafruit Feather M0 Adalogger",
            STM32F746G_DISCO => "STM32F746G Discovery",
            STM32F429I_DISCO => "STM32F429I Discovery",
            _ => return None,
        })
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
    /// Manifest layout version. v2 added the identity's board/chip tail
    /// (`board_model(2) | chip_idcode(4) | chip_devid(4)` between the name and the count);
    /// v1 manifests (pre-chip-id targets) still decode, their chip fields reading `0`.
    pub const VERSION: u8 = 2;

    /// `version(1) | abi(2) | hash(8) | name_len(1) | name | board_model(2) |
    /// chip_idcode(4) | chip_devid(4) | count(2) | count x id(4)`, LE.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut payload = Vec::with_capacity(
            1 + ProfileIdentity::FIXED_LEN
                + ProfileIdentity::NAME_CAP
                + ProfileIdentity::CHIP_LEN
                + 2
                + self.intrinsic_ids.len() * 4,
        );
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
    /// A v1 payload (a pre-chip-id target) decodes with its chip fields at `0` = unknown.
    #[must_use]
    pub fn decode(payload: &[u8]) -> Option<Self> {
        let with_chip = match payload.first() {
            Some(1) => false,
            Some(&Self::VERSION) => true,
            _ => return None,
        };
        let (identity, consumed) = ProfileIdentity::decode_head(payload.get(1..)?, with_chip)?;
        let ids_at = 1 + consumed;
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
    fn board_chip_identity_rides_the_ack_and_degrades_to_unknown() {
        let identity = ProfileIdentity::new(1, 42, "kernel-floor").with_chip(
            board_model::SAMW25_XPLAINED_PRO,
            0x0bc1_1477,
            0xDEAD_0001,
        );
        let ack = HelloAck { chosen: 1, caps: Capabilities(Capabilities::PROFILE_CHIPID), profile: Some(identity) };
        let back = HelloAck::decode(&ack.encode()).expect("decodes");
        let profile = back.profile.expect("identity present");
        assert_eq!(profile.board_model, board_model::SAMW25_XPLAINED_PRO);
        assert_eq!(profile.chip_idcode, 0x0bc1_1477);
        assert_eq!(profile.chip_devid, 0xDEAD_0001);
        assert_eq!(profile, identity);

        let mut old_wire = ack.encode();
        old_wire.truncate(old_wire.len() - ProfileIdentity::CHIP_LEN);
        let old = HelloAck::decode(&old_wire).expect("still an ack").profile.expect("identity present");
        assert_eq!((old.board_model, old.chip_idcode, old.chip_devid), (0, 0, 0));
        assert_eq!(old.name(), "kernel-floor");

        let mut ragged = ack.encode();
        ragged.truncate(ragged.len() - 3);
        let ragged = HelloAck::decode(&ragged).expect("still an ack").profile.expect("present");
        assert_eq!(ragged.chip_idcode, 0);
    }

    #[test]
    fn profile_manifest_v1_still_decodes_with_unknown_chip() {
        let mut v1 = vec![1u8];
        v1.extend_from_slice(&7u16.to_le_bytes());
        v1.extend_from_slice(&99u64.to_le_bytes());
        v1.push(2);
        v1.extend_from_slice(b"kf");
        v1.extend_from_slice(&2u16.to_le_bytes());
        v1.extend_from_slice(&10u32.to_le_bytes());
        v1.extend_from_slice(&11u32.to_le_bytes());
        let manifest = ProfileManifest::decode(&v1).expect("a v1 manifest decodes");
        assert_eq!(manifest.identity.abi, 7);
        assert_eq!(manifest.identity.name(), "kf");
        assert_eq!(manifest.identity.chip_idcode, 0, "v1 carries no chip identity");
        assert_eq!(manifest.intrinsic_ids, vec![10, 11]);
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
