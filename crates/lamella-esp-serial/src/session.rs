//! Driving one flash write from start to finish: the order of commands, and what to do with each
//! answer.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use crate::{
    deflate, request, response, Action, Command, Connector, FrameReader, ResetInto, Response,
    ResponseError, StatusLen, SYNC_PAYLOAD,
};

/// The ways this protocol differs BETWEEN PARTS, gathered in one place.
///
/// # Why these are parameters and not constants
///
/// The vendor publishes this protocol per chip and warns in writing that it differs between chips.
/// Every field here is a difference this project has actually run into, and each one shares an
/// unpleasant property: **guessing wrong does not fail cleanly.** A wrong status length mis-slices any
/// response carrying a payload, and a wrong argument count is refused with an error that says only
/// "invalid message".
///
/// So a part is described rather than assumed, and the constants below carry only parts whose values
/// this project has established against the part itself. **A part not listed here needs its values
/// established, not guessed** -- and doing so is cheap: drive a small write and read what is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dialect {
    /// How many trailing status bytes this target's responses carry.
    pub status_len: StatusLen,
    /// Whether this target's flash-write declaration takes a fifth argument word.
    ///
    /// Newer parts take one more word than older ones on the command that opens a write. **Sending
    /// four to a part that wants five is refused as an invalid message** -- an error that names nothing
    /// about argument counts, so it is not discoverable from the failure.
    pub flash_begin_takes_fifth_word: bool,
}

impl Dialect {
    /// The ESP32-C6.
    ///
    /// Its write declaration takes the fifth argument word -- four are refused -- and its responses
    /// carry the four-byte status form, which a verification reply shows as a 32-character digest
    /// followed by four status bytes in a 36-byte body.
    pub const ESP32C6: Dialect =
        Dialect { status_len: StatusLen::FOUR, flash_begin_takes_fifth_word: true };
}

/// The attached flash chip's geometry, as the loader needs it described.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlashParams {
    /// The chip's total size in bytes.
    pub total_size: u32,
    /// Erase-block size.
    pub block_size: u32,
    /// Erase-sector size -- the smallest erasable unit, and so the granularity a write erases at.
    pub sector_size: u32,
    /// Programming page size.
    pub page_size: u32,
    /// Which status-register bits the loader should mind.
    pub status_mask: u32,
}

impl FlashParams {
    /// The geometry a serial NOR flash of `total_size` bytes conventionally has: 64 KiB erase blocks,
    /// 4 KiB sectors, 256-byte pages.
    ///
    /// These are the near-universal values for this class of part rather than something read from a
    /// particular chip's datasheet, which is why the type stays constructible field by field: a part
    /// that differs must be described rather than assumed.
    #[must_use]
    pub const fn serial_nor(total_size: u32) -> FlashParams {
        FlashParams {
            total_size,
            block_size: 64 * 1024,
            sector_size: 4 * 1024,
            page_size: 256,
            status_mask: 0xFFFF,
        }
    }
}

/// How much of the image travels in one data command.
///
/// A choice of this crate's rather than a published limit: the write is declared up front with its
/// packet size, so the loader is told what to expect rather than assuming. One sector keeps a single
/// packet small enough to be uncontroversial for any receive buffer while still amortizing the
/// per-packet exchange over a useful amount of data. It is a constant rather than a parameter
/// because a caller has no information with which to choose better.
const BLOCK: usize = 4096;

/// How many times to send the establishing command before giving up.
///
/// A target may be mid-boot, or may need its reset to settle, so the first attempts going unanswered
/// is expected rather than exceptional -- which is precisely why this has a bound. Without one, a
/// board that will never answer looks identical to one that is nearly ready.
const SYNC_ATTEMPTS: u32 = 10;

/// What the host should do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Perform this, then poll again.
    Do(Action),
    /// An answer is outstanding: read from the line, feed what arrives, then poll again. If nothing
    /// arrives within `timeout_ms`, feed nothing and poll again -- the session decides whether that
    /// is a retry or a failure, because only it knows which command is outstanding.
    Await {
        /// How long to wait before giving up on this read and polling again.
        timeout_ms: u32,
    },
    /// The write finished AND the target's own read-back of flash matched the image (see the module
    /// docs).
    Done {
        /// The digest the DEVICE reported over the range written, exactly as it arrived -- kept so a
        /// caller can display it, not so a caller can check it. Reaching this variant already means
        /// it matched.
        device_digest: Vec<u8>,
        /// Which encoding the target answered in, since this differs between the ROM loader and a RAM
        /// one and a caller reporting the digest wants to say which it is showing.
        encoding: DigestEncoding,
    },
    /// The write failed, with the reason.
    Failed(Error),
}

/// Why a session stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The target never answered the establishing command. Almost always a board not in download
    /// mode, or a connector whose control signals do not reach its reset -- not a protocol fault.
    NoResponse,
    /// The target answered a command with a failure status.
    Rejected {
        /// Which command.
        command: u8,
        /// The target's error byte.
        error: u8,
    },
    /// A frame arrived that could not be read as a response.
    Malformed(ResponseError),
    /// The target answered a different command than the one outstanding. Kept distinct from
    /// [`Error::Malformed`] because it means the conversation has desynchronized rather than that a
    /// packet was corrupt, and the remedy differs: resynchronize, do not retry the command.
    OutOfStep {
        /// What was outstanding.
        expected: u8,
        /// What answered.
        got: u8,
    },
    /// The image does not fit the chip as described.
    DoesNotFit {
        /// Where the write would end.
        end: u64,
        /// The chip's size.
        capacity: u32,
    },
    /// **The target read its own flash back and it does not match the image.** The write completed at
    /// the protocol level -- every block was acknowledged and its per-block checksum accepted -- and
    /// the contents are still wrong, which is precisely the case the specification says the per-block
    /// checksum does not cover.
    NotVerified {
        /// The digest the target reported, exactly as it arrived on the wire.
        device: Vec<u8>,
        /// The digest of the image, in the encoding the target appeared to be using -- or the
        /// hexadecimal form when the target's answer matched neither length, so the two are
        /// comparable by eye in a report.
        expected: Vec<u8>,
    },
}

/// Which form the target answered the verification command in.
///
/// **The ROM and a RAM loader do not agree on this**, and the length is the only way to tell them
/// apart from the wire. It is reported rather than assumed for the same reason [`StatusLen`] is a
/// parameter: a wrong guess here does not fail loudly, it makes a correct flash write look corrupt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DigestEncoding {
    /// Thirty-two lowercase hexadecimal characters.
    AsciiHex,
    /// Sixteen raw bytes.
    Raw,
}

/// Which command the session is waiting on, and what to do with its answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Emitting the reset sequence; nothing is outstanding.
    Resetting,
    /// Establishing the byte stream, with attempts remaining.
    Syncing(u32),
    /// Attaching the flash chip to the loader's controller.
    Attaching,
    /// Describing the chip's geometry.
    DescribingChip,
    /// Declaring the write.
    Beginning,
    /// Sending block `n` of the image.
    Sending(usize),
    /// Asking for the digest.
    Verifying,
    /// Closing the write.
    Ending,
    /// Emitting the final reset; nothing is outstanding.
    Finishing,
    /// Nothing more to do.
    Complete,
}

/// One flash write, driven by the caller.
#[derive(Debug)]
pub struct Session {
    dialect: Dialect,
    params: FlashParams,
    offset: u32,
    image: Vec<u8>,
    phase: Phase,
    reader: FrameReader,
    /// Actions produced but not yet handed out -- a reset is many actions and the caller takes one at
    /// a time.
    queue: VecDeque<Action>,
    /// The reset sequence to emit once the write is done.
    finish_with: Vec<Action>,
    /// The image compressed, when this session drives the compressed path -- **and the fact that it
    /// is the compressed path.**
    ///
    /// One field rather than a payload plus a flag, so the two cannot disagree about which commands
    /// the session is speaking, and so a plain write does not carry a second copy of its image.
    deflated: Option<Vec<u8>>,
    /// The digest the device reported, once it has -- and once it has MATCHED.
    digest: Vec<u8>,
    /// Which encoding that digest arrived in; set with `digest`.
    encoding: Option<DigestEncoding>,
    /// Set when the session has decided it is over.
    failure: Option<Error>,
}

impl Session {
    /// Begins a write of `image` at `offset`, over `connector`.
    ///
    /// `dialect` carries the ways this protocol differs between parts, each of which is refused with
    /// an error that names something else -- see [`Dialect`].
    #[must_use]
    pub fn write_flash(
        connector: Connector,
        dialect: Dialect,
        params: FlashParams,
        offset: u32,
        image: Vec<u8>,
    ) -> Session {
        Session::new(connector, dialect, params, offset, image, None)
    }

    /// Begins a COMPRESSED write of `image` at `offset`: the same image, sent as a compressed stream
    /// that the target's own ROM inflates as it writes.
    ///
    /// Verification is unchanged and still means what it says -- the target reads its flash back and
    /// the digest is compared against the digest of `image`, so reaching [`Step::Done`] means the
    /// UNCOMPRESSED bytes arrived, not merely that a stream was accepted.
    ///
    /// # What this costs and what it does not
    ///
    /// It saves transfer time in proportion to how well the image compresses and nothing else: the
    /// same range is erased, the same bytes end up in flash, and the same digest is checked. See
    /// [`deflate::Method`] for the two encoders, one of which deliberately compresses nothing and
    /// exists to tell a wrong command sequence apart from a wrong stream.
    ///
    /// **The erase this declares is rounded up** -- see [`Session::declared_extent`], which is where
    /// the whole reasoning about how far lives, because getting it wrong erases flash the caller did
    /// not ask about without any error saying so.
    #[must_use]
    pub fn write_flash_compressed(
        connector: Connector,
        dialect: Dialect,
        params: FlashParams,
        offset: u32,
        image: Vec<u8>,
        method: deflate::Method,
    ) -> Session {
        let deflated = deflate::zlib(&image, method);
        Session::new(connector, dialect, params, offset, image, Some(deflated))
    }

    /// The two constructors' common body.
    fn new(
        connector: Connector,
        dialect: Dialect,
        params: FlashParams,
        offset: u32,
        image: Vec<u8>,
        deflated: Option<Vec<u8>>,
    ) -> Session {
        let end = u64::from(offset) + image.len() as u64;
        let failure = (end > u64::from(params.total_size))
            .then_some(Error::DoesNotFit { end, capacity: params.total_size });
        Session {
            dialect,
            params,
            offset,
            image,
            phase: if failure.is_some() { Phase::Complete } else { Phase::Resetting },
            reader: FrameReader::new(),
            queue: crate::reset_sequence(connector, ResetInto::DownloadMode).into(),
            finish_with: crate::reset_sequence(connector, ResetInto::Flash),
            deflated,
            digest: Vec::new(),
            encoding: None,
            failure,
        }
    }

    /// The bytes the data commands carry: the compressed form when there is one, else the image.
    fn payload(&self) -> &[u8] {
        self.deflated.as_deref().unwrap_or(&self.image)
    }

    /// How many bytes of content will cross the wire, which for a compressed write is the compressed
    /// size -- the only place the saving is visible, since nothing else about the write changes.
    #[must_use]
    pub fn transfer_len(&self) -> usize {
        self.payload().len()
    }

    /// How many data commands the write takes.
    ///
    /// This counts what is SENT, so on the compressed path it is over the compressed bytes -- which is
    /// also what the declaration tells the target to expect.
    #[must_use]
    pub fn total_blocks(&self) -> usize {
        self.payload().len().div_ceil(BLOCK)
    }

    /// How many blocks have been acknowledged -- for a caller showing progress.
    #[must_use]
    pub fn blocks_sent(&self) -> usize {
        match self.phase {
            Phase::Sending(n) => n,
            Phase::Resetting | Phase::Syncing(_) | Phase::Attaching | Phase::DescribingChip
            | Phase::Beginning => 0,
            _ => self.total_blocks(),
        }
    }

    /// Tells the session the host waited and nothing arrived.
    ///
    /// This is a distinct call rather than an empty [`Session::feed`], because silence MEANS something
    /// and what it means depends on which command is outstanding. While establishing the stream it is
    /// ordinary -- the part may still be booting -- and costs one of a bounded number of attempts.
    /// After that it is a fault: a target that has answered once and then goes quiet is not going to
    /// be coaxed by repetition, and retrying forever would turn a broken board into a hang.
    pub fn timeout(&mut self) {
        match self.phase {
            Phase::Syncing(remaining) => {
                let left = remaining.saturating_sub(1);
                self.phase = Phase::Syncing(left);
                if left > 0 {
                    self.queue.push_back(Action::Write(request(Command::Sync, &SYNC_PAYLOAD)));
                }
            }
            Phase::Resetting | Phase::Finishing | Phase::Complete => {}
            _ => self.failure = Some(Error::NoResponse),
        }
    }

    /// Feeds bytes that arrived on the line.
    pub fn feed(&mut self, bytes: &[u8]) {
        for body in self.reader.push(bytes) {
            if self.failure.is_some() || self.phase == Phase::Complete {
                continue;
            }
            match response(&body, self.dialect.status_len) {
                Err(ResponseError::NotAResponse) => continue,
                Err(problem) => self.failure = Some(Error::Malformed(problem)),
                Ok(answer) => self.accept(answer),
            }
        }
    }

    /// What to do next.
    pub fn poll(&mut self) -> Step {
        if let Some(problem) = &self.failure {
            return Step::Failed(problem.clone());
        }
        if let Some(action) = self.queue.pop_front() {
            return Step::Do(action);
        }
        match self.phase {
            Phase::Resetting => {
                self.phase = Phase::Syncing(SYNC_ATTEMPTS);
                self.send(Command::Sync, &SYNC_PAYLOAD)
            }
            Phase::Syncing(0) => Step::Failed(Error::NoResponse),
            Phase::Syncing(_) => Step::Await { timeout_ms: 500 },
            Phase::Complete => match self.encoding {
                None => Step::Failed(self.failure.clone().unwrap_or(Error::NoResponse)),
                Some(encoding) => {
                    Step::Done { device_digest: self.digest.clone(), encoding }
                }
            },
            Phase::Finishing => {
                self.phase = Phase::Complete;
                self.queue.extend(self.finish_with.iter().cloned());
                self.poll()
            }
            _ => Step::Await { timeout_ms: 3_000 },
        }
    }

    /// Queues one command and reports it as the next action.
    fn send(&mut self, command: Command, data: &[u8]) -> Step {
        Step::Do(Action::Write(request(command, data)))
    }

    /// The command that opens this session's write.
    ///
    /// The three commands come in matched triples and **a session must not mix them**: the compressed
    /// declaration followed by plain data commands is answered as an out-of-step conversation on a good
    /// day and writes compressed bytes into flash verbatim on a bad one. Chosen in one place each so
    /// no call site can pair them up wrongly.
    fn begin_command(&self) -> Command {
        if self.deflated.is_some() {
            Command::FlashDeflBegin
        } else {
            Command::FlashBegin
        }
    }

    /// The command that carries one block of this session's write.
    fn data_command(&self) -> Command {
        if self.deflated.is_some() {
            Command::FlashDeflData
        } else {
            Command::FlashData
        }
    }

    /// The command that closes this session's write.
    fn end_command(&self) -> Command {
        if self.deflated.is_some() {
            Command::FlashDeflEnd
        } else {
            Command::FlashEnd
        }
    }

    /// Handles one well-formed response, advancing the phase.
    fn accept(&mut self, answer: Response) {
        let expected = match self.phase {
            Phase::Syncing(_) => Command::Sync as u8,
            Phase::Attaching => Command::SpiAttach as u8,
            Phase::DescribingChip => Command::SpiSetParams as u8,
            Phase::Beginning => self.begin_command() as u8,
            Phase::Sending(_) => self.data_command() as u8,
            Phase::Verifying => Command::SpiFlashMd5 as u8,
            Phase::Ending => self.end_command() as u8,
            Phase::Resetting | Phase::Finishing | Phase::Complete => return,
        };
        if answer.command != expected {
            if answer.command == Command::Sync as u8 {
                return;
            }
            self.failure = Some(Error::OutOfStep { expected, got: answer.command });
            return;
        }
        if !answer.ok {
            self.failure = Some(Error::Rejected { command: expected, error: answer.error });
            return;
        }
        self.advance(answer);
    }

    /// Moves to the next phase after a successful answer.
    fn advance(&mut self, answer: Response) {
        self.phase = match self.phase {
            Phase::Syncing(_) => Phase::Attaching,
            Phase::Attaching => Phase::DescribingChip,
            Phase::DescribingChip => Phase::Beginning,
            Phase::Beginning => Phase::Sending(0),
            Phase::Sending(n) if n + 1 < self.total_blocks() => Phase::Sending(n + 1),
            Phase::Sending(_) => Phase::Verifying,
            Phase::Verifying => {
                match self.check_digest(&answer.data) {
                    Some(encoding) => {
                        self.digest = answer.data;
                        self.encoding = Some(encoding);
                        Phase::Ending
                    }
                    None => {
                        self.failure = Some(Error::NotVerified {
                            device: answer.data,
                            expected: crate::digest::md5_hex(&self.image),
                        });
                        return;
                    }
                }
            }
            Phase::Ending => Phase::Finishing,
            other => other,
        };
        let next = match self.phase {
            Phase::Attaching => Some((Command::SpiAttach, words(&[0]))),
            Phase::DescribingChip => Some((
                Command::SpiSetParams,
                words(&[
                    0,
                    self.params.total_size,
                    self.params.block_size,
                    self.params.sector_size,
                    self.params.page_size,
                    self.params.status_mask,
                ]),
            )),
            Phase::Beginning => Some((self.begin_command(), self.flash_begin_args())),
            Phase::Sending(_) => None,
            Phase::Verifying => Some((
                Command::SpiFlashMd5,
                words(&[self.offset, self.image.len() as u32, 0, 0]),
            )),
            Phase::Ending => Some((self.end_command(), words(&[0]))),
            _ => None,
        };
        if let Some((command, data)) = next {
            self.queue.push_back(Action::Write(request(command, &data)));
        }
        if let Phase::Sending(n) = self.phase {
            let (header, block) = self.block_parts(n);
            self.queue.push_back(Action::Write(crate::data_request(
                self.data_command(),
                &header,
                block,
            )));
        }
    }

    /// The extent the declaration's FIRST argument describes -- **and the two write paths do not mean
    /// the same quantity by it**, even though it is the same word in the same position.
    ///
    /// * The plain path declares the size to ERASE, and the image's own length is that.
    /// * The compressed path declares the size AFTER INFLATION, and without the RAM loader this crate
    ///   does not use the specification requires that rounded UP to the flash's erase granularity.
    ///
    /// The two words after it go the other way: they count the packets that will be SENT and how big
    /// they are, so on the compressed path those describe the COMPRESSED bytes. One word describes the
    /// image; the next two describe the transfer.
    ///
    /// **Sending the compressed size here instead erases less than the write goes on to fill, and
    /// still writes plausibly** -- every block is acknowledged and every per-block checksum passes,
    /// because none of them know how much was erased. The target's own read-back is the only thing that
    /// catches it, which is why this session does not treat that read-back as optional.
    ///
    /// # Which granularity, and why the SMALLER one
    ///
    /// The specification says "erase block", and this protocol's own chip-geometry command distinguishes
    /// a 64 KiB block from a 4 KiB sector, so the phrase can be read either way. This rounds to the
    /// SECTOR -- the smallest erasable unit -- and the reason is that **the two readings fail in
    /// opposite directions and only one of them fails loudly.**
    ///
    /// Declaring too little is refused, or writes into flash that was not erased and then fails
    /// verification: loud, and nothing outside the requested range is touched. Declaring too much
    /// ERASES FLASH THE CALLER DID NOT ASK ABOUT -- up to sixteen sectors of it -- and no error reports
    /// that, because verification covers the image's range and not the erase's. A write that quietly
    /// clears the next 60 KiB is the worse failure by a wide margin.
    ///
    /// It also leaves this path's erase footprint IDENTICAL to the plain path's: a plain write declares
    /// its exact length and the target erases whole sectors regardless. So the smaller reading adds no
    /// exposure the plain path does not already have, and the larger one would add a great deal.
    fn declared_extent(&self) -> u32 {
        if self.deflated.is_none() {
            return self.image.len() as u32;
        }
        let sector = self.params.sector_size.max(1) as usize;
        (self.image.len().div_ceil(sector) * sector) as u32
    }

    /// The arguments the write declaration carries: how much, in how many packets of what size, where
    /// -- and on some parts a fifth word.
    ///
    /// **The fifth word is not optional padding.** A part that wants it refuses four words as an
    /// invalid message, and a part that does not want it is equally entitled to refuse five, so this
    /// follows the dialect rather than sending the longer form to everyone. The specification gives the
    /// compressed declaration the same fifth word as the plain one, and for the same reason: it is
    /// passed to the ROM loader, which is the only loader this crate speaks to.
    fn flash_begin_args(&self) -> Vec<u8> {
        let mut args = alloc::vec![
            self.declared_extent(),
            self.total_blocks() as u32,
            BLOCK as u32,
            self.offset,
        ];
        if self.dialect.flash_begin_takes_fifth_word {
            args.push(0);
        }
        words(&args)
    }

    /// Whether `reported` is this image's digest, and in which encoding -- or `None` if it is neither.
    ///
    /// **Both encodings are accepted rather than one being chosen, because which one arrives is a
    /// property of the loader on the other end** and the specification documents this protocol per
    /// chip precisely because such things differ. Deciding by LENGTH is safe: the two forms are 32 and
    /// 16 bytes, so a reply cannot be read as the wrong one.
    fn check_digest(&self, reported: &[u8]) -> Option<DigestEncoding> {
        if reported.len() == crate::digest::DIGEST_LEN * 2
            && reported == crate::digest::md5_hex(&self.image).as_slice()
        {
            return Some(DigestEncoding::AsciiHex);
        }
        if reported.len() == crate::digest::DIGEST_LEN
            && reported == crate::digest::md5(&self.image).as_slice()
        {
            return Some(DigestEncoding::Raw);
        }
        None
    }

    /// Block `n` of the image, as its four descriptive words and the bytes themselves -- **kept apart
    /// because the checksum covers only the second half.**
    ///
    /// Returning them separately rather than concatenated is what makes that impossible to get wrong at
    /// the call site: there is no combined payload for a caller to accidentally checksum. See
    /// [`crate::data_request`] for what the two halves become on the wire.
    ///
    /// The final block is NOT padded. The loader is told each packet's true length, and padding would
    /// write bytes the caller did not ask for past the end of its image -- into flash it may be using
    /// for something else. On the compressed path padding would be worse than that: trailing bytes past
    /// a stream's end are not part of the stream, and an inflater is entitled to treat them as a
    /// malformed one.
    fn block_parts(&self, n: usize) -> (Vec<u8>, &[u8]) {
        let payload = self.payload();
        let start = n * BLOCK;
        let chunk = &payload[start..(start + BLOCK).min(payload.len())];
        (words(&[chunk.len() as u32, n as u32, 0, 0]), chunk)
    }
}

/// Little-endian words, which is how every argument in this protocol is carried.
fn words(values: &[u32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(values.len() * 4);
    for value in values {
        out.extend_from_slice(&value.to_le_bytes());
    }
    out
}

/// A framed success response to `command`, carrying `data`. Test scaffolding, kept beside the session
/// it feeds so the two cannot drift apart.
#[cfg(test)]
fn ok_response(command: u8, data: &[u8], status_len: StatusLen) -> Vec<u8> {
    let mut packet = alloc::vec![0x01, command];
    packet.extend_from_slice(&((data.len() + status_len.0) as u16).to_le_bytes());
    packet.extend_from_slice(&0u32.to_le_bytes());
    packet.extend_from_slice(data);
    packet.extend_from_slice(&alloc::vec![0u8; status_len.0]);
    crate::frame(&packet)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drives a session to completion against a target that answers everything successfully, and
    /// returns the commands it sent in order.
    fn run(session: &mut Session, digest: &[u8]) -> (Vec<u8>, Step) {
        let mut sent = Vec::new();
        for _ in 0..10_000 {
            match session.poll() {
                Step::Do(Action::Write(bytes)) => {
                    let body = crate::unescape(&bytes[1..bytes.len() - 1]);
                    let command = body[1];
                    sent.push(command);
                    let payload: &[u8] =
                        if command == Command::SpiFlashMd5 as u8 { digest } else { &[] };
                    session.feed(&ok_response(command, payload, StatusLen::FOUR));
                }
                Step::Do(_) => {}
                Step::Await { .. } => {}
                done => return (sent, done),
            }
        }
        panic!("session did not finish");
    }

    fn session_for(image: Vec<u8>) -> Session {
        Session::write_flash(
            Connector::UartBridge,
            Dialect::ESP32C6,
            FlashParams::serial_nor(8 * 1024 * 1024),
            0x10_0000,
            image,
        )
    }

    /// Where every compressed-path test writes, and the geometry it writes to.
    const OFFSET: u32 = 0x10_0000;

    fn compressed_session_for(image: Vec<u8>, method: deflate::Method) -> Session {
        Session::write_flash_compressed(
            Connector::UartBridge,
            Dialect::ESP32C6,
            FlashParams::serial_nor(8 * 1024 * 1024),
            OFFSET,
            image,
            method,
        )
    }

    /// Drives a session as [`run`] does, but keeps each request's DATA as well as its opcode -- which is
    /// what the declaration's arguments have to be read out of.
    fn run_recording(session: &mut Session, digest: &[u8]) -> (Vec<(u8, Vec<u8>)>, Step) {
        let mut sent = Vec::new();
        for _ in 0..10_000 {
            match session.poll() {
                Step::Do(Action::Write(bytes)) => {
                    let body = crate::unescape(&bytes[1..bytes.len() - 1]);
                    let command = body[1];
                    sent.push((command, body[8..].to_vec()));
                    let payload: &[u8] =
                        if command == Command::SpiFlashMd5 as u8 { digest } else { &[] };
                    session.feed(&ok_response(command, payload, StatusLen::FOUR));
                }
                Step::Do(_) | Step::Await { .. } => {}
                done => return (sent, done),
            }
        }
        panic!("session did not finish");
    }

    /// Reads little-endian words out of a request's data.
    fn args(data: &[u8]) -> Vec<u32> {
        data.chunks_exact(4)
            .map(|word| u32::from_le_bytes([word[0], word[1], word[2], word[3]]))
            .collect()
    }

    /// A compressed write speaks the compressed command triple throughout, and **still verifies against
    /// the digest of the UNCOMPRESSED image** -- which is the property that makes it a write rather than
    /// a transfer that was accepted.
    #[test]
    fn a_compressed_write_issues_the_compressed_commands_and_still_verifies_the_image() {
        let image = alloc::vec![0xAB; 5000];
        let mut session = compressed_session_for(image.clone(), deflate::Method::Fixed);
        let truthful = crate::digest::md5_hex(&image);
        let (sent, done) = run_recording(&mut session, &truthful);
        let opcodes: Vec<u8> = sent.iter().map(|(command, _)| *command).collect();
        assert_eq!(
            opcodes,
            alloc::vec![
                Command::Sync as u8,
                Command::SpiAttach as u8,
                Command::SpiSetParams as u8,
                Command::FlashDeflBegin as u8,
                Command::FlashDeflData as u8,
                Command::SpiFlashMd5 as u8,
                Command::FlashDeflEnd as u8,
            ]
        );
        assert_eq!(
            done,
            Step::Done { device_digest: truthful, encoding: DigestEncoding::AsciiHex }
        );
    }

    /// **THE ARGUMENT MEANINGS, PINNED.** The declaration's first word describes the IMAGE -- its
    /// inflated size, rounded up to a sector -- and the two after it describe the TRANSFER, which is
    /// the compressed byte count expressed as a packet count and a packet size.
    ///
    /// Sending the compressed size in the first word erases less than the write fills, and every
    /// acknowledgement still arrives: this test is the only thing that fails when they are swapped,
    /// short of a target's read-back.
    #[test]
    fn the_compressed_declaration_describes_the_image_then_the_transfer() {
        let image: Vec<u8> = (0..5000u32).map(|n| (n % 7) as u8).collect();
        let mut session = compressed_session_for(image.clone(), deflate::Method::Fixed);
        let compressed_len = session.transfer_len();
        assert!(compressed_len < image.len(), "the test needs an image that compresses");
        let (sent, _) = run_recording(&mut session, &crate::digest::md5_hex(&image));
        let (_, declaration) =
            sent.iter().find(|(command, _)| *command == Command::FlashDeflBegin as u8).expect("sent");
        let words = args(declaration);
        assert_eq!(words.len(), 5, "the fifth word this part requires");
        assert_eq!(words[0], 8192, "the UNCOMPRESSED 5,000 bytes rounded up to two 4 KiB sectors");
        assert_eq!(words[1], compressed_len.div_ceil(BLOCK) as u32, "packets of COMPRESSED bytes");
        assert_eq!(words[2], BLOCK as u32);
        assert_eq!(words[3], OFFSET);
        assert_ne!(words[0] as usize, compressed_len);
    }

    /// The data commands carry the compressed stream exactly: every byte, in order, and the last packet
    /// short rather than padded. **Trailing padding is worse here than on the plain path** -- bytes past
    /// a stream's end are not part of it, and an inflater may reject the whole thing.
    #[test]
    fn the_data_commands_carry_the_whole_compressed_stream_unpadded() {
        let image: Vec<u8> = (0..12_000u32).map(|n| (n % 251) as u8).collect();
        let expected = deflate::zlib(&image, deflate::Method::Fixed);
        let mut session = compressed_session_for(image.clone(), deflate::Method::Fixed);
        let (sent, done) = run_recording(&mut session, &crate::digest::md5_hex(&image));
        let mut carried = Vec::new();
        for (command, data) in &sent {
            if *command == Command::FlashDeflData as u8 {
                let declared = args(&data[..16])[0] as usize;
                assert_eq!(declared, data.len() - 16, "a packet declares the bytes it carries");
                carried.extend_from_slice(&data[16..]);
            }
        }
        assert_eq!(carried, expected, "the stream arrived byte for byte");
        assert!(matches!(done, Step::Done { .. }));
    }

    /// The verification digest covers the IMAGE's range in flash, not the compressed length and not the
    /// rounded erase extent. A digest over the compressed length would disagree for a reason that has
    /// nothing to do with the write; one over the rounded extent would include bytes the caller never
    /// supplied.
    #[test]
    fn the_verification_covers_the_image_range_not_the_transfer_or_the_erase() {
        let image = alloc::vec![0x5A; 5000];
        let mut session = compressed_session_for(image.clone(), deflate::Method::Fixed);
        let compressed_len = session.transfer_len();
        let (sent, _) = run_recording(&mut session, &crate::digest::md5_hex(&image));
        let (_, request) =
            sent.iter().find(|(command, _)| *command == Command::SpiFlashMd5 as u8).expect("sent");
        let words = args(request);
        assert_eq!(words[0], OFFSET);
        assert_eq!(words[1], image.len() as u32);
        assert_ne!(words[1] as usize, compressed_len);
        assert_ne!(words[1], 8192, "not the rounded erase extent");
    }

    /// The rounding itself, at the boundaries where an off-by-one lives -- and the plain path declaring
    /// its exact length, unchanged, because the same word means a different thing there.
    #[test]
    fn the_declared_extent_rounds_a_compressed_write_up_to_a_whole_sector() {
        for (length, expected) in [(1usize, 4096u32), (4095, 4096), (4096, 4096), (4097, 8192)] {
            let session =
                compressed_session_for(alloc::vec![0x11; length], deflate::Method::Fixed);
            assert_eq!(session.declared_extent(), expected, "a {length}-byte image");
            let plain = session_for(alloc::vec![0x11; length]);
            assert_eq!(plain.declared_extent(), length as u32, "the plain path is unrounded");
        }
        let empty = compressed_session_for(Vec::new(), deflate::Method::Fixed);
        assert_eq!(empty.declared_extent(), 0);
    }

    /// **The rung that compresses nothing still drives the whole compressed path.** That is its entire
    /// purpose: it fails only if the command sequence or the declared sizes are wrong, so a rejection
    /// with it has one candidate cause instead of two.
    #[test]
    fn the_stored_method_drives_the_same_command_path() {
        let image: Vec<u8> = (0..9000u32).map(|n| (n % 13) as u8).collect();
        let mut session = compressed_session_for(image.clone(), deflate::Method::Stored);
        assert!(session.transfer_len() > image.len(), "storing costs a little rather than saving");
        let (sent, done) = run_recording(&mut session, &crate::digest::md5_hex(&image));
        assert!(sent.iter().any(|(command, _)| *command == Command::FlashDeflBegin as u8));
        assert!(sent.iter().all(|(command, _)| *command != Command::FlashBegin as u8));
        assert!(matches!(done, Step::Done { .. }));
    }

    /// The command order is the protocol's, and every command appears exactly where it should.
    #[test]
    fn a_write_issues_the_commands_in_order() {
        let image = alloc::vec![0xAB; 100];
        let mut session = session_for(image.clone());
        let truthful = crate::digest::md5_hex(&image);
        let (sent, done) = run(&mut session, &truthful);
        assert_eq!(
            sent,
            alloc::vec![
                Command::Sync as u8,
                Command::SpiAttach as u8,
                Command::SpiSetParams as u8,
                Command::FlashBegin as u8,
                Command::FlashData as u8,
                Command::SpiFlashMd5 as u8,
                Command::FlashEnd as u8,
            ]
        );
        assert_eq!(
            done,
            Step::Done { device_digest: truthful, encoding: DigestEncoding::AsciiHex }
        );
    }

    /// **THE DEFECT THE COMPARISON EXISTS FOR, stated in a test.** Every block is acknowledged, every
    /// per-block checksum is accepted, and the flash contents are still wrong -- which is exactly the
    /// case the specification says the per-block checksum does not cover. A session that fetched the
    /// digest without comparing it would report [`Step::Done`] here, carrying the proof of its own
    /// failure in a field the caller was trusted to check.
    #[test]
    fn a_target_whose_flash_disagrees_is_a_failure_not_a_completed_write() {
        let image = alloc::vec![0xAB; 100];
        let mut session = session_for(image.clone());
        let wrong = crate::digest::md5_hex(&alloc::vec![0xAC; 100]);
        assert_eq!(wrong.len(), 32, "a wrong digest is the same shape as a right one");
        let (_, outcome) = run(&mut session, &wrong);
        assert_eq!(
            outcome,
            Step::Failed(Error::NotVerified {
                device: wrong,
                expected: crate::digest::md5_hex(&image),
            }),
            "a mismatch names both digests so a report can show them"
        );
    }

    /// Both digest encodings verify, because which one arrives is a property of the loader on the
    /// other end rather than something this crate chooses. The same image, the same session, two
    /// truthful answers in the two published forms.
    #[test]
    fn either_digest_encoding_verifies_and_says_which_it_was() {
        let image = alloc::vec![0x5A; 3];
        for (reply, expected) in [
            (crate::digest::md5_hex(&image), DigestEncoding::AsciiHex),
            (crate::digest::md5(&image).to_vec(), DigestEncoding::Raw),
        ] {
            let mut session = session_for(image.clone());
            let (_, done) = run(&mut session, &reply);
            assert_eq!(done, Step::Done { device_digest: reply, encoding: expected });
        }
    }

    /// A digest of the right VALUE but the wrong LENGTH is not accepted -- which is what stops the
    /// two-encoding tolerance from becoming a way for a mis-sliced response to slip through. The
    /// hexadecimal form with two stray status bytes still attached is the concrete case, and it is the
    /// exact damage a wrong [`StatusLen`] does.
    #[test]
    fn a_digest_with_status_bytes_still_attached_does_not_verify() {
        let image = alloc::vec![0x5A; 3];
        let mut mis_sliced = crate::digest::md5_hex(&image);
        mis_sliced.extend_from_slice(&[0x00, 0x00]);
        let mut session = session_for(image);
        let (_, outcome) = run(&mut session, &mis_sliced);
        assert!(
            matches!(outcome, Step::Failed(Error::NotVerified { .. })),
            "a digest that is right except for its length must not verify: {outcome:?}"
        );
    }

    /// An image spanning several blocks sends one data command per block, and the LAST one is short
    /// rather than padded -- padding would write bytes past the image, into flash the caller may be
    /// using for something else.
    #[test]
    fn the_last_block_is_short_not_padded() {
        let image_len = BLOCK * 2 + 7;
        let session = session_for(alloc::vec![0xCD; image_len]);
        assert_eq!(session.total_blocks(), 3);
        let mut lengths = Vec::new();
        for _ in 0..3 {
            let n = lengths.len();
            let (header, block) = session.block_parts(n);
            let declared = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
            lengths.push((declared as usize, block.len()));
        }
        assert_eq!(lengths, alloc::vec![(BLOCK, BLOCK), (BLOCK, BLOCK), (7, 7)]);
    }

    /// A target that never answers the establishing command fails with a reason a user can act on,
    /// after a bounded number of attempts -- not by hanging.
    #[test]
    fn a_silent_target_fails_as_no_response_rather_than_hanging() {
        let mut session = session_for(alloc::vec![0x00; 16]);
        for _ in 0..200 {
            match session.poll() {
                Step::Failed(problem) => {
                    assert_eq!(problem, Error::NoResponse);
                    return;
                }
                Step::Await { .. } => session.timeout(),
                _ => {}
            }
        }
        panic!("a silent target should have failed by now");
    }

    /// A rejection names the command and the error byte, so a caller reports WHICH step the target
    /// refused rather than "flashing failed".
    #[test]
    fn a_rejection_names_the_command_and_the_error() {
        let mut session = session_for(alloc::vec![0x00; 16]);
        loop {
            match session.poll() {
                Step::Do(Action::Write(bytes)) => {
                    let body = crate::unescape(&bytes[1..bytes.len() - 1]);
                    if body[1] == Command::Sync as u8 {
                        session.feed(&ok_response(Command::Sync as u8, &[], StatusLen::FOUR));
                    } else {
                        let mut packet = alloc::vec![0x01, body[1]];
                        packet.extend_from_slice(&4u16.to_le_bytes());
                        packet.extend_from_slice(&0u32.to_le_bytes());
                        packet.extend_from_slice(&[0x01, 0x05, 0x00, 0x00]);
                        session.feed(&crate::frame(&packet));
                    }
                }
                Step::Failed(problem) => {
                    assert_eq!(
                        problem,
                        Error::Rejected { command: Command::SpiAttach as u8, error: 0x05 }
                    );
                    return;
                }
                _ => {}
            }
        }
    }

    /// An image that would run past the end of the chip is refused BEFORE anything is written --
    /// a write that overruns has already erased whatever it reached.
    #[test]
    fn an_oversized_image_is_refused_before_the_first_byte_moves() {
        let mut session = Session::write_flash(
            Connector::UartBridge,
            Dialect::ESP32C6,
            FlashParams::serial_nor(1024),
            512,
            alloc::vec![0x00; 1024],
        );
        assert_eq!(
            session.poll(),
            Step::Failed(Error::DoesNotFit { end: 1536, capacity: 1024 })
        );
    }

    /// **A DEFECT SILICON FOUND: a data command's checksum covers the BLOCK, not the payload
    /// that carries it.** Computing it over both halves is refused, and refused with the target's
    /// checksum error -- which points at the arithmetic and says nothing about the extent, so the error
    /// actively misleads. The XOR fold made this worse than a random mistake would be: it is
    /// order-independent, so the wrong extent still produces a stable, plausible-looking byte.
    ///
    /// Asserted by recovering the checksum from the emitted frame and requiring it to equal the fold
    /// over the block, AND to differ from the fold over the whole payload. The second half is the red
    /// proof: without it, a session that checksummed everything would pass.
    #[test]
    fn a_data_command_checksums_the_block_and_not_its_header_words() {
        let image: Vec<u8> = (0..(BLOCK + 100)).map(|i| (i % 253) as u8).collect();
        let session = session_for(image);
        let (header, block) = session.block_parts(1);
        assert!(header.iter().any(|&b| b != 0), "the header words must not be all zero");

        let framed = crate::data_request(Command::FlashData, &header, block);
        let body = crate::unescape(&framed[1..framed.len() - 1]);
        let carried = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);

        let over_block = u32::from(crate::checksum(block));
        let mut whole = header.clone();
        whole.extend_from_slice(block);
        let over_everything = u32::from(crate::checksum(&whole));

        assert_eq!(carried, over_block, "the checksum is the fold over the block");
        assert_ne!(
            over_block, over_everything,
            "the two extents must differ here, or this test proves nothing"
        );
        assert_eq!(
            usize::from(u16::from_le_bytes([body[2], body[3]])),
            header.len() + block.len(),
            "the declared length covers the header words too"
        );
    }

    /// **A DEFECT SILICON FOUND: some parts want a fifth argument word on the write
    /// declaration.** Four words are refused as an invalid message, which names nothing about counts.
    /// Both arms are asserted, because sending five to a part that wants four is the same mistake in
    /// the other direction.
    #[test]
    fn the_write_declaration_follows_the_dialect_on_its_argument_count() {
        let image = alloc::vec![0x11; 10];
        for (takes_fifth, expected_words) in [(true, 5), (false, 4)] {
            let session = Session::write_flash(
                Connector::UartBridge,
                Dialect { status_len: StatusLen::FOUR, flash_begin_takes_fifth_word: takes_fifth },
                FlashParams::serial_nor(8 * 1024 * 1024),
                0x20_0000,
                image.clone(),
            );
            let args = session.flash_begin_args();
            assert_eq!(
                args.len(),
                expected_words * 4,
                "flash_begin_takes_fifth_word = {takes_fifth} means {expected_words} words"
            );
            assert_eq!(
                u32::from_le_bytes([args[0], args[1], args[2], args[3]]),
                image.len() as u32,
                "the declared size stays first"
            );
        }
    }

    /// **THE DEFECT SILICON FOUND, and no host-side test could have.** A real target answers ONE
    /// establishing command EIGHT times; the first reply advances the session, and the surplus seven
    /// arrive while the next command is outstanding. A test target that answers each request once does
    /// not have surplus replies, so this shape exists only against a part.
    ///
    /// The session must survive it, and this drives the whole prologue with a target that behaves the
    /// way the measured one does.
    #[test]
    fn surplus_answers_to_the_establishing_command_do_not_desynchronize_the_session() {
        let image = alloc::vec![0x77; 32];
        let truthful = crate::digest::md5_hex(&image);
        let mut session = session_for(image);
        let mut saw_attach = false;
        for _ in 0..10_000 {
            match session.poll() {
                Step::Do(Action::Write(bytes)) => {
                    let body = crate::unescape(&bytes[1..bytes.len() - 1]);
                    let command = body[1];
                    if command == Command::Sync as u8 {
                        let mut burst = Vec::new();
                        for _ in 0..8 {
                            burst.extend_from_slice(&ok_response(
                                Command::Sync as u8,
                                &[],
                                StatusLen::FOUR,
                            ));
                        }
                        session.feed(&burst);
                        continue;
                    }
                    if command == Command::SpiAttach as u8 {
                        saw_attach = true;
                    }
                    let payload: &[u8] =
                        if command == Command::SpiFlashMd5 as u8 { &truthful } else { &[] };
                    let mut packet = alloc::vec![0x01, command];
                    packet.extend_from_slice(&((payload.len() + 4) as u16).to_le_bytes());
                    packet.extend_from_slice(&0u32.to_le_bytes());
                    packet.extend_from_slice(payload);
                    packet.extend_from_slice(&[0u8; 4]);
                    session.feed(&crate::frame(&packet));
                }
                Step::Done { .. } => {
                    assert!(saw_attach, "the session reached the attach it was stopping before");
                    return;
                }
                Step::Failed(problem) => {
                    panic!("surplus sync replies must not stop the session: {problem:?}")
                }
                Step::Do(_) | Step::Await { .. } => {}
            }
        }
        panic!("session did not finish");
    }

    /// An answer to a command that is not outstanding desynchronizes the conversation, and that is
    /// reported distinctly from a corrupt packet -- the remedies differ. **Tolerating a late `Sync`
    /// must not widen into tolerating anything**, so this stays and uses a different opcode.
    #[test]
    fn an_answer_to_the_wrong_command_is_out_of_step() {
        let mut session = session_for(alloc::vec![0x00; 16]);
        loop {
            match session.poll() {
                Step::Do(Action::Write(bytes)) => {
                    let body = crate::unescape(&bytes[1..bytes.len() - 1]);
                    if body[1] == Command::Sync as u8 {
                        session.feed(&ok_response(Command::Sync as u8, &[], StatusLen::FOUR));
                    } else {
                        session.feed(&ok_response(Command::FlashEnd as u8, &[], StatusLen::FOUR));
                    }
                }
                Step::Failed(Error::OutOfStep { expected, got }) => {
                    assert_eq!(expected, Command::SpiAttach as u8);
                    assert_eq!(got, Command::FlashEnd as u8);
                    return;
                }
                Step::Failed(other) => panic!("wrong failure: {other:?}"),
                _ => {}
            }
        }
    }

    /// The session resets the part into download mode before speaking, and back into flash when done,
    /// so a caller that drives it to completion leaves a running board rather than a parked one.
    #[test]
    fn a_session_brackets_the_write_with_resets() {
        let image = alloc::vec![0x00; 16];
        let truthful = crate::digest::md5_hex(&image);
        let mut session = session_for(image);
        let mut signals_before_sync = 0;
        let mut saw_sync = false;
        let mut signals_after_end = 0;
        let mut saw_end = false;
        for _ in 0..10_000 {
            match session.poll() {
                Step::Do(Action::SetDtr(_) | Action::SetRts(_)) => {
                    if !saw_sync {
                        signals_before_sync += 1;
                    } else if saw_end {
                        signals_after_end += 1;
                    }
                }
                Step::Do(Action::Write(bytes)) => {
                    let body = crate::unescape(&bytes[1..bytes.len() - 1]);
                    let command = body[1];
                    if command == Command::Sync as u8 {
                        saw_sync = true;
                    }
                    if command == Command::FlashEnd as u8 {
                        saw_end = true;
                    }
                    let payload: &[u8] =
                        if command == Command::SpiFlashMd5 as u8 { &truthful } else { &[] };
                    let mut packet = alloc::vec![0x01, command];
                    packet.extend_from_slice(&((payload.len() + 4) as u16).to_le_bytes());
                    packet.extend_from_slice(&0u32.to_le_bytes());
                    packet.extend_from_slice(payload);
                    packet.extend_from_slice(&[0u8; 4]);
                    session.feed(&crate::frame(&packet));
                }
                Step::Do(_) | Step::Await { .. } => {}
                Step::Done { .. } => break,
                Step::Failed(problem) => panic!("unexpected failure: {problem:?}"),
            }
        }
        assert!(signals_before_sync >= 4, "a reset into download mode precedes the first command");
        assert!(signals_after_end >= 3, "a reset back into flash follows the last one");
    }
}
