#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

//! The Espressif serial-bootloader protocol: what a host says over a serial line to program an
//! ESP32 part's flash, with no transport of its own.

extern crate alloc;

use alloc::vec::Vec;

/// The compressor the compressed write path needs, which the target's own ROM inflates.
pub mod deflate;
/// The digest the protocol's verification command answers with, computed host-side for comparison.
pub mod digest;
/// Driving one flash write from start to finish -- the command ORDER, over the primitives below.
pub mod session;

pub use digest::{md5, md5_hex};
pub use session::{Error, FlashParams, Session, Step};

/// The byte that begins and ends a frame on the wire.
const FRAME_DELIMITER: u8 = 0xC0;
/// The byte that introduces an escape sequence within a frame.
const ESCAPE: u8 = 0xDB;
/// Escaped forms: a literal delimiter is `ESCAPE, ESCAPED_DELIMITER`; a literal escape is
/// `ESCAPE, ESCAPED_ESCAPE`.
const ESCAPED_DELIMITER: u8 = 0xDC;
const ESCAPED_ESCAPE: u8 = 0xDD;

/// The checksum's seed. The checksum is that seed XOR-ed with every byte of the payload, and it is
/// meaningful only for the commands that carry data to be written; the rest may send zero.
const CHECKSUM_SEED: u8 = 0xEF;

/// Wraps `payload` in one wire frame.
///
/// **The escaping happens last, and that ordering is a specification requirement rather than an
/// implementation detail.** A frame's declared length and its checksum both describe the payload
/// BEFORE escaping, so a frame's encoded size may exceed the length it declares. Computing either
/// over the escaped bytes yields a frame the target rejects -- and rejects with a checksum error,
/// which points at the checksum rather than at the escaping that actually caused it.
#[must_use]
pub fn frame(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 2);
    out.push(FRAME_DELIMITER);
    for &byte in payload {
        match byte {
            FRAME_DELIMITER => out.extend_from_slice(&[ESCAPE, ESCAPED_DELIMITER]),
            ESCAPE => out.extend_from_slice(&[ESCAPE, ESCAPED_ESCAPE]),
            other => out.push(other),
        }
    }
    out.push(FRAME_DELIMITER);
    out
}

/// Removes the escaping from one frame's contents (the bytes between its delimiters).
///
/// A trailing lone escape, and an escape followed by anything the specification does not define, are
/// passed through as themselves rather than dropped or rejected. This decoder is reading a line that
/// may carry a target's boot chatter alongside protocol frames, and a decoder that discarded bytes it
/// did not expect would turn a framing question into a silent content change.
#[must_use]
pub fn unescape(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len());
    let mut rest = body.iter().copied();
    while let Some(byte) = rest.next() {
        if byte != ESCAPE {
            out.push(byte);
            continue;
        }
        match rest.next() {
            Some(ESCAPED_DELIMITER) => out.push(FRAME_DELIMITER),
            Some(ESCAPED_ESCAPE) => out.push(ESCAPE),
            Some(other) => {
                out.push(ESCAPE);
                out.push(other);
            }
            None => out.push(ESCAPE),
        }
    }
    out
}

/// The checksum a data-bearing command carries: [`CHECKSUM_SEED`] XOR every byte of `data`.
#[must_use]
pub fn checksum(data: &[u8]) -> u8 {
    data.iter().fold(CHECKSUM_SEED, |sum, &byte| sum ^ byte)
}

/// Accumulates bytes arriving on the line and hands back whole frames.
///
/// # Why this cannot just split on the delimiter
///
/// The line is not a clean channel carrying only frames. A target that has just been reset narrates
/// its boot over the same wire, and a part whose firmware writes diagnostics writes them here too, so
/// bytes arrive that belong to no frame at all. Anything outside a pair of delimiters is therefore
/// DISCARDED rather than treated as a frame or as an error -- resynchronizing is normal operation on
/// this line, not a fault to report.
///
/// # The boundary that breaks a naive reader
///
/// Bytes arrive in whatever chunks the host's read happens to produce, and those boundaries fall
/// anywhere -- including between an escape byte and the byte it escapes. A reader that unescaped as it
/// consumed would have to carry that half-finished pair across the gap, and forgetting to is a bug
/// that appears only when a payload's reserved byte lands at a chunk boundary: rare, data-dependent,
/// and invisible in a test that feeds whole frames. So this reader does not unescape while it reads.
/// It collects a frame's raw body, and unescaping happens once, on the complete body, where no
/// boundary exists.
#[derive(Debug, Default)]
pub struct FrameReader {
    /// The current frame's raw bytes, once a start delimiter has been seen.
    body: Vec<u8>,
    /// Whether a start delimiter has been seen and not yet closed.
    inside: bool,
}

impl FrameReader {
    /// A reader with nothing buffered.
    #[must_use]
    pub fn new() -> FrameReader {
        FrameReader { body: Vec::new(), inside: false }
    }

    /// Accepts bytes from the line and returns every complete frame's UNESCAPED contents, in order.
    ///
    /// An empty frame -- two delimiters with nothing between them -- is not returned. Back-to-back
    /// delimiters are how an idle line and a frame boundary both look, so treating that as a frame
    /// would manufacture packets out of quiet.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        let mut frames = Vec::new();
        for &byte in bytes {
            if byte == FRAME_DELIMITER {
                if self.inside && !self.body.is_empty() {
                    frames.push(unescape(&self.body));
                }
                self.body.clear();
                self.inside = true;
                continue;
            }
            if self.inside {
                self.body.push(byte);
            }
        }
        frames
    }

    /// How many bytes of an unfinished frame are buffered. For a caller that wants to bound how much
    /// noise it will hold before giving up on a partial frame.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.body.len()
    }
}

/// One thing the host must do on the caller's behalf. This is the whole of this crate's contact with
/// the outside world: it emits these and is handed back whatever arrived.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Write these bytes to the port.
    Write(Vec<u8>),
    /// Set the data-terminal-ready control signal.
    ///
    /// Separate from [`Action::SetRts`] on purpose, and it is not a style choice: the two signals
    /// cannot be changed together. The part's own manual says so -- "most operating systems only allow
    /// setting or resetting DTR and RTS separately, but not in tandem" -- and the browser API that
    /// will carry this says the same of the platform calls underneath it. A combined action would
    /// promise an atomicity no host can deliver.
    SetDtr(bool),
    /// Set the request-to-send control signal.
    SetRts(bool),
    /// Wait this many milliseconds before continuing.
    Delay(u32),
    /// Close the port and reopen it at this baud rate. A serial line's rate cannot be changed while it
    /// is open -- true of the OS APIs and of the browser's, where the rate is fixed at open time.
    Reopen {
        /// The rate to reopen at.
        baud: u32,
    },
}

/// Which of a board's two possible serial connections the host is talking through.
///
/// This is not cosmetic: the two are DIFFERENT USB DEVICES with different wiring to the chip, so the
/// reset sequence, the signal policy, and even whether a reset re-samples the boot strapping pin all
/// differ between them. A flasher that assumed one would silently fail on the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Connector {
    /// The chip's own USB serial device. Its control signals are interpreted by the chip itself: one
    /// sets a download-mode flag, the other resets the core.
    ///
    /// Its limit is worth knowing before designing a user experience on it: the reset it can trigger
    /// does not re-sample the boot strapping pin, so a part put into download mode by other means
    /// cannot always be brought out of it from the host.
    ChipUsbSerial,
    /// A separate USB-to-UART bridge chip on the board, whose control signals are wired to the chip's
    /// reset and boot-strap pins. Its reset is a real one and does re-sample the strap, which makes it
    /// the more reliable of the two for entering and leaving download mode.
    UartBridge,
}

/// Where a reset should leave the part.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetInto {
    /// The serial bootloader, ready to accept commands.
    DownloadMode,
    /// Whatever is programmed in flash.
    Flash,
}

/// The control-signal sequence that resets the part, for the connector in use.
///
/// # For [`Connector::ChipUsbSerial`] this is transcribed from the part's manual, INCLUDING steps that
/// look redundant
///
/// The manual gives both sequences as step tables with an "internal state" column, and some steps do
/// not change that state: a step clears a signal that is already clear, or sets one already set. They
/// are not mistakes and they must not be optimized away. The manual's own note on those rows says
/// "Propagate DTR", and its prose says why: "some drivers (e.g., the standard CDC-ACM driver on
/// Windows) do not set DTR until RTS is set and the user needs to explicitly set RTS in order to
/// 'propagate' the DTR value."
///
/// So a redundant-looking write exists to make a HOST DRIVER flush a change it is sitting on. Anyone
/// deduplicating this sequence by its resulting state would remove exactly the steps that make it work
/// on the most common desktop platform, and the failure would appear as an intermittent inability to
/// enter download mode on one operating system only.
///
/// # For [`Connector::UartBridge`] the download-mode sequence is MEASURED, and the obvious derivation
/// from the wiring is WRONG
///
/// The vendor documents that a development board's bridge has its request-to-send wired to the part's
/// enable and its data-terminal-ready to the boot strap, both active low, but publishes no step table.
/// The sequence that follows naively from that description -- assert the strap, assert enable, release
/// enable, release the strap -- **does not work, and it fails silently: it produces no reset at all.**
///
/// The reason is a board fact the chip's documentation does not contain. The two signals do not reach
/// enable and the strap independently; they pass through an interlock, and **when BOTH are asserted
/// NEITHER line is pulled.** So the naive sequence's first two steps put the board in the
/// nothing-pulled state, and no edge on enable ever happens. A terminal program that asserts both
/// signals on open therefore does not reset the board, which is what the interlock is for.
///
/// What works, and what the sequence below does: assert enable ALONE first (so it really is pulled),
/// then -- while still in reset -- assert the strap, then release enable. The part samples the strap as
/// enable rises, and the strap is asserted at that instant.
///
/// Established by driving each candidate against an ESP32-C6 development board and reading the part's
/// own boot banner back, which names the mode it came up in:
///
/// ```text
///   assert enable alone, release              rst:0x1 (POWERON),boot:0x2c (SPI_FAST_FLASH_BOOT)
///   the naive strap-first shape               no output at all -- never reset
///   enable, then strap, then release enable   rst:0x1 (POWERON),boot:0x24 (DOWNLOAD(USB/UART0/...))
///                                             waiting for download
/// ```
///
/// The boot-mode byte corroborates it independently of the mode name: it differs in exactly the bit
/// that reports the strap pin's level.
///
/// The boot-into-flash sequence needed no correction -- asserting enable with the strap released was
/// already right, and the same measurement confirms it.
#[must_use]
pub fn reset_sequence(connector: Connector, into: ResetInto) -> Vec<Action> {
    match (connector, into) {
        (Connector::ChipUsbSerial, ResetInto::DownloadMode) => alloc::vec![
            Action::SetDtr(false),
            Action::SetRts(false),
            Action::SetDtr(true),
            Action::SetRts(false),
            Action::SetRts(true),
            Action::SetDtr(false),
            Action::SetRts(true),
            Action::SetRts(false),
        ],
        (Connector::ChipUsbSerial, ResetInto::Flash) => alloc::vec![
            Action::SetDtr(false),
            Action::SetRts(false),
            Action::SetRts(true),
            Action::SetRts(false),
        ],
        (Connector::UartBridge, ResetInto::DownloadMode) => alloc::vec![
            Action::SetDtr(false),
            Action::SetRts(true),
            Action::Delay(100),
            Action::SetDtr(true),
            Action::SetRts(false),
            Action::Delay(50),
            Action::SetDtr(false),
        ],
        (Connector::UartBridge, ResetInto::Flash) => alloc::vec![
            Action::SetDtr(false),
            Action::SetRts(true),
            Action::Delay(100),
            Action::SetRts(false),
        ],
    }
}

/// The commands this crate uses, by their protocol opcode.
///
/// Only the ones a stubless flasher needs are named. The opcodes the target's RAM loader adds are
/// deliberately absent (see the module docs) -- naming them would invite a caller to send one and get
/// the "unimplemented" status back from a ROM that never had them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Command {
    /// Begin a flash write: erase the target range and declare how the data will arrive.
    FlashBegin = 0x02,
    /// One block of flash data. Carries a meaningful checksum.
    FlashData = 0x03,
    /// End the flash write, and choose whether the target reboots or stays in the loader.
    FlashEnd = 0x04,
    /// Establish the byte stream. The only command whose payload is a fixed pattern.
    Sync = 0x08,
    /// Describe the attached flash chip's geometry to the loader.
    SpiSetParams = 0x0B,
    /// Attach the flash chip to the loader's SPI controller.
    SpiAttach = 0x0D,
    /// Change the line's baud rate. The line must be reopened afterwards.
    ChangeBaudRate = 0x0F,
    /// Begin a COMPRESSED flash write.
    FlashDeflBegin = 0x10,
    /// One block of compressed flash data. Carries a meaningful checksum.
    FlashDeflData = 0x11,
    /// End a compressed flash write.
    FlashDeflEnd = 0x12,
    /// Ask the target for a digest of a flash range, to verify what was written.
    SpiFlashMd5 = 0x13,
}

impl Command {
    /// Whether this command's checksum field is meaningful. For every other command the field may be
    /// zero -- and this crate still computes it, because a correct value is never wrong and a
    /// conditional would be one more thing to get wrong per command.
    #[must_use]
    pub const fn carries_data(self) -> bool {
        matches!(self, Command::FlashData | Command::FlashDeflData)
    }
}

/// The fixed payload of a [`Command::Sync`] request: a four-byte lead-in then thirty-two `0x55`.
pub const SYNC_PAYLOAD: [u8; 36] = {
    let mut payload = [0x55u8; 36];
    payload[0] = 0x07;
    payload[1] = 0x07;
    payload[2] = 0x12;
    payload[3] = 0x20;
    payload
};

/// The direction byte of a request, and of a response.
const DIRECTION_REQUEST: u8 = 0x00;
const DIRECTION_RESPONSE: u8 = 0x01;
/// Every packet carries an eight-byte header before its data.
const HEADER_LEN: usize = 8;

/// Builds one framed request packet: the direction byte, the opcode, the data length, the checksum,
/// then the data -- little-endian throughout, and framed by [`frame`].
#[must_use]
pub fn request(command: Command, data: &[u8]) -> Vec<u8> {
    let mut packet = Vec::with_capacity(HEADER_LEN + data.len());
    packet.push(DIRECTION_REQUEST);
    packet.push(command as u8);
    packet.extend_from_slice(&(data.len() as u16).to_le_bytes());
    packet.extend_from_slice(&u32::from(checksum(data)).to_le_bytes());
    packet.extend_from_slice(data);
    frame(&packet)
}

/// Builds one framed data-bearing request, where the checksum covers the DATA BLOCK ALONE.
///
/// # Why the checksum's extent is a separate concern from the payload's
///
/// A data command's payload is four descriptive words followed by the block being written, and **the
/// checksum is over the block, not over the payload that carries it.** Computing it over the whole
/// payload produces a frame the target rejects -- and rejects with its checksum error, which points at
/// the arithmetic rather than at the extent that is actually wrong. The two are impossible to tell
/// apart from the error alone, which is why this exists as its own function with the extent in its
/// name rather than as an argument to [`request`].
#[must_use]
pub fn data_request(command: Command, header: &[u8], block: &[u8]) -> Vec<u8> {
    let length = header.len() + block.len();
    let mut packet = Vec::with_capacity(HEADER_LEN + length);
    packet.push(DIRECTION_REQUEST);
    packet.push(command as u8);
    packet.extend_from_slice(&(length as u16).to_le_bytes());
    packet.extend_from_slice(&u32::from(checksum(block)).to_le_bytes());
    packet.extend_from_slice(header);
    packet.extend_from_slice(block);
    frame(&packet)
}

/// A decoded response packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    /// The opcode this answers.
    pub command: u8,
    /// The response's value word -- what a register read returns, and zero for most commands.
    pub value: u32,
    /// The response's data, with its trailing status bytes removed.
    pub data: Vec<u8>,
    /// Whether the target reported success.
    pub ok: bool,
    /// The target's error byte when it did not, else zero.
    pub error: u8,
}

/// How many trailing status bytes a target's responses carry.
///
/// This is a PARAMETER rather than a constant because it is not the same everywhere: the vendor
/// documents this protocol per chip and warns that it differs between chips, and the status length is
/// one of the places it does. Guessing it does not fail cleanly -- the status bytes come off the END
/// of the data, so a wrong length silently mis-slices every response that carries data, and a digest
/// read back for verification would compare unequal for a reason that has nothing to do with flash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusLen(pub usize);

impl StatusLen {
    /// The four-byte form: status, error, then two reserved bytes.
    pub const FOUR: StatusLen = StatusLen(4);
    /// The two-byte form: status then error.
    pub const TWO: StatusLen = StatusLen(2);
}

/// An unparseable response, named so a caller can say which way it was malformed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseError {
    /// Fewer bytes than a header, or fewer than the header plus the status bytes.
    Truncated,
    /// The first byte was not the response direction -- most likely a request echoed back, or the
    /// target's boot chatter mistaken for a frame.
    NotAResponse,
    /// The header's declared data length disagrees with the bytes present.
    LengthMismatch,
}

/// Decodes one response packet's contents (a frame body, already unescaped).
///
/// # Errors
/// [`ResponseError`] describing how the packet was malformed.
pub fn response(packet: &[u8], status_len: StatusLen) -> Result<Response, ResponseError> {
    if packet.len() < HEADER_LEN {
        return Err(ResponseError::Truncated);
    }
    if packet[0] != DIRECTION_RESPONSE {
        return Err(ResponseError::NotAResponse);
    }
    let declared = usize::from(u16::from_le_bytes([packet[2], packet[3]]));
    let value = u32::from_le_bytes([packet[4], packet[5], packet[6], packet[7]]);
    let body = &packet[HEADER_LEN..];
    if body.len() != declared {
        return Err(ResponseError::LengthMismatch);
    }
    let split = body.len().checked_sub(status_len.0).ok_or(ResponseError::Truncated)?;
    let (data, status) = body.split_at(split);
    Ok(Response {
        command: packet[1],
        value,
        data: data.to_vec(),
        ok: status[0] == 0,
        error: if status[0] == 0 { 0 } else { status[1] },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A payload with nothing to escape survives unchanged inside its delimiters.
    #[test]
    fn a_plain_payload_is_framed_between_delimiters() {
        assert_eq!(frame(&[0x01, 0x02, 0x03]), [0xC0, 0x01, 0x02, 0x03, 0xC0]);
        assert_eq!(frame(&[]), [0xC0, 0xC0]);
    }

    /// The two bytes that cannot appear literally inside a frame are the delimiter and the escape.
    #[test]
    fn the_two_reserved_bytes_are_escaped() {
        assert_eq!(frame(&[0xC0]), [0xC0, 0xDB, 0xDC, 0xC0]);
        assert_eq!(frame(&[0xDB]), [0xC0, 0xDB, 0xDD, 0xC0]);
        assert_eq!(frame(&[0xC0, 0xDB]), [0xC0, 0xDB, 0xDC, 0xDB, 0xDD, 0xC0]);
    }

    /// **The property the ordering rule exists for**: escaping makes a frame LONGER than the payload
    /// whose length and checksum it declares. A frame that escaped first and measured afterwards
    /// would declare this payload as four bytes instead of two.
    #[test]
    fn escaping_grows_the_frame_past_the_length_it_declares() {
        let payload = [0xC0, 0xDB];
        let framed = frame(&payload);
        assert_eq!(payload.len(), 2);
        assert_eq!(framed.len() - 2, 4, "the escaped body is longer than the payload");
    }

    /// Framing round-trips through unescaping, including on the reserved bytes.
    #[test]
    fn unescaping_inverts_framing() {
        for payload in [
            &[][..],
            &[0x00][..],
            &[0xC0][..],
            &[0xDB][..],
            &[0xDB, 0xDC][..],
            &[0xC0, 0xDB, 0xC0, 0xDB][..],
            &[0x07, 0x07, 0x12, 0x20][..],
        ] {
            let framed = frame(payload);
            let body = &framed[1..framed.len() - 1];
            assert_eq!(unescape(body), payload, "round trip of {payload:02X?}");
        }
    }

    /// An escape the specification does not define, and a truncated one, pass through as themselves.
    /// Asserted because the tempting alternative -- dropping them -- would silently alter a payload.
    #[test]
    fn an_undefined_escape_passes_through_rather_than_vanishing() {
        assert_eq!(unescape(&[0xDB, 0x41]), [0xDB, 0x41]);
        assert_eq!(unescape(&[0xDB]), [0xDB]);
        assert_eq!(unescape(&[0x41, 0xDB]), [0x41, 0xDB]);
    }

    /// The checksum of nothing is the seed, and it is an XOR fold so it is order-independent and
    /// self-inverting -- both worth pinning, because a sum or a CRC would pass the empty case too.
    #[test]
    fn the_checksum_is_a_seeded_xor_fold() {
        assert_eq!(checksum(&[]), 0xEF);
        assert_eq!(checksum(&[0x00]), 0xEF);
        assert_eq!(checksum(&[0xEF]), 0x00);
        assert_eq!(checksum(&[0x12, 0x34]), checksum(&[0x34, 0x12]));
        assert_eq!(checksum(&[0xA5, 0xA5]), 0xEF);
    }
}

#[cfg(test)]
mod packet_tests {
    use super::*;

    /// A request's header is direction, opcode, little-endian length, then the checksum as a word.
    #[test]
    fn a_request_header_is_direction_opcode_length_checksum() {
        let framed = request(Command::FlashData, &[0xAA, 0xBB]);
        let body = unescape(&framed[1..framed.len() - 1]);
        assert_eq!(body[0], 0x00, "direction: a request");
        assert_eq!(body[1], 0x03, "FlashData's opcode");
        assert_eq!(&body[2..4], &2u16.to_le_bytes(), "the DATA length, little-endian");
        assert_eq!(
            &body[4..8],
            &u32::from(checksum(&[0xAA, 0xBB])).to_le_bytes(),
            "the checksum occupies a whole word"
        );
        assert_eq!(&body[8..], &[0xAA, 0xBB]);
    }

    /// The sync payload is a fixed pattern: a four-byte lead-in then thirty-two `0x55`.
    #[test]
    fn the_sync_payload_is_the_fixed_pattern() {
        assert_eq!(SYNC_PAYLOAD.len(), 36);
        assert_eq!(&SYNC_PAYLOAD[..4], &[0x07, 0x07, 0x12, 0x20]);
        assert!(SYNC_PAYLOAD[4..].iter().all(|&b| b == 0x55), "thirty-two 0x55 follow");
    }

    /// **The framing rule this protocol makes easy to break.** `0xC0` and `0xDB` in a data block are
    /// escaped on the wire, but the length and checksum describe the UNESCAPED data -- so a sync
    /// payload's `0x55`s are inert, while a data block full of reserved bytes still declares its true
    /// length. Asserted on a payload made entirely of the two reserved bytes.
    #[test]
    fn length_and_checksum_describe_the_data_not_the_wire_bytes() {
        let data = [0xC0, 0xDB, 0xC0, 0xDB];
        let framed = request(Command::FlashData, &data);
        let body = unescape(&framed[1..framed.len() - 1]);
        assert_eq!(&body[2..4], &4u16.to_le_bytes(), "declares FOUR, its unescaped length");
        assert_eq!(&body[8..], &data);
        assert!(framed.len() > HEADER_LEN + data.len() + 2);
    }

    /// Only the data-bearing commands need a meaningful checksum, and this crate computes one anyway.
    #[test]
    fn the_data_bearing_commands_are_the_ones_that_need_a_checksum() {
        assert!(Command::FlashData.carries_data());
        assert!(Command::FlashDeflData.carries_data());
        assert!(!Command::FlashBegin.carries_data());
        assert!(!Command::Sync.carries_data());
        assert!(!Command::SpiFlashMd5.carries_data());
    }

    /// A success response with no payload: the status bytes are all the data there is.
    #[test]
    fn a_bare_success_response_parses() {
        let mut packet = alloc::vec![0x01, 0x08];
        packet.extend_from_slice(&4u16.to_le_bytes());
        packet.extend_from_slice(&0u32.to_le_bytes());
        packet.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
        let parsed = response(&packet, StatusLen::FOUR).expect("parses");
        assert!(parsed.ok);
        assert_eq!(parsed.error, 0);
        assert_eq!(parsed.command, 0x08);
        assert!(parsed.data.is_empty());
    }

    /// A failure carries its error byte, and the caller can report WHICH failure rather than "failed".
    #[test]
    fn a_failure_response_keeps_its_error_byte() {
        let mut packet = alloc::vec![0x01, 0x02];
        packet.extend_from_slice(&4u16.to_le_bytes());
        packet.extend_from_slice(&0u32.to_le_bytes());
        packet.extend_from_slice(&[0x01, 0x08, 0x00, 0x00]);
        let parsed = response(&packet, StatusLen::FOUR).expect("parses");
        assert!(!parsed.ok);
        assert_eq!(parsed.error, 0x08);
    }

    /// **The defect a wrong status length causes, stated in a test.** The status comes off the END of
    /// the data, so reading a four-byte status as two leaves two status bytes stuck on the payload --
    /// and a digest compared for verification then differs for a reason that has nothing to do with
    /// flash. The same packet is parsed both ways and the payloads differ.
    #[test]
    fn a_wrong_status_length_silently_corrupts_the_payload() {
        let digest = [0xDEu8; 16];
        let mut packet = alloc::vec![0x01, 0x13];
        packet.extend_from_slice(&((digest.len() + 4) as u16).to_le_bytes());
        packet.extend_from_slice(&0u32.to_le_bytes());
        packet.extend_from_slice(&digest);
        packet.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);

        let right = response(&packet, StatusLen::FOUR).expect("parses");
        assert_eq!(right.data, digest, "the digest, with four status bytes removed");

        let wrong = response(&packet, StatusLen::TWO).expect("also parses -- that is the danger");
        assert_ne!(wrong.data, digest);
        assert_eq!(wrong.data.len(), 18, "two status bytes left stuck on the payload");
    }

    /// The three ways a packet is malformed are distinguished, so a caller can say which.
    #[test]
    fn malformed_packets_are_told_apart() {
        assert_eq!(response(&[0x01, 0x08], StatusLen::FOUR), Err(ResponseError::Truncated));
        let mut echoed = alloc::vec![0x00, 0x08];
        echoed.extend_from_slice(&4u16.to_le_bytes());
        echoed.extend_from_slice(&0u32.to_le_bytes());
        echoed.extend_from_slice(&[0, 0, 0, 0]);
        assert_eq!(response(&echoed, StatusLen::FOUR), Err(ResponseError::NotAResponse));
        let mut short = alloc::vec![0x01, 0x08];
        short.extend_from_slice(&9u16.to_le_bytes());
        short.extend_from_slice(&0u32.to_le_bytes());
        short.extend_from_slice(&[0, 0, 0, 0]);
        assert_eq!(response(&short, StatusLen::FOUR), Err(ResponseError::LengthMismatch));
    }
}

#[cfg(test)]
mod reset_tests {
    use super::*;

    /// Replays a sequence and returns the (dtr, rts) state after each step, so a test can compare
    /// against the manual's own "internal state" column instead of against this code's output.
    fn states(actions: &[Action]) -> Vec<(bool, bool)> {
        let mut dtr = false;
        let mut rts = false;
        let mut out = Vec::new();
        for action in actions {
            match action {
                Action::SetDtr(value) => dtr = *value,
                Action::SetRts(value) => rts = *value,
                _ => continue,
            }
            out.push((dtr, rts));
        }
        out
    }

    /// The download-mode sequence reproduces the manual's internal-state column exactly.
    #[test]
    fn the_download_mode_sequence_matches_the_published_state_table() {
        let actions = reset_sequence(Connector::ChipUsbSerial, ResetInto::DownloadMode);
        assert_eq!(
            states(&actions),
            [
                (false, false),
                (false, false),
                (true, false),
                (true, false),
                (true, true),
                (false, true),
                (false, true),
                (false, false),
            ]
        );
    }

    /// The boot-from-flash sequence likewise.
    #[test]
    fn the_boot_from_flash_sequence_matches_the_published_state_table() {
        let actions = reset_sequence(Connector::ChipUsbSerial, ResetInto::Flash);
        assert_eq!(
            states(&actions),
            [(false, false), (false, false), (false, true), (false, false)]
        );
    }

    /// **The property that stops a future reader from "cleaning up" this sequence.** Two of the eight
    /// steps leave the signal state unchanged -- they exist to make a host driver flush a DTR change it
    /// is holding, per the manual's own note. Deduplicating by resulting state would delete exactly
    /// those two, and the sequence would then fail on the most common desktop platform only. So the
    /// redundancy is asserted as a REQUIREMENT rather than tolerated.
    #[test]
    fn the_sequence_keeps_its_no_op_propagate_steps() {
        let actions = reset_sequence(Connector::ChipUsbSerial, ResetInto::DownloadMode);
        let seen = states(&actions);
        let mut no_ops = 0;
        for pair in seen.windows(2) {
            if pair[0] == pair[1] {
                no_ops += 1;
            }
        }
        assert_eq!(
            no_ops, 3,
            "three steps leave the state unchanged (one initializing, two propagating); removing them \
             breaks download mode on hosts whose driver defers DTR until RTS is written"
        );
        assert_eq!(actions.len(), 8, "eight steps, as published");
    }

    /// Never both signals in one action: no host can change them together, so the API must not imply
    /// it can. Asserted across every sequence rather than trusted to review.
    #[test]
    fn no_action_ever_sets_both_signals_at_once() {
        for connector in [Connector::ChipUsbSerial, Connector::UartBridge] {
            for into in [ResetInto::DownloadMode, ResetInto::Flash] {
                for action in reset_sequence(connector, into) {
                    assert!(
                        matches!(
                            action,
                            Action::SetDtr(_) | Action::SetRts(_) | Action::Delay(_)
                        ),
                        "a reset sequence emits one signal at a time, or a delay: {action:?}"
                    );
                }
            }
        }
    }

    /// Both connectors end a reset with the part out of reset and neither signal asserted -- otherwise
    /// the host would leave the board held down, which reads as a dead board rather than a bug here.
    #[test]
    fn every_reset_leaves_the_part_released() {
        for connector in [Connector::ChipUsbSerial, Connector::UartBridge] {
            for into in [ResetInto::DownloadMode, ResetInto::Flash] {
                let actions = reset_sequence(connector, into);
                let final_state = *states(&actions).last().expect("a sequence has signal steps");
                assert_eq!(
                    final_state,
                    (false, false),
                    "{connector:?}/{into:?} must leave both signals clear"
                );
            }
        }
    }

    /// **The property a plausible-looking rewrite would break.**
    ///
    /// A board's bridge reaches enable and the boot strap through an interlock, so the state where BOTH
    /// signals are asserted pulls NEITHER line. That makes two arrangements indistinguishable on paper
    /// and completely different in effect:
    ///
    /// * assert the strap and then enable -- no line is ever pulled, so there is no reset;
    /// * assert enable, then the strap, then release enable -- a real reset, with the strap asserted
    ///   at the instant the part samples it.
    ///
    /// The first shape was this crate's original derivation and it produced no output whatsoever from a
    /// real part. So the test is not "the sequence contains these actions" but **"enable is asserted at
    /// a moment when the strap is not, and the strap is asserted before enable is released"** -- the two
    /// facts the board actually cares about.
    #[test]
    fn the_bridge_download_sequence_pulls_enable_before_it_asserts_the_strap() {
        let actions = reset_sequence(Connector::UartBridge, ResetInto::DownloadMode);
        let seen = states(&actions);

        let enable_pulled = seen.iter().position(|&(strap, enable)| enable && !strap);
        assert!(
            enable_pulled.is_some(),
            "enable must be asserted while the strap is released, else the interlock pulls neither \
             line and the part is never reset: {seen:?}"
        );
        let pulled_at = enable_pulled.expect("just checked");

        let strap_then_release = seen
            .windows(2)
            .position(|pair| pair[0] == (true, true) && pair[1] == (true, false));
        assert!(
            strap_then_release.is_some(),
            "the release must go from strap-asserted-in-reset to strap-asserted-out-of-reset, which \
             is the transition the part samples the strap on: {seen:?}"
        );
        assert!(
            strap_then_release.expect("just checked") > pulled_at,
            "enable is pulled BEFORE the strap goes on, not after"
        );
    }

    /// **THE RED PROOF, kept in the tree rather than performed once.**
    ///
    /// The refuted sequence is spelled out here so the property above is shown to REJECT it. Without
    /// this, the test above passes for the shipped sequence and nobody can tell whether it would have
    /// caught the shape that failed on silicon -- and the refuted shape is the one a reader deriving
    /// from the wiring description will write again.
    #[test]
    fn the_property_rejects_the_shape_that_produced_no_reset() {
        let refuted = alloc::vec![
            Action::SetDtr(true),
            Action::SetRts(true),
            Action::Delay(100),
            Action::SetRts(false),
            Action::Delay(50),
            Action::SetDtr(false),
        ];
        let seen = states(&refuted);
        assert!(
            !seen.iter().any(|&(strap, enable)| enable && !strap),
            "the refuted shape never pulls enable on its own -- which is exactly why a real part \
             produced no output for it: {seen:?}"
        );
    }

    /// The bridge's sequences carry delays and the chip's own do not -- because one drives a real reset
    /// line with a rise time and the other sets a flag the chip reads. Pinned so the distinction is not
    /// lost in a later tidy-up.
    #[test]
    fn only_the_bridge_sequences_need_delays() {
        let chip = reset_sequence(Connector::ChipUsbSerial, ResetInto::DownloadMode);
        assert!(!chip.iter().any(|a| matches!(a, Action::Delay(_))));
        let bridge = reset_sequence(Connector::UartBridge, ResetInto::DownloadMode);
        assert!(bridge.iter().any(|a| matches!(a, Action::Delay(_))));
    }
}

#[cfg(test)]
mod reader_tests {
    use super::*;

    /// A frame fed whole comes back whole.
    #[test]
    fn one_whole_frame_reads_back() {
        let mut reader = FrameReader::new();
        let framed = frame(&[0x01, 0x02, 0x03]);
        assert_eq!(reader.push(&framed), alloc::vec![alloc::vec![0x01, 0x02, 0x03]]);
    }

    /// **Boot chatter is normal on this line, not an error.** Bytes outside a frame are dropped and the
    /// frame between them still reads -- which is what "resynchronizing is normal operation" means.
    #[test]
    fn noise_around_a_frame_is_discarded_not_reported() {
        let mut reader = FrameReader::new();
        let mut stream = b"rst:0x1 (POWERON),boot:0xc\r\n".to_vec();
        stream.extend_from_slice(&frame(&[0xAA, 0xBB]));
        stream.extend_from_slice(b"\r\nI (102) esp_image: segment 0\r\n");
        assert_eq!(reader.push(&stream), alloc::vec![alloc::vec![0xAA, 0xBB]]);
    }

    /// Several frames in one read come back in order.
    #[test]
    fn several_frames_in_one_chunk_come_back_in_order() {
        let mut reader = FrameReader::new();
        let mut stream = frame(&[0x11]);
        stream.extend_from_slice(&frame(&[0x22]));
        stream.extend_from_slice(&frame(&[0x33]));
        assert_eq!(
            reader.push(&stream),
            alloc::vec![alloc::vec![0x11], alloc::vec![0x22], alloc::vec![0x33]]
        );
    }

    /// A frame split across reads is reassembled, at every possible split point.
    #[test]
    fn a_frame_split_anywhere_is_reassembled() {
        let payload = [0x01, 0x02, 0x03, 0x04];
        let framed = frame(&payload);
        for split in 0..=framed.len() {
            let mut reader = FrameReader::new();
            let mut got = reader.push(&framed[..split]);
            got.extend(reader.push(&framed[split..]));
            assert_eq!(got, alloc::vec![payload.to_vec()], "split at {split}");
        }
    }

    /// **THE BOUNDARY BUG THIS READER IS SHAPED TO AVOID.** An escape byte and the byte it escapes land
    /// in different reads. A reader that unescaped as it consumed would have to carry the half pair
    /// across the gap; this one collects the raw body and unescapes once, so there is no gap to carry.
    /// Asserted at every split of a payload made entirely of reserved bytes.
    #[test]
    fn an_escape_pair_split_across_reads_survives() {
        let payload = [0xC0, 0xDB, 0xC0, 0xDB];
        let framed = frame(&payload);
        for split in 0..=framed.len() {
            let mut reader = FrameReader::new();
            let mut got = reader.push(&framed[..split]);
            got.extend(reader.push(&framed[split..]));
            assert_eq!(
                got,
                alloc::vec![payload.to_vec()],
                "an escape pair split at {split} must still decode"
            );
        }
    }

    /// Byte-at-a-time delivery is the pathological case for any accumulator, so it gets its own test.
    #[test]
    fn one_byte_at_a_time_still_yields_the_frame() {
        let payload = [0xC0, 0x42, 0xDB];
        let framed = frame(&payload);
        let mut reader = FrameReader::new();
        let mut got = Vec::new();
        for &byte in &framed {
            got.extend(reader.push(&[byte]));
        }
        assert_eq!(got, alloc::vec![payload.to_vec()]);
    }

    /// Back-to-back delimiters must not manufacture an empty frame: an idle line and a frame boundary
    /// look identical, so a reader that reported one would invent packets out of quiet.
    #[test]
    fn adjacent_delimiters_do_not_become_an_empty_frame() {
        let mut reader = FrameReader::new();
        assert!(reader.push(&[0xC0, 0xC0, 0xC0, 0xC0]).is_empty());
        assert_eq!(reader.push(&frame(&[0x07])), alloc::vec![alloc::vec![0x07]]);
    }

    /// A frame that never closes leaves its bytes pending rather than being emitted, so a caller can
    /// bound how much unterminated noise it will hold.
    #[test]
    fn an_unterminated_frame_stays_pending() {
        let mut reader = FrameReader::new();
        assert!(reader.push(&[0xC0, 0x01, 0x02, 0x03]).is_empty());
        assert_eq!(reader.pending(), 3);
        assert_eq!(reader.push(&[0xC0]), alloc::vec![alloc::vec![0x01, 0x02, 0x03]]);
        assert_eq!(reader.pending(), 0);
    }

    /// End to end: a real response packet, arriving after boot chatter and split mid-escape, parses.
    /// The pieces are individually tested above; this asserts they compose, which is where a
    /// layering mistake would show up rather than in any one of them.
    #[test]
    fn a_response_survives_noise_a_split_and_an_escape_together() {
        let digest = [0xC0u8, 0xDB, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06,
                      0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E];
        let mut packet = alloc::vec![0x01, Command::SpiFlashMd5 as u8];
        packet.extend_from_slice(&((digest.len() + 4) as u16).to_le_bytes());
        packet.extend_from_slice(&0u32.to_le_bytes());
        packet.extend_from_slice(&digest);
        packet.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
        let framed = frame(&packet);

        let mut stream = b"boot:0xc (SPI_FAST_FLASH_BOOT)\r\n".to_vec();
        stream.extend_from_slice(&framed);
        let mut reader = FrameReader::new();
        let mut frames = Vec::new();
        for chunk in stream.chunks(3) {
            frames.extend(reader.push(chunk));
        }
        assert_eq!(frames.len(), 1, "exactly one frame, the chatter dropped");
        let parsed = response(&frames[0], StatusLen::FOUR).expect("parses");
        assert!(parsed.ok);
        assert_eq!(parsed.data, digest, "the digest survived escaping, splitting and noise");
    }
}
