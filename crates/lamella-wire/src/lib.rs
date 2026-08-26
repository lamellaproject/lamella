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

/// Which carrier owns the one debug session a target has to give, when several can reach it at
/// once. A pure decision -- no carrier, no input or output -- so the rule can be tested whole.
pub mod session;

/// Which pairing keys a target accepts, and when a replacement displaces the key it replaces.
/// The rotation policy only -- no cryptography, which the caller supplies as a verifier.
pub mod pairing;

/// Carrying frames between two carriers, for a target a host cannot open directly. Forwards
/// without interpreting and without reframing, so a message type it has never heard of crosses.
pub mod relay;

/// Identity for the TCP carrier -- the port a target serving Lamella Link listens on by default.
///
/// One home, for the same reason the USB identity has one: a host, a board's firmware and a relay
/// daemon must agree, and a number written down in three places is a number that drifts.
pub mod tcp {
    /// The default TCP port for Lamella Link.
    ///
    /// Not registered with any numbering authority, and deliberately not one of the ports a
    /// debug-adapter protocol already claims -- a board on a desk should not have to compete with
    /// whatever else is listening. The value is the decimal reading of this project's USB vendor
    /// id ([`super::usb::VID`]), so the two identities are the same number in two notations and
    /// neither has to be memorized separately.
    ///
    /// It is a DEFAULT, not a requirement. A relay daemon forwarding several boards gives each one
    /// its own port, and every tool that takes a target string takes an explicit port with it.
    pub const DEFAULT_PORT: u16 = super::usb::VID;
}

/// The frame's leading sync magic ("LW" -- Lamella Wire). A receiver scans for it to find a frame
/// boundary after attaching mid-stream or recovering from line noise.
const SYNC: [u8; 2] = [0x4C, 0x57];
/// Bytes before the payload: `SYNC(2) | LEN(2) | TYPE(1) | SEQ(2)`.
const HEADER_LEN: usize = 7;
/// Trailing CRC-16 width.
const CRC_LEN: usize = 2;

/// The most payload bytes one frame can carry, which is what a `u16` `LEN` field can count.
///
/// Public because it is a fact a SENDER has to plan around rather than discover: a caller with more
/// than this must chunk (as the deploy, bundle and module-firmware ops do), and the alternative to
/// knowing the number is finding out from [`encode_frame`] returning `None` after the data already
/// exists.
pub const MAX_PAYLOAD: usize = u16::MAX as usize;

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
    /// Target -> host, unsolicited: the session this carrier held has been taken by another one.
    /// The payload is the new holder's [`super::session::ChannelClass`] as one byte.
    ///
    /// # Why the loser is told rather than left to find out
    ///
    /// A target that can be reached on several carriers at once has one session to give, and a
    /// carrier can lose it while its host believes it still has it. Without this, that host
    /// discovers the loss from its next operation being refused -- or, if it was only listening,
    /// never discovers it at all and simply reports a target that went quiet.
    ///
    /// Naming the new holder's CLASS is the useful part: *a cable took it* tells a remote user
    /// that somebody is at the board, which is a situation to wait out rather than a fault to
    /// investigate.
    pub const SESSION_REVOKED: u8 = 0x07;
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
/// So an unimplemented message type is REFUSED rather than ignored: a two-byte reply turns the worst
/// observable into the most precise one.
///
/// # The payload shape
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

    /// Another carrier holds the debug session. Byte 1 is that carrier's
    /// [`super::session::ChannelClass`].
    ///
    /// Distinct from every other refusal because the remedy is neither to stop asking nor to
    /// reconnect: the request was well formed and the target implements it, and the answer will
    /// change when the other carrier lets go. **A caller that reads this as a fault reports a
    /// broken board where the truth is that a colleague has it plugged in.**
    pub const SESSION_HELD: u8 = 0x02;

    /// The payload refusing a request because another carrier of class `holder` holds the session.
    #[must_use]
    pub fn session_held(holder: u8) -> [u8; 2] {
        [SESSION_HELD, holder]
    }

    /// The holding carrier's class byte from a [`session_held`] payload, or `None` when the
    /// payload is some other refusal.
    #[must_use]
    pub fn session_holder(payload: &[u8]) -> Option<u8> {
        match payload {
            [SESSION_HELD, holder, ..] => Some(*holder),
            _ => None,
        }
    }

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
/// corrupted length is also caught).
///
/// `None` when `payload` exceeds [`MAX_PAYLOAD`] -- such a payload has no representation on this
/// wire, and there is nothing this function can return that would be one.
///
/// # Why this refuses instead of truncating
///
/// Clamping the length to `u16::MAX` and computing the CRC **over the truncated bytes** produces a
/// perfectly well-formed frame: correct magic, consistent length, valid checksum, and silently
/// missing content. Every integrity mechanism the framing has agrees it is fine, and the receiver
/// has no way to tell -- a short payload and a truncated one are the same bytes.
///
/// **That is the worst failure shape available here**, and worse than the silence a refusal
/// produces: silence is at least a symptom. A CRC that validates over incomplete data converts a
/// sender's mistake into a receiver's wrong answer, and the receiver is the side that cannot
/// possibly diagnose it.
///
/// It is reachable rather than theoretical. Callers that stream (the deploy, bundle and
/// module-firmware ops) chunk and are fine, but a target building a `RUN_RESULT` from a deployed
/// program's console output does not chunk -- a chatty program on a roomy part is one path to a
/// payload this cannot carry.
#[must_use]
pub fn encode_frame(msg_type: u8, seq: u16, payload: &[u8]) -> Option<Vec<u8>> {
    if payload.len() > MAX_PAYLOAD {
        return None;
    }
    let len = payload.len();
    let mut frame = Vec::with_capacity(HEADER_LEN + len + CRC_LEN);
    frame.extend_from_slice(&SYNC);
    frame.extend_from_slice(&(len as u16).to_le_bytes());
    frame.push(msg_type);
    frame.extend_from_slice(&seq.to_le_bytes());
    frame.extend_from_slice(payload);
    let crc = crc16(&frame[2..]);
    frame.extend_from_slice(&crc.to_le_bytes());
    Some(frame)
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
    /// loads through a different front end than a baked image.
    ///
    /// **Advertised by the micro:bit v2 Python serve (`microbit-v2-py`), which sets this bit and no
    /// other**: that tier has no CIL runtime, no baked image and no source-level debugger, so a bit
    /// set there that nothing implements would turn a clean refusal into a request accepted and then
    /// unanswerable.
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
    /// The target serves the interpreter a MONOTONIC CLOCK that was checked to be MOVING at boot,
    /// so a program on it can measure elapsed time -- `DateTime`, `Environment.TickCount`,
    /// `Thread.Sleep` and every timeout built on them.
    ///
    /// # Why a capability bit and not a silence
    ///
    /// This is the one seam whose failure has no error to report. A clock is a plain
    /// `fn() -> u64`: a source that never advances returns a perfectly well-formed answer, and every
    /// caller believes it. A self-timing benchmark on such a board reported **0 ms** for thousands
    /// of iterations of real work while its checksum gate PASSED -- the computation right, the
    /// duration nonsense, nothing anywhere in error. **A capability that is present but dead is
    /// worse than one that is absent, because absent can be reported.** This bit is the reporting.
    ///
    /// It says nothing about ACCURACY. The scale is the board's own core-clock figure, so a
    /// firmware on an untrimmed RC oscillator sets this bit and still delivers that oscillator's
    /// tolerance. The promise is that time PASSES, which is the property a frozen counter breaks.
    pub const MONOTONIC_CLOCK: u32 = 1 << 15;

    /// Every named bit, with a short human label -- so a host tool can PRINT a capability set
    /// instead of a hexadecimal number.
    ///
    /// It lives here rather than in each tool because a table of names kept next to its consumers
    /// is a second spelling of this list, and a second spelling goes stale silently: the bit that
    /// gets forgotten is always the newest one, which is the one someone is trying to see.
    ///
    /// **This list being INCOMPLETE is a survivable state and is handled, not asserted away.**
    /// [`Capabilities::describe`] reports a set bit with no entry here as `unknown bit N` rather than
    /// dropping it, so a capability added without a label is VISIBLE in the output instead of absent
    /// from it. A name that is missing should cost a reader one puzzled moment, never a wrong answer.
    pub const NAMED: &'static [(u32, &'static str)] = &[
        (Self::DEBUG_BASIC, "DEBUG_BASIC"),
        (Self::BREAKPOINTS, "BREAKPOINTS"),
        (Self::STEPPING, "STEPPING"),
        (Self::MEM_WRITE, "MEM_WRITE"),
        (Self::LOCALS, "LOCALS"),
        (Self::REPL_RUN, "REPL_RUN"),
        (Self::REPL_SOURCE, "REPL_SOURCE"),
        (Self::AOT_ATTACH, "AOT_ATTACH"),
        (Self::BAKED_IMAGE, "BAKED_IMAGE"),
        (Self::DEBUG_ATTACH, "DEBUG_ATTACH"),
        (Self::PROFILE_CHIPID, "PROFILE_CHIPID"),
        (Self::TELEMETRY, "TELEMETRY"),
        (Self::BUNDLE, "BUNDLE"),
        (Self::RESIDENT_CORLIB, "RESIDENT_CORLIB"),
        (Self::LIVE_MEMORY, "LIVE_MEMORY"),
        (Self::MONOTONIC_CLOCK, "MONOTONIC_CLOCK"),
    ];

    /// Whether this set includes `flag`.
    #[must_use]
    pub fn has(self, flag: u32) -> bool {
        self.0 & flag == flag
    }

    /// The set as labels, in bit order, with any bit [`NAMED`](Self::NAMED) does not cover reported
    /// as `unknown bit N`. An empty set renders as `none`.
    #[must_use]
    pub fn describe(self) -> alloc::string::String {
        use alloc::string::ToString;
        let mut parts: Vec<alloc::string::String> = Vec::new();
        let mut unnamed = self.0;
        for (bit, name) in Self::NAMED {
            if self.0 & bit != 0 {
                parts.push((*name).to_string());
                unnamed &= !bit;
            }
        }
        for index in 0..u32::BITS {
            if unnamed & (1 << index) != 0 {
                parts.push(alloc::format!("unknown bit {index}"));
            }
        }
        if parts.is_empty() {
            return "none".to_string();
        }
        parts.join(" | ")
    }

    /// The capabilities present in BOTH sets (what a session can use).
    ///
    /// **This answers "what may we DO", never "what is the target".** Some bits describe the
    /// target alone -- whether its clock moves, whether it holds a resident corlib, what chip it
    /// says it is -- and intersecting one of those with the host's own offer can only subtract a
    /// true fact. Read [`Negotiated::target_caps`] for those; the loss is silent otherwise, because
    /// it happens before any caller sees the value.
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

    /// Raspberry Pi Pico W / Pico WH (RP2040 + CYW43439 wireless). The same RP2040 image as the
    /// plain Pico -- the radio takes no part in booting -- but a distinct board: GP23, GP24, GP25
    /// and GP29 carry the radio here, and the user LED moves off the RP2040 entirely onto the
    /// CYW43439's own WL_GPIO0.
    pub const PICO_W: u16 = 22;

    /// MuseLab nanoCH32V003 v1.0 (WCH CH32V003F4U6, QingKe V2A, RV32EC) -- the first RISC-V board
    /// here that is not RV32I, and the smallest part: 16 KB of flash and 2 KB of SRAM, reached by
    /// AOT-compiled images flashed through the single-wire debug pin rather than by a Link wire.
    pub const NANO_CH32V003: u16 = 23;

    /// SAM4S Xplained (ATSAM4S16C, LQFP100) -- the plain Xplained, NOT the Xplained Pro this
    /// family's other board is. A different part on the same family (1 MB flash and 128 KB SRAM
    /// against the Pro's 2 MB and 160 KB), a different on-board bridge, and an external SRAM on
    /// the static memory bus that the Pro does not carry.
    pub const SAM4S_XPLAINED: u16 = 24;

    /// SAM4N Xplained Pro (ATSAM4N16C, QFP100) -- this vendor's SAM4 entry point: 1 MB of flash
    /// like the SAM4S parts and 80 KB of SRAM against their 128 and 160, and no USB device port
    /// at all, which is what makes it a separate family rather than another part row.
    pub const SAM4N_XPLAINED_PRO: u16 = 25;

    /// SAM4E Xplained Pro (ATSAM4E16E, LQFP144) -- this vendor's connectivity SAM4: an Ethernet
    /// MAC and two CAN controllers its SAM4S and SAM4N siblings do not carry, and a core with a
    /// floating point unit where theirs have none.
    pub const SAM4E_XPLAINED_PRO: u16 = 26;

    /// SAM4L8 Xplained Pro (ATSAM4LC8C, TQFP100) -- this vendor's low-power SAM4 and the one that
    /// shares least with the rest of the line: 512 KB of flash and 64 KB of SRAM, no PMC at all,
    /// and ONE GPIO controller whose ports are register banks rather than separate peripherals.
    pub const SAM4L8_XPLAINED_PRO: u16 = 27;

    /// NUCLEO-F429ZI (STM32F429ZI on ST's MB1137 Nucleo-144) -- the SAME PART as the F429
    /// Discovery board on a different carrier, which is what makes the pair a controlled test of
    /// the chip/board split: everything that differs between them is board truth by construction.
    pub const NUCLEO_F429ZI: u16 = 28;

    /// NUCLEO-F439ZI (STM32F439ZI on ST's MB1137 Nucleo-144) -- the SAME CARRIER as the
    /// NUCLEO-F429ZI with a different part on it, which is the other half of that pair's test:
    /// one varies the board and holds the part, this one varies the part and holds the board.
    /// The two parts answer the same device id and the same JTAG id code, so this number is the
    /// only thing that tells the two boards apart.
    pub const NUCLEO_F439ZI: u16 = 29;

    /// 32F769IDISCOVERY (STM32F769NI, Cortex-M7) -- the first `stm32f769`-family board, and this
    /// vendor's counter-example on LED polarity: its user LEDs SINK where every other ST board
    /// here sources, while its user button reads high when pressed like theirs do.
    pub const STM32F769I_DISCO: u16 = 30;

    /// STM32072B-EVAL (STM32F072VB, Cortex-M0) -- the first EVALUATION board here rather than a
    /// Discovery or a Nucleo, and the first with no path from the target's serial to the host
    /// through the board at all: its debugger is an ST-LINK/V2 rather than a V2-1, so the console
    /// leaves over RS-232 to a D connector.
    pub const STM32072B_EVAL: u16 = 31;

    /// NUCLEO-H755ZI-Q (STM32H755ZI on ST's MB1363 Nucleo-144) -- the first board here whose part
    /// carries TWO processors, a Cortex-M7 beside a Cortex-M4. This number stays one per BOARD and
    /// does not split with them: the board is one physical thing whichever core is executing, so
    /// which core an image was built for is a property of the image rather than of the carrier.
    pub const NUCLEO_H755ZI_Q: u16 = 32;

    /// Arduino GIGA R1 WiFi (STM32H747XIH6 on Arduino's ABX00063) -- the same silicon as the
    /// NUCLEO-H755ZI-Q in a 240-ball package, and the pair's value is that their conventions are
    /// opposite: this board's three user LEDs SINK where that one's source. It is also the first
    /// ST-part board here with no on-board debugger and no serial bridge at all, so its console
    /// path is the target's own USB device controller rather than a probe's virtual COM port.
    pub const ARDUINO_GIGA_R1_WIFI: u16 = 33;

    /// Arduino Portenta H7 (STM32H747XIH6 on Arduino's ABX00042) -- the same silicon as the
    /// NUCLEO-H755ZI-Q and the GIGA, on a module whose debug signals leave through its
    /// high-density connectors rather than any header of its own. Its datasheet covers three
    /// products, and nothing a debug port can read separates them: the part is identical and the
    /// difference is which components are populated. This number is the only discriminator, and it
    /// has to be told rather than discovered.
    pub const ARDUINO_PORTENTA_H7: u16 = 35;

    /// Arduino UNO R4 Minima (Renesas R7FA4M1AB3CFM on Arduino's ABX00080) -- a vendor no other
    /// board here uses, and the board whose LEDs disagree with each other: its built-in LED
    /// SOURCES while the two serial-activity LEDs beside it SINK, so one board carries both
    /// conventions. Its debug path is its own SWD connector with no probe fitted, which is a
    /// per-board reading rather than a property shared with the WiFi variant of the same product.
    pub const ARDUINO_UNO_R4_MINIMA: u16 = 34;

    /// Arduino UNO Q (STM32U585AII6TR on Arduino's ABX00162/ABX00173) -- a microcontroller sharing
    /// one board with a Qualcomm QRB2210 application processor running Linux. THE ONLY BOARD
    /// HERE THAT CAN ANNOUNCE ITSELF OVER NO WIRE THIS PROTOCOL OWNS: its own USB does not reach
    /// the connector, its LPUART1 lands on the application processor rather than on a bridge, and
    /// it has no debug connector at all -- the application processor IS its debugger. So this
    /// number identifies a board that a host reaches only THROUGH the Linux side, and its board
    /// file states no carrier for exactly that reason.
    pub const ARDUINO_UNO_Q: u16 = 36;

    /// Microchip SAM D11 Xplained Pro (ATSAMD11D14AM, Cortex-M0+, 16 KB flash / 4 KB SRAM). Its kit
    /// prints
    /// only "ATSAMD11D14A", which names four ordering codes across three packages; the board is
    /// the 24-pin QFN, identified by the four pads its headers name that no other package carries
    /// and confirmed by a DSU read returning DID `0x10030100`.
    pub const ATSAMD11_XPLAINED_PRO: u16 = 37;

    /// Microchip ATSAMD10 Xplained Mini (ATSAMD10D14AM, Cortex-M0+, 16 KB flash / 4 KB SRAM) --
    /// the same core and memory as the SAM D11 Xplained Pro on a part with no USB at all, and the
    /// first board here whose flash size could not be read off any document it ships with: its
    /// package comes in 16 KB and 8 KB variants with identical pads, and only DID `0x10020100`
    /// separates them. An mEDBG rather than an EDBG, so a different USB product id.
    pub const ATSAMD10_XPLAINED_MINI: u16 = 38;

    /// ST NUCLEO-L011K4 (STM32L011K4T6, Cortex-M0+, 16 KB flash / 2 KB SRAM) -- the smallest RAM
    /// of any board here. Its virtual COM port does NOT ride the PA2/PA3 pair every other Nucleo
    /// uses: the receive line is PA15, and on this part USART2 is selected at AF4 where the L476
    /// selects it at AF7, so neither the pin nor the function number carries across from a sibling.
    pub const NUCLEO_L011K4: u16 = 39;

    /// ST NUCLEO-U5A5ZJ-Q (STM32U5A5ZJT6Q, Cortex-M33, 4 MB flash / 2496 KB contiguous SRAM) --
    /// the largest RAM of any board here, and the MB1549 reference board it shares with the
    /// NUCLEO-U575ZI-Q carries two different parts, so the board id and not the board shape is
    /// what selects the chip row. Its strata are `csp/stm32u5a5`, NOT `csp/stm32u585`: the U5's
    /// reference manual puts the two in different product columns and gives this one a GPIOJ the
    /// U585 does not have.
    pub const NUCLEO_U5A5ZJ_Q: u16 = 40;

    /// ST NUCLEO-L053R8 (STM32L053R8T6, Cortex-M0+, 64 KB flash / 8 KB SRAM). Its strata are
    /// `csp/stm32l053`, NOT `csp/stm32l0`: that family is the L0x1 line at category 1, and this
    /// part is L0x3 at category 3 -- a different reference manual, a different datasheet, three
    /// more GPIO ports and a USART1 the other line does not have.
    pub const NUCLEO_L053R8: u16 = 41;

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
            PICO_W => "Raspberry Pi Pico W",
            NANO_CH32V003 => "MuseLab nanoCH32V003",
            SAM4S_XPLAINED => "SAM4S Xplained",
            SAM4N_XPLAINED_PRO => "SAM4N Xplained Pro",
            SAM4E_XPLAINED_PRO => "SAM4E Xplained Pro",
            SAM4L8_XPLAINED_PRO => "SAM4L8 Xplained Pro",
            NUCLEO_F429ZI => "NUCLEO-F429ZI",
            NUCLEO_F439ZI => "NUCLEO-F439ZI",
            STM32F769I_DISCO => "STM32F769I Discovery",
            STM32072B_EVAL => "STM32072B-EVAL",
            NUCLEO_H755ZI_Q => "NUCLEO-H755ZI-Q",
            ARDUINO_GIGA_R1_WIFI => "Arduino GIGA R1 WiFi",
            ARDUINO_UNO_R4_MINIMA => "Arduino UNO R4 Minima",
            ARDUINO_PORTENTA_H7 => "Arduino Portenta H7",
            ARDUINO_UNO_Q => "Arduino UNO Q",
            ATSAMD11_XPLAINED_PRO => "SAM D11 Xplained Pro",
            ATSAMD10_XPLAINED_MINI => "ATSAMD10 Xplained Mini",
            NUCLEO_L011K4 => "NUCLEO-L011K4",
            NUCLEO_U5A5ZJ_Q => "NUCLEO-U5A5ZJ-Q",
            NUCLEO_L053R8 => "NUCLEO-L053R8",
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
    /// The capabilities both sides offer -- what this SESSION can use. Ask this before using a
    /// feature, because using one needs code at both ends.
    pub caps: Capabilities,
    /// What the TARGET advertised about itself, before any intersection -- what this BOARD is.
    ///
    /// Two different questions were being answered by one field, and only one of them survives an
    /// intersection. "Will a deploy work?" is genuinely two-sided: the host must implement it too,
    /// so masking it against the host's own set is correct. "Does this board's clock move?" is a
    /// property of the target alone, and intersecting it with the host's offer can only ever
    /// subtract -- a host that does not claim [`Capabilities::MONOTONIC_CLOCK`] for itself reports
    /// every board as having no clock, whatever the board said.
    ///
    /// That is invisible by construction rather than by oversight, which is why it needs a separate
    /// field rather than a rule about which bits to be careful with: the masking happens before any
    /// caller can look, so no amount of care at the call site can recover the answer. A tool asking
    /// what a board IS reads this; a tool asking what it may DO reads [`Self::caps`].
    pub target_caps: Capabilities,
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
/// INTERSECTION (only what both sides offer) + the target's RAW advertisement + the target's profile
/// identity as advertised.
///
/// Both capability sets are kept because they answer different questions -- see
/// [`Negotiated::target_caps`]. The intersection alone lost every target-only property, and lost it
/// where no caller could notice.
#[must_use]
pub fn host_finish(ack: &HelloAck, host_caps: Capabilities) -> Negotiated {
    Negotiated {
        version: ack.chosen,
        caps: host_caps.intersect(ack.caps),
        target_caps: ack.caps,
        profile: ack.profile,
    }
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

/// Why an exchange on this wire could not be completed -- the carrier failing, or an answer the
/// caller cannot act on.
///
/// It opened life as a carrier error and outgrew that: the variants below divide by the REMEDY, and
/// the last three are cases where the carrier worked perfectly. Reporting them as a carrier fault or
/// (worse) as a timeout sends the reader to look at a cable that is fine.
///
/// `#[non_exhaustive]`: three variants beyond the original two have landed as the protocol's silent
/// failures were found, and the supply is not obviously exhausted. Matching downstream therefore
/// needs a wildcard arm, so the next variant is additive rather than a break at every call site.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum TransportError {
    /// The link is closed / disconnected.
    Closed,
    /// A carrier-level failure (I/O, USB).
    Carrier,
    /// The payload cannot be framed: a frame's `LEN` is a `u16`, so a payload over 65,535 bytes has
    /// no representation on this wire at all.
    ///
    /// It is a distinct answer from [`Carrier`](Self::Carrier) because the remedy is different and
    /// the carrier is fine: the sender must CHUNK (the deploy and bundle ops already do) or send
    /// less. Nothing was transmitted.
    PayloadTooLarge,
    /// The target REFUSED the request: it answered [`msg::ERROR`] rather than the expected reply.
    ///
    /// `reason` is the payload's reason byte (see the [`error`] module) and `msg_type` the message
    /// type it names, or `0` when the refusal named none -- no message type is `0`, so the sentinel
    /// cannot be mistaken for one.
    ///
    /// Distinct from [`Closed`](Self::Closed) for the [`PayloadTooLarge`](Self::PayloadTooLarge)
    /// reason: the carrier is fine and the target answered promptly. The remedy is to STOP asking --
    /// this target does not implement the op -- where a closed link says reconnect. A caller that
    /// polls a refusal to its deadline reports a timeout, which is the one reading that sends the
    /// reader to the cable.
    Refused {
        /// The refusal's reason byte.
        reason: u8,
        /// The message type refused, or `0` if the refusal named none.
        msg_type: u8,
    },
    /// A reply arrived at the expected sequence and did NOT decode.
    ///
    /// The answer is IN HAND and unusable, which is not the same as "not in yet": a caller told
    /// `None` polls on and reports a timeout for a target that already replied. `msg_type` is the
    /// type that failed to decode, so the reader knows which decoder disagreed with the sender.
    MalformedReply {
        /// The message type whose payload would not decode.
        msg_type: u8,
    },
}

/// The carrier seam, at the FRAME level: a byte carrier (USB-CDC / UART) implements it over the
/// [`encode_frame`] / [`FrameReader`] framing; a packet carrier (HID / WinUSB) wraps frames into its
/// reports / bulk transfers. Non-blocking: [`Transport::poll`] returns `None` when no frame is ready.
pub trait Transport {
    /// Send one logical frame.
    fn send(&mut self, msg_type: u8, seq: u16, payload: &[u8]) -> Result<(), TransportError>;
    /// Return the next received frame, or `None` if none is ready yet.
    fn poll(&mut self) -> Result<Option<Frame>, TransportError>;

    /// Send one logical frame ONLY if the carrier can take it now.
    ///
    /// `Ok(true)` -- committed, exactly as [`Transport::send`]. `Ok(false)` -- the carrier would
    /// have had to wait, and **nothing was written**, so the caller may drop the frame or try later
    /// with the stream undisturbed. `Err` is a real failure, as ever.
    ///
    /// # Which frames belong here
    ///
    /// **A reply is worth waiting for; an unsolicited frame is not.** The peer that just sent a
    /// request is demonstrably there, and blocking is what delivers the answer it is waiting on. But
    /// a target telling a carrier that its session has been revoked, or asking one whether it is
    /// still alive, is speaking to a host that may have walked away -- and on a carrier whose
    /// transmit path is flow-controlled by the host reading it, that costs a full blocking write
    /// while everything else the loop serves waits behind it.
    ///
    /// Measured on a SAMW25 with two live carriers: a liveness probe sent to a native-USB carrier
    /// whose host had closed its handle -- the device still `Configured`, nothing draining the bulk
    /// IN endpoint -- stalled the serve loop for about three and a half seconds, which was longer
    /// than the claimant waiting on that very probe was prepared to wait. **The answer the probe
    /// wanted was in the send all along: a carrier that will not take a packet is a carrier nobody
    /// is reading.**
    ///
    /// # The default is the honest answer for most carriers
    ///
    /// It forwards to [`Transport::send`] and reports `true`, so a carrier that cannot block -- a
    /// UART that writes into a register, an in-memory pipe, a queue with its own cap -- needs no
    /// implementation and gives up nothing. Only a carrier with host-driven flow control has a
    /// reason to override it.
    ///
    /// An implementor must keep the all-or-nothing promise: `Ok(false)` means the wire was not
    /// touched. A half-written frame is worse than an unsent one, because the far side reassembles
    /// by length and pays a resynchronization for the difference.
    fn try_send(&mut self, msg_type: u8, seq: u16, payload: &[u8]) -> Result<bool, TransportError> {
        self.send(msg_type, seq, payload).map(|()| true)
    }
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
        self.sent
            .extend_from_slice(&encode_frame(msg_type, seq, payload).ok_or(TransportError::PayloadTooLarge)?);
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

    /// A capability set renders as names, and -- the half that matters -- a bit nobody named is
    /// REPORTED rather than dropped.
    ///
    /// The dropping version is the failure this guards: a host tool that silently omits an unnamed
    /// bit shows a SHORTER capability list than the board actually sent, and the bit most likely to
    /// be missing from the table is the newest one, which is the one someone is looking for.
    #[test]
    fn a_capability_set_names_what_it_knows_and_reports_what_it_does_not() {
        assert_eq!(Capabilities(0).describe(), "none");
        assert_eq!(Capabilities(Capabilities::BAKED_IMAGE).describe(), "BAKED_IMAGE");
        assert_eq!(
            Capabilities(Capabilities::MONOTONIC_CLOCK | Capabilities::BAKED_IMAGE).describe(),
            "BAKED_IMAGE | MONOTONIC_CLOCK"
        );
        assert_eq!(
            Capabilities(Capabilities::BAKED_IMAGE | (1 << 31)).describe(),
            "BAKED_IMAGE | unknown bit 31"
        );
        assert_eq!(Capabilities(1 << 20).describe(), "unknown bit 20");
    }

    /// The table itself: one bit per entry, no duplicates, no zero rows. A duplicated bit would print
    /// one capability under two names; a multi-bit entry would claim a set it does not have.
    #[test]
    fn the_capability_name_table_is_one_distinct_bit_per_row() {
        let mut seen = 0u32;
        for (bit, name) in Capabilities::NAMED {
            assert_eq!(bit.count_ones(), 1, "{name} must be exactly one bit");
            assert_eq!(seen & bit, 0, "{name} duplicates a bit already named");
            seen |= bit;
        }
        assert_eq!(
            seen.count_ones(),
            32 - seen.leading_zeros(),
            "the named bits should be contiguous from bit 0; a gap means a constant went unnamed"
        );
    }

    #[test]
    fn frame_round_trips() {
        let bytes = encode_frame(msg::HELLO, 7, &[1, 2, 3, 4]).expect("a 4-byte payload frames");
        let mut reader = FrameReader::new();
        reader.push(&bytes);
        let frame = reader.next_frame().expect("a complete frame");
        assert_eq!(frame.msg_type, msg::HELLO);
        assert_eq!(frame.seq, 7);
        assert_eq!(frame.payload, vec![1, 2, 3, 4]);
        assert!(reader.next_frame().is_none());
    }

    /// The boundary, from both sides, because only one of the two is easy to get right.
    ///
    /// A payload of exactly [`MAX_PAYLOAD`] is the largest this wire can carry and must still frame
    /// and survive a round trip; one byte more has no representation and must be REFUSED. The old
    /// code clamped instead, producing a frame whose CRC validated over truncated content -- so a
    /// test that only checked "a big payload still returns bytes" passed on the defect.
    #[test]
    fn the_largest_payload_frames_and_one_byte_more_is_refused() {
        let largest = alloc::vec![0xA5u8; MAX_PAYLOAD];
        let bytes = encode_frame(msg::PING, 3, &largest).expect("MAX_PAYLOAD is carryable");
        let mut reader = FrameReader::new();
        reader.push(&bytes);
        let frame = reader.next_frame().expect("the largest frame round-trips");
        assert_eq!(frame.payload.len(), MAX_PAYLOAD, "the payload arrived whole");
        assert_eq!(frame.payload, largest, "and unaltered");

        let too_big = alloc::vec![0xA5u8; MAX_PAYLOAD + 1];
        assert_eq!(
            encode_frame(msg::PING, 4, &too_big),
            None,
            "a payload one byte over the length field must be refused, not truncated into a frame \
             whose CRC then certifies the missing content as intact"
        );
    }

    #[test]
    fn reader_reassembles_across_chunks() {
        let bytes = encode_frame(0x42, 0xBEEF, &[9, 9, 9]).expect("a 3-byte payload frames");
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
        let good = encode_frame(msg::PING, 1, &[0xAB]).expect("a 1-byte payload frames");
        let mut corrupt = encode_frame(msg::PING, 2, &[0xCD]).expect("a 1-byte payload frames");
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

        assert!(session.target_caps.has(Capabilities::BREAKPOINTS));
        assert!(session.target_caps.has(Capabilities::DEBUG_BASIC));
        assert!(!session.target_caps.has(Capabilities::REPL_RUN));
    }

    #[test]
    fn incompatible_versions_nak() {
        let host = Hello { range: ProtocolRange { min: 5, max: 6 }, caps: Capabilities::default() };
        let err = target_respond(&host, ProtocolRange::single(1), Capabilities::default());
        assert_eq!(err, Err(Nak { target_range: ProtocolRange::single(1) }));
    }
}
