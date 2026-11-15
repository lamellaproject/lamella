//! The station: the requests the caller makes once the driver is ready --
//! the radio up, a scan, a join, a disconnect -- each a sequence of control
//! exchanges and waits on events, one bounded step per poll, and the link
//! machine that keeps a joined network joined.
//!
//! Every event taken while the station is ready passes through one funnel:
//! while the link is up, a loss event -- a deauthentication or
//! disassociation, a link indication with a reason, a supplicant failure --
//! lowers the link inside the handling of that event, so a burst of them
//! moves the machine once and the rest are plain events; a stage that is
//! waiting on an event consumes the one it waits for; every other event
//! reaches the caller as an outcome. A lost link and a failed join both
//! leave the network held and the link down: after a rest the station
//! scans, and when the network was seen re-enters the join at its
//! disassociation step with the secret re-supplied, without a ceiling,
//! until the caller disconnects. A scan the firmware reports complete
//! with the network in none of its records is followed by a second one
//! whose probe requests carry the network's name, so that a network which
//! does not broadcast its name is found; a scan that ended any other way
//! is followed by none, and the next cycle scans afresh. The one failure
//! it does not retry is a recovery scan that sees the network advertising
//! no suite the credential can use, which detaches the network. A failed
//! attempt that is the sign of a refused secret -- on a WPA2 join the
//! supplicant's handshake timeout, on a WPA3 join an association failure
//! behind a failed authentication -- is retried as every other, behind a
//! rest of its own: five seconds, doubled at each such failure in a row up
//! to five minutes, longer by under a fifth once the caller gave a seed,
//! and put back to five seconds by a join that completes. After the radio
//! up, a request the firmware answers with "not up" is retried a bounded
//! number of times, since the radio's settle outlives the event that
//! announces it. On request the station reads the
//! firmware's capability string and keeps its word on whether it runs the
//! SAE exchange itself.

use core::fmt;

use crate::backplane::Window;
use crate::clock::Micros;
use crate::control::{
    CAPABILITIES_MAX, Exchange, Payload, Progress, Reply, Request, SERVICE_POLL_US, STATUS_NOT_UP,
    cmd, flags, var,
};
use crate::driver::{Outcome, Wake};
use crate::error::Refusal;
use crate::event::{Event, Link, number, reason, status};
use crate::frame::{Layer, Served};
use crate::link::{
    self, Credential, JoinFailure, LinkState, Network, STEP_FIRST, STEP_LAST, STEP_PROTECTION,
    STEP_REENTRY, Security,
};
use crate::scan::{self, Advertised, RESULT_HEAD_LEN, ScanEnd, ScanRecord};
use crate::transport::Transport;

/// The bound on the wait for the radio event after the radio up; its
/// expiry is not a failure.
pub const RADIO_WAIT_US: Micros = 1_000_000;
/// The retries of a request the firmware answers with "not up".
pub const NOT_UP_TRIES: u8 = 5;
/// The rest before each such retry.
pub const NOT_UP_RETRY_US: Micros = 100_000;
/// The bound on a scan's results.
pub const SCAN_WAIT_US: Micros = 5_000_000;
/// The bound on a join attempt's completing event.
pub const JOIN_WAIT_US: Micros = 15_000_000;
/// The rest after a lost link before the first recovery cycle.
pub const LOSS_SETTLE_US: Micros = 1_000_000;
/// The rest after a failed or empty recovery cycle before the next.
pub const CYCLE_REST_US: Micros = 5_000_000;
/// The rest behind the first failed attempt that is the sign of a refused
/// secret.
pub const REFUSED_REST_US: Micros = 5_000_000;
/// The longest rest behind such an attempt.
pub const REFUSED_REST_MAX_US: Micros = 300_000_000;
/// The hold after the supplicant's word on a secured join (WPA2-PSK or
/// WPA3-SAE) before the join completes, measured on the CYW43439's
/// firmware 7.95.49: on a WPA3 join a frame written within about a
/// millisecond of the supplicant's word was lost three times of three and
/// frames written 25, 86 and 126 milliseconds after it were carried; on a
/// WPA2 join a frame written within about a millisecond was carried twice
/// and lost once; the hold is twice the smallest delta that carried.
pub const SUPPLICANT_HOLD_US: Micros = 50_000;
/// The address's length.
pub const ADDRESS_LEN: usize = 6;

/// The stage name of the radio up refused.
pub const STAGE_RADIO_UP: &str = "radio up";
/// The stage name of the address query refused, or answered short.
pub const STAGE_ADDRESS: &str = "address query";
/// The stage name of the scan mode refused.
pub const STAGE_SCAN_MODE: &str = "scan mode";
/// The stage name of the scan start refused.
pub const STAGE_SCAN_START: &str = "scan start";
/// The stage name of the scan start answered "not up" past the retries
/// with the radio up: the regulatory data was not loaded.
pub const STAGE_SCAN_REGULATORY: &str = "scan regulatory data";
/// The stage name of the capability query; it never refuses, since a
/// reply that does not answer is handed back as such.
pub const STAGE_CAPABILITIES: &str = "capabilities";

/// The firmware's answer to the capability query: whether it answered,
/// the status word when it did not, the string's length, and the two
/// words looked for in it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Capabilities {
    /// The firmware answered with the string.
    pub answered: bool,
    /// The status word of a reply that did not answer; 0 otherwise.
    pub status: u32,
    /// The string's length in bytes.
    pub len: u16,
    /// The word `sae` is in the string: the firmware runs the SAE exchange
    /// itself.
    pub sae: bool,
    /// The word `mfp` is in the string: the firmware names the protection
    /// of management frames.
    pub mfp: bool,
}

const CAPABILITIES_QUERY: Request<'static> = Request {
    stage: STAGE_CAPABILITIES,
    set: false,
    cmd: cmd::GET_VAR,
    name: var::CAPABILITIES,
    payload: Payload::Zeros(CAPABILITIES_MAX),
};

const RADIO_UP: Request<'static> = Request {
    stage: STAGE_RADIO_UP,
    set: true,
    cmd: cmd::UP,
    name: b"",
    payload: Payload::Zeros(0),
};

const ADDRESS_QUERY: Request<'static> = Request {
    stage: STAGE_ADDRESS,
    set: false,
    cmd: cmd::GET_VAR,
    name: var::ADDRESS,
    payload: Payload::Zeros(ADDRESS_LEN),
};

const SCAN_MODE: Request<'static> = Request {
    stage: STAGE_SCAN_MODE,
    set: true,
    cmd: cmd::SET_PASSIVE_SCAN,
    name: b"",
    payload: Payload::Word(scan::request::ACTIVE as u32),
};

/// A network's name that a scan asks for, shown by its length alone in a
/// debug print, as the held network's is.
#[derive(Clone, Copy)]
struct Name<'b> {
    bytes: &'b [u8],
}

impl fmt::Debug for Name<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Name")
            .field("len", &self.bytes.len())
            .finish()
    }
}

/// The scan start: the plain scan's, or the scan's that asks for `named`.
const fn scan_start<'b>(sync: u16, named: Option<Name<'b>>) -> Request<'b> {
    Request {
        stage: STAGE_SCAN_START,
        set: true,
        cmd: cmd::SET_VAR,
        name: var::SCAN,
        payload: match named {
            Some(name) => Payload::ScanFor {
                sync,
                name: name.bytes,
            },
            None => Payload::Scan { sync },
        },
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Retried {
    Address,
    ScanMode,
    ScanStart,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    Idle,
    UpCommand,
    RadioWait {
        deadline: Micros,
    },
    AddressQuery {
        tries: u8,
    },
    Retry {
        until: Micros,
        tries: u8,
        then: Retried,
    },
    ScanMode {
        tries: u8,
    },
    ScanStart {
        tries: u8,
    },
    ScanResults,
    ScanYield,
    NotFound,
    Mismatch,
    JoinStep {
        step: u8,
    },
    JoinWait {
        deadline: Micros,
    },
    Disconnecting,
    CapabilitiesQuery,
}

/// The station's state at `Ready`.
#[derive(Debug)]
pub struct Station<'b> {
    stage: Stage,
    exchange: Option<Exchange<'b>>,
    radio_up: bool,
    address: Option<[u8; ADDRESS_LEN]>,
    network: Option<Network<'b>>,
    link: LinkState,
    scans: u16,
    sync: u16,
    scan_deadline: Micros,
    scan_status: u32,
    yield_at: Option<usize>,
    records: u32,
    seen: bool,
    seen_compatible: bool,
    mismatch: Option<Advertised>,
    /// The name a `scan_for` request asked for; the name the scan in
    /// flight carries, when it is a second scan; and whether a record of
    /// the request's scans carried the name asked for.
    wanted: Option<Name<'b>>,
    named: Option<Name<'b>>,
    heard: bool,
    recovering: bool,
    rest_until: Micros,
    /// The failed attempts in a row that were the sign of a refused
    /// secret; the state of the spread's generator, zero until a seed is
    /// given; and the rest that the last failed attempt began where it was
    /// such a sign, zero otherwise.
    refused: u32,
    spread: u32,
    backoff: Micros,
    assoc_status: u32,
    auth_status: u32,
    associated: bool,
    supplicant_up: bool,
    bssid: Option<[u8; ADDRESS_LEN]>,
    supplicant_at: Option<Micros>,
    protection: bool,
    sae_offload: Option<bool>,
    disconnect: bool,
    record: Option<(usize, usize)>,
    capability: Option<(usize, usize)>,
}

impl<'b> Station<'b> {
    /// A station at `Ready` with nothing requested.
    pub const fn new() -> Self {
        Station {
            stage: Stage::Idle,
            exchange: None,
            radio_up: false,
            address: None,
            network: None,
            link: LinkState::Detached,
            scans: 0,
            sync: 0,
            scan_deadline: 0,
            scan_status: 0,
            yield_at: None,
            records: 0,
            seen: false,
            seen_compatible: false,
            mismatch: None,
            wanted: None,
            named: None,
            heard: false,
            recovering: false,
            rest_until: 0,
            refused: 0,
            spread: 0,
            backoff: 0,
            assoc_status: 0,
            auth_status: 0,
            associated: false,
            supplicant_up: false,
            bssid: None,
            supplicant_at: None,
            protection: false,
            sae_offload: None,
            disconnect: false,
            record: None,
            capability: None,
        }
    }

    /// The link's state.
    pub const fn link(&self) -> LinkState {
        self.link
    }

    /// The chip's address, once read.
    pub const fn address(&self) -> Option<[u8; ADDRESS_LEN]> {
        self.address
    }

    /// The rest that the last failed attempt began, where its failure was
    /// the sign of a refused secret for the join's kind: the caller's word
    /// on when the next try comes. `None` behind every other failure,
    /// behind a join that completed and behind a join request.
    pub const fn backoff(&self) -> Option<Micros> {
        if self.backoff == 0 {
            None
        } else {
            Some(self.backoff)
        }
    }

    /// The seed of the spread on the rests behind refused secrets. Until a
    /// seed is given those rests carry no spread. A seed of zero is taken
    /// as one, since the generator's zero state never leaves zero.
    pub fn seed(&mut self, seed: u32) {
        self.spread = if seed == 0 { 1 } else { seed };
    }

    /// Whether nothing is in flight.
    pub fn is_idle(&self) -> bool {
        self.stage == Stage::Idle
    }

    /// Whether a request's frame is built and not yet accepted by the chip:
    /// it holds an issued sequence number and goes out before any data
    /// frame.
    pub fn is_sending(&self) -> bool {
        self.exchange.as_ref().is_some_and(Exchange::is_sending)
    }

    /// The record the last poll handed back: its offset and end in the
    /// buffer.
    pub const fn record(&self) -> Option<(usize, usize)> {
        self.record
    }

    /// The capability string the last poll handed back: its offset and
    /// length in the buffer.
    pub const fn capability(&self) -> Option<(usize, usize)> {
        self.capability
    }

    /// Record the capability query; accepted with nothing in flight and no
    /// disconnect pending.
    pub fn capabilities(&mut self) -> bool {
        if self.stage != Stage::Idle || self.disconnect {
            return false;
        }
        self.stage = Stage::CapabilitiesQuery;
        self.exchange = None;
        true
    }

    /// Record the radio-up request; accepted once per attach, with nothing
    /// in flight.
    pub fn up(&mut self) -> bool {
        if self.stage != Stage::Idle || self.radio_up {
            return false;
        }
        self.stage = Stage::UpCommand;
        self.exchange = None;
        true
    }

    /// Record a scan request; accepted after the radio up, with nothing in
    /// flight and the link detached or up.
    pub fn scan(&mut self) -> bool {
        let allowed = self.stage == Stage::Idle
            && self.radio_up
            && !self.disconnect
            && matches!(self.link, LinkState::Detached | LinkState::Up);
        if !allowed {
            return false;
        }
        self.stage = Stage::ScanMode { tries: 0 };
        self.begin_scan();
        true
    }

    /// Record a scan request that looks for the network named `name` (1 to
    /// 32 bytes); accepted as `scan` is. It is the scan of `scan`, and when
    /// the firmware reports that scan complete with the name in none of its
    /// records, a second scan whose probe requests carry the name, which a
    /// network that does not broadcast its name answers. The records of
    /// both are handed back and one outcome ends the request.
    pub fn scan_for(&mut self, name: &'b [u8]) -> bool {
        if name.is_empty() || name.len() > scan::SSID_MAX || !self.scan() {
            return false;
        }
        self.wanted = Some(Name { bytes: name });
        true
    }

    /// Record a join request; accepted after the radio up, with nothing in
    /// flight and no network held, for an SSID of 1 to 32 bytes and a
    /// passphrase of 8 to 64 or none.
    pub fn join(&mut self, ssid: &'b [u8], passphrase: Option<&'b [u8]>) -> bool {
        let credential = match passphrase {
            None => Credential::Open,
            Some(bytes) => Credential::Passphrase(bytes),
        };
        self.join_with(ssid, credential)
    }

    /// Record a join request with a credential of any kind; accepted as
    /// `join` is, for a credential inside its bounds; a WPA3 password is
    /// refused once the capability query has answered without the word
    /// `sae`.
    pub fn join_with(&mut self, ssid: &'b [u8], credential: Credential<'b>) -> bool {
        let allowed = self.stage == Stage::Idle
            && self.radio_up
            && !self.disconnect
            && self.link == LinkState::Detached;
        if !allowed {
            return false;
        }
        let Some(network) = Network::with(ssid, credential) else {
            return false;
        };
        if network.kind() == Security::Wpa3Sae && self.sae_offload == Some(false) {
            return false;
        }
        self.network = Some(network);
        self.link = LinkState::Joining;
        self.backoff = 0;
        self.assoc_status = 0;
        self.auth_status = 0;
        self.associated = false;
        self.supplicant_up = false;
        self.bssid = None;
        self.supplicant_at = None;
        self.stage = Stage::JoinStep { step: STEP_FIRST };
        self.exchange = None;
        true
    }

    /// Record a disconnect; accepted whenever a network is held. The
    /// network is dropped at once; the disassociation is sent as soon as
    /// no exchange is outstanding. The count of refused secrets is put
    /// back by every call, accepted or not: the caller ended the trying.
    pub fn disconnect(&mut self) -> bool {
        self.refused = 0;
        if self.link == LinkState::Detached {
            return false;
        }
        self.network = None;
        self.link = LinkState::Detached;
        self.recovering = false;
        self.backoff = 0;
        self.disconnect = true;
        true
    }

    fn begin_scan(&mut self) {
        self.next_sync();
        self.records = 0;
        self.seen = false;
        self.seen_compatible = false;
        self.mismatch = None;
        self.wanted = None;
        self.named = None;
        self.heard = false;
        self.yield_at = None;
        self.exchange = None;
    }

    /// A sync number of its own for the next scan request: the count of
    /// this station's scans, never zero.
    fn next_sync(&mut self) {
        self.scans = self.scans.wrapping_add(1);
        if self.scans == 0 {
            self.scans = 1;
        }
        self.sync = self.scans;
    }

    /// One bounded step.
    pub fn step<T: Transport>(
        &mut self,
        bus: &mut T,
        window: &mut Window,
        buf: &mut [u8],
        layer: &mut Layer,
        ids: &mut u16,
        now: Micros,
    ) -> Result<Wake, Refusal> {
        self.record = None;
        self.capability = None;
        if self.disconnect && self.exchange.is_none() {
            self.disconnect = false;
            self.yield_at = None;
            self.stage = Stage::Disconnecting;
        }
        match self.stage {
            Stage::Idle => {
                if self.link == LinkState::Down && now >= self.rest_until {
                    self.recovering = true;
                    self.stage = Stage::ScanMode { tries: 0 };
                    self.begin_scan();
                    return Ok(Wake::Again);
                }
                self.pass(bus, window, buf, layer, now)
            }
            Stage::UpCommand => {
                let reply = match self.exchange_step(RADIO_UP, bus, window, buf, layer, ids, now)? {
                    Progress::Wake(wake) => return Ok(wake),
                    Progress::Event(event) => return self.funnel(event, buf, layer, now),
                    Progress::Data(len) => return self.data(len, layer),
                    Progress::Done(reply) => reply,
                };
                judge(reply, STAGE_RADIO_UP)?;
                self.exchange = None;
                self.stage = Stage::RadioWait {
                    deadline: now + RADIO_WAIT_US,
                };
                Ok(Wake::Again)
            }
            Stage::RadioWait { deadline } => {
                if now >= deadline {
                    self.stage = Stage::AddressQuery { tries: 0 };
                    self.exchange = None;
                    return Ok(Wake::Again);
                }
                self.pass(bus, window, buf, layer, now)
            }
            Stage::AddressQuery { tries } => {
                let reply =
                    match self.exchange_step(ADDRESS_QUERY, bus, window, buf, layer, ids, now)? {
                        Progress::Wake(wake) => return Ok(wake),
                        Progress::Event(event) => return self.funnel(event, buf, layer, now),
                        Progress::Data(len) => return self.data(len, layer),
                        Progress::Done(reply) => reply,
                    };
                if let Some(wake) = self.not_up(reply, tries, Retried::Address, now) {
                    return Ok(wake);
                }
                judge(reply, STAGE_ADDRESS)?;
                if reply.len < ADDRESS_LEN {
                    return Err(Refusal::new(STAGE_ADDRESS, reply.len as u32));
                }
                let mut address = [0u8; ADDRESS_LEN];
                address.copy_from_slice(&buf[reply.at..reply.at + ADDRESS_LEN]);
                self.address = Some(address);
                self.radio_up = true;
                self.exchange = None;
                self.stage = Stage::Idle;
                Ok(Wake::Done(Outcome::Up { address }))
            }
            Stage::Retry { until, tries, then } => {
                if now < until {
                    return Ok(Wake::At(until));
                }
                self.stage = match then {
                    Retried::Address => Stage::AddressQuery { tries },
                    Retried::ScanMode => Stage::ScanMode { tries },
                    Retried::ScanStart => Stage::ScanStart { tries },
                };
                self.exchange = None;
                Ok(Wake::Again)
            }
            Stage::ScanMode { tries } => {
                let reply =
                    match self.exchange_step(SCAN_MODE, bus, window, buf, layer, ids, now)? {
                        Progress::Wake(wake) => return Ok(wake),
                        Progress::Event(event) => return self.funnel(event, buf, layer, now),
                        Progress::Data(len) => return self.data(len, layer),
                        Progress::Done(reply) => reply,
                    };
                if let Some(wake) = self.not_up(reply, tries, Retried::ScanMode, now) {
                    return Ok(wake);
                }
                judge(reply, STAGE_SCAN_MODE)?;
                self.exchange = None;
                self.stage = Stage::ScanStart { tries: 0 };
                Ok(Wake::Again)
            }
            Stage::ScanStart { tries } => {
                let request = scan_start(self.sync, self.named);
                let reply = match self.exchange_step(request, bus, window, buf, layer, ids, now)? {
                    Progress::Wake(wake) => return Ok(wake),
                    Progress::Event(event) => return self.funnel(event, buf, layer, now),
                    Progress::Data(len) => return self.data(len, layer),
                    Progress::Done(reply) => reply,
                };
                if let Some(wake) = self.not_up(reply, tries, Retried::ScanStart, now) {
                    return Ok(wake);
                }
                if reply.status == STATUS_NOT_UP {
                    return Err(Refusal::new(STAGE_SCAN_REGULATORY, reply.status));
                }
                judge(reply, STAGE_SCAN_START)?;
                self.exchange = None;
                self.scan_deadline = now + SCAN_WAIT_US;
                self.stage = Stage::ScanResults;
                Ok(Wake::Again)
            }
            Stage::ScanResults => {
                if now >= self.scan_deadline {
                    return self.scan_end(ScanEnd::TimedOut);
                }
                self.pass(bus, window, buf, layer, now)
            }
            Stage::ScanYield => match self.next_record(buf, layer) {
                Some(record) => Ok(Wake::Done(Outcome::ScanRecord(record))),
                None => {
                    self.yield_at = None;
                    if self.scan_status == status::PARTIAL {
                        self.stage = Stage::ScanResults;
                        return self.pass(bus, window, buf, layer, now);
                    }
                    self.scan_end(ScanEnd::of_status(self.scan_status))
                }
            },
            Stage::NotFound => {
                self.stage = Stage::Idle;
                self.backoff = 0;
                self.rest_until = now + CYCLE_REST_US;
                Ok(Wake::Done(Outcome::JoinFailed(JoinFailure::NotFound)))
            }
            Stage::Mismatch => {
                let advertised = self.mismatch.take().unwrap_or_default();
                self.join_failed(JoinFailure::Mismatch { advertised }, now)
            }
            Stage::JoinStep { step } => {
                let Some(network) = self.network else {
                    return self.finish_exchange(bus, window, buf, layer, ids, now);
                };
                let reset = self.protection;
                let Some(request) = link::request(network, step, reset) else {
                    self.stage = Stage::JoinStep { step: step + 1 };
                    return Ok(Wake::Again);
                };
                let reply = match self.exchange_step(request, bus, window, buf, layer, ids, now)? {
                    Progress::Wake(wake) => return Ok(wake),
                    Progress::Event(event) => return self.funnel(event, buf, layer, now),
                    Progress::Data(len) => return self.data(len, layer),
                    Progress::Done(reply) => reply,
                };
                judge(reply, request.stage)?;
                self.exchange = None;
                if step == STEP_PROTECTION {
                    self.protection = network.kind() == Security::Wpa3Sae;
                }
                self.stage = if step >= STEP_LAST {
                    self.assoc_status = 0;
                    self.auth_status = 0;
                    self.associated = false;
                    self.supplicant_up = false;
                    self.bssid = None;
                    self.supplicant_at = None;
                    Stage::JoinWait {
                        deadline: now + JOIN_WAIT_US,
                    }
                } else {
                    let mut next = step + 1;
                    if link::request(network, next, reset).is_none() {
                        next += 1;
                    }
                    Stage::JoinStep { step: next }
                };
                Ok(Wake::Again)
            }
            Stage::CapabilitiesQuery => {
                let reply = match self.exchange_step(
                    CAPABILITIES_QUERY,
                    bus,
                    window,
                    buf,
                    layer,
                    ids,
                    now,
                )? {
                    Progress::Wake(wake) => return Ok(wake),
                    Progress::Event(event) => return self.funnel(event, buf, layer, now),
                    Progress::Data(len) => return self.data(len, layer),
                    Progress::Done(reply) => reply,
                };
                self.exchange = None;
                self.stage = Stage::Idle;
                let capabilities = if reply.status != 0 || reply.flags & flags::ERROR != 0 {
                    let word = if reply.status != 0 {
                        reply.status
                    } else {
                        reply.flags
                    };
                    Capabilities {
                        answered: false,
                        status: word,
                        ..Capabilities::default()
                    }
                } else {
                    let bytes = &buf[reply.at..reply.at + reply.len];
                    let len = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
                    let string = &bytes[..len];
                    let has = |word: &[u8]| string.split(|&b| b == b' ').any(|token| token == word);
                    let capabilities = Capabilities {
                        answered: true,
                        status: 0,
                        len: len as u16,
                        sae: has(b"sae"),
                        mfp: has(b"mfp"),
                    };
                    self.sae_offload = Some(capabilities.sae);
                    self.capability = Some((reply.at, len));
                    capabilities
                };
                Ok(Wake::Done(Outcome::Capabilities(capabilities)))
            }
            Stage::JoinWait { deadline } => {
                if now >= deadline {
                    return self.join_failed(JoinFailure::Timeout, now);
                }
                if let Some(expiry) = self.hold_expiry() {
                    if now >= expiry {
                        return self.joined_held();
                    }
                    // Both words are in and the hold runs: the channel serviced
                    // meanwhile, the wake no later than the hold's expiry.
                    return Ok(match self.pass(bus, window, buf, layer, now)? {
                        Wake::Irq(t) | Wake::At(t) if t > expiry => Wake::At(expiry),
                        wake => wake,
                    });
                }
                self.pass(bus, window, buf, layer, now)
            }
            Stage::Disconnecting => {
                let reply = match self.exchange_step(
                    link::DISCONNECT,
                    bus,
                    window,
                    buf,
                    layer,
                    ids,
                    now,
                )? {
                    Progress::Wake(wake) => return Ok(wake),
                    Progress::Event(event) => return self.funnel(event, buf, layer, now),
                    Progress::Data(len) => return self.data(len, layer),
                    Progress::Done(reply) => reply,
                };
                judge(reply, link::STAGE_DISCONNECT)?;
                self.exchange = None;
                self.stage = Stage::Idle;
                Ok(Wake::Done(Outcome::Disconnected))
            }
        }
    }

    /// One step of the exchange for `request`, built on the first call.
    #[allow(clippy::too_many_arguments)]
    fn exchange_step<T: Transport>(
        &mut self,
        request: Request<'b>,
        bus: &mut T,
        window: &mut Window,
        buf: &mut [u8],
        layer: &mut Layer,
        ids: &mut u16,
        now: Micros,
    ) -> Result<Progress, Refusal> {
        let exchange = self
            .exchange
            .get_or_insert_with(|| Exchange::new(request, now));
        exchange.step(bus, window, buf, layer, ids, now)
    }

    /// The network was dropped while a join step's exchange may be
    /// outstanding: the exchange runs to its reply, judged by the step's
    /// stage name, then clears, and the stage goes idle so the disconnect
    /// that dropped the network can send the disassociation.
    fn finish_exchange<T: Transport>(
        &mut self,
        bus: &mut T,
        window: &mut Window,
        buf: &mut [u8],
        layer: &mut Layer,
        ids: &mut u16,
        now: Micros,
    ) -> Result<Wake, Refusal> {
        let Some(exchange) = &mut self.exchange else {
            self.stage = Stage::Idle;
            return Ok(Wake::Again);
        };
        let stage = exchange.request().stage;
        let reply = match exchange.step(bus, window, buf, layer, ids, now)? {
            Progress::Wake(wake) => return Ok(wake),
            Progress::Event(event) => return self.funnel(event, buf, layer, now),
            Progress::Data(len) => return self.data(len, layer),
            Progress::Done(reply) => reply,
        };
        judge(reply, stage)?;
        self.exchange = None;
        self.stage = Stage::Idle;
        Ok(Wake::Again)
    }

    /// The retry of a request answered "not up", while tries remain.
    fn not_up(&mut self, reply: Reply, tries: u8, then: Retried, now: Micros) -> Option<Wake> {
        if reply.status != STATUS_NOT_UP || tries >= NOT_UP_TRIES {
            return None;
        }
        self.exchange = None;
        let until = now + NOT_UP_RETRY_US;
        self.stage = Stage::Retry {
            until,
            tries: tries + 1,
            then,
        };
        Some(Wake::At(until))
    }

    /// One service pass, every event through the funnel.
    fn pass<T: Transport>(
        &mut self,
        bus: &mut T,
        window: &mut Window,
        buf: &mut [u8],
        layer: &mut Layer,
        now: Micros,
    ) -> Result<Wake, Refusal> {
        match layer.service(bus, window, buf)? {
            Served::Nothing => Ok(Wake::Irq(now + SERVICE_POLL_US)),
            Served::Consumed => Ok(Wake::Again),
            Served::Control(_) => {
                layer.frames.dropped += 1;
                Ok(Wake::Again)
            }
            Served::Event(event) => self.funnel(event, buf, layer, now),
            Served::Data { len } => self.data(len, layer),
        }
    }

    /// A data frame taken: handed to the caller while the link is up,
    /// dropped and counted otherwise.
    fn data(&mut self, len: usize, layer: &mut Layer) -> Result<Wake, Refusal> {
        if self.link == LinkState::Up {
            return Ok(Wake::Done(Outcome::Frame { len }));
        }
        layer.frames.data_dropped += 1;
        layer.forget_data();
        Ok(Wake::Again)
    }

    /// Every event: the loss set against an up link first, then the stage's
    /// own consumption, then the caller's.
    fn funnel(
        &mut self,
        event: Event,
        buf: &[u8],
        layer: &Layer,
        now: Micros,
    ) -> Result<Wake, Refusal> {
        if self.link == LinkState::Up && matches!(event.link(), Link::Down | Link::SupplicantDown) {
            self.link = LinkState::Down;
            self.rest_until = now + LOSS_SETTLE_US;
            return Ok(Wake::Done(Outcome::LinkLost(event)));
        }
        match self.stage {
            Stage::RadioWait { .. } if event.number == number::RADIO => {
                self.stage = Stage::AddressQuery { tries: 0 };
                self.exchange = None;
                Ok(Wake::Again)
            }
            Stage::ScanResults if event.number == number::ESCAN_RESULT => {
                let payload = layer.last_event().map(|(at, len)| &buf[at..at + len]);
                match payload.and_then(scan::head) {
                    Some(head) if head.sync == self.sync => {
                        self.scan_status = event.status;
                        self.yield_at = Some(RESULT_HEAD_LEN);
                        match self.next_record(buf, layer) {
                            Some(record) => Ok(Wake::Done(Outcome::ScanRecord(record))),
                            None => self.records_out(),
                        }
                    }
                    _ => Ok(Wake::Done(Outcome::Event(event))),
                }
            }
            Stage::JoinWait { .. } => self.join_event(event, now),
            _ => Ok(Wake::Done(Outcome::Event(event))),
        }
    }

    /// The next record of the result in the buffer, the cursor moved past
    /// it; `None` when the result's records are out.
    fn next_record(&mut self, buf: &[u8], layer: &Layer) -> Option<ScanRecord> {
        let at = self.yield_at?;
        let (payload_at, payload_len) = layer.last_event()?;
        let payload = &buf[payload_at..payload_at + payload_len];
        let walked = scan::walk(payload, at)?;
        self.records += 1;
        if let Some(network) = self.network
            && network.ssid() == walked.record.ssid()
        {
            self.seen = true;
            if walked.record.security.compatible(network.kind()) {
                self.seen_compatible = true;
            } else {
                self.mismatch = Some(walked.record.security);
            }
        }
        if let Some(name) = self.wanted
            && name.bytes == walked.record.ssid()
        {
            self.heard = true;
        }
        self.record = Some((payload_at + at, payload_at + walked.end));
        self.yield_at = walked.next;
        self.stage = Stage::ScanYield;
        Some(walked.record)
    }

    /// A result carrying no record was taken: more results to come, or the
    /// end.
    fn records_out(&mut self) -> Result<Wake, Refusal> {
        self.yield_at = None;
        if self.scan_status == status::PARTIAL {
            self.stage = Stage::ScanResults;
            return Ok(Wake::Again);
        }
        self.scan_end(ScanEnd::of_status(self.scan_status))
    }

    /// The name a second scan is to carry, when one follows the scan that
    /// ended with `end`: after a plain scan the firmware reported complete,
    /// the held network's name when the recovery cycle saw it in no
    /// record, or the name a `scan_for` request asked for and heard in
    /// none. A plain scan that ended any other way is followed by no second
    /// scan: the firmware may still be scanning, and a scan start it
    /// answers busy would park the driver.
    fn unseen(&self, end: ScanEnd) -> Option<Name<'b>> {
        if end != ScanEnd::Complete || self.named.is_some() {
            return None;
        }
        if self.recovering {
            return match self.network {
                Some(network) if !self.seen => Some(Name {
                    bytes: network.ssid(),
                }),
                _ => None,
            };
        }
        if self.heard { None } else { self.wanted }
    }

    /// The scan ended: a second scan that asks for the network by name
    /// when the first did not see it, with no outcome between the two;
    /// else the outcome, and the recovery cycle's verdict when the scan
    /// was its own.
    fn scan_end(&mut self, end: ScanEnd) -> Result<Wake, Refusal> {
        if let Some(name) = self.unseen(end) {
            // The scan mode stands from the plain scan's own request, and
            // the records' count and what they showed run on over both
            // scans; the second scan has a sync number and a bound of its
            // own.
            self.next_sync();
            self.named = Some(name);
            self.yield_at = None;
            self.exchange = None;
            self.stage = Stage::ScanStart { tries: 0 };
            return Ok(Wake::Again);
        }
        let records = self.records;
        self.wanted = None;
        self.named = None;
        self.yield_at = None;
        self.exchange = None;
        if self.recovering {
            self.recovering = false;
            if self.seen_compatible && self.network.is_some() {
                self.link = LinkState::Joining;
                self.assoc_status = 0;
                self.auth_status = 0;
                self.associated = false;
                self.supplicant_up = false;
                self.bssid = None;
                self.supplicant_at = None;
                self.stage = Stage::JoinStep { step: STEP_REENTRY };
            } else if self.seen && self.network.is_some() {
                self.stage = Stage::Mismatch;
            } else {
                self.stage = Stage::NotFound;
            }
        } else {
            self.stage = Stage::Idle;
        }
        Ok(Wake::Done(Outcome::ScanDone { end, records }))
    }

    /// An event during the wait on the completing event. An open join
    /// completes on the association; a secured join (WPA2 or WPA3) once
    /// BOTH the supplicant's word and the firmware's association success
    /// have arrived, in either order, AND the hold after the supplicant's
    /// word has passed -- a frame written within a moment of that word is
    /// lost on this firmware on both kinds, whatever the association
    /// word's place.
    fn join_event(&mut self, event: Event, now: Micros) -> Result<Wake, Refusal> {
        let Some(network) = self.network else {
            return Ok(Wake::Done(Outcome::Event(event)));
        };
        match event.link() {
            Link::Associated => {
                self.bssid = Some(event.address);
                if !network.is_secured() || (self.supplicant_up && self.hold_passed(now)) {
                    self.joined(event)
                } else {
                    self.associated = true;
                    Ok(Wake::Done(Outcome::Event(event)))
                }
            }
            Link::SupplicantUp => {
                if network.is_secured() {
                    // The hold starts at the supplicant's word; the join completes
                    // once the association word is in and the hold has passed.
                    self.supplicant_up = true;
                    self.supplicant_at = Some(now);
                }
                Ok(Wake::Done(Outcome::Event(event)))
            }
            Link::JoinFailed => self.join_failed(
                JoinFailure::Association {
                    status: event.status,
                    assoc_status: self.assoc_status,
                    auth_status: self.auth_status,
                },
                now,
            ),
            Link::SupplicantDown => self.join_failed(
                JoinFailure::Supplicant {
                    reason: reason::bare(event.reason, reason::SUPPLICANT_OFFSET),
                },
                now,
            ),
            Link::Down => self.join_failed(
                JoinFailure::Lost {
                    number: event.number,
                    reason: event.reason,
                },
                now,
            ),
            Link::Unchanged => {
                if event.number == number::ASSOC {
                    self.assoc_status = event.status;
                    if self.bssid.is_none() {
                        self.bssid = Some(event.address);
                    }
                }
                if event.number == number::AUTH {
                    self.auth_status = event.status;
                }
                Ok(Wake::Done(Outcome::Event(event)))
            }
        }
    }

    /// The join completes: the link up, and the access point's address
    /// the one the firmware's association word named -- on an open join
    /// the completing event, on a secured join arrived before the
    /// completion -- with the association event's in its place if no word
    /// came; the supplicant's event carries another address.
    fn joined(&mut self, event: Event) -> Result<Wake, Refusal> {
        self.link_up();
        let bssid = self.bssid.unwrap_or(event.address);
        Ok(Wake::Done(Outcome::Joined { bssid }))
    }

    /// The link up behind a join that completed, on either path: nothing in
    /// flight, and the count of refused secrets put back.
    fn link_up(&mut self) {
        self.link = LinkState::Up;
        self.stage = Stage::Idle;
        self.exchange = None;
        self.recovering = false;
        self.refused = 0;
        self.backoff = 0;
    }

    /// The hold's expiry on a secured join whose two words are both in.
    fn hold_expiry(&self) -> Option<Micros> {
        let secured = self.network.is_some_and(|n| n.is_secured());
        if secured && self.associated && self.supplicant_up {
            self.supplicant_at.map(|at| at + SUPPLICANT_HOLD_US)
        } else {
            None
        }
    }

    /// Whether the hold after the supplicant's word has passed.
    fn hold_passed(&self, now: Micros) -> bool {
        self.supplicant_at
            .is_some_and(|at| now >= at + SUPPLICANT_HOLD_US)
    }

    /// The join completes at the hold's expiry, with the address the
    /// association word named.
    fn joined_held(&mut self) -> Result<Wake, Refusal> {
        self.link_up();
        let bssid = self.bssid.unwrap_or([0; ADDRESS_LEN]);
        Ok(Wake::Done(Outcome::Joined { bssid }))
    }

    /// The attempt failed: the network stays held and the link down for the
    /// next cycle, except on a mismatch, which detaches it. Behind the sign
    /// of a refused secret the rest before that cycle is the refusals' own.
    fn join_failed(&mut self, failure: JoinFailure, now: Micros) -> Result<Wake, Refusal> {
        self.stage = Stage::Idle;
        self.exchange = None;
        self.recovering = false;
        self.backoff = 0;
        if let JoinFailure::Mismatch { .. } = failure {
            self.network = None;
            self.link = LinkState::Detached;
        } else {
            self.link = LinkState::Down;
            let refused = self
                .network
                .is_some_and(|network| failure.refused_secret(network.kind()));
            let rest = if refused {
                self.refused_once_more()
            } else {
                CYCLE_REST_US
            };
            self.rest_until = now + rest;
        }
        Ok(Wake::Done(Outcome::JoinFailed(failure)))
    }

    /// One refused secret more in the row: the rest before the next try,
    /// with its spread, kept for the caller. The count stops where the
    /// rest has reached its cap, so no count ends the trying.
    fn refused_once_more(&mut self) -> Micros {
        if refused_rest(self.refused) < REFUSED_REST_MAX_US {
            self.refused += 1;
        }
        self.spread = spread_step(self.spread);
        let rest = refused_rest(self.refused);
        self.backoff = rest + spread_of(rest, self.spread);
        self.backoff
    }
}

impl Default for Station<'_> {
    fn default() -> Self {
        Self::new()
    }
}

/// The rest before the next try behind the `count`-th failed attempt in a
/// row that was the sign of a refused secret, without its spread:
/// `REFUSED_REST_US`, doubled at each one after the first, and
/// `REFUSED_REST_MAX_US` once a doubling reaches it, whatever the count.
pub const fn refused_rest(count: u32) -> Micros {
    let mut rest = REFUSED_REST_US;
    let mut doubled = 1;
    while doubled < count && rest < REFUSED_REST_MAX_US {
        rest *= 2;
        doubled += 1;
    }
    if rest > REFUSED_REST_MAX_US {
        REFUSED_REST_MAX_US
    } else {
        rest
    }
}

/// One step of the spread's generator: a xorshift of 32 bits, whose zero
/// state never leaves zero.
pub(crate) const fn spread_step(state: u32) -> u32 {
    let mut word = state;
    word ^= word << 13;
    word ^= word >> 17;
    word ^= word << 5;
    word
}

/// The spread on `rest` for the generator's `word`: the word over 2^32 of
/// a fifth of the rest, so from nothing to under a fifth.
pub(crate) const fn spread_of(rest: Micros, word: u32) -> Micros {
    ((rest / 5) * word as Micros) >> 32
}

/// A reply that refuses -- a non-zero status, or the error flag -- refuses
/// by `stage` with the status word (the flags word when the status is
/// zero).
fn judge(reply: Reply, stage: &'static str) -> Result<(), Refusal> {
    if reply.status != 0 || reply.flags & flags::ERROR != 0 {
        let word = if reply.status != 0 {
            reply.status
        } else {
            reply.flags
        };
        return Err(Refusal::new(stage, word));
    }
    Ok(())
}
