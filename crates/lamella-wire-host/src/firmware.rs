//! The host half of the DEVICE / FIRMWARE block (`0x90`-`0x9F`): update the thing that serves this
//! protocol, over the protocol.

use std::time::{Duration, Instant};

use lamella_wire::{Frame, error, msg};

use crate::{Transport, TransportError};

/// A message type's own name, from the wire crate's allocation table.
///
/// **READ OUT OF [`msg::ALL`] RATHER THAN TRANSCRIBED**, because a second list of these names would
/// be a second thing to keep in step with a byte allocation that just moved wholesale. A byte absent
/// from the table is unallocated, which is itself worth printing plainly.
fn name_of(msg_type: u8) -> &'static str {
    msg::ALL
        .iter()
        .find(|(byte, _)| *byte == msg_type)
        .map_or("an unallocated message type", |(_, name)| *name)
}

/// Why a firmware op did not produce its answer.
///
/// **A REFUSAL IS NOT A TRANSPORT FAILURE, AND COLLAPSING THEM COSTS THE READER THE REMEDY.** A
/// target that does not implement an op, a target whose debug session another carrier holds, and a
/// target that has stopped answering are three different situations with three different next
/// steps -- rebuild the firmware, ask a colleague, check the cable -- and a single error type would
/// make them one sentence.
#[derive(Debug)]
pub enum FirmwareError {
    /// The target refused the op as one it does not implement. Carries the type byte it refused,
    /// which is what lets a caller name the op rather than the number.
    Unsupported(u8),
    /// Another carrier holds the session, so the request was well formed and the answer will change
    /// when that carrier lets go. Carries the holding carrier's channel class.
    SessionHeld(u8),
    /// The reply arrived but was shorter than the field it must carry.
    Malformed(&'static str),
    /// The carrier failed, or nothing answered inside the timeout.
    Transport(TransportError),
}

impl std::fmt::Display for FirmwareError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FirmwareError::Unsupported(msg_type) => write!(
                f,
                "this target does not implement {} ({msg_type:#04x}) -- its firmware was built \
                 without the device/firmware block",
                name_of(*msg_type)
            ),
            FirmwareError::SessionHeld(holder) => write!(
                f,
                "another carrier (class {holder:#04x}) holds this target's session; the request is \
                 well formed and will be answered once that carrier lets go"
            ),
            FirmwareError::Malformed(what) => write!(f, "the target's reply carried no {what}"),
            FirmwareError::Transport(inner) => write!(f, "{inner:?}"),
        }
    }
}

impl From<TransportError> for FirmwareError {
    fn from(inner: TransportError) -> Self {
        FirmwareError::Transport(inner)
    }
}

/// What firmware a target has installed -- the answer to [`msg::FW_STATUS`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FwStatus {
    /// One of [`msg::fw_state`].
    pub state: u8,
    /// The size of the region an image occupies.
    pub region_size: u32,
    /// The image's CRC as the target holds it.
    pub image_crc: u32,
    /// Which signing key verified it, or zero.
    pub key_id: u32,
    /// The part's program granule in bytes, or zero where the target does not report one.
    ///
    /// **A HOST CANNOT DERIVE THIS AND MUST NOT GUESS IT.** See [`chunk_len`], which is the only
    /// thing that should read it.
    pub write_unit: u32,
    /// How many bytes of an unfinished transfer the target is holding, for resuming one.
    ///
    /// **Zero is also what a target that cannot resume reports**, and the two are deliberately the
    /// same answer: both mean "start from the beginning", which is the only safe reading. A target
    /// offering resume proves it by reporting a count AND a running checksum the host can check its
    /// own prefix against -- resume is offered, never assumed.
    pub written: u32,
}

impl FwStatus {
    /// The state as a sentence, so a caller prints a reason rather than a number.
    #[must_use]
    pub fn state_text(&self) -> &'static str {
        match self.state {
            msg::fw_state::NONE => "nothing installed",
            msg::fw_state::VERIFIED => "installed and verified",
            msg::fw_state::UNVERIFIABLE => "installed but could not be verified",
            _ => "an installed state this host does not know",
        }
    }
}

/// What a [`msg::FW_ACTIVATE`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FwActivation {
    /// One of [`msg::fw_activate_status`].
    pub status: u8,
    /// The slot running right now -- unchanged by an activation, which takes effect at next boot.
    pub active_slot: u8,
    /// The slot that will boot next.
    pub next_slot: u8,
}

impl FwActivation {
    /// Whether the target accepted the request.
    #[must_use]
    pub fn accepted(&self) -> bool {
        matches!(
            self.status,
            msg::fw_activate_status::ACTIVATED | msg::fw_activate_status::ACTIVATED_ONE_BOOT
        )
    }

    /// The outcome as a sentence a person can act on.
    ///
    /// **THE TWO DOWNGRADE REFUSALS ARE DELIBERATELY WORDED APART, AND THAT IS THE POINT OF
    /// RENDERING THIS RATHER THAN RETURNING A NUMBER.** One is a build that did not opt in; the
    /// other is silicon that cannot go back at all, because a monotonic counter or a fuse has
    /// already advanced and no capability undoes that. **One is a rebuild and the other is a
    /// different board**, so the two sentences must never be interchangeable.
    #[must_use]
    pub fn text(&self) -> &'static str {
        match self.status {
            msg::fw_activate_status::ACTIVATED => "activated -- it takes effect at the next boot",
            msg::fw_activate_status::ACTIVATED_ONE_BOOT => {
                "activated for ONE boot; confirm it after that boot or it reverts"
            }
            msg::fw_activate_status::NO_SUCH_SLOT => "refused: there is no such slot",
            msg::fw_activate_status::SLOT_UNUSABLE => {
                "refused: that slot is empty, or its image could not be verified"
            }
            msg::fw_activate_status::DOWNGRADE_REFUSED => {
                "refused: that would go BACKWARD, and this firmware was not built to permit it. \
                 Rebuild with the rollback capability compiled in and the same request will work"
            }
            msg::fw_activate_status::DOWNGRADE_IMPOSSIBLE => {
                "refused: going backward is not possible on this silicon. Its anti-rollback record \
                 is a counter or a fuse and has already advanced, so no rebuild can undo it -- \
                 this needs a different board"
            }
            _ => "refused, for a reason this host does not know",
        }
    }
}

/// Ask the target what firmware it has installed. Writes nothing.
///
/// # Errors
/// [`FirmwareError::Unsupported`] where the target has no firmware-update block; otherwise a
/// carrier failure or a timeout.
pub fn fw_status(
    transport: &mut impl Transport,
    seq: u16,
    timeout: Duration,
) -> Result<FwStatus, FirmwareError> {
    transport.send(msg::FW_STATUS, seq, &[])?;
    let payload = await_reply(transport, seq, msg::FW_STATUS_RESULT, msg::FW_STATUS, timeout)?;
    if payload.len() < 21 {
        return Err(FirmwareError::Malformed("a full firmware status"));
    }
    let word = |at: usize| u32::from_le_bytes(payload[at..at + 4].try_into().expect("four bytes"));
    Ok(FwStatus {
        state: payload[0],
        region_size: word(1),
        image_crc: word(5),
        key_id: word(9),
        write_unit: word(13),
        written: word(17),
    })
}

/// The largest chunk of at most `preferred` bytes that a part with this program granule can take,
/// or `None` when there is no such chunk.
///
/// # Why a host must be told the granule instead of choosing one
///
/// A part programs a whole granule at a time, so a chunk that does not start and end on one cannot
/// be written without rewriting bytes already present -- which most parts do not permit twice
/// between erases. **The granules that have actually been read out of manuals span 2 to 64 bytes,
/// and the widest belongs to a Cortex-M0+ rather than to any of the large parts.** So the value is
/// not guessable from the size or the age of the silicon, and a plausible default would be wrong on
/// the part least likely to be suspected.
///
/// `None` when the target reports no granule (`write_unit` of zero) or when even one granule
/// exceeds `preferred`. **Both are refusals rather than fallbacks**: the whole reason this is
/// reported is that guessing it corrupts a partly-programmed granule.
///
/// The final chunk of an image is whatever remains and need not be a whole number of granules --
/// padding the tail out to one belongs to the target, which knows what its erased byte is.
#[must_use]
pub fn chunk_len(write_unit: u32, preferred: usize) -> Option<usize> {
    let unit = usize::try_from(write_unit).ok().filter(|&unit| unit != 0)?;
    let chunk = (preferred / unit) * unit;
    if chunk == 0 { None } else { Some(chunk) }
}

/// What a [`fw_write`] chunk did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FwWrite {
    /// One of [`msg::fw_write_status`].
    pub status: u8,
    /// How many bytes of the chunk were programmed.
    pub accepted: u32,
    /// CRC32 of everything programmed so far, **read back out of flash**.
    ///
    /// See [`fw_write`] for why it covers the range rather than the chunk, and why it is taken from
    /// the flash rather than from the bytes that arrived.
    pub running_crc: u32,
}

/// What a [`fw_commit`] concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FwCommit {
    /// One of [`msg::fw_commit_status`].
    pub status: u8,
    /// The image's CRC as the target computed it over the whole written range.
    pub image_crc: u32,
}

/// Program one chunk of a firmware image at `offset` within the update region.
///
/// `offset` must be a multiple of the target's program granule and `chunk` should be sized with
/// [`chunk_len`]; a target refuses anything else rather than rewriting a partly-programmed granule.
///
/// # The checksum covers the RANGE SO FAR, and it is read back out of flash
///
/// Both halves of that were open questions and both go the same way, for the same reason: **the
/// cheaper alternative re-checks something that is already checked and leaves the thing that can
/// silently fail unchecked.**
///
/// - **The range, not the chunk.** A per-chunk checksum cannot see a chunk that was acknowledged
///   and landed at the wrong offset, or one that was never sent at all. A running one diverges at
///   the first chunk where the host and the target stop agreeing, which names where to look.
/// - **Read back from flash, not accumulated from the bytes that arrived.** The frame already
///   carries a checksum, so a receive-side one would verify the carrier twice and the flash never.
///   Flash is where a write is silently lost -- a granule that did not take, a region that was not
///   erased -- and reading it back is the only thing that can tell.
///
/// It costs the target a rolling checksum rather than a re-read: fold each chunk's read-back into
/// the running value as it goes, which is linear in the image rather than quadratic in the chunks.
///
/// # Errors
/// [`FirmwareError::Unsupported`] where the target has no firmware-update block; otherwise a
/// carrier failure or a timeout. **A REFUSAL BY THE TARGET IS NOT AN ERROR HERE** -- a misaligned
/// offset, a full region, a failed program -- it comes back in [`FwWrite::status`], because the
/// target understood the request and answered it.
pub fn fw_write(
    transport: &mut impl Transport,
    seq: u16,
    offset: u32,
    chunk: &[u8],
    timeout: Duration,
) -> Result<FwWrite, FirmwareError> {
    let mut payload = Vec::with_capacity(4 + chunk.len());
    payload.extend_from_slice(&offset.to_le_bytes());
    payload.extend_from_slice(chunk);
    transport.send(msg::FW_WRITE, seq, &payload)?;
    let reply = await_reply(transport, seq, msg::FW_RESULT, msg::FW_WRITE, timeout)?;
    if reply.len() < 9 {
        return Err(FirmwareError::Malformed("a status, a byte count and a running checksum"));
    }
    Ok(FwWrite {
        status: reply[0],
        accepted: u32::from_le_bytes(reply[1..5].try_into().expect("four bytes")),
        running_crc: u32::from_le_bytes(reply[5..9].try_into().expect("four bytes")),
    })
}

/// Finish a firmware transfer: the host states what it believes it wrote, and the target agrees or
/// refuses.
///
/// # The host asserts and the target checks, deliberately in that direction
///
/// **A target that reported its own checksum and a host that accepted it would verify nothing** --
/// the two would agree by construction, and a transfer that went wrong in a way affecting both
/// would be confirmed rather than caught. So the expectation travels host to target: `total_len`
/// and `expected_crc` are what the HOST computed over the bytes it sent, and the target's job is to
/// disagree when its flash says otherwise.
///
/// [`FwCommit::image_crc`] comes back so a mismatch can be reported with both numbers rather than
/// just the fact of one.
///
/// # Errors
/// [`FirmwareError::Unsupported`] where the target has no firmware-update block; otherwise a
/// carrier failure or a timeout. A refusal -- short, mismatched, rejected -- is a
/// [`FwCommit::status`] rather than an error, for the reason [`fw_write`] gives.
pub fn fw_commit(
    transport: &mut impl Transport,
    seq: u16,
    total_len: u32,
    expected_crc: u32,
    timeout: Duration,
) -> Result<FwCommit, FirmwareError> {
    let mut payload = [0u8; 8];
    payload[..4].copy_from_slice(&total_len.to_le_bytes());
    payload[4..].copy_from_slice(&expected_crc.to_le_bytes());
    transport.send(msg::FW_COMMIT, seq, &payload)?;
    let reply = await_reply(transport, seq, msg::FW_COMMIT_RESULT, msg::FW_COMMIT, timeout)?;
    if reply.len() < 5 {
        return Err(FirmwareError::Malformed("a status and the image checksum"));
    }
    Ok(FwCommit {
        status: reply[0],
        image_crc: u32::from_le_bytes(reply[1..5].try_into().expect("four bytes")),
    })
}

/// Choose which installed image boots next. Writes no firmware.
///
/// `slot` is [`msg::fw_slot::OTHER`] for the ordinary flip-try-flip-back loop, which is what lets a
/// host swap sides without knowing or tracking which one it is on. `intent` is
/// [`msg::fw_intent::PERMANENT`] or [`msg::fw_intent::ONE_BOOT`].
///
/// **IT TAKES EFFECT AT THE NEXT BOOT AND DOES NOT RESET THE BOARD.** One op, one effect: the
/// running image keeps running, and asking for a reset is a separate thing the existing paths
/// already do. A combined "activate and reboot" would make the answer arrive from a board that was
/// already on its way down, which is the shape that makes a failure impossible to attribute.
///
/// # Errors
/// [`FirmwareError::Unsupported`] where the target has no firmware-update block. A REFUSAL by the
/// target -- no such slot, an unusable slot, a downgrade it will not perform -- is NOT an error
/// here: it comes back in [`FwActivation::status`], because the target answered the question.
pub fn fw_activate(
    transport: &mut impl Transport,
    seq: u16,
    slot: u8,
    intent: u8,
    timeout: Duration,
) -> Result<FwActivation, FirmwareError> {
    transport.send(msg::FW_ACTIVATE, seq, &[slot, intent])?;
    let payload = await_reply(transport, seq, msg::FW_ACTIVATE_RESULT, msg::FW_ACTIVATE, timeout)?;
    if payload.len() < 3 {
        return Err(FirmwareError::Malformed("a status and both slot numbers"));
    }
    Ok(FwActivation { status: payload[0], active_slot: payload[1], next_slot: payload[2] })
}

/// Ask the target to enter a bootloader.
///
/// **THE TWO ARE NOT INTERCHANGEABLE, AND THE DIFFERENCE IS WHETHER THIS PROTOCOL SURVIVES.**
/// [`msg::ENTER_SW_BOOTLOADER`] stays on the same transport, so the session continues and the
/// update finishes over it. [`msg::ENTER_HW_BOOTLOADER`] hands the device to the silicon vendor's
/// own loader, which CHANGES ITS USB CLASS -- mass storage, or DFU -- and at that point this
/// protocol is gone and the host must finish over whatever the vendor's bootloader speaks.
///
/// **So there is no reply to wait for on the hardware route**, and pretending otherwise would spend
/// a timeout every call. `which` decides whether this waits at all.
///
/// # Errors
/// [`FirmwareError::Unsupported`] where the target implements no such bootloader entry.
pub fn enter_bootloader(
    transport: &mut impl Transport,
    seq: u16,
    which: Bootloader,
    timeout: Duration,
) -> Result<(), FirmwareError> {
    let op = match which {
        Bootloader::Software => msg::ENTER_SW_BOOTLOADER,
        Bootloader::Hardware => msg::ENTER_HW_BOOTLOADER,
    };
    transport.send(op, seq, &[])?;
    if matches!(which, Bootloader::Hardware) {
        return Ok(());
    }
    match await_refusal(transport, seq, op, timeout) {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// Which bootloader [`enter_bootloader`] asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bootloader {
    /// The installed Lamella bootloader. Stays on this transport.
    Software,
    /// The silicon vendor's own. The device changes class and this protocol ends.
    Hardware,
}

/// Waits for `expect` at `seq`, turning a refusal into a named error rather than a timeout.
fn await_reply(
    transport: &mut impl Transport,
    seq: u16,
    expect: u8,
    asked: u8,
    timeout: Duration,
) -> Result<Vec<u8>, FirmwareError> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        while let Some(Frame { msg_type, seq: reply_seq, payload }) = transport.poll()? {
            if reply_seq != seq {
                continue;
            }
            if msg_type == expect {
                return Ok(payload);
            }
            if msg_type == msg::ERROR {
                return Err(refusal(&payload, asked));
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    Err(FirmwareError::Transport(TransportError::Closed))
}

/// Watches only for a refusal of `asked`, for ops whose success is silence.
fn await_refusal(
    transport: &mut impl Transport,
    seq: u16,
    asked: u8,
    timeout: Duration,
) -> Option<FirmwareError> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match transport.poll() {
            Ok(Some(Frame { msg_type, seq: reply_seq, payload })) => {
                if reply_seq == seq && msg_type == msg::ERROR {
                    return Some(refusal(&payload, asked));
                }
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(2)),
            Err(inner) => return Some(FirmwareError::Transport(inner)),
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use lamella_wire::MemTransport;

    /// A transport already holding `reply` as though the target had sent it.
    fn answered(msg_type: u8, seq: u16, payload: &[u8]) -> MemTransport {
        let mut transport = MemTransport::new();
        let mut encoder = MemTransport::new();
        encoder.send(msg_type, seq, payload).expect("encode a frame");
        let bytes = encoder.take_sent();
        transport.feed(&bytes);
        transport
    }

    const SOON: Duration = Duration::from_millis(200);

    #[test]
    fn a_status_reply_is_read_field_by_field() {
        let mut payload = vec![msg::fw_state::VERIFIED];
        for word in [0x0002_0000u32, 0xDEAD_BEEF, 7, 64, 2048] {
            payload.extend_from_slice(&word.to_le_bytes());
        }
        let mut transport = answered(msg::FW_STATUS_RESULT, 1, &payload);
        let status = fw_status(&mut transport, 1, SOON).expect("a status");
        assert_eq!(status.region_size, 0x0002_0000);
        assert_eq!(status.image_crc, 0xDEAD_BEEF);
        assert_eq!(status.key_id, 7);
        assert_eq!(status.write_unit, 64);
        assert_eq!(status.written, 2048);
        assert_eq!(status.state_text(), "installed and verified");
    }

    #[test]
    fn a_status_that_stops_before_the_granule_is_not_a_status() {
        let mut payload = vec![msg::fw_state::VERIFIED];
        for word in [0x0002_0000u32, 0xDEAD_BEEF, 7] {
            payload.extend_from_slice(&word.to_le_bytes());
        }
        assert_eq!(payload.len(), 13, "the shape this record had before it grew");
        let mut transport = answered(msg::FW_STATUS_RESULT, 1, &payload);
        assert!(
            matches!(fw_status(&mut transport, 1, SOON), Err(FirmwareError::Malformed(_))),
            "a short status is refused by name"
        );
    }

    #[test]
    fn a_target_that_does_not_implement_the_op_is_named_rather_than_timed_out() {
        let refused = error::unknown_message_type(msg::FW_ACTIVATE);
        let mut transport = answered(msg::ERROR, 4, &refused);
        let started = Instant::now();
        let error = fw_activate(&mut transport, 4, msg::fw_slot::OTHER, msg::fw_intent::PERMANENT, SOON)
            .expect_err("a refusal is not a status");
        assert!(matches!(error, FirmwareError::Unsupported(msg::FW_ACTIVATE)));
        assert!(error.to_string().contains("FW_ACTIVATE"), "it names the op: {error}");
        assert!(started.elapsed() < SOON, "a refusal must not cost the timeout");
    }

    #[test]
    fn a_slot_refusal_is_an_answer_and_not_an_error() {
        let mut transport =
            answered(msg::FW_ACTIVATE_RESULT, 2, &[msg::fw_activate_status::NO_SUCH_SLOT, 0, 0]);
        let result = fw_activate(&mut transport, 2, 9, msg::fw_intent::PERMANENT, SOON)
            .expect("the target answered, so this is not a call failure");
        assert!(!result.accepted());
        assert_eq!(result.text(), "refused: there is no such slot");
    }

    #[test]
    fn the_two_downgrade_refusals_never_read_the_same() {
        let mut seen: Vec<&str> = Vec::new();
        for status in 4u8..=5 {
            let mut transport = answered(msg::FW_ACTIVATE_RESULT, 3, &[status, 0, 0]);
            let result = fw_activate(&mut transport, 3, 0, msg::fw_intent::PERMANENT, SOON)
                .expect("an answered refusal");
            assert!(!result.accepted());
            seen.push(result.text());
        }
        assert_ne!(seen[0], seen[1], "a rebuild and a different board cannot share a sentence");
    }

    #[test]
    fn an_activation_sends_slot_and_intent_and_nothing_that_reboots() {
        let mut transport =
            answered(msg::FW_ACTIVATE_RESULT, 5, &[msg::fw_activate_status::ACTIVATED, 0, 1]);
        let _ = transport.take_sent();
        let result = fw_activate(&mut transport, 5, msg::fw_slot::OTHER, msg::fw_intent::ONE_BOOT, SOON)
            .expect("an activation");
        assert_eq!(result.active_slot, 0, "the running image is unchanged");
        assert_eq!(result.next_slot, 1);
        let sent = transport.take_sent();
        assert!(
            sent.windows(2).any(|w| w == [msg::fw_slot::OTHER, msg::fw_intent::ONE_BOOT]),
            "the payload is exactly slot then intent"
        );
    }

    #[test]
    fn entering_the_vendor_bootloader_does_not_wait_for_an_answer_that_cannot_come() {
        let mut transport = MemTransport::new();
        let started = Instant::now();
        enter_bootloader(&mut transport, 6, Bootloader::Hardware, Duration::from_secs(30))
            .expect("no reply is the expected outcome");
        assert!(started.elapsed() < Duration::from_secs(1), "it must not wait");
    }
}

/// Reads an `ERROR` payload into the refusal it names.
///
/// `asked` is the FALLBACK, not the answer: the payload carries which type was refused, and
/// trusting the local guess over the target's own word is how a host reports the wrong op when a
/// reply arrives for something else in flight.
fn refusal(payload: &[u8], asked: u8) -> FirmwareError {
    match payload.first().copied() {
        Some(error::SESSION_HELD) => {
            FirmwareError::SessionHeld(error::session_holder(payload).unwrap_or(0))
        }
        _ => FirmwareError::Unsupported(error::refused_message_type(payload).unwrap_or(asked)),
    }
}
