//! The control path: the sixteen-byte control header inside a control
//! frame, one request outstanding at a time and its reply matched by a
//! request id, the two request kinds (a numbered command; a named
//! variable), the regulatory blob's chunked download, and the opening
//! sequence that configures the running firmware, asks its version
//! string and tells it which events to deliver.
//!
//! A control frame is the frame header (the payload at offset 12, channel
//! 0), the control header (the command number; the length of everything
//! after the control header; the flags; the status), a variable's name
//! NUL-terminated when the request names one, and the payload. The flags
//! carry the set bit (bit 1), the interface index (bits 15:12) and the
//! request id (bits 31:16), a counter incremented before every request;
//! the reply echoes the id, may set the error bit (bit 0), and carries the
//! firmware's status, non-zero on failure. A get of a named variable sends
//! the name and receives the value in its place, right after the control
//! header. Only one request is outstanding at a time; a reply that does not
//! carry the outstanding id is dropped.

use crate::backplane::Window;
use crate::clock::Micros;
use crate::cores::{SDIOD, sdiod};
use crate::driver::Wake;
use crate::error::Refusal;
use crate::event::{Event, EventMask, MASK_LEN};
use crate::frame::{FRAME_BUF, HEADER_LEN, Header, Layer, Served, channel};
use crate::link::{
    KEY_LEN, Passphrase, SAE_PASSWORD_LEN, SSID_STRUCT_LEN, SaePassword, key_structure,
    sae_password_structure, ssid_structure,
};
use crate::scan::{PARAMS_LEN, params};
use crate::transport::Transport;

/// The control header's length in bytes.
pub const CONTROL_HEADER_LEN: usize = 16;
/// The bytes of a control frame before its name or payload: the frame
/// header and the control header.
pub const REQUEST_HEAD: usize = HEADER_LEN + CONTROL_HEADER_LEN;
/// The station interface's index.
pub const IFACE_STATION: u32 = 0;
/// The capacity of the version query's reply, in bytes.
pub const VERSION_MAX: usize = 64;
/// The capacity of the capability query's reply, in bytes: the firmware's
/// capability string is a space-separated list of words, some hundreds of
/// bytes long, and a capacity under its length is answered with a status,
/// not the string.
pub const CAPABILITIES_MAX: usize = 768;
/// The regulatory download's largest chunk, in bytes.
pub const REGULATORY_CHUNK: usize = 1400;
/// The regulatory download's chunk header length.
pub const DOWNLOAD_HEADER_LEN: usize = 12;

/// The cadence at which a waiting driver asks to be called again: at the
/// interrupt, or this much later.
pub const SERVICE_POLL_US: Micros = 10_000;
/// The bound on waiting to transmit: for the credit window to open, or for
/// the chip's receive side to accept the frame.
pub const SEND_WAIT_US: Micros = 3_000_000;
/// The bound on waiting for a reply.
pub const REPLY_WAIT_US: Micros = 10_000_000;

/// Command numbers.
pub mod cmd {
    /// Bring the radio up.
    pub const UP: u32 = 2;
    /// Set the infrastructure mode.
    pub const SET_INFRA: u32 = 20;
    /// Set the 802.11 authentication type.
    pub const SET_AUTH: u32 = 22;
    /// Set the network name: the trigger of a join.
    pub const SET_SSID: u32 = 26;
    /// Select an active or a passive scan.
    pub const SET_PASSIVE_SCAN: u32 = 49;
    /// Disassociate.
    pub const DISASSOC: u32 = 52;
    /// Set the power-save mode.
    pub const SET_PM: u32 = 86;
    /// Set the 802.11g mode.
    pub const SET_GMODE: u32 = 110;
    /// Set the cipher.
    pub const SET_WSEC: u32 = 134;
    /// Set the authentication mode.
    pub const SET_WPA_AUTH: u32 = 165;
    /// Get a named variable.
    pub const GET_VAR: u32 = 262;
    /// Set a named variable.
    pub const SET_VAR: u32 = 263;
    /// Set the passphrase the chip derives the key from.
    pub const SET_WSEC_PMK: u32 = 268;
}

/// The status word the firmware answers while its interface is not up.
pub const STATUS_NOT_UP: u32 = 0xFFFF_FFFC;

/// Variable names.
pub mod var {
    /// The firmware's version string.
    pub const VERSION: &[u8] = b"ver";
    /// The chip's own address.
    pub const ADDRESS: &[u8] = b"cur_etheraddr";
    /// The enhanced scan.
    pub const SCAN: &[u8] = b"escan";
    /// The on-chip supplicant, addressed to a BSS configuration: its
    /// value follows a 32-bit interface index.
    pub const SUP_WPA: &[u8] = b"bsscfg:sup_wpa";
    /// The regulatory blob's chunked download.
    pub const REGULATORY: &[u8] = b"clmload";
    /// Frame aggregation on the packet channel.
    pub const GLOM: &[u8] = b"bus:txglom";
    /// Roaming.
    pub const ROAM_OFF: &[u8] = b"roam_off";
    /// The supplicant's EAPOL version, addressed to a BSS configuration:
    /// its value follows a 32-bit interface index.
    pub const SUP_WPA2_EAPVER: &[u8] = b"bsscfg:sup_wpa2_eapver";
    /// The events the firmware delivers: the event mask.
    pub const EVENT_MASK: &[u8] = b"event_msgs";
    /// The protection of management frames: none, capable or required.
    pub const MFP: &[u8] = b"mfp";
    /// The password of a WPA3 network, in the 130-byte password structure.
    pub const SAE_PASSWORD: &[u8] = b"sae_password";
    /// The firmware's capability string: its feature words, space-separated.
    pub const CAPABILITIES: &[u8] = b"cap";
}

/// The fields of the control header's flags word.
pub mod flags {
    /// Set by the firmware in a reply that failed.
    pub const ERROR: u32 = 1 << 0;
    /// A set; clear for a get.
    pub const SET: u32 = 1 << 1;
    /// The interface index's shift.
    pub const IFACE_SHIFT: u32 = 12;
    /// The request id's shift.
    pub const ID_SHIFT: u32 = 16;
}

/// The fields of the regulatory download's chunk header.
pub mod download {
    /// The downloader's version, in bits 15:12 of the flag.
    pub const VERSION: u16 = 0x1000;
    /// The first chunk.
    pub const BEGIN: u16 = 0x0002;
    /// The last chunk.
    pub const END: u16 = 0x0004;
    /// The regulatory data type.
    pub const TYPE_REGULATORY: u16 = 0x0002;
}

/// The stage name of an expected version string that is empty.
pub const STAGE_EXPECTED_VERSION: &str = "expected version";
/// The stage name of a credit window that never opened.
pub const STAGE_CREDIT: &str = "transmit credit";
/// The stage name of a chip whose receive side never accepted the frame.
pub const STAGE_TRANSMIT_READY: &str = "transmit ready";
/// The stage name of a reply that never came.
pub const STAGE_REPLY_TIMEOUT: &str = "control reply timeout";
/// The stage name of a regulatory chunk the firmware refused.
pub const STAGE_REGULATORY: &str = "regulatory download";
/// The stage name of the glomming setting refused.
pub const STAGE_GLOM: &str = "glomming off";
/// The stage name of the power-save setting refused.
pub const STAGE_POWER_SAVE: &str = "power save off";
/// The stage name of the G-mode setting refused.
pub const STAGE_GMODE: &str = "G mode auto";
/// The stage name of the roaming setting refused.
pub const STAGE_ROAM: &str = "roaming off";
/// The stage name of the supplicant's EAPOL version refused.
pub const STAGE_EAPOL: &str = "supplicant EAPOL version";
/// The stage name of the version query refused.
pub const STAGE_VERSION_QUERY: &str = "firmware version query";
/// The stage name of a version string without the expected one in it.
pub const STAGE_VERSION_MISMATCH: &str = "firmware version mismatch";
/// The stage name of the event mask refused.
pub const STAGE_EVENT_MASK: &str = "event mask";

/// The control header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ControlHeader {
    /// The command number.
    pub cmd: u32,
    /// The bytes after this header: the name and the payload.
    pub len: u32,
    /// The flags.
    pub flags: u32,
    /// The status: 0 in a request; the firmware's word in a reply.
    pub status: u32,
}

impl ControlHeader {
    /// A request's header for the station interface.
    pub const fn request(cmd: u32, len: u32, set: bool, id: u16) -> Self {
        let mut flags = ((id as u32) << flags::ID_SHIFT) | (IFACE_STATION << flags::IFACE_SHIFT);
        if set {
            flags |= flags::SET;
        }
        ControlHeader {
            cmd,
            len,
            flags,
            status: 0,
        }
    }

    /// The request id in the flags.
    pub const fn id(&self) -> u16 {
        (self.flags >> flags::ID_SHIFT) as u16
    }

    /// Whether the error flag is set.
    pub const fn is_error(&self) -> bool {
        self.flags & flags::ERROR != 0
    }

    /// Write the sixteen bytes at the head of `into`, little-endian.
    pub fn write(&self, into: &mut [u8]) {
        into[0..4].copy_from_slice(&self.cmd.to_le_bytes());
        into[4..8].copy_from_slice(&self.len.to_le_bytes());
        into[8..12].copy_from_slice(&self.flags.to_le_bytes());
        into[12..16].copy_from_slice(&self.status.to_le_bytes());
    }

    /// The header at the head of `bytes`, or `None` for fewer than sixteen.
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < CONTROL_HEADER_LEN {
            return None;
        }
        let word = |at: usize| {
            u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
        };
        Some(ControlHeader {
            cmd: word(0),
            len: word(4),
            flags: word(8),
            status: word(12),
        })
    }
}

/// The regulatory download's chunk header: the flag (the downloader's
/// version, the first and last marks), the type, the chunk's length, the
/// unused checksum.
pub fn download_header(first: bool, last: bool, len: u32) -> [u8; DOWNLOAD_HEADER_LEN] {
    let mut flag = download::VERSION;
    if first {
        flag |= download::BEGIN;
    }
    if last {
        flag |= download::END;
    }
    let mut header = [0u8; DOWNLOAD_HEADER_LEN];
    header[0..2].copy_from_slice(&flag.to_le_bytes());
    header[2..4].copy_from_slice(&download::TYPE_REGULATORY.to_le_bytes());
    header[4..8].copy_from_slice(&len.to_le_bytes());
    header
}

/// A length rounded up to a multiple of eight.
pub const fn round8(len: usize) -> usize {
    (len + 7) & !7
}

/// The bytes of the chunk at `at` of a blob of `blob_len` bytes.
pub const fn chunk_len(blob_len: usize, at: usize) -> usize {
    let rest = blob_len - at;
    if rest < REGULATORY_CHUNK {
        rest
    } else {
        REGULATORY_CHUNK
    }
}

/// A request's payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Payload<'b> {
    /// One 32-bit word.
    Word(u32),
    /// Two 32-bit words.
    Words(u32, u32),
    /// Zero bytes: a reply's capacity.
    Zeros(usize),
    /// The chunk of the regulatory blob at `at`, under its download
    /// header, the value padded with zeros to a multiple of eight bytes.
    Chunk {
        /// The blob.
        blob: &'b [u8],
        /// The chunk's offset in the blob.
        at: usize,
    },
    /// The event mask's bytes.
    Mask(EventMask),
    /// The 68-byte key structure carrying a passphrase.
    Key(Passphrase<'b>),
    /// The 130-byte password structure carrying a WPA3 password.
    SaePassword(SaePassword<'b>),
    /// The 36-byte SSID structure.
    Ssid(&'b [u8]),
    /// The 74-byte parameters of a scan tagged with a sync id.
    Scan {
        /// The sync id the results echo.
        sync: u16,
    },
}

impl Payload<'_> {
    /// The payload's length in bytes.
    pub const fn len(&self) -> usize {
        match *self {
            Payload::Word(_) => 4,
            Payload::Words(_, _) => 8,
            Payload::Zeros(n) => n,
            Payload::Chunk { blob, at } => round8(DOWNLOAD_HEADER_LEN + chunk_len(blob.len(), at)),
            Payload::Mask(_) => MASK_LEN,
            Payload::Key(_) => KEY_LEN,
            Payload::SaePassword(_) => SAE_PASSWORD_LEN,
            Payload::Ssid(_) => SSID_STRUCT_LEN,
            Payload::Scan { .. } => PARAMS_LEN,
        }
    }

    /// Whether the payload has no bytes.
    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Write the payload at the head of `into`.
    pub fn write(&self, into: &mut [u8]) {
        match *self {
            Payload::Word(word) => into[..4].copy_from_slice(&word.to_le_bytes()),
            Payload::Words(first, second) => {
                into[..4].copy_from_slice(&first.to_le_bytes());
                into[4..8].copy_from_slice(&second.to_le_bytes());
            }
            Payload::Zeros(n) => into[..n].fill(0),
            Payload::Chunk { blob, at } => {
                let n = chunk_len(blob.len(), at);
                let total = round8(DOWNLOAD_HEADER_LEN + n);
                let header = download_header(at == 0, at + n == blob.len(), n as u32);
                into[..DOWNLOAD_HEADER_LEN].copy_from_slice(&header);
                into[DOWNLOAD_HEADER_LEN..DOWNLOAD_HEADER_LEN + n]
                    .copy_from_slice(&blob[at..at + n]);
                into[DOWNLOAD_HEADER_LEN + n..total].fill(0);
            }
            Payload::Mask(mask) => into[..MASK_LEN].copy_from_slice(mask.as_bytes()),
            Payload::Key(passphrase) => into[..KEY_LEN].copy_from_slice(&key_structure(passphrase)),
            Payload::SaePassword(password) => {
                into[..SAE_PASSWORD_LEN].copy_from_slice(&sae_password_structure(password));
            }
            Payload::Ssid(ssid) => into[..SSID_STRUCT_LEN].copy_from_slice(&ssid_structure(ssid)),
            Payload::Scan { sync } => into[..PARAMS_LEN].copy_from_slice(&params(sync)),
        }
    }
}

/// One request: the stage name a refusal carries, its kind, its command
/// number, the variable's name (empty for a numbered command) and its
/// payload.
#[derive(Clone, Copy, Debug)]
pub struct Request<'b> {
    /// The stage name of a reply that refuses.
    pub stage: &'static str,
    /// A set (`true`) or a get.
    pub set: bool,
    /// The command number.
    pub cmd: u32,
    /// The variable's name, empty for a numbered command.
    pub name: &'static [u8],
    /// The payload.
    pub payload: Payload<'b>,
}

impl Request<'_> {
    const fn name_len(&self) -> usize {
        if self.name.is_empty() {
            0
        } else {
            self.name.len() + 1
        }
    }

    /// The frame's length: the two headers, the name with its NUL, the
    /// payload.
    pub const fn frame_len(&self) -> usize {
        REQUEST_HEAD + self.name_len() + self.payload.len()
    }

    /// Build the frame at the head of `buf` with the transmit sequence and
    /// the request id; returns the frame's length.
    pub fn build(&self, buf: &mut [u8], sequence: u8, id: u16) -> usize {
        let name_len = self.name_len();
        let payload_len = self.payload.len();
        let total = REQUEST_HEAD + name_len + payload_len;
        Header::transmit(total as u16, sequence, channel::CONTROL, HEADER_LEN as u8).write(buf);
        ControlHeader::request(self.cmd, (name_len + payload_len) as u32, self.set, id)
            .write(&mut buf[HEADER_LEN..]);
        let mut at = REQUEST_HEAD;
        if name_len > 0 {
            buf[at..at + self.name.len()].copy_from_slice(self.name);
            buf[at + self.name.len()] = 0;
            at += name_len;
        }
        self.payload.write(&mut buf[at..total]);
        total
    }
}

/// A reply matched to the outstanding request: the status and the flags,
/// and where its payload lies in the buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reply {
    /// The firmware's status word.
    pub status: u32,
    /// The flags word.
    pub flags: u32,
    /// The payload's offset in the frame.
    pub at: usize,
    /// The payload's length: the reply's own count.
    pub len: usize,
}

/// The reply in a control frame whose payload starts at `data_offset`,
/// when it is the outstanding request's: the frame must hold the control
/// header, the header's count must not exceed the bytes present, and the
/// id must be `id`.
pub fn take_reply(frame: &[u8], data_offset: usize, id: u16) -> Option<Reply> {
    let header = ControlHeader::parse(frame.get(data_offset..)?)?;
    let at = data_offset + CONTROL_HEADER_LEN;
    let len = header.len as usize;
    if len > frame.len() - at || header.id() != id {
        return None;
    }
    Some(Reply {
        status: header.status,
        flags: header.flags,
        at,
        len,
    })
}

/// The firmware's version string as the version query answered it, with
/// its trailing NUL, carriage-return and line-feed bytes stripped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Version {
    bytes: [u8; VERSION_MAX],
    len: u8,
}

impl Version {
    /// No string.
    pub const EMPTY: Version = Version {
        bytes: [0; VERSION_MAX],
        len: 0,
    };

    /// The string from a reply's payload: at most the capacity, stripped.
    pub fn from_reply(reply: &[u8]) -> Self {
        let mut bytes = [0u8; VERSION_MAX];
        let mut len = reply.len().min(VERSION_MAX);
        bytes[..len].copy_from_slice(&reply[..len]);
        while len > 0 && matches!(bytes[len - 1], 0 | b'\n' | b'\r') {
            len -= 1;
        }
        bytes[len..].fill(0);
        Version {
            bytes,
            len: len as u8,
        }
    }

    /// The string's bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }

    /// The string as text, when it is valid UTF-8.
    pub fn as_str(&self) -> Option<&str> {
        core::str::from_utf8(self.as_bytes()).ok()
    }

    /// The string's length.
    pub const fn len(&self) -> usize {
        self.len as usize
    }

    /// Whether there is no string.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Whether the string contains `expected` (never for an empty
    /// `expected`).
    pub fn contains(&self, expected: &[u8]) -> bool {
        !expected.is_empty()
            && self
                .as_bytes()
                .windows(expected.len())
                .any(|w| w == expected)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Send { since: Micros, total: usize },
    Wait { deadline: Micros },
}

/// One request and its reply, as a machine the caller steps: the send,
/// gated on the credit window with the receive side serviced meanwhile and
/// bounded in time; then the wait for the reply that carries the request's
/// id, other frames consumed meanwhile, bounded in time. An event frame
/// taken meanwhile is handed back, the step unchanged. A frame carrying a
/// key structure or a password structure is cleared from the buffer once
/// the bus has taken it.
#[derive(Clone, Copy, Debug)]
pub struct Exchange<'b> {
    request: Request<'b>,
    step: Step,
    id: u16,
}

/// What a step of an exchange handed back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Progress {
    /// A wake to return.
    Wake(Wake),
    /// An event was taken meanwhile; the request is still outstanding.
    Event(Event),
    /// A data frame was taken meanwhile, its Ethernet frame of this length
    /// in the buffer; the request is still outstanding.
    Data(usize),
    /// The reply is in the buffer.
    Done(Reply),
}

impl<'b> Exchange<'b> {
    /// An exchange for `request`, its send wait starting at `now`.
    pub const fn new(request: Request<'b>, now: Micros) -> Self {
        Exchange {
            request,
            step: Step::Send {
                since: now,
                total: 0,
            },
            id: 0,
        }
    }

    /// The request.
    pub const fn request(&self) -> &Request<'b> {
        &self.request
    }

    /// The request id, once the frame is built; 0 before.
    pub const fn id(&self) -> u16 {
        self.id
    }

    /// Whether the request's frame is built and not yet accepted by the
    /// chip: it holds an issued sequence number and goes out before any
    /// other frame.
    pub const fn is_sending(&self) -> bool {
        matches!(self.step, Step::Send { total, .. } if total != 0)
    }

    /// One step: at most one frame written or read, plus the service
    /// pass's register accesses.
    pub fn step<T: Transport>(
        &mut self,
        bus: &mut T,
        window: &mut Window,
        buf: &mut [u8],
        layer: &mut Layer,
        ids: &mut u16,
        now: Micros,
    ) -> Result<Progress, Refusal> {
        match self.step {
            Step::Send { since, total } => {
                if now >= since + SEND_WAIT_US {
                    let stage = if layer.credit.open() {
                        STAGE_TRANSMIT_READY
                    } else {
                        STAGE_CREDIT
                    };
                    return Err(Refusal::new(stage, layer.credit.word()));
                }
                let total = if total == 0 {
                    if !layer.credit.open() {
                        return Ok(match layer.service(bus, window, buf)? {
                            Served::Nothing => Progress::Wake(Wake::Irq(now + SERVICE_POLL_US)),
                            Served::Consumed => Progress::Wake(Wake::Again),
                            Served::Control(_) => {
                                layer.frames.dropped += 1;
                                Progress::Wake(Wake::Again)
                            }
                            Served::Event(event) => Progress::Event(event),
                            Served::Data { len } => Progress::Data(len),
                        });
                    }
                    let sequence = layer.credit.issue();
                    *ids = ids.wrapping_add(1);
                    self.id = *ids;
                    let total = self.request.build(buf, sequence, self.id);
                    self.step = Step::Send { since, total };
                    total
                } else {
                    total
                };
                if bus.f2_write(&buf[..total])? {
                    layer.frames.sent += 1;
                    if matches!(
                        self.request.payload,
                        Payload::Key(_) | Payload::SaePassword(_)
                    ) {
                        buf[..total].fill(0);
                    }
                    self.step = Step::Wait {
                        deadline: now + REPLY_WAIT_US,
                    };
                    Ok(Progress::Wake(Wake::Again))
                } else {
                    Ok(Progress::Wake(Wake::At(now + SERVICE_POLL_US)))
                }
            }
            Step::Wait { deadline } => {
                if now >= deadline {
                    let word = (u32::from(self.id) << flags::ID_SHIFT) | self.request.cmd;
                    return Err(Refusal::new(STAGE_REPLY_TIMEOUT, word));
                }
                match layer.service(bus, window, buf)? {
                    Served::Nothing => Ok(Progress::Wake(Wake::Irq(now + SERVICE_POLL_US))),
                    Served::Consumed => Ok(Progress::Wake(Wake::Again)),
                    Served::Event(event) => Ok(Progress::Event(event)),
                    Served::Data { len } => Ok(Progress::Data(len)),
                    Served::Control(header) => {
                        let frame = &buf[..usize::from(header.size)];
                        match take_reply(frame, usize::from(header.data_offset), self.id) {
                            Some(reply) => Ok(Progress::Done(reply)),
                            None => {
                                layer.frames.dropped += 1;
                                Ok(Progress::Wake(Wake::Again))
                            }
                        }
                    }
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Setup,
    Regulatory { at: usize },
    Configure { step: u8 },
    Version,
    Events,
}

/// The version query: a get of the firmware's version string into a
/// 64-byte reply.
const VERSION_QUERY: Request<'static> = Request {
    stage: STAGE_VERSION_QUERY,
    set: false,
    cmd: cmd::GET_VAR,
    name: var::VERSION,
    payload: Payload::Zeros(VERSION_MAX),
};

/// The configuration settings of the opening sequence, in order: frame
/// aggregation off, power save off, 802.11g mode automatic, roaming off,
/// the supplicant's EAPOL version at its default.
const CONFIGURE: [Request<'static>; 5] = [
    Request {
        stage: STAGE_GLOM,
        set: true,
        cmd: cmd::SET_VAR,
        name: var::GLOM,
        payload: Payload::Word(0),
    },
    Request {
        stage: STAGE_POWER_SAVE,
        set: true,
        cmd: cmd::SET_PM,
        name: b"",
        payload: Payload::Word(0),
    },
    Request {
        stage: STAGE_GMODE,
        set: true,
        cmd: cmd::SET_GMODE,
        name: b"",
        payload: Payload::Word(1),
    },
    Request {
        stage: STAGE_ROAM,
        set: true,
        cmd: cmd::SET_VAR,
        name: var::ROAM_OFF,
        payload: Payload::Word(1),
    },
    Request {
        stage: STAGE_EAPOL,
        set: true,
        cmd: cmd::SET_VAR,
        name: var::SUP_WPA2_EAPVER,
        payload: Payload::Words(IFACE_STATION, 0xFFFF_FFFF),
    },
];

/// The largest request, a regulatory chunk, fits the frame buffer.
const _: () = assert!(
    REQUEST_HEAD + var::REGULATORY.len() + 1 + round8(DOWNLOAD_HEADER_LEN + REGULATORY_CHUNK)
        <= FRAME_BUF
);
/// The capability query's frame fits it too.
const _: () = assert!(REQUEST_HEAD + var::CAPABILITIES.len() + 1 + CAPABILITIES_MAX <= FRAME_BUF);

/// The opening sequence: the interrupt setup, the regulatory download, the
/// five configuration settings, the version query, the event mask.
#[derive(Clone, Copy, Debug)]
pub struct Opening<'b> {
    regulatory: &'b [u8],
    expected: Option<&'b [u8]>,
    events: EventMask,
    phase: Phase,
    exchange: Option<Exchange<'b>>,
}

/// What a step of the opening handed back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenStep {
    /// A wake to return.
    Wake(Wake),
    /// An event was taken meanwhile; the opening continues.
    Event(Event),
    /// The control path answers and the firmware delivers the events
    /// asked for: the firmware's version string.
    Ready(Version),
}

impl<'b> Opening<'b> {
    /// The opening with the regulatory blob (empty: no download), the
    /// expected version string (`None`: no check) and the events to ask
    /// for.
    pub const fn new(regulatory: &'b [u8], expected: Option<&'b [u8]>, events: EventMask) -> Self {
        Opening {
            regulatory,
            expected,
            events,
            phase: Phase::Setup,
            exchange: None,
        }
    }

    fn request(&self) -> Request<'b> {
        match self.phase {
            Phase::Regulatory { at } => Request {
                stage: STAGE_REGULATORY,
                set: true,
                cmd: cmd::SET_VAR,
                name: var::REGULATORY,
                payload: Payload::Chunk {
                    blob: self.regulatory,
                    at,
                },
            },
            Phase::Configure { step } => CONFIGURE[usize::from(step)],
            Phase::Events => Request {
                stage: STAGE_EVENT_MASK,
                set: true,
                cmd: cmd::SET_VAR,
                name: var::EVENT_MASK,
                payload: Payload::Mask(self.events),
            },
            Phase::Setup | Phase::Version => VERSION_QUERY,
        }
    }

    fn advance(&mut self) {
        self.phase = match self.phase {
            Phase::Setup => {
                if self.regulatory.is_empty() {
                    Phase::Configure { step: 0 }
                } else {
                    Phase::Regulatory { at: 0 }
                }
            }
            Phase::Regulatory { at } => {
                let at = at + chunk_len(self.regulatory.len(), at);
                if at == self.regulatory.len() {
                    Phase::Configure { step: 0 }
                } else {
                    Phase::Regulatory { at }
                }
            }
            Phase::Configure { step } => {
                if usize::from(step) + 1 < CONFIGURE.len() {
                    Phase::Configure { step: step + 1 }
                } else {
                    Phase::Version
                }
            }
            Phase::Version => Phase::Events,
            Phase::Events => Phase::Events,
        };
    }

    /// One step: the setup in the first call; then one step of the current
    /// exchange, its reply checked and the next exchange built; an event
    /// taken meanwhile handed back with the exchange unchanged.
    #[allow(clippy::too_many_arguments)]
    pub fn step<T: Transport>(
        &mut self,
        bus: &mut T,
        window: &mut Window,
        buf: &mut [u8],
        layer: &mut Layer,
        ids: &mut u16,
        now: Micros,
        version: &mut Version,
    ) -> Result<OpenStep, Refusal> {
        if self.phase == Phase::Setup {
            if let Some(expected) = self.expected
                && expected.is_empty()
            {
                return Err(Refusal::new(STAGE_EXPECTED_VERSION, 0));
            }
            let mailbox = bus.f2_interrupt_setup()?;
            layer.set_mailbox(mailbox);
            if mailbox {
                window.write32(bus, SDIOD + sdiod::HOSTINTMASK, sdiod::HOST_INTERRUPTS)?;
                window.write8(bus, SDIOD + sdiod::FUNCINTMASK, sdiod::FUNCTION_2_INTERRUPT)?;
            }
            self.advance();
            self.exchange = Some(Exchange::new(self.request(), now));
            return Ok(OpenStep::Wake(Wake::Again));
        }
        if self.exchange.is_none() {
            self.exchange = Some(Exchange::new(self.request(), now));
        }
        let Some(exchange) = &mut self.exchange else {
            return Ok(OpenStep::Wake(Wake::Again));
        };
        let reply = match exchange.step(bus, window, buf, layer, ids, now)? {
            Progress::Wake(wake) => return Ok(OpenStep::Wake(wake)),
            Progress::Event(event) => return Ok(OpenStep::Event(event)),
            Progress::Data(_) => {
                layer.frames.data_dropped += 1;
                layer.forget_data();
                return Ok(OpenStep::Wake(Wake::Again));
            }
            Progress::Done(reply) => reply,
        };
        let stage = exchange.request().stage;
        if reply.status != 0 || reply.flags & flags::ERROR != 0 {
            let word = if reply.status != 0 {
                reply.status
            } else {
                reply.flags
            };
            return Err(Refusal::new(stage, word));
        }
        if self.phase == Phase::Version {
            let n = reply.len.min(VERSION_MAX);
            *version = Version::from_reply(&buf[reply.at..reply.at + n]);
            if let Some(expected) = self.expected
                && !version.contains(expected)
            {
                return Err(Refusal::new(STAGE_VERSION_MISMATCH, version.len() as u32));
            }
        }
        if self.phase == Phase::Events {
            self.exchange = None;
            return Ok(OpenStep::Ready(*version));
        }
        self.advance();
        self.exchange = Some(Exchange::new(self.request(), now));
        Ok(OpenStep::Wake(Wake::Again))
    }
}
