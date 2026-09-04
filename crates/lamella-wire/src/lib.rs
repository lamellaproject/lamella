//! The Lamella Link debug + REPL protocol -- the carrier-agnostic core shared by the host
//! front-ends (the DAP adapter + the gdb/lldb-style CLI) and the on-device runner.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::vec::Vec;

/// The current protocol version this build implements. A peer advertises a [`ProtocolRange`] around it.
pub const PROTOCOL_VERSION: u16 = 1;

pub mod usb;

pub mod session;

pub mod pairing;

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

/// The payload every Lamella Link target must be able to absorb in ONE frame, whether or not it
/// advertises a larger one through [`HelloAck::max_inbound_payload`].
///
/// # Why the protocol states a floor rather than leaving it to the sender
///
/// A byte-stream target drains its carrier into a fixed ring from an interrupt, and a full ring
/// DROPS. A frame larger than that ring cannot be assembled by a target that is busy while it
/// arrives, however patient either end is: the reader waits for bytes that were discarded while it
/// was being told about them. Nothing times out that a longer timeout would fix, and the host reads
/// the result as a target that stopped answering -- which sends somebody to look at the cable.
///
/// Without a floor a sender has two options and both are wrong. It can send what the wire allows
/// and hang on the smallest board, or it can guess the smallest ring in the tree -- a number with
/// no owner, no reason attached, and no way to be right about a board added later. A floor makes
/// the small case a REQUIREMENT a target has to meet rather than a limit each sender has to
/// rediscover, and it is what makes [`HelloAck::max_inbound_payload`] safe to leave unset.
///
/// 240 bytes, which is 249 on the wire and so fits the smallest receive ring any serve firmware in
/// this tree uses, with room left over. It is deliberately round rather than the exact 247 that
/// ring allows: this is a number that goes into a conformance sentence somebody has to implement
/// against, not one derived at a call site.
pub const MIN_INBOUND_PAYLOAD: usize = 240;

/// The largest payload a receiver holding a `buffer`-byte assembly buffer can take, accounting for
/// the framing the payload arrives wrapped in.
///
/// # Why the arithmetic is here rather than at the caller
///
/// The framing overhead is not public and should not be. A firmware that subtracted its own idea of
/// the header size would be silently wrong the day the header changes -- and silently wrong here
/// means OVER-advertising, which is the failure this whole mechanism exists to prevent. So a target
/// states the number it actually knows, which is how big its ring is, and the layer that owns the
/// header does the subtraction.
///
/// Saturating rather than underflowing: a buffer too small to hold any frame carries no payload,
/// which is a true answer, and a panic in a `const` initializer on a firmware is not a diagnosis
/// anybody gets to read.
#[must_use]
pub const fn max_payload_for_buffer(buffer: usize) -> usize {
    buffer.saturating_sub(HEADER_LEN + CRC_LEN)
}

pub mod msg;

pub mod arch;

pub mod surface;

/// What an [`crate::msg::ERROR`] carries: why a frame was refused.
///
/// # Why a refusal is a message rather than a silence
///
/// [`crate::msg::ERROR`] is the answer to a message type a target does not implement. Without one, such a
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
/// # A refusal is NOT a [`HelloNak`]
///
/// [`msg::HELLO_NAK`] answers a [`Hello`] whose version range does not overlap: the session cannot begin at
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
/// wire, and there is nothing this function can return that would be one -- or when `msg_type` is
/// one of the two bytes that are permanently not message types ([`msg::is_valid_type`]).
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
/// module-firmware ops) chunk and are fine, but a target building one reply out of a deployed
/// program's console output does not chunk -- a chatty program on a roomy part is one path to a
/// payload this cannot carry.
#[must_use]
pub fn encode_frame(msg_type: u8, seq: u16, payload: &[u8]) -> Option<Vec<u8>> {
    if payload.len() > MAX_PAYLOAD || !msg::is_valid_type(msg_type) {
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
pub struct FrameReader {
    buf: Vec<u8>,
    /// The largest payload a header may claim before it is treated as garbage. See
    /// [`FrameReader::with_max_payload`].
    max_payload: usize,
}

impl Default for FrameReader {
    /// A reader that will wait for any length the protocol allows.
    fn default() -> Self {
        Self::new()
    }
}

impl FrameReader {
    /// A new, empty reader that will wait for any length the protocol allows.
    #[must_use]
    pub fn new() -> Self {
        Self { buf: Vec::new(), max_payload: MAX_PAYLOAD }
    }

    /// A reader that treats a header claiming more than `max_payload` bytes as garbage and
    /// resynchronizes at once, instead of waiting for a frame that size to arrive.
    ///
    /// # Why a bounded reader exists
    ///
    /// The length is two bytes read out of the stream, and the CRC that would reject them cannot be
    /// checked until the whole declared frame has arrived. So **one corrupted length byte makes an
    /// unbounded reader wait for up to 65,535 bytes**, swallowing every real frame that follows into
    /// the same buffer until the count is satisfied. At 115200 baud that is about six seconds of a
    /// link that looks dead; on anything slower it is minutes.
    ///
    /// It is worse than a stall, because the cost compounds: [`FrameReader::push`] sizes its reserve
    /// through a scan of the buffer, so every byte appended to a stalled reader costs more than the
    /// last. On a polled UART that feedback is enough to turn keeping up into overrunning -- **the
    /// stall causes the corruption that keeps it stalled** -- and the loop breaks only when
    /// something outside the reader resets it.
    ///
    /// A target usually knows what it can be sent. A firmware whose largest inbound frame is a
    /// handshake turns six seconds of swallowed stream into one discarded byte by saying so.
    /// **[`FrameReader::new`] is unchanged**: a host, or a target that really does receive deployed
    /// images, keeps the full range by saying nothing.
    ///
    /// A `max_payload` at or above [`MAX_PAYLOAD`] is simply no bound, and is not an error: the
    /// length field is a `u16`, so no header can claim more than that however large a number this
    /// is given.
    #[must_use]
    pub fn with_max_payload(max_payload: usize) -> Self {
        Self { buf: Vec::new(), max_payload }
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

    /// Whether a header's declared length is one this reader could ever complete.
    ///
    /// ONE definition, because two places act on it -- what to reserve toward, and what to discard --
    /// and a rule with two implementations gains its next case in one of them. They must agree
    /// exactly: a reader that reserved toward a length it then discarded would pay the cost of the
    /// frame it refused.
    fn believable_length(&self, len: usize) -> bool {
        len <= self.max_payload
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
        if !msg::is_valid_type(self.buf[sync + 4]) {
            return 0;
        }
        let len = u16::from_le_bytes([self.buf[sync + 2], self.buf[sync + 3]]) as usize;
        if !self.believable_length(len) {
            return 0;
        }
        sync + HEADER_LEN + len + CRC_LEN
    }

    /// Pull the next complete, CRC-valid frame, or `None` if more bytes are needed. Leading garbage and
    /// a CRC-failed frame are discarded (resync on the next SYNC).
    ///
    /// A header whose TYPE byte is one of the two that can never be a message type
    /// ([`msg::is_valid_type`]) is discarded as soon as the header is readable, rather than after
    /// waiting for the payload it claims. That matters because both of those bytes are what unwritten
    /// memory reads as: a run of erased flash or zeroed RAM arriving on a carrier can otherwise
    /// declare a long payload and hold the reader waiting for bytes nothing will send.
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
            if !msg::is_valid_type(self.buf[4]) {
                self.buf.drain(0..1);
                continue;
            }
            let len = u16::from_le_bytes([self.buf[2], self.buf[3]]) as usize;
            if !self.believable_length(len) {
                self.buf.drain(0..1);
                continue;
            }
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
///
/// # Why the word is sixty-four bits wide, and grouped
///
/// The asymmetry decides the width: a word that FILLS is a protocol version bump on a settled
/// protocol, and a word that never fills costs four extra bytes once per session. The extra masking
/// on a narrow part is a handshake cost, not a hot-loop one.
///
/// A bit sits in the family of the MESSAGE BLOCK it gates, with room left in each family. That is
/// not tidiness: it makes a family a MASK, so "any debug capability at all" or "every artifact kind"
/// is one test rather than a list that has to be kept in step with this one. Bits scattered by the
/// order features happened to land in cannot be masked, and the grouping immediately moves one bit
/// out of the family its old name implied and into the one whose op it actually gates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Capabilities(pub u64);

impl Capabilities {

    /// The identity in the `HELLO_ACK` carries the STRUCTURED product and chip fields, so a host can
    /// identify the hardware without parsing a display name. The fields may still read `0` =
    /// unknown: a custom board reports no model, and a firmware may not know its own chip ids.
    pub const PROFILE_CHIPID: u64 = 1 << 0;
    /// The target serves the interpreter a MONOTONIC CLOCK that was checked to be MOVING at boot, so
    /// a program on it can measure elapsed time -- dates, tick counts, sleeps, and every timeout
    /// built on them.
    ///
    /// # Why a capability bit and not a silence
    ///
    /// This is the one seam whose failure has no error to report. A clock is a plain function
    /// returning a number: a source that never advances returns a perfectly well-formed answer, and
    /// every caller believes it. A self-timing benchmark on such a board reports zero milliseconds
    /// for thousands of iterations of real work while its own checksum check passes -- the
    /// computation right, the duration nonsense, nothing anywhere in error. A capability that is
    /// present but dead is worse than one that is absent, because absent can be reported. This bit
    /// is the reporting.
    ///
    /// It says nothing about ACCURACY. The scale is the board's own core-clock figure, so a firmware
    /// on an untrimmed oscillator sets this bit and still delivers that oscillator's tolerance. The
    /// promise is that time PASSES, which is the property a frozen counter breaks.
    pub const MONOTONIC_CLOCK: u64 = 1 << 1;


    /// Halt, resume and read memory.
    pub const DEBUG_BASIC: u64 = 1 << 8;
    /// Set and clear breakpoints.
    pub const BREAKPOINTS: u64 = 1 << 9;
    /// Step in, over and out.
    pub const STEPPING: u64 = 1 << 10;
    /// Inspect managed locals and frames.
    pub const LOCALS: u64 = 1 << 11;
    /// Write target memory.
    pub const MEM_WRITE: u64 = 1 << 12;
    /// Attach to a program that is ALREADY RUNNING on an interpreted tier, and leave it running.
    ///
    /// Two attach bits rather than one, because they are two AGENTS and not two modes of one. An
    /// interpreted agent drives the interpreter's own session -- its offsets, its frames, its
    /// locals. A native agent drives the core: hardware comparators, a native unwinder, a map from
    /// machine addresses back to the code a person wrote. A firmware can carry either without the
    /// other and most carry exactly one; with a single bit, a board that can debug its own
    /// interpreter would have to claim it can debug native code or deny it can debug anything.
    pub const ATTACH_INTERPRETED: u64 = 1 << 13;
    /// Attach to a program that is ALREADY RUNNING as native machine code, and leave it running.
    ///
    /// Sharper on this side than on the interpreted one: on a board with no debug port, attaching to
    /// a running native program is not one way in, it is the way in.
    pub const ATTACH_NATIVE: u64 = 1 << 14;


    /// Load and run an assembly. The first of the four ARTIFACT-KIND bits, which are contiguous so
    /// [`Capabilities::ARTIFACT_KINDS`] can ask about all of them at once.
    pub const REPL_RUN: u64 = 1 << 24;
    /// Load and run a host-BAKED image -- the path for a target that cannot parse an assembly, where
    /// the host bakes each submission and ships the image.
    pub const BAKED_IMAGE: u64 = 1 << 25;
    /// Load and run a Python bundle.
    ///
    /// A host should gate on this in preference to sending the op and reading the answer. Both work
    /// -- an unimplemented type is refused by name rather than dropped -- but this bit arrives in a
    /// handshake the session already exchanges, so gating on it costs no round trip and it says what
    /// a target CAN do rather than one thing at a time.
    pub const BUNDLE: u64 = 1 << 26;
    /// Load and run ECMAScript bytecode.
    pub const JS: u64 = 1 << 27;
    /// Parse and interpret SOURCE on the device, rather than only artifacts a host compiled.
    pub const REPL_SOURCE: u64 = 1 << 28;
    /// The target holds a RESIDENT class library in flash, so it accepts a bare program assembly and
    /// resolves that program's library references out of the resident copy -- a program reaches the
    /// board as its own kilobytes instead of as an image carrying a library with it.
    ///
    /// # Presence is only half the question
    ///
    /// A host must also know it is the library the program was compiled against, and getting that
    /// wrong is SILENT: a library declaring a seam the firmware compiled out still loads, and the
    /// method keeps a placeholder body that returns zero. So the identity carries a content hash of
    /// the resident surface, and a host that recorded that hash for a firmware it trusts can compare
    /// and be sure. This bit says the path exists; the hash says it is the right one.
    pub const RESIDENT_CORLIB: u64 = 1 << 29;
    /// Start the PERSISTENTLY DEPLOYED artifact halted, in place -- a debug session over an artifact
    /// already in flash, with nothing sent over the wire to begin it.
    ///
    /// The name states what it does. It was once named for attaching, which it never did: it BOOTS
    /// the deployed artifact, so a program that had been running for hours was replaced by a fresh
    /// copy of itself at its entry point. Attaching to something already running is
    /// [`Capabilities::ATTACH_INTERPRETED`] and [`Capabilities::ATTACH_NATIVE`], which are different bits because they are a
    /// different thing.
    pub const DEBUG_BOOT_DEPLOYED: u64 = 1 << 30;


    /// On-device telemetry: the host subscribes to device signals and the target streams samples
    /// asynchronously. RESERVED -- no firmware advertises this bit and no host may rely on it.
    pub const TELEMETRY: u64 = 1 << 40;
    /// The target answers a memory read and write WHILE a deployed program is still running -- the
    /// on-target half of a host evaluating against a live program rather than a stopped one.
    ///
    /// # Why this is a separate bit from [`Capabilities::DEBUG_BASIC`] and [`Capabilities::MEM_WRITE`]
    ///
    /// Those two describe the same two verbs on the HALTED channel, where the program is stopped at
    /// a known point and its state is at rest. This bit says the verbs are answered with the program
    /// in motion, which is a different promise about a different situation: what a host reads may be
    /// mid-update, and what it writes lands in a program that did not expect it.
    ///
    /// The distinction is not academic on a controller. Halting a live one is an EVENT -- a motor
    /// keeps turning, a valve stays where it was -- so inspecting a running system and poking a
    /// stopped one are separate products, and a host must be able to tell which it is talking to. A
    /// target that only sets [`Capabilities::DEBUG_BASIC`] can still be inspected; it just has to be stopped
    /// first, and the host has to say so.
    pub const LIVE_MEMORY: u64 = 1 << 41;


    /// The target can hand off to the silicon vendor's own bootloader, after which it is no longer
    /// speaking this protocol.
    pub const HW_BOOTLOADER: u64 = 1 << 48;
    /// The target can hand off to an INSTALLED Lamella bootloader on the same transport.
    ///
    /// Answered from what is installed, never from what the firmware was built to support: the
    /// question a host is asking is whether the handoff will land somewhere, and a build-time answer
    /// to that is a guess about a different board's flash.
    pub const SW_BOOTLOADER: u64 = 1 << 49;
    /// The target accepts a firmware image over the wire. Firmware flashing is a compile-in
    /// capability of this protocol rather than a separate product, which is what lets a board with
    /// two firmware slots write the other one with no bootloader present at all.
    pub const FW_UPDATE: u64 = 1 << 50;
    /// The target permits activating an installed image whose version is LOWER than the running one.
    ///
    /// Compile-in, and never settable over the wire. Going back to an image already installed and
    /// already verified is not an install, so the monotonic check that exists to stop an attacker
    /// re-installing a signed old image gates the WRITE and not the activation -- but an attacker
    /// holding such an image plus an activation op gets the same outcome with the write removed,
    /// which is why the permission is built in rather than asked for. Selecting a HIGHER or equal
    /// version is an ordinary update and needs nothing.
    pub const FW_ROLLBACK: u64 = 1 << 51;
    /// The target accepts a deliberately unsigned firmware image. Compile-in, and never settable
    /// over the wire, for the same reason as [`Capabilities::FW_ROLLBACK`].
    pub const UNSIGNED_FW: u64 = 1 << 52;


    /// Everything in the SESSION family: what the handshake carries, and what the board is.
    pub const FAMILY_SESSION: u64 = 0x0000_0000_0000_00FF;
    /// Everything in the DEBUG family. `caps & FAMILY_DEBUG != 0` is "can this board be debugged at
    /// all", which is what a tool asks before offering to try.
    pub const FAMILY_DEBUG: u64 = 0x0000_0000_00FF_FF00;
    /// Everything in the LOAD, DEPLOY and EXEC family.
    pub const FAMILY_ARTIFACT: u64 = 0x0000_00FF_FF00_0000;
    /// Everything in the PROFILE, TELEMETRY and LIVE family.
    pub const FAMILY_OBSERVE: u64 = 0x0000_FF00_0000_0000;
    /// Everything in the DEVICE and FIRMWARE family.
    pub const FAMILY_DEVICE: u64 = 0x00FF_0000_0000_0000;

    /// The four ARTIFACT-KIND bits together: an assembly, a baked image, a Python bundle,
    /// ECMAScript bytecode.
    ///
    /// They are contiguous so this mask exists at all. "Which kinds of artifact does this board
    /// take" is the question a host asks before offering to build one, and answering it from four
    /// scattered bits means four tests that have to be kept in step with a fifth kind arriving.
    pub const ARTIFACT_KINDS: u64 = Self::REPL_RUN | Self::BAKED_IMAGE | Self::BUNDLE | Self::JS;

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
    pub const NAMED: &'static [(u64, &'static str)] = &[
        (Self::PROFILE_CHIPID, "PROFILE_CHIPID"),
        (Self::MONOTONIC_CLOCK, "MONOTONIC_CLOCK"),
        (Self::DEBUG_BASIC, "DEBUG_BASIC"),
        (Self::BREAKPOINTS, "BREAKPOINTS"),
        (Self::STEPPING, "STEPPING"),
        (Self::LOCALS, "LOCALS"),
        (Self::MEM_WRITE, "MEM_WRITE"),
        (Self::ATTACH_INTERPRETED, "ATTACH_INTERPRETED"),
        (Self::ATTACH_NATIVE, "ATTACH_NATIVE"),
        (Self::REPL_RUN, "REPL_RUN"),
        (Self::BAKED_IMAGE, "BAKED_IMAGE"),
        (Self::BUNDLE, "BUNDLE"),
        (Self::JS, "JS"),
        (Self::REPL_SOURCE, "REPL_SOURCE"),
        (Self::RESIDENT_CORLIB, "RESIDENT_CORLIB"),
        (Self::DEBUG_BOOT_DEPLOYED, "DEBUG_BOOT_DEPLOYED"),
        (Self::TELEMETRY, "TELEMETRY"),
        (Self::LIVE_MEMORY, "LIVE_MEMORY"),
        (Self::HW_BOOTLOADER, "HW_BOOTLOADER"),
        (Self::SW_BOOTLOADER, "SW_BOOTLOADER"),
        (Self::FW_UPDATE, "FW_UPDATE"),
        (Self::FW_ROLLBACK, "FW_ROLLBACK"),
        (Self::UNSIGNED_FW, "UNSIGNED_FW"),
    ];

    /// Whether this set includes `flag`.
    #[must_use]
    pub fn has(self, flag: u64) -> bool {
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
        for index in 0..u64::BITS {
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
    /// target alone -- whether its clock moves, whether it holds a resident library, what chip it
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
    /// Encoded size: `min(2) | max(2) | caps(8)`.
    const ENCODED_LEN: usize = 12;

    /// `min(2) | max(2) | caps(8)`, little-endian.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut payload = Vec::with_capacity(Self::ENCODED_LEN);
        payload.extend_from_slice(&self.range.min.to_le_bytes());
        payload.extend_from_slice(&self.range.max.to_le_bytes());
        payload.extend_from_slice(&self.caps.0.to_le_bytes());
        payload
    }

    /// Decode, tolerating a longer payload (a newer peer's trailing fields are skipped).
    #[must_use]
    pub fn decode(payload: &[u8]) -> Option<Self> {
        let caps = payload.get(4..Self::ENCODED_LEN)?;
        Some(Self {
            range: ProtocolRange {
                min: u16::from_le_bytes([payload[0], payload[1]]),
                max: u16::from_le_bytes([payload[2], payload[3]]),
            },
            caps: Capabilities(u64::from_le_bytes(caps.try_into().ok()?)),
        })
    }
}

/// One resident runtime a target carries, and everything a host needs to decide whether a program
/// it compiled will resolve against it.
///
/// One record per resident runtime rather than one per board, because a board can hold more than one
/// -- and can hold two of the SAME kind at different levels, which is why the level and the hash sit
/// here rather than beside the product model.
///
/// # Four fields, four questions, and none of them substitutes for another
///
/// ```text
/// lib_version       which contract was this library built against
/// lib_file_version  which BUILD of that contract, so two boards can be ORDERED
/// hash              is this the exact build my program was compiled against
/// caps              a per-runtime capability claim, reserved
/// ```
///
/// The HASH stays authoritative and the version is never the compatibility test. A library is built
/// per profile with capability symbols on or off, so two builds can share a version and differ in
/// bytes -- and that difference is the one whose consequence is SILENT, because a seam compiled out
/// keeps a placeholder body that answers zero. Only the hash sees it.
///
/// What the versions add is the DIRECTION and the MESSAGE. A hash is a content digest, so a host
/// that finds a mismatch can honestly say only *these differ* -- which tells a person nothing about
/// what to do next. A version orders them, so the sentence becomes *the board is older, update it*
/// or *your toolchain is older, update that*. The three outcomes:
///
/// ```text
/// hash equal                       proceed, silently
/// hash differs, versions differ    orderable -- name which side is behind
/// hash differs, versions equal     same contract level, different build
/// ```
///
/// The third row is the ordinary case, because a version states a COMPATIBILITY LEVEL rather than
/// counting builds: it moves when the contract moves, so it is stable for long stretches by design,
/// and a host reading it as a build counter would call every capability-symbol difference a match.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Surface {
    /// Which runtime this is (see [`msg::tier`]). `0` is not a value: erased flash and zeroed RAM
    /// both present as one.
    pub tier: u8,
    /// That runtime's ABI level, bumped only when an existing seam's semantics change
    /// incompatibly.
    pub abi: u16,
    /// The CONTENT hash of the resident surface -- this runtime's own registry fingerprint, folded
    /// with the resident library's content hash on a target that holds one.
    ///
    /// The library belongs in here because it IS part of the resident surface: without it this field
    /// cannot tell apart two firmwares that differ only in the library they carry, which is exactly
    /// the difference a host must not get wrong. A target with no resident library reports the
    /// registry's fingerprint alone.
    pub hash: u64,
    /// The resident library's own declared version -- the GENERATION of the contract it implements,
    /// read out of the resident image rather than from a firmware constant.
    ///
    /// A constant would be a second spelling that goes stale the first time the library moves, and
    /// the spelling that goes stale is the one nobody is looking at. A runtime with no managed
    /// library reports all zeros, which is also what an assembly declaring no version reports.
    pub lib_version: [u16; 4],
    /// The resident library's own declared FILE version -- which BUILD of that generation.
    ///
    /// A separate field from [`Surface::lib_version`] and never spliced into it. A file version
    /// carries its own leading pair, so folding the two into one four-part number discards that pair
    /// and produces a number present in neither -- a tool printing it would print a fiction. The
    /// leading pair repeating the generation is a free consistency check worth asserting on: a build
    /// where the two disagree is broken.
    pub lib_file_version: [u16; 4],
    /// RESERVED per-runtime capability claim, and it must be zero.
    ///
    /// Capabilities are per-BOARD and residency is per-RUNTIME, so the board-level word cannot say
    /// *this runtime is debuggable and that one is not* on a board holding both. Two bytes reserved
    /// here is what keeps the precise statement reachable without a version bump; zero means no
    /// per-runtime claim, use the board-level word.
    pub caps: u16,
}

impl Surface {
    /// Encoded size, every field at its natural alignment:
    ///
    /// ```text
    /// hash: u64                   @ 0
    /// lib_version: [u16; 4]       @ 8
    /// lib_file_version: [u16; 4]  @ 16
    /// abi: u16                    @ 24
    /// caps: u16                   @ 26    RESERVED, must be zero
    /// tier: u8                    @ 28
    /// reserved: [u8; 3]           @ 29    RESERVED, must be zero
    /// ```
    ///
    /// # Why the order, and it is not about the size being round
    ///
    /// Widest first is what makes the record COPYABLE. Ordered by declaration it was internally
    /// misaligned -- a `u16` at offset 1 and a `u64` at offset 3 -- so no implementation on either
    /// side could ever lay a structure over it, and byte-wise decoding was forced rather than
    /// chosen. Padding alone would have bought a tidy stride and nothing else. This buys a record
    /// either end can copy whole on every part in this set, including the ones that fault on an
    /// unaligned 64-bit read.
    pub const ENCODED_LEN: usize = 32;

    fn encode_into(&self, payload: &mut Vec<u8>) {
        payload.extend_from_slice(&self.hash.to_le_bytes());
        for part in self.lib_version {
            payload.extend_from_slice(&part.to_le_bytes());
        }
        for part in self.lib_file_version {
            payload.extend_from_slice(&part.to_le_bytes());
        }
        payload.extend_from_slice(&self.abi.to_le_bytes());
        payload.extend_from_slice(&self.caps.to_le_bytes());
        payload.push(self.tier);
        payload.extend_from_slice(&[0u8; 3]);
    }

    /// `None` when the record is short, or when either RESERVED field is nonzero.
    ///
    /// Refusing a nonzero reserved field is what makes it reservable at all. A decoder that
    /// tolerated one would let a later firmware put meaning there and be silently misread by every
    /// host built before it -- which is the same failure as never having reserved the bytes, arriving
    /// later and harder to find.
    fn decode_from(bytes: &[u8]) -> Option<Self> {
        let bytes: &[u8; Self::ENCODED_LEN] = bytes.get(..Self::ENCODED_LEN)?.try_into().ok()?;
        let caps = u16::from_le_bytes([bytes[26], bytes[27]]);
        if caps != 0 || bytes[29..32] != [0, 0, 0] {
            return None;
        }
        let quad = |at: usize| {
            [
                u16::from_le_bytes([bytes[at], bytes[at + 1]]),
                u16::from_le_bytes([bytes[at + 2], bytes[at + 3]]),
                u16::from_le_bytes([bytes[at + 4], bytes[at + 5]]),
                u16::from_le_bytes([bytes[at + 6], bytes[at + 7]]),
            ]
        };
        Some(Self {
            hash: u64::from_le_bytes(bytes[0..8].try_into().ok()?),
            lib_version: quad(8),
            lib_file_version: quad(16),
            abi: u16::from_le_bytes([bytes[24], bytes[25]]),
            caps,
            tier: bytes[28],
        })
    }

    /// The version field this record leaves ALL-ZERO although the target's own claims say it must
    /// have one -- `"lib_version"` or `"lib_file_version"` -- or `None` when the record is
    /// consistent with what the target says it holds.
    ///
    /// `resident_library` is the target's own claim to hold a resident managed library
    /// ([`Capabilities::RESIDENT_CORLIB`] on the same acknowledgement), which is what makes the two
    /// cases different bytes.
    ///
    /// # Why all-zero cannot simply mean "no version"
    ///
    /// It has to mean that for most surfaces: a Python or ECMAScript surface has no managed library
    /// and a native one resolves nothing, so all-zero is correct and expected for them, and it is
    /// also what an assembly declaring no version reports. **But a target that says it holds a
    /// resident library and then reports no version for it is describing a read that failed**, and
    /// without this rule that is the same eight bytes as a target that honestly has nothing to
    /// state. The absence is then read as a stated value -- a host compares versions, finds two
    /// zeros, and concludes the board matches whatever it is holding.
    ///
    /// **A reader that meets `Some` should refuse the record and name the field**, because the
    /// repair is a firmware fix and no comparison built on the value can be right. It is not a
    /// reason to stop talking to the board: everything else in the identity is still true.
    #[must_use]
    pub fn unreadable_version(&self, resident_library: bool) -> Option<&'static str> {
        if self.tier != msg::tier::CIL || !resident_library {
            return None;
        }
        if self.lib_version == [0; 4] {
            return Some("lib_version");
        }
        if self.lib_file_version == [0; 4] {
            return Some("lib_file_version");
        }
        None
    }
}

/// What is at the far end of the wire: the product, what it can run, which firmware build is
/// answering, which silicon it is, and every runtime resident on it.
///
/// It is UNCONDITIONAL -- every `HELLO_ACK` carries one -- and `0` means unknown throughout, so a
/// custom board with nothing to declare costs eleven bytes and never has to lie. A display NAME is
/// deliberately not here: it lives in the profile manifest, where it has no length limit to be
/// truncated by.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct TargetIdentity {
    /// The PRODUCT code from [`product_model`]. `0` = unknown.
    pub product_model: u16,
    /// What machine code this target runs, from [`arch`]. `0` = unknown.
    pub arch: u16,
    /// Which FIRMWARE BUILD is answering, as `[days, build]`: days since the start of the year 2000,
    /// then which build of that day, counting from zero. `[0, 0]` = unknown.
    ///
    /// # Why the firmware needs a field of its own
    ///
    /// A hash cannot supply it. Both of [`Surface::hash`]'s inputs are content the firmware SERVES
    /// rather than the firmware itself, so a fix that leaves the seam registry alone -- a framing
    /// repair, a scheduler fix, a flash-driver correction -- produces a byte-identical hash. Two
    /// firmware builds differing in a shipped defect are indistinguishable without this.
    ///
    /// And it cannot be fixed by hashing MORE: folding the firmware image's own digest into
    /// [`Surface::hash`] would destroy what that field is for, since it has to stay equal to the
    /// value a host recorded for the library a program was compiled against, and must not move when
    /// an unrelated runtime fix ships. Two facts, two fields.
    ///
    /// Version-shaped rather than digest-shaped, because the sentence a person needs is *yours is
    /// older, reflash* and a digest cannot order two builds. It is the same day-and-build scheme the
    /// resident library's file version uses, so a build system carries ONE date scheme rather than
    /// two, and it deliberately carries no leading generation pair: one number, one question.
    pub firmware_version: [u16; 2],
    /// Which chip-identity scheme [`TargetIdentity::chip_id`] follows (see [`chip_id_kind`]).
    /// `0` = none offered.
    pub chip_id_kind: u8,
    /// The chip's identity bytes under [`TargetIdentity::chip_id_kind`], opaque to this layer.
    ///
    /// Kind and length rather than a pair of fixed fields, because the identity a part answers is
    /// not one shape: some architectures answer a debug-port code and a vendor device id, others a
    /// vendor, an architecture and an implementation register, and a part reached over neither
    /// answers nothing at all. A shape borrowed from one architecture makes the others report
    /// zeros that read as unknown.
    pub chip_id: Vec<u8>,
    /// Every runtime resident on this target, in the target's own order. EMPTY means no resident
    /// interpreter -- a bootloader, or a target running native code -- and WHICH of those it is, is
    /// read from the capability word rather than from this count.
    pub surfaces: Vec<Surface>,
}

/// Which scheme a [`TargetIdentity::chip_id`] follows. Codes are wire values: append-only, never
/// renumbered.
pub mod chip_id_kind {
    /// No chip identity offered.
    pub const NONE: u8 = 0;
    /// An eight-byte identity: the debug port's own identification code, then the vendor's device
    /// id register, both little-endian.
    ///
    /// Both are needed and neither is enough. A debug-port code names a PORT CLASS and is shared
    /// across unrelated parts, so a tool naming a part from it alone will name the wrong one
    /// confidently; the vendor register is what separates them, and which register that is differs
    /// per family.
    pub const DEBUG_PORT_AND_DEVICE_ID: u8 = 1;
    /// A twelve-byte identity: the vendor, architecture and implementation registers a RISC-V core
    /// answers, each little-endian.
    pub const RISCV_MVENDOR_MARCH_MIMP: u8 = 2;
}

impl TargetIdentity {
    /// Encoded size before the variable parts: `product_model(2) | arch(2) | firmware_version(4) |
    /// chip_id_kind(1) | chip_id_len(1)`.
    const FIXED_LEN: usize = 10;

    /// The first surface record whose version fields contradict `resident_library` -- the target's
    /// own claim to hold a resident managed library -- as `(tier, field name)`.
    ///
    /// ONE walk, because both ends ask it: a target checks what it is about to advertise and a host
    /// checks what it was told, and a second copy of the rule would gain its next case in one of
    /// them. See [`Surface::unreadable_version`] for what it decides and why.
    #[must_use]
    pub fn unreadable_surface_version(&self, resident_library: bool) -> Option<(u8, &'static str)> {
        self.surfaces.iter().find_map(|surface| {
            surface.unreadable_version(resident_library).map(|field| (surface.tier, field))
        })
    }

    /// The identity with its chip fields filled.
    #[must_use]
    pub fn with_chip_id(mut self, kind: u8, id: &[u8]) -> Self {
        self.chip_id_kind = kind;
        self.chip_id = id.to_vec();
        self
    }

    /// The identity with one more resident runtime appended.
    #[must_use]
    pub fn with_surface(mut self, surface: Surface) -> Self {
        self.surfaces.push(surface);
        self
    }

    fn encode_into(&self, payload: &mut Vec<u8>) {
        payload.extend_from_slice(&self.product_model.to_le_bytes());
        payload.extend_from_slice(&self.arch.to_le_bytes());
        for part in self.firmware_version {
            payload.extend_from_slice(&part.to_le_bytes());
        }
        payload.push(self.chip_id_kind);
        let id_len = self.chip_id.len().min(u8::MAX as usize);
        payload.push(id_len as u8);
        payload.extend_from_slice(&self.chip_id[..id_len]);
        let count = self.surfaces.len().min(u8::MAX as usize);
        payload.push(count as u8);
        for surface in &self.surfaces[..count] {
            surface.encode_into(payload);
        }
    }

    /// Decode from the start of `bytes`, returning the identity and how many bytes it took.
    ///
    /// `None` on a truncated identity rather than a partial one. The identity is unconditional, so
    /// a payload that does not carry a whole one is a malformed message rather than a target with
    /// less to say -- and a target with nothing to say has an all-zero identity to send.
    fn decode_from(bytes: &[u8]) -> Option<(Self, usize)> {
        let head = bytes.get(..Self::FIXED_LEN)?;
        let chip_id_len = head[9] as usize;
        let mut at = Self::FIXED_LEN;
        let chip_id = bytes.get(at..at + chip_id_len)?.to_vec();
        at += chip_id_len;
        let count = *bytes.get(at)? as usize;
        at += 1;
        let mut surfaces = Vec::with_capacity(count);
        for _ in 0..count {
            surfaces.push(Surface::decode_from(bytes.get(at..)?)?);
            at += Surface::ENCODED_LEN;
        }
        Some((
            Self {
                product_model: u16::from_le_bytes([head[0], head[1]]),
                arch: u16::from_le_bytes([head[2], head[3]]),
                firmware_version: [
                    u16::from_le_bytes([head[4], head[5]]),
                    u16::from_le_bytes([head[6], head[7]]),
                ],
                chip_id_kind: head[8],
                chip_id,
                surfaces,
            },
            at,
        ))
    }
}

/// Known PRODUCT codes for [`TargetIdentity::product_model`] -- the products the in-tree serve
/// firmwares are built for. Host registries mirror these codes to display names; `0` stays
/// "unknown" so a custom board never lies. Codes are wire values: append-only, never renumber.
///
/// # Why the field is a PRODUCT and not a board
///
/// The value space already holds things that are not boards. One value is a MODULE rather than a
/// board with headers, and its own fact table opens by saying so: its debug signals leave through
/// its connectors, so whether a debug port is reachable at all is a property of the carrier it is
/// seated in, and every carrier is the same value here. The field also has to carry products that
/// are not development boards and named virtual targets, and it is the discriminator of LAST
/// RESORT -- on some parts nothing a debug port can read separates the products a single value
/// covers, so a model number is the only thing that can tell them apart, and it has to be told
/// rather than discovered.
///
/// Naming it for the target instead was the other candidate, and it loses on a collision: "target"
/// already means the far end of the wire, a compiler's architecture triple, and a row in a flashing
/// table. Beside a field named [`TargetIdentity::arch`], a target model reads as *which model of
/// target architecture*, and that misreading is silent.
pub mod product_model {
    /// Unknown / custom product (the chip fields may still identify the silicon).
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

    /// AutomationDirect / FACTS Engineering P1AM-100 (ATSAMD21G18A, Cortex-M0+, 256 KB flash /
    /// 32 KB SRAM) -- an industrial PLC CPU in the Arduino MKR form factor. Its strata are
    /// `csp/samd21` and the part is the one the MKR boards carry, but the board is not one of
    /// them: its real outputs are P1000 modules on a backplane, reached over a SPI link to a
    /// separate base controller, so nothing this part drives is a rack channel.
    pub const P1AM_100: u16 = 42;

    /// Arduino Opta WiFi (AFX00002) -- an industrial PLC on an STM32H747XI, the same part the
    /// Portenta H7 and the GIGA carry. The part is shared and the board is not: its four outputs
    /// are Finder relays rated 250 VAC at 10 A, so a pad here energizes a coil rather than lighting
    /// something, and the three Opta variants differ in fitted parts that no register can see.
    pub const ARDUINO_OPTA_WIFI: u16 = 43;

    /// ST NUCLEO-L073RZ (STM32L073RZT6, Cortex-M0+, 192 KB flash / 20 KB SRAM). Its strata are
    /// `csp/stm32l073`, NOT `csp/stm32l053`: both are RM0367's STM32L0x3 line, and the manual
    /// splits that line into categories whose silicon differs -- this one has GPIOE, a whole port
    /// the other lacks, and a dual-bank NVM where the other is single bank. The two parts bond the
    /// same 51 pads in this package, so a pad list cannot tell them apart.
    pub const NUCLEO_L073RZ: u16 = 44;

    /// Microchip SAM E51 Curiosity Nano (EV76S68A, ATSAME51J20A, Cortex-M4F, 1 MB flash /
    /// 256 KB SRAM). Its strata are `csp/same54`, which is the D5x/E5x family rather than one
    /// part: this board carries the 64-pin package of it, so port A is bonded as the Xplained
    /// Pro's is and port B is cut to twenty-two pads with ports C and D absent entirely. The
    /// debugger is an nEDBG rather than an EDBG, which changes the USB product id and not the
    /// carrier.
    pub const SAME51_CURIOSITY_NANO: u16 = 45;

    /// Microchip SAM D21 Curiosity Nano (SAMD21-CNANO, ATSAMD21G17D, Cortex-M0+, 128 KB flash /
    /// 16 KB SRAM). Its strata are `csp/samd21`, and the part is the same 48-pin G package as the
    /// Xplained Pro family's `atsamd21g18a` with half the flash and half the SRAM. The debugger is
    /// an nEDBG, as the E51 Curiosity Nano's is. Its 32.768 kHz crystal is NOT connected as the
    /// board ships, which is the opposite of the E51 Nano and is a board fact rather than a part
    /// one.
    pub const SAMD21_CURIOSITY_NANO: u16 = 46;

    /// Microchip SAM R21 Xplained Pro (ATSAMR21-XPRO, ATSAMR21G18A, Cortex-M0+, 256 KB flash /
    /// 32 KB SRAM). Its strata are `csp/samr21`, which is a family of its own rather than a SAM
    /// D21 part row: the die carries an AT86RF233 transceiver in the same package, and the radio
    /// takes a SERCOM, four pads of control and status lines, and its own 16 MHz crystal off the
    /// top of the D21's budget. The visible figure is the I/O count -- 28 pins on a 48-pin part
    /// where a SAM D21 G has 38. The debugger is an EDBG and the carrier is its CDC virtual port.
    pub const SAMR21_XPLAINED_PRO: u16 = 47;

    /// The display name for a `product_model` wire value, or `None` for an unrecognized code. This is the one
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
            P1AM_100 => "P1AM-100",
            ARDUINO_OPTA_WIFI => "Arduino Opta WiFi",
            NUCLEO_L073RZ => "NUCLEO-L073RZ",
            SAME51_CURIOSITY_NANO => "SAM E51 Curiosity Nano",
            SAMD21_CURIOSITY_NANO => "SAM D21 Curiosity Nano",
            SAMR21_XPLAINED_PRO => "SAM R21 Xplained Pro",
            _ => return None,
        })
    }
}

/// The target's `HELLO_ACK`: the negotiated version, the target's capabilities, and what the target
/// IS.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HelloAck {
    /// The negotiated protocol version (the top of the overlapping range).
    pub chosen: u16,
    /// The capabilities the target offers.
    pub caps: Capabilities,
    /// What the target is. Unconditional -- a target with nothing to declare sends an all-zero
    /// identity, which reads as unknown rather than as absent.
    pub identity: TargetIdentity,
    /// The most payload this target can absorb in ONE frame over the carrier this handshake
    /// crossed, or `None` when it did not say -- in which case a sender uses
    /// [`MIN_INBOUND_PAYLOAD`], which every target must meet.
    ///
    /// # Why it rides the handshake rather than the profile manifest
    ///
    /// It is a property of the SESSION's carrier, not of the board. One firmware on one board
    /// accepts a payload over TCP that its own serial port drops, so a per-board fact cannot carry
    /// this number correctly however it is cached. The manifest is also fetched only on a cache
    /// miss, and a sender needs this BEFORE its first large frame rather than after; a handshake is
    /// the one exchange that has already happened by then, over the carrier in question.
    ///
    /// # Why a field and not a capability bit
    ///
    /// A bit cannot carry a number, and the number is the whole content: a target saying only that
    /// it has a limit leaves the sender exactly where it started. It is an `Option` rather than a
    /// zero-means-unknown value because zero is a legal thing to compute from a tiny buffer, and a
    /// target that genuinely can take nothing must not read as one that declined to say.
    pub max_inbound_payload: Option<u16>,
}

impl HelloAck {
    /// Encoded size before the identity: `chosen(2) | caps(8)`.
    const FIXED_LEN: usize = 10;

    /// This acknowledgement, declaring the most payload the target can absorb in one frame.
    ///
    /// Takes the number in payload bytes -- [`max_payload_for_buffer`] converts a ring size into
    /// one -- and clamps to what the wire's `u16` length field can express, because a target
    /// roomier than the protocol still cannot be sent more than the protocol carries.
    #[must_use]
    pub fn with_max_inbound_payload(mut self, payload_bytes: usize) -> Self {
        self.max_inbound_payload = Some(payload_bytes.min(MAX_PAYLOAD) as u16);
        self
    }

    /// `chosen(2) | caps(8)`, little-endian, then the identity, then the trailing fields.
    ///
    /// # Why the extension point is a tail rather than a wider identity
    ///
    /// [`TargetIdentity`] is a settled record with a fixed field order, and widening it moves every
    /// byte after the inserted field on both sides at once. A tail moves nothing: this message's
    /// decoder already skipped whatever followed the identity, so a build predating a trailing
    /// field ignores it by the rule it was already written to rather than by luck.
    ///
    /// **Trailing fields are therefore APPEND-ONLY.** A reader stops at the first one it does not
    /// know, so removing or reordering one silently re-points every field after it -- and silently
    /// is the operative word, because the bytes still decode.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut payload = Vec::with_capacity(Self::FIXED_LEN + TargetIdentity::FIXED_LEN);
        payload.extend_from_slice(&self.chosen.to_le_bytes());
        payload.extend_from_slice(&self.caps.0.to_le_bytes());
        self.identity.encode_into(&mut payload);
        if let Some(max_inbound) = self.max_inbound_payload {
            payload.extend_from_slice(&max_inbound.to_le_bytes());
        }
        payload
    }

    /// Decode, tolerating a longer payload (a newer peer's trailing fields are skipped).
    ///
    /// A trailing field that is absent decodes as `None` rather than failing: a payload ending
    /// early is what a peer built against an earlier revision of this message sends, and it is a
    /// complete answer to every question that revision could ask.
    #[must_use]
    pub fn decode(payload: &[u8]) -> Option<Self> {
        let caps = payload.get(2..Self::FIXED_LEN)?;
        let (identity, identity_len) =
            TargetIdentity::decode_from(payload.get(Self::FIXED_LEN..)?)?;
        let tail = Self::FIXED_LEN + identity_len;
        let max_inbound_payload =
            payload.get(tail..tail + 2).map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]));
        Some(Self {
            chosen: u16::from_le_bytes([payload[0], payload[1]]),
            caps: Capabilities(u64::from_le_bytes(caps.try_into().ok()?)),
            identity,
            max_inbound_payload,
        })
    }

    /// The first surface record in this acknowledgement whose version fields contradict the
    /// capabilities beside them, as `(tier, field name)`; `None` when every record is consistent.
    ///
    /// A target-side reading of [`TargetIdentity::unreadable_surface_version`]: this is where both
    /// facts are in hand on the way OUT. A host reads the same rule off its [`Negotiated`].
    #[must_use]
    pub fn unreadable_surface_version(&self) -> Option<(u8, &'static str)> {
        self.identity
            .unreadable_surface_version(self.caps.has(Capabilities::RESIDENT_CORLIB))
    }
}

/// The target's `HELLO_NAK`: no version overlap; it reports the target's own range so the host can
/// say which side has to move.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HelloNak {
    /// The target's own supported range.
    pub target_range: ProtocolRange,
}

impl HelloNak {
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

/// The highest version both ranges support, or `None` if the ranges are disjoint (-> a `HELLO_NAK`).
pub fn negotiate(host: ProtocolRange, target: ProtocolRange) -> Option<u16> {
    let lo = host.min.max(target.min);
    let hi = host.max.min(target.max);
    (lo <= hi).then_some(hi)
}

/// The negotiated session parameters the host uses after a successful handshake.
#[derive(Clone, Debug, PartialEq, Eq)]
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
    /// What the target is.
    pub identity: TargetIdentity,
    /// What the target advertised as the most payload it can absorb in one frame, exactly as sent
    /// -- `None` when it did not say. Send by [`Self::inbound_payload_limit`]; this is the raw
    /// advertisement, for a tool reporting what a board claimed rather than what to do about it.
    pub max_inbound_payload: Option<u16>,
}

impl Negotiated {
    /// The most payload one frame may carry TO the target on this session: what it advertised, or
    /// [`MIN_INBOUND_PAYLOAD`] when it advertised nothing.
    ///
    /// Never below the floor even when a target advertises less, because the floor is a
    /// REQUIREMENT rather than a default. A target declaring three bytes has misreported itself,
    /// and clamping up keeps one misdeclaration from stalling every transfer into chunks too small
    /// to make progress -- the sender is wrong about that board either way, and this is the wrong
    /// that terminates.
    #[must_use]
    pub fn inbound_payload_limit(&self) -> usize {
        self.max_inbound_payload
            .map_or(MIN_INBOUND_PAYLOAD, |advertised| usize::from(advertised).max(MIN_INBOUND_PAYLOAD))
    }

    /// The most DATA one chunked op may carry to this target in one frame:
    /// [`Self::inbound_payload_limit`] less the `(offset, total)` header
    /// ([`msg::CHUNK_HEADER_LEN`]) every chunk puts ahead of its bytes.
    ///
    /// # This is a carrier bound and NOT the whole answer
    ///
    /// A caller must still round DOWN to whatever its destination requires -- an image path wants
    /// each chunk to start on the target's flash write unit, and that is a property of the board's
    /// flash controller rather than of the wire the chunk arrived over. Two facts with two
    /// different lifetimes: this one is per-session, that one is per-board.
    #[must_use]
    pub fn max_chunk_data(&self) -> usize {
        self.inbound_payload_limit().saturating_sub(msg::CHUNK_HEADER_LEN)
    }
    /// The first surface record whose version fields contradict what the TARGET claimed about
    /// itself, as `(tier, field name)`; `None` when every record is consistent.
    ///
    /// Read against [`Self::target_caps`] and never against [`Self::caps`]: whether a board holds a
    /// resident library is a property of the board, and the intersection can only subtract -- a host
    /// that does not claim [`Capabilities::RESIDENT_CORLIB`] for itself would find every board
    /// consistent, which is exactly the case this check exists to catch.
    #[must_use]
    pub fn unreadable_surface_version(&self) -> Option<(u8, &'static str)> {
        self.identity
            .unreadable_surface_version(self.target_caps.has(Capabilities::RESIDENT_CORLIB))
    }
}

/// The target's reply to a `HELLO`: accept with the negotiated version, the target's capabilities
/// and the target's identity, or reject with the target's range.
///
/// The identity is an argument rather than something a caller attaches afterwards, because it is
/// unconditional: an acknowledgement is not complete without one, and a firmware that forgot to
/// attach it would advertise itself as an unknown product on unknown silicon with no resident
/// runtime -- a well-formed answer that is entirely wrong.
///
/// `max_inbound_payload` is what this carrier can absorb in one frame -- pass
/// [`Transport::max_inbound_payload`] straight through. It is a REQUIRED argument, and `None` is
/// how a carrier declines rather than something a caller can leave out, because four different
/// handshakes in this tree build an acknowledgement and a rule with several implementations gains
/// its next case in none of them. A site that forgets this one does not compile.
pub fn target_respond(
    host: &Hello,
    target_range: ProtocolRange,
    target_caps: Capabilities,
    identity: TargetIdentity,
    max_inbound_payload: Option<usize>,
) -> Result<HelloAck, HelloNak> {
    match negotiate(host.range, target_range) {
        Some(chosen) => {
            let ack = HelloAck { chosen, caps: target_caps, identity, max_inbound_payload: None };
            Ok(match max_inbound_payload {
                Some(bytes) => ack.with_max_inbound_payload(bytes),
                None => ack,
            })
        }
        None => Err(HelloNak { target_range }),
    }
}

/// The host's session parameters from the target's `HELLO_ACK`: the chosen version + the capability
/// INTERSECTION (only what both sides offer) + the target's RAW advertisement + the target's
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
        identity: ack.identity.clone(),
        max_inbound_payload: ack.max_inbound_payload,
    }
}

/// The full resident-profile MANIFEST a target returns for a profile request: the identity a
/// handshake already carried, plus everything that does not fit in one -- the resident library's
/// capability-symbol bitmap, the profile's display name, and the complete list of runtime seams the
/// target registers.
///
/// A host asks only when the identity's hash misses its cache, which is the identity-and-manifest
/// split: the cheap answer rides every handshake, and the expensive one is fetched once per distinct
/// firmware.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileManifest {
    /// The same identity the `HELLO_ACK` advertises.
    pub identity: TargetIdentity,
    /// Which capability symbols the resident library was built with (see [`surface`]).
    ///
    /// This is what makes the version beside it safe to publish. A version states which GENERATION a
    /// library was built against and deliberately says nothing about how much of that generation is
    /// present, because a library is built down per profile -- so without this, the version is a
    /// claim nothing can check. With it, a host answers *does this board have what my program needs*
    /// before a deploy, instead of meeting an unresolved reference in a board's console after one.
    pub surface: u64,
    /// The profile's display name, with no length limit -- which is what it is doing here rather
    /// than in the handshake, where a fixed-size identity would have to truncate it.
    pub name: alloc::string::String,
    /// Every runtime seam this target registers, in registry order.
    pub intrinsic_ids: Vec<u32>,
}

impl ProfileManifest {
    /// Manifest layout version.
    ///
    /// A decoder refuses any other value rather than reading the bytes into these fields, because a
    /// different layout describes a different identity shape and a tolerated mismatch is a wrong
    /// answer instead of a refusal.
    pub const VERSION: u8 = 1;

    /// `version(1) | identity | surface(8) | name_len(2) | name | count(2) | count x id(4)`, LE.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut payload = Vec::with_capacity(
            1 + TargetIdentity::FIXED_LEN + 8 + 2 + self.name.len() + 2 + self.intrinsic_ids.len() * 4,
        );
        payload.push(Self::VERSION);
        self.identity.encode_into(&mut payload);
        payload.extend_from_slice(&self.surface.to_le_bytes());
        let name_len = self.name.len().min(u16::MAX as usize);
        payload.extend_from_slice(&(name_len as u16).to_le_bytes());
        payload.extend_from_slice(&self.name.as_bytes()[..name_len]);
        let count = self.intrinsic_ids.len().min(u16::MAX as usize);
        payload.extend_from_slice(&(count as u16).to_le_bytes());
        for id in &self.intrinsic_ids[..count] {
            payload.extend_from_slice(&id.to_le_bytes());
        }
        payload
    }

    /// Decode, tolerating a longer payload; `None` on an unknown version, a name that is not UTF-8,
    /// or a truncated list.
    #[must_use]
    pub fn decode(payload: &[u8]) -> Option<Self> {
        if payload.first() != Some(&Self::VERSION) {
            return None;
        }
        let (identity, consumed) = TargetIdentity::decode_from(payload.get(1..)?)?;
        let mut at = 1 + consumed;
        let surface = u64::from_le_bytes(payload.get(at..at + 8)?.try_into().ok()?);
        at += 8;
        let name_len = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?) as usize;
        at += 2;
        let name = core::str::from_utf8(payload.get(at..at + name_len)?).ok()?.into();
        at += name_len;
        let count = u16::from_le_bytes(payload.get(at..at + 2)?.try_into().ok()?) as usize;
        at += 2;
        let mut intrinsic_ids = Vec::with_capacity(count);
        for index in 0..count {
            let at = at + index * 4;
            intrinsic_ids.push(u32::from_le_bytes(payload.get(at..at + 4)?.try_into().ok()?));
        }
        Some(Self { identity, surface, name, intrinsic_ids })
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
    /// The two ends share no protocol version, so no session began: the target answered
    /// [`msg::HELLO_NAK`] with the range it supports.
    ///
    /// # Why this is not [`Closed`](Self::Closed)
    ///
    /// It was, and that made the protocol's one deliberate incompatibility signal unreadable. The
    /// handshake is DESIGNED to detect this -- both ends carry a range and [`negotiate`] reports no
    /// overlap -- and the target's `HELLO_NAK` names the range it can speak, which is the whole
    /// content of the answer. Reporting it as a closed link discarded that and pointed the reader at
    /// the cable, which is the one place the fault is not.
    ///
    /// **The remedy is a build, not a reconnect**: one end has to change, and these two numbers are
    /// what say which. A host newer than the target means the target needs reflashing; a target
    /// newer than the host means the tools need updating.
    VersionMismatch {
        /// The lowest protocol version the target can speak.
        target_min: u16,
        /// The highest protocol version the target can speak. Equal to `target_min` for a target
        /// that supports exactly one, which every in-tree firmware does today.
        target_max: u16,
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

    /// The most payload THIS carrier can absorb in one frame, or `None` for no limit worth
    /// declaring. A target's handshake advertises it ([`HelloAck::max_inbound_payload`]).
    ///
    /// # Why the carrier answers rather than the firmware
    ///
    /// The limit is a property of the receive path, and a board commonly has more than one. A SAM
    /// E54 serves this protocol over USB, over an EDBG serial port and over Ethernet at the same
    /// time, from one firmware and one serve loop -- and its serial ring is the only one of the
    /// three that constrains anything. A firmware-wide answer would have to be the smallest of
    /// them, which penalizes every other carrier for the worst one; the object that owns the ring
    /// is the object that knows.
    ///
    /// `None` -- the default -- means this carrier declines to declare one, and a sender then uses
    /// [`MIN_INBOUND_PAYLOAD`]. That is the SAFE reading and it is why this could be added without
    /// touching a single existing carrier: a transport that says nothing is sent the floor, which
    /// every target must accept anyway. Declaring is how a roomy carrier earns bigger frames, not
    /// how a small one stays safe.
    ///
    /// Answer in PAYLOAD bytes, from [`max_payload_for_buffer`] -- the framing overhead is not
    /// public, and a carrier subtracting its own idea of it would over-declare on the day it moves.
    fn max_inbound_payload(&self) -> Option<usize> {
        None
    }

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
            "MONOTONIC_CLOCK | BAKED_IMAGE"
        );
        assert_eq!(
            Capabilities(Capabilities::BAKED_IMAGE | (1 << 63)).describe(),
            "BAKED_IMAGE | unknown bit 63"
        );
        assert_eq!(Capabilities(1 << 20).describe(), "unknown bit 20");
    }

    /// The table itself: one bit per entry, no duplicates, no zero rows. A duplicated bit would print
    /// one capability under two names; a multi-bit entry would claim a set it does not have.
    #[test]
    fn the_capability_name_table_is_one_distinct_bit_per_row() {
        let mut seen = 0u64;
        for (bit, name) in Capabilities::NAMED {
            assert_eq!(bit.count_ones(), 1, "{name} must be exactly one bit");
            assert_eq!(seen & bit, 0, "{name} duplicates a bit already named");
            seen |= bit;
        }
    }

    /// EVERY named bit sits in exactly one family, and the families do not overlap.
    ///
    /// This is what replaced a contiguity check, and it is the assertion the grouping actually
    /// needs: bits are no longer packed from zero, so contiguity would now fail on correct code
    /// while saying nothing about the property that matters. A bit outside every family is one that
    /// no family mask can see, and a mask that silently misses a bit answers "this board cannot be
    /// debugged" about a board that can.
    #[test]
    fn every_named_capability_sits_in_exactly_one_family() {
        let families = [
            ("SESSION", Capabilities::FAMILY_SESSION),
            ("DEBUG", Capabilities::FAMILY_DEBUG),
            ("ARTIFACT", Capabilities::FAMILY_ARTIFACT),
            ("OBSERVE", Capabilities::FAMILY_OBSERVE),
            ("DEVICE", Capabilities::FAMILY_DEVICE),
        ];
        let mut union = 0u64;
        for (name, mask) in families {
            assert_eq!(mask.count_ones(), 8 * (mask.count_ones() / 8), "{name} is a whole-byte span");
            assert_eq!(union & mask, 0, "{name} overlaps a family already declared");
            union |= mask;
        }
        for (bit, name) in Capabilities::NAMED {
            let homes = families.iter().filter(|(_, mask)| bit & mask != 0).count();
            assert_eq!(homes, 1, "{name} must sit in exactly one family, not {homes}");
        }
        let kinds = Capabilities::ARTIFACT_KINDS;
        assert_eq!(kinds.count_ones(), 4, "four artifact kinds");
        assert_eq!(
            kinds.count_ones(),
            64 - kinds.leading_zeros() - kinds.trailing_zeros(),
            "the artifact-kind bits must be contiguous or the mask is not the set"
        );
        assert_eq!(kinds & Capabilities::FAMILY_ARTIFACT, kinds, "and they live in their own family");
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

    /// A fixture identity with something in every field, so a decoder that drops one is caught by
    /// the round trip rather than by a reader noticing later.
    fn an_identity() -> TargetIdentity {
        TargetIdentity {
            product_model: product_model::SAMW25_XPLAINED_PRO,
            arch: arch::THUMBV6M,
            firmware_version: [9734, 0],
            ..TargetIdentity::default()
        }
        .with_chip_id(chip_id_kind::DEBUG_PORT_AND_DEVICE_ID, &0x0bc1_1477u32.to_le_bytes())
        .with_surface(Surface {
            tier: msg::tier::CIL,
            abi: 1,
            hash: 0xDEAD_BEEF_0BAD_F00D,
            lib_version: [1, 0, 0, 0],
            lib_file_version: [1, 0, 9734, 0],
            caps: 0,
        })
    }

    #[test]
    fn the_ack_carries_the_whole_identity_and_every_field_survives() {
        let identity = an_identity();
        let ack = HelloAck {
            chosen: 1,
            caps: Capabilities(Capabilities::PROFILE_CHIPID | Capabilities::RESIDENT_CORLIB),
            identity: identity.clone(),
            max_inbound_payload: None,
        };
        let bytes = ack.encode();
        let back = HelloAck::decode(&bytes).expect("an ack decodes");
        assert_eq!(back, ack, "every field round-trips");
        assert_eq!(back.identity.product_model, product_model::SAMW25_XPLAINED_PRO);
        assert_eq!(back.identity.chip_id, 0x0bc1_1477u32.to_le_bytes());
        assert_eq!(back.identity.surfaces.len(), 1);

        assert_eq!(u16::from_le_bytes([bytes[0], bytes[1]]), 1);
        assert_eq!(bytes[2..10], ack.caps.0.to_le_bytes());
    }

    /// The buffer conversion is proved against the ENCODER, not against a restatement of it.
    ///
    /// A test asserting `max_payload_for_buffer(256) == 247` would pass whatever both sides did,
    /// because it computes the same subtraction the function does -- the arithmetic checking
    /// itself. What has to be true is that a payload of the returned size FRAMES into the buffer
    /// it was measured for, and that one more byte does not. That is a statement about
    /// `encode_frame`, so `encode_frame` is what answers it.
    #[test]
    fn the_buffer_conversion_answers_what_actually_fits_a_ring() {
        for ring in [64usize, 256, 512, 4096] {
            let payload = max_payload_for_buffer(ring);
            let frame = encode_frame(msg::PING, 1, &alloc::vec![0xA5u8; payload])
                .expect("a payload sized to the ring frames");
            assert_eq!(frame.len(), ring, "a ring of {ring} is filled exactly, not approximately");

            let one_more = encode_frame(msg::PING, 1, &alloc::vec![0xA5u8; payload + 1])
                .expect("one byte more still frames");
            assert!(
                one_more.len() > ring,
                "a ring of {ring} cannot hold {payload} + 1 bytes of payload",
            );
        }
    }

    /// A buffer too small to hold any frame at all reports zero rather than underflowing.
    ///
    /// Reachable from a firmware that hands over the size of something that is not a receive ring,
    /// and the alternative on a release build is a wrapped `usize` -- an advertisement of about
    /// four billion bytes, which is the worst possible direction for this number to be wrong in.
    #[test]
    fn a_buffer_smaller_than_the_framing_carries_no_payload() {
        for tiny in [0usize, 1, 8] {
            assert_eq!(max_payload_for_buffer(tiny), 0, "{tiny} bytes cannot hold a frame");
        }
    }

    /// The floor fits the smallest receive ring in this tree, which is the only property that makes
    /// it safe to send to a target that has advertised nothing.
    #[test]
    fn the_floor_frames_into_the_smallest_ring_any_serve_uses() {
        const SMALLEST_SERVE_RING: usize = 256;
        let frame = encode_frame(msg::PING, 1, &alloc::vec![0u8; MIN_INBOUND_PAYLOAD])
            .expect("the floor is a carryable payload");
        assert!(
            frame.len() <= SMALLEST_SERVE_RING,
            "the floor is {} on the wire and the smallest ring is {SMALLEST_SERVE_RING}",
            frame.len(),
        );
        assert!(
            MIN_INBOUND_PAYLOAD <= max_payload_for_buffer(SMALLEST_SERVE_RING),
            "the floor must not exceed what that ring can take",
        );
    }

    /// A target that declares a limit round-trips it, and one that declares none says so.
    #[test]
    fn the_inbound_limit_round_trips_and_absent_is_a_separate_answer() {
        let declared = HelloAck {
            chosen: 1,
            caps: Capabilities(0),
            identity: an_identity(),
            max_inbound_payload: None,
        }
        .with_max_inbound_payload(max_payload_for_buffer(512));
        let back = HelloAck::decode(&declared.encode()).expect("an ack decodes");
        assert_eq!(back.max_inbound_payload, Some(503), "a 512-byte ring, less the framing");
        assert_eq!(back, declared, "every field round-trips");

        let silent = HelloAck {
            chosen: 1,
            caps: Capabilities(0),
            identity: an_identity(),
            max_inbound_payload: None,
        };
        let quiet = HelloAck::decode(&silent.encode()).expect("an ack decodes");
        assert_eq!(quiet.max_inbound_payload, None, "silence is not a zero");
    }

    /// A target roomier than the wire still advertises only what the wire can carry.
    ///
    /// The `LEN` field is a `u16`, so a target with a megabyte of buffer that advertised it would
    /// be describing frames this protocol has no way to express -- and the number would arrive
    /// TRUNCATED rather than large, which is the shape that lies quietly.
    #[test]
    fn an_advertisement_is_clamped_to_what_the_wire_can_express() {
        let roomy = HelloAck {
            chosen: 1,
            caps: Capabilities(0),
            identity: TargetIdentity::default(),
            max_inbound_payload: None,
        }
        .with_max_inbound_payload(1_048_576);
        assert_eq!(roomy.max_inbound_payload, Some(MAX_PAYLOAD as u16));
    }

    /// BOTH DIRECTIONS OF THE VERSION SKEW, which is the property that let this field be added
    /// after the message-type space was settled.
    ///
    /// An older peer's acknowledgement simply ends after the identity, and a newer one's carries
    /// two bytes an older reader was already skipping. Neither is a decode failure and neither
    /// changes what the identity says -- so the two builds interoperate, each getting the answer
    /// its own version can act on.
    #[test]
    fn an_ack_decodes_across_a_build_that_predates_the_trailing_field() {
        let identity = an_identity();
        let modern = HelloAck {
            chosen: 1,
            caps: Capabilities(0),
            identity: identity.clone(),
            max_inbound_payload: None,
        }
        .with_max_inbound_payload(503);
        let with_tail = modern.encode();

        let without_tail = &with_tail[..with_tail.len() - 2];
        let old_target = HelloAck::decode(without_tail).expect("an older ack still decodes");
        assert_eq!(old_target.max_inbound_payload, None);
        assert_eq!(old_target.identity, identity, "the identity is untouched by the tail");

        let (as_old_host_sees_it, consumed) =
            TargetIdentity::decode_from(&with_tail[HelloAck::FIXED_LEN..])
                .expect("the identity decodes out of a tail-bearing payload");
        assert_eq!(as_old_host_sees_it, identity, "an older host reads the same board");
        assert_eq!(
            HelloAck::FIXED_LEN + consumed + 2,
            with_tail.len(),
            "the tail is the only thing after the identity",
        );
    }

    /// What a host SENDS by: the floor when nothing was advertised, the advertisement when there
    /// was one, and the floor again when a target under-declares.
    #[test]
    fn a_session_sends_by_the_advertisement_or_by_the_floor() {
        fn session(max_inbound_payload: Option<u16>) -> Negotiated {
            host_finish(
                &HelloAck {
                    chosen: 1,
                    caps: Capabilities(0),
                    identity: TargetIdentity::default(),
                    max_inbound_payload,
                },
                Capabilities(0),
            )
        }

        assert_eq!(
            session(None).inbound_payload_limit(),
            MIN_INBOUND_PAYLOAD,
            "a target that said nothing is sent the floor",
        );
        assert_eq!(
            session(Some(4087)).inbound_payload_limit(),
            4087,
            "a roomy target is sent what it asked for",
        );
        assert_eq!(
            session(Some(3)).inbound_payload_limit(),
            MIN_INBOUND_PAYLOAD,
            "an under-declaration does not stall a transfer into chunks that cannot progress",
        );
        assert_eq!(session(None).max_chunk_data(), MIN_INBOUND_PAYLOAD - msg::CHUNK_HEADER_LEN);
    }

    /// A chunk sized by `max_chunk_data` FRAMES within what the target said it can take -- proved
    /// by encoding one, rather than by re-deriving the subtraction that produced it.
    #[test]
    fn a_chunk_sized_for_a_session_fits_the_ring_it_was_sized_from() {
        for ring in [256usize, 512, 4096] {
            let advertised = max_payload_for_buffer(ring);
            let session = host_finish(
                &HelloAck {
                    chosen: 1,
                    caps: Capabilities(0),
                    identity: TargetIdentity::default(),
                    max_inbound_payload: None,
                }
                .with_max_inbound_payload(advertised),
                Capabilities(0),
            );

            let mut payload = alloc::vec![0u8; msg::CHUNK_HEADER_LEN];
            payload.extend_from_slice(&alloc::vec![0xA5u8; session.max_chunk_data()]);
            let frame = encode_frame(msg::DEPLOY_IMAGE, 1, &payload).expect("a chunk frames");
            assert_eq!(
                frame.len(),
                ring,
                "a full chunk to a {ring}-byte ring lands inside it, not one byte over",
            );
        }
    }

    /// A target with nothing to declare still sends an identity, and it decodes as UNKNOWN rather
    /// than as absent.
    ///
    /// That is the whole of what "unconditional" buys: a host has one shape to parse, and the
    /// difference between *a custom board* and *a board that did not answer* stops being a length
    /// comparison. The all-zero form is eleven bytes.
    #[test]
    fn an_identity_with_nothing_to_declare_is_still_an_identity() {
        let ack = HelloAck {
            chosen: 1,
            caps: Capabilities(0),
            identity: TargetIdentity::default(),
            max_inbound_payload: None,
        };
        let bytes = ack.encode();
        assert_eq!(bytes.len(), 10 + 11, "the head, then the smallest identity");
        let back = HelloAck::decode(&bytes).expect("an ack decodes");
        assert_eq!(back.identity.product_model, product_model::UNKNOWN);
        assert_eq!(back.identity.arch, arch::UNKNOWN);
        assert_eq!(back.identity.chip_id_kind, chip_id_kind::NONE);
        assert!(back.identity.surfaces.is_empty(), "no resident interpreter");
    }

    /// A truncated identity is a DECODE FAILURE, not a shorter identity.
    ///
    /// The tolerant reading was correct while the identity was optional and a peer might genuinely
    /// have had none to send. It is wrong now: every field has an unknown value it can carry, so a
    /// payload that stops mid-identity is a framing defect, and reporting it as a board that knows
    /// less about itself would hide the defect behind a plausible answer.
    #[test]
    fn a_truncated_identity_fails_rather_than_shrinking() {
        let ack = HelloAck {
            chosen: 1,
            caps: Capabilities(0),
            identity: an_identity(),
            max_inbound_payload: None,
        };
        let bytes = ack.encode();
        for cut in 1..bytes.len() {
            assert_eq!(HelloAck::decode(&bytes[..cut]), None, "a payload cut at {cut} is not an ack");
        }
        assert!(HelloAck::decode(&bytes).is_some(), "and the whole payload is");
    }

    /// A board holding TWO runtimes of the SAME kind at different levels -- the case a capability
    /// bit per runtime could not describe, and the reason the level and the hash sit on the record.
    #[test]
    fn several_resident_runtimes_ride_one_identity() {
        let identity = an_identity().with_surface(Surface {
            tier: msg::tier::CIL,
            abi: 2,
            hash: 0x0102_0304_0506_0708,
            lib_version: [2, 0, 0, 0],
            lib_file_version: [2, 0, 9734, 1],
            caps: 0,
        });
        let ack =
            HelloAck { chosen: 1, caps: Capabilities(0), identity, max_inbound_payload: None };
        let back = HelloAck::decode(&ack.encode()).expect("decodes");
        assert_eq!(back.identity.surfaces.len(), 2);
        assert_eq!(back.identity.surfaces[0].abi, 1);
        assert_eq!(back.identity.surfaces[1].abi, 2);
        assert_eq!(back.identity.surfaces[1].lib_file_version, [2, 0, 9734, 1]);
    }

    /// The two version fields are SEPARATE on the wire, and the record is the size it says it is.
    ///
    /// Splicing them into one four-part number was refused for a reason a size check cannot state
    /// but a reader here should not have to rediscover: a file version carries its own leading pair,
    /// so a splice drops that pair and yields a number present in neither field.
    #[test]
    fn a_surface_record_is_thirty_two_bytes_and_keeps_its_two_versions_apart() {
        let surface = Surface {
            tier: msg::tier::CIL,
            abi: 0x1234,
            hash: 0x1122_3344_5566_7788,
            lib_version: [2, 0, 0, 0],
            lib_file_version: [12, 5, 9734, 7],
            caps: 0,
        };
        let mut bytes = Vec::new();
        surface.encode_into(&mut bytes);
        assert_eq!(bytes.len(), Surface::ENCODED_LEN);
        assert_eq!(Surface::ENCODED_LEN, 32);
        let back = Surface::decode_from(&bytes).expect("decodes");
        assert_eq!(back, surface);
        assert_eq!(back.lib_version, [2, 0, 0, 0], "the generation is untouched");
        assert_eq!(back.lib_file_version, [12, 5, 9734, 7], "and so is the build's own leading pair");
    }

    /// EVERY FIELD AT ITS NATURAL ALIGNMENT, asserted by OFFSET rather than by size.
    ///
    /// The size is what a reader checks and it is not the property: any 32-byte arrangement passes
    /// a size assertion, including one that puts the 64-bit field at offset 3 again. What the layout
    /// is for is that either end can copy the record whole -- on parts that fault on an unaligned
    /// 64-bit read, that is the difference between a `memcpy` and a hard fault -- and only the
    /// offsets say so.
    #[test]
    fn every_surface_field_lands_at_its_natural_alignment() {
        let surface = Surface {
            hash: 0x1122_3344_5566_7788,
            lib_version: [0x0102, 0x0304, 0x0506, 0x0708],
            lib_file_version: [0x1112, 0x1314, 0x1516, 0x1718],
            abi: 0xABCD,
            caps: 0,
            tier: msg::tier::PYTHON,
        };
        let mut bytes = Vec::new();
        surface.encode_into(&mut bytes);
        assert_eq!(bytes.len(), 32);

        assert_eq!(bytes[0..8], 0x1122_3344_5566_7788u64.to_le_bytes(), "hash at 0, 8-aligned");
        assert_eq!(bytes[8..10], 0x0102u16.to_le_bytes(), "lib_version at 8");
        assert_eq!(bytes[16..18], 0x1112u16.to_le_bytes(), "lib_file_version at 16");
        assert_eq!(bytes[24..26], 0xABCDu16.to_le_bytes(), "abi at 24, 2-aligned");
        assert_eq!(bytes[26..28], [0, 0], "caps at 26, reserved");
        assert_eq!(bytes[28], msg::tier::PYTHON, "tier at 28");
        assert_eq!(bytes[29..32], [0, 0, 0], "and three reserved bytes to the boundary");
    }

    /// A NONZERO reserved field is REFUSED, both of them.
    ///
    /// Tolerating one is the same as never having reserved the bytes: a later firmware puts meaning
    /// there and every host built before it reads the record as though nothing had changed. The
    /// refusal is what makes the bytes claimable later.
    #[test]
    fn a_nonzero_reserved_field_is_refused_rather_than_ignored() {
        let surface = Surface { tier: msg::tier::CIL, abi: 1, ..Surface::default() };
        let mut bytes = Vec::new();
        surface.encode_into(&mut bytes);
        assert!(Surface::decode_from(&bytes).is_some(), "the all-zero reserved form decodes");

        let mut caps_set = bytes.clone();
        caps_set[26] = 1;
        assert_eq!(Surface::decode_from(&caps_set), None, "a per-surface caps claim is refused");

        for at in 29..32 {
            let mut padded = bytes.clone();
            padded[at] = 0xFF;
            assert_eq!(Surface::decode_from(&padded), None, "byte {at} is reserved and must be zero");
        }
    }

    /// A board carries the identity it was FLASHED with, so both eras have to be findable.
    ///
    /// The failure this guards is not a wrong answer, it is an ABSENCE: a scan matching only the
    /// current vendor id does not report an older board as older, it does not report it at all --
    /// and a board nobody lists is a board nobody reprograms.
    #[test]
    fn a_link_is_recognized_under_either_vendor_id_it_has_ever_had() {
        use usb::{LEGACY_VID, LinkIdentity, PID, VID, identify};
        assert_eq!(identify(VID, PID), Some(LinkIdentity::Current));
        assert_eq!(identify(LEGACY_VID, PID), Some(LinkIdentity::Legacy));
        assert_ne!(VID, LEGACY_VID, "two eras, or this test proves nothing");
        assert_eq!(identify(LEGACY_VID, PID + 1), None, "another device under the shared id");
        assert_eq!(identify(VID + 1, PID), None, "another vendor entirely");
    }

    #[test]
    fn profile_manifest_round_trips_and_fails_loud_on_damage() {
        let manifest = ProfileManifest {
            identity: an_identity(),
            surface: surface::NETFX_1_1 | surface::NETFX_2_0 | surface::FLOAT | surface::GENERICS,
            name: "kernel-floor".into(),
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
        assert_eq!(
            ProfileManifest::decode(&[2]),
            None,
            "and so is any OTHER version, whose bytes describe a different identity shape"
        );
    }

    /// The profile name lives here precisely because it has no cap, so a name past any cap a fixed
    /// identity would have imposed must survive whole.
    #[test]
    fn the_profile_name_is_not_capped_in_the_manifest() {
        let long = "a-profile-name-far-longer-than-any-fixed-identity-field-would-have-carried";
        let manifest = ProfileManifest {
            identity: TargetIdentity::default(),
            surface: 0,
            name: long.into(),
            intrinsic_ids: Vec::new(),
        };
        let back = ProfileManifest::decode(&manifest.encode()).expect("decodes");
        assert_eq!(back.name, long);
    }

    /// Every top-level message type declared in the type space, scraped from the source rather than
    /// listed here, as `(name, byte)`.
    ///
    /// A message type sits at file top level and a payload enumeration sits indented inside a
    /// module, which is what the indentation test distinguishes: the two spaces legitimately reuse
    /// small numbers, and folding them together would report collisions that are not collisions.
    fn declared_message_types() -> Vec<(&'static str, u8)> {
        let mut found = Vec::new();
        for line in include_str!("msg.rs").lines() {
            let Some(rest) = line.strip_prefix("pub const ") else { continue };
            let Some((name, value)) = rest.split_once(": u8 = ") else { continue };
            let literal = value.split_once(';').expect("a terminating semicolon").0.trim();
            let byte = literal
                .strip_prefix("0x")
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
                .unwrap_or_else(|| panic!("{name} = {literal}: a message type is a hexadecimal byte"));
            found.push((name, byte));
        }
        found
    }

    /// A HOST AND A TARGET THAT SHARE NO VERSION PRODUCE AN ANSWER, AND THE ANSWER NAMES THE RANGE.
    ///
    /// This is the protocol's one deliberate incompatibility signal. The `HELLO_NAK` carries the
    /// target's own range so a host can say WHICH END has to move -- a fact that is useless if it is
    /// not carried all the way to a person, which is what `TransportError::VersionMismatch` is for.
    #[test]
    fn no_shared_version_is_a_nak_that_names_the_target_range() {
        let host = Hello { range: ProtocolRange { min: 4, max: 6 }, caps: Capabilities::default() };
        let target = ProtocolRange { min: 1, max: 1 };
        let nak = target_respond(&host, target, Capabilities::default(), an_identity(), None)
            .expect_err("4..6 and 1..1 do not overlap");
        assert_eq!(nak.target_range, target, "the refusal names what the target CAN speak");

        let back = HelloNak::decode(&nak.encode()).expect("a NAK decodes");
        assert_eq!(back.target_range, target);
    }

    /// AND THE OVERLAPPING CASE STILL NEGOTIATES, so the test above is about disjoint ranges rather
    /// than about a handshake that stopped working.
    #[test]
    fn an_overlapping_range_still_chooses_the_highest_common_version() {
        let host = Hello { range: ProtocolRange { min: 1, max: 3 }, caps: Capabilities::default() };
        let ack = target_respond(
            &host,
            ProtocolRange { min: 2, max: 5 },
            Capabilities::default(),
            an_identity(),
            None,
        )
        .expect("2..3 overlaps");
        assert_eq!(ack.chosen, 3, "the highest both can speak");
    }

    /// Every architecture declared in `arch.rs`, scraped from the source rather than listed here,
    /// as `(name, value)`.
    fn declared_arch_values() -> Vec<(&'static str, u16)> {
        let mut found = Vec::new();
        for line in include_str!("arch.rs").lines() {
            let Some(rest) = line.strip_prefix("pub const ") else { continue };
            let Some((name, value)) = rest.split_once(": u16 = ") else { continue };
            let literal = value.split_once(';').expect("a terminating semicolon").0.trim();
            let code: u16 = literal
                .parse()
                .unwrap_or_else(|_| panic!("{name} = {literal}: an arch code is a decimal u16"));
            found.push((name, code));
        }
        found
    }

    /// AN ARCHITECTURE IS DECLARED IN THREE PLACES AND ALL THREE MUST CARRY IT: the constant, the
    /// display table, and the target-triple table.
    ///
    /// This is [`arch`]'s own documented hazard with nothing checking it. That module says a typo in
    /// a triple "is a target that silently reports `UNKNOWN`" -- and the same is true of an omission,
    /// which is the likelier mistake: a value added to the constants and to one table reads as done
    /// at every site a reviewer looks at. `UNKNOWN` is the one value that must NOT appear in either
    /// table, because it is the answer for a target that did not say.
    #[test]
    fn every_architecture_is_declared_in_all_three_places() {
        for (name, code) in declared_arch_values() {
            if code == arch::UNKNOWN {
                assert!(
                    arch::name(code).is_none(),
                    "UNKNOWN must not be nameable -- it is the absence of an answer",
                );
                continue;
            }
            assert!(
                arch::NAMED.iter().any(|(value, _)| *value == code),
                "{name} ({code}) is declared but NAMED does not carry it",
            );
            let triple = arch::TARGET_TRIPLES.iter().find(|(value, _)| *value == code);
            let (_, triple) = triple
                .unwrap_or_else(|| panic!("{name} ({code}) is declared but TARGET_TRIPLES does not carry it"));
            assert_eq!(
                arch::from_target_triple(triple),
                code,
                "{name}: the triple {triple} does not resolve back to it",
            );
        }
    }

    /// NEITHER TABLE HAS A ROW NOTHING DECLARES, and no two architectures share a code.
    ///
    /// The mirror of the test above: that one catches a value missing from a table, this one catches
    /// a table row whose constant was renamed or deleted, and a collision -- which on an append-only
    /// wire registry is the mistake that cannot be undone once a board has shipped reporting it.
    #[test]
    fn the_arch_tables_declare_nothing_extra_and_no_code_is_claimed_twice() {
        let declared = declared_arch_values();
        for (code, label) in arch::NAMED {
            assert!(
                declared.iter().any(|(_, value)| value == code),
                "NAMED carries {label} ({code}) which no constant declares",
            );
        }
        for (code, triple) in arch::TARGET_TRIPLES {
            assert!(
                declared.iter().any(|(_, value)| value == code),
                "TARGET_TRIPLES carries {triple} ({code}) which no constant declares",
            );
        }
        for (index, (name, code)) in declared.iter().enumerate() {
            for (other, other_code) in &declared[index + 1..] {
                assert_ne!(code, other_code, "{name} and {other} both claim arch code {code}");
            }
        }
    }

    /// A HOST BUILD, AND ANY TRIPLE THIS TREE DOES NOT BUILD FOR, RESOLVES TO `UNKNOWN`.
    ///
    /// `UNKNOWN` is a value with a meaning -- *this target did not say* -- and the lookup must reach
    /// it by falling through rather than by matching anything. The near-miss is the case worth
    /// stating: `thumbv8m.base` and `thumbv8m.main` differ by four characters and are different
    /// machines, so a prefix-matching lookup would answer confidently and wrongly.
    #[test]
    fn an_unbuilt_triple_resolves_to_unknown_rather_than_a_near_neighbour() {
        for stranger in [
            "x86_64-pc-windows-msvc",
            "thumbv8m.base",
            "thumbv8m.base-none-eabihf",
            "thumbv6m",
            "",
        ] {
            assert_eq!(
                arch::from_target_triple(stranger),
                arch::UNKNOWN,
                "{stranger} is not a triple this tree builds for",
            );
        }
        assert_eq!(arch::from_target_triple("thumbv8m.base-none-eabi"), arch::THUMBV8M_BASE);
        assert_eq!(arch::from_target_triple("thumbv8m.main-none-eabi"), arch::THUMBV8M_MAIN);
    }

    /// NO TWO MESSAGE TYPES CLAIM ONE BYTE, and none claims a byte that can never be one.
    ///
    /// This is the failure the whole allocation is arranged to prevent, and it is invisible in a
    /// file of constant declarations: two ops on one byte do not fail loudly, they make a host and a
    /// target exchange nothing and time out, which reads as a broken cable.
    #[test]
    fn no_two_message_types_claim_the_same_byte() {
        let types = declared_message_types();
        assert!(
            types.len() >= 60,
            "only {} message types were read out of the source -- the reader is broken, not the table",
            types.len()
        );
        for (index, (name, byte)) in types.iter().enumerate() {
            assert!(msg::is_valid_type(*byte), "{name} claims {byte:#04x}, which is never a type");
            for (other, other_byte) in types.iter().skip(index + 1) {
                assert_ne!(byte, other_byte, "{name} and {other} both claim {byte:#04x}");
            }
        }
    }

    /// The enumeration and the declarations agree, in BOTH directions.
    ///
    /// A type declared and left out of the table is one no host can print and no gate above can see;
    /// a table row with no declaration behind it is a name for something that does not exist. The
    /// table is read by tools, so either half being wrong is a wrong answer somewhere else.
    #[test]
    fn the_message_table_names_exactly_what_the_source_declares() {
        let declared = declared_message_types();
        for (name, byte) in &declared {
            assert_eq!(
                msg::name(*byte),
                Some(*name),
                "{name} ({byte:#04x}) is declared but the table does not name it"
            );
        }
        assert_eq!(declared.len(), msg::ALL.len(), "the table has a row nothing declares");
    }

    /// Every allocated type sits inside a declared block, and the blocks do not overlap.
    ///
    /// A byte outside every block is one nobody would find by looking at the map, which is how a
    /// second op gets minted onto it later.
    #[test]
    fn every_message_type_sits_in_exactly_one_block() {
        for window in msg::BLOCKS.windows(2) {
            let ((_, _, prev_last), (name, first, _)) = (window[0], window[1]);
            assert!(prev_last < first, "{name} starts inside the block before it");
        }
        for (byte, name) in msg::ALL {
            let homes = msg::BLOCKS.iter().filter(|(_, first, last)| byte >= first && byte <= last).count();
            assert_eq!(homes, 1, "{name} ({byte:#04x}) must sit in exactly one block, not {homes}");
        }
    }

    /// THE MIRROR: a deploy op is its load op plus 0x10, for every artifact and for the discard.
    ///
    /// It is what makes adding a language ONE number in each half at the same offset, and a reader
    /// can convert between the two halves by arithmetic rather than by a table. Stated in the map
    /// and true of nothing unless something checks it -- the pairs sit sixteen bytes apart in the
    /// source, which is exactly far enough that an eye slides over a mismatch.
    #[test]
    fn a_deploy_op_is_its_load_op_plus_the_block_offset() {
        const MIRROR: u8 = 0x10;
        let pairs = [
            (msg::LOAD_PE, msg::DEPLOY_PE, "PE"),
            (msg::LOAD_IMAGE, msg::DEPLOY_IMAGE, "IMAGE"),
            (msg::LOAD_BUNDLE, msg::DEPLOY_BUNDLE, "BUNDLE"),
            (msg::LOAD_JS, msg::DEPLOY_JS, "JS"),
            (msg::LOAD_CLEAR, msg::DEPLOY_CLEAR, "CLEAR"),
        ];
        for (load, deploy, name) in pairs {
            assert_eq!(deploy, load + MIRROR, "{name}: the deploy op is not its load op mirrored");
        }
        assert!(
            msg::XFER_RESULT + MIRROR != msg::DEPLOY_STATUS,
            "the shared controls are not part of the mirror"
        );
    }

    /// The two bytes that are never message types are refused at BOTH ends: an encoder will not
    /// produce one, and a reader steps over one rather than waiting for the payload it claims.
    ///
    /// The waiting is the part worth a test. Both bytes are what unwritten memory reads as, so a run
    /// of erased flash on a carrier declares a payload of up to 65,535 bytes -- and a reader that
    /// waited for it would swallow every real frame behind it into the same buffer.
    #[test]
    fn the_two_impossible_type_bytes_are_refused_at_both_ends() {
        assert_eq!(encode_frame(0x00, 1, &[]), None, "zeroed RAM is not a message");
        assert_eq!(encode_frame(0xFF, 1, &[]), None, "erased flash is not a message");

        let good = encode_frame(msg::PING, 9, &[0x11]).expect("a 1-byte payload frames");
        for impossible in [0x00u8, 0xFF] {
            let mut stream = alloc::vec![SYNC[0], SYNC[1]];
            stream.extend_from_slice(&60_000u16.to_le_bytes());
            stream.extend_from_slice(&[impossible, 0, 0]);
            let mut reader = FrameReader::new();
            reader.push(&stream);
            reader.push(&good);
            let frame = reader
                .next_frame()
                .expect("the real frame is reachable without waiting for a payload nothing will send");
            assert_eq!(frame.seq, 9, "type {impossible:#04x} was stepped over, not waited for");
        }
    }

    /// THE STALL A BOUND EXISTS FOR. A header claiming a payload the reader will never be sent must
    /// be discarded AT ONCE -- not waited for, and not waited for while every real frame behind it
    /// is swallowed into the same buffer.
    #[test]
    fn a_bounded_reader_refuses_an_unbelievable_length_without_waiting_for_it() {
        let mut stream = alloc::vec![SYNC[0], SYNC[1]];
        stream.extend_from_slice(&60_000u16.to_le_bytes());
        stream.extend_from_slice(&[msg::PING, 0, 0]);
        let good = encode_frame(msg::PING, 7, &[0x11]).expect("a 1-byte payload frames");

        let mut reader = FrameReader::with_max_payload(64);
        reader.push(&stream);
        reader.push(&good);
        let frame = reader.next_frame().expect("the real frame is reachable immediately");
        assert_eq!(frame.seq, 7, "the bogus header was stepped over, not waited for");
        assert_eq!(frame.payload, vec![0x11]);
    }

    /// THE CONTROL FOR IT. An unbounded reader is unchanged, which is correct for a host that really
    /// can be sent a 60,000-byte frame. If this ever starts behaving like the bounded case, the
    /// bound has leaked into the default and every deploy would break.
    #[test]
    fn the_default_reader_still_waits_for_a_long_frame() {
        let mut stream = alloc::vec![SYNC[0], SYNC[1]];
        stream.extend_from_slice(&60_000u16.to_le_bytes());
        stream.extend_from_slice(&[msg::PING, 0, 0]);
        let good = encode_frame(msg::PING, 7, &[0x11]).expect("a 1-byte payload frames");

        let mut reader = FrameReader::new();
        reader.push(&stream);
        reader.push(&good);
        assert!(
            reader.next_frame().is_none(),
            "an unbounded reader waits for the 60,000 bytes it was promised"
        );
    }

    /// The boundary, stated rather than left to a reader of the comparison: a payload exactly at the
    /// bound is a frame, and one byte more is garbage.
    ///
    /// There is deliberately no arm here for a bound ABOVE the wire's own cap. It cannot be
    /// observed: the length field is a `u16`, so no header reaches such a bound, and a test asserting
    /// that a frame still decodes would pass whatever the constructor did with the number. A guard
    /// that cannot go red is not a guard, and one written anyway would report a property nothing
    /// holds.
    #[test]
    fn the_bound_is_inclusive() {
        let payload = [0xA5u8; 16];
        let framed = encode_frame(msg::PING, 3, &payload).expect("16 bytes frames");

        let mut exact = FrameReader::with_max_payload(16);
        exact.push(&framed);
        assert_eq!(exact.next_frame().map(|f| f.seq), Some(3), "16 <= 16 is accepted");

        let mut tight = FrameReader::with_max_payload(15);
        tight.push(&framed);
        assert!(tight.next_frame().is_none(), "16 > 15 is discarded");
    }

    /// The surface bitmap: one distinct bit per symbol, and the era bits are exactly the era mask.
    #[test]
    fn the_surface_bitmap_is_one_distinct_bit_per_symbol() {
        let mut seen = 0u64;
        for (bit, symbol) in surface::NAMED {
            assert_eq!(bit.count_ones(), 1, "{symbol} must be exactly one bit");
            assert_eq!(seen & bit, 0, "{symbol} duplicates a bit already named");
            seen |= bit;
        }
        assert_eq!(
            surface::NETFX_MASK.count_ones(),
            4,
            "the era mask is the four era bits and nothing else"
        );
        assert_eq!(surface::NETFX_MASK & !seen, 0, "and every one of them is a named symbol");
    }

    /// The subset check, in the direction that decides whether a program will run.
    #[test]
    fn a_board_missing_a_symbol_is_named_rather_than_merely_refused() {
        let program = surface::FLOAT | surface::GENERICS | surface::NETFX_2_0;
        let board = surface::FLOAT | surface::NETFX_2_0;
        assert!(!surface::accepts(program, board));
        assert_eq!(surface::missing(program, board), surface::GENERICS);
        assert_eq!(surface::bit_of("LAMELLA_SURFACE_GENERICS"), Some(surface::GENERICS));
        assert!(surface::accepts(board, program), "a program needing less runs on a board with more");
        assert_eq!(surface::missing(program, program), 0);
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
        let ack = target_respond(&received, target_range, target_caps, an_identity(), None)
            .expect("a compatible version");
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
        let err = target_respond(
            &host,
            ProtocolRange::single(1),
            Capabilities::default(),
            TargetIdentity::default(),
            None,
        );
        assert_eq!(err, Err(HelloNak { target_range: ProtocolRange::single(1) }));
    }
}

