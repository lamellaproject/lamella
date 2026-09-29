//! The pump: the driver's state machine, advanced one bounded step per
//! `poll`, returning a wake that says when to call again or an outcome.
//!
//! A request records an intent and returns at once. `poll` performs at most
//! one bulk transfer or one packet-channel frame plus a bounded handful of
//! register accesses, and never waits on time inside: every wait is a
//! deadline returned to the caller. The requests, in order: `attach` brings
//! the bus up and identifies the chip; `upload` writes the firmware and
//! settings images and starts the firmware; `open` pushes the regulatory
//! blob, configures the running firmware, queries its version and tells it
//! which events to deliver, after which the driver is ready and services
//! the packet channel on every poll, handing each event to the caller as an
//! outcome. Once ready: `up` brings the radio up and reads the chip's
//! address; `scan` lists the networks in range, one record per outcome;
//! `join` associates with a network the caller names and keeps the link
//! joined, returning to a scan and re-entering the join on every loss until
//! `disconnect`.

use core::fmt;

use crate::backplane::{CHIPCOMMON, Window, clkcsr, f1};
use crate::clock::{Clock, Micros};
use crate::control::{OpenStep, Opening, Version};
use crate::data::{self, TRANSMIT_BUF};
use crate::download::{Download, Progress};
use crate::error::Refusal;
use crate::event::{Event, EventMask};
use crate::frame::{FRAME_BUF, Frames, Layer};
use crate::link::{Credential, JoinFailure, LinkState};
use crate::scan::{ScanEnd, ScanRecord};
use crate::station::{ADDRESS_LEN, Capabilities, Station};
use crate::transport::{Attach, Func, Transport};

/// What the caller does before calling `poll` again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wake {
    /// More bounded work is ready: call again at once.
    Again,
    /// Call again at or after this instant.
    At(Micros),
    /// Call again when the host interrupt asserts, or at this instant,
    /// whichever comes first. A caller that cannot sample the interrupt
    /// line waits for the instant.
    Irq(Micros),
    /// Nothing is pending; the next request starts new work.
    Idle,
    /// An outcome: read it, then call again.
    Done(Outcome),
}

/// The result of a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The bus is attached, the chip identified and its ALP clock running.
    Attached {
        /// The 32-bit word at internal address 0x18000000; its low 16 bits
        /// are the chip identity.
        chip_id: u32,
    },
    /// The firmware runs and the packet channel is ready.
    Uploaded {
        /// Whether the firmware initialized its save/restore engine, in
        /// which case the keep-bus-on request is on and the chip's bus
        /// function stays powered through the firmware's own sleep.
        save_restore: bool,
    },
    /// The control path answers, the firmware is configured and delivers
    /// the events asked for.
    Ready {
        /// The firmware's version string, as it answered the version
        /// query.
        version: Version,
    },
    /// The firmware delivered an event; its payload is readable through
    /// [`Driver::event_payload`] until the next poll that services the
    /// packet channel.
    Event(Event),
    /// The radio is up and the chip's own address, the address of record,
    /// was read back.
    Up {
        /// The chip's address.
        address: [u8; ADDRESS_LEN],
    },
    /// A scan saw a network; the record's bytes are readable through
    /// [`Driver::record_bytes`] until the next poll that services the
    /// packet channel.
    ScanRecord(ScanRecord),
    /// A scan ended.
    ScanDone {
        /// How it ended.
        end: ScanEnd,
        /// The records handed back.
        records: u32,
    },
    /// The join completed: the link is up.
    Joined {
        /// The access point's address, as the firmware's association word
        /// named it (a secured join waits for that word; an open join
        /// completes on it).
        bssid: [u8; ADDRESS_LEN],
    },
    /// A join attempt failed; the link machine decides what follows.
    JoinFailed(JoinFailure),
    /// The link was lost: the event that lowered it. Handed back once per
    /// loss; the rest of a burst are plain events.
    LinkLost(Event),
    /// The network was dropped and the disassociation sent.
    Disconnected,
    /// The capability query was answered, or not: the firmware's word on
    /// its features, the string readable through
    /// [`Driver::capability_string`] until the next poll.
    Capabilities(Capabilities),
    /// A data frame arrived while the link is up; its Ethernet frame of
    /// this length is readable through [`Driver::frame_payload`] until the
    /// next poll that services the packet channel.
    Frame {
        /// The Ethernet frame's length.
        len: usize,
    },
    /// The machine stopped at a named stage.
    Refused(Refusal),
}

/// The stage name of a chip whose identity word does not match the
/// transport's expectation.
pub const STAGE_CHIP_IDENTITY: &str = "chip identity";
/// The stage name of an ALP clock request the chip did not grant.
pub const STAGE_ALP_CLOCK: &str = "ALP clock request";

/// The ALP request written to the chip clock register: the hardware clock
/// requests squelched, the ALP clock requested and forced.
const ALP_REQUEST: u8 = clkcsr::FORCE_HW_CLKREQ_OFF | clkcsr::ALP_AVAIL_REQ | clkcsr::FORCE_ALP;
/// ALP readiness polls: ten tries, ten milliseconds apart.
const ALP_TRIES: u8 = 10;
const ALP_RETRY_US: Micros = 10_000;
/// The settle after the ALP request is released.
const ALP_SETTLE_US: Micros = 100_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Off,
    BusAttach,
    ChipId,
    AlpRequest,
    AlpPoll { tries: u8, next: Micros },
    AlpRelease,
    Settle { until: Micros },
    Attached,
    Downloading,
    Running,
    Opening,
    Ready,
    Parked,
}

/// The driver: the state machine the caller pumps. The lifetime is that
/// of the images and the blob the caller stages. Printing it prints its
/// state and never its buffers: the frame buffer holds a key structure
/// while the passphrase step's frame is being written, and the transmit
/// buffer holds the caller's frame.
pub struct Driver<'b> {
    state: State,
    window: Window,
    chip_id: u32,
    download: Option<Download<'b>>,
    opening: Option<Opening<'b>>,
    layer: Layer,
    ids: u16,
    version: Version,
    events: EventMask,
    station: Station<'b>,
    frame: [u8; FRAME_BUF],
    frame_taken: bool,
    data: [u8; TRANSMIT_BUF],
    staged: usize,
    refusal: Option<Refusal>,
}

/// The transmit side of the driver, borrowed apart from the received
/// frame so a reply can be staged while the frame is read in place.
pub(crate) struct Sender<'t, 'b> {
    data: &'t mut [u8; TRANSMIT_BUF],
    staged: &'t mut usize,
    layer: &'t mut Layer,
    state: &'t mut State,
    refusal: &'t mut Option<Refusal>,
    station: &'t Station<'b>,
}

/// Whether an Ethernet frame of `len` bytes can be staged: the driver
/// ready, the link up, no frame staged, the length inside the unit.
fn can_stage(state: State, link: LinkState, staged: usize, len: usize) -> bool {
    state == State::Ready && link == LinkState::Up && staged == 0 && data::fits(len)
}

/// Whether the staged frame can be written now: the link up, no request
/// half sent, the window open.
fn can_write(link: LinkState, sending: bool, open: bool) -> bool {
    link == LinkState::Up && !sending && open
}

impl<'t, 'b> Sender<'t, 'b> {
    /// Whether a frame could be staged: the driver ready, the link up,
    /// nothing staged. The device adapter's question.
    #[cfg(feature = "smoltcp")]
    pub(crate) fn can_stage(&self) -> bool {
        can_stage(
            *self.state,
            self.station.link(),
            *self.staged,
            data::ETHERNET_HEADER_LEN,
        )
    }

    /// Whether a staged frame could be written now.
    pub(crate) fn can_write(&self) -> bool {
        can_write(
            self.station.link(),
            self.station.is_sending(),
            self.layer.credit.open(),
        )
    }

    /// The slot for an Ethernet frame of `len` bytes, the frame staged.
    /// The device adapter's transmit token stages through it.
    #[cfg(feature = "smoltcp")]
    pub(crate) fn stage(&mut self, len: usize) -> Option<&mut [u8]> {
        if !can_stage(*self.state, self.station.link(), *self.staged, len) {
            return None;
        }
        *self.staged = len;
        Some(&mut self.data[data::HEAD..data::HEAD + len])
    }

    /// Write the staged frame now if it can be written: `Ok(true)` written,
    /// `Ok(false)` kept; a bus refusal parks the driver.
    pub(crate) fn flush<T: Transport>(&mut self, bus: &mut T) -> Result<bool, Refusal> {
        if *self.state != State::Ready || *self.staged == 0 || !self.can_write() {
            return Ok(false);
        }
        match data::write(bus, self.data, *self.staged, self.layer) {
            Ok(true) => {
                *self.staged = 0;
                Ok(true)
            }
            Ok(false) => Ok(false),
            Err(refusal) => {
                *self.state = State::Parked;
                *self.refusal = Some(refusal);
                Err(refusal)
            }
        }
    }
}

impl<'b> Driver<'b> {
    /// A driver with no request recorded, asking for the default events.
    pub const fn new() -> Self {
        Driver {
            state: State::Off,
            window: Window::new(),
            chip_id: 0,
            download: None,
            opening: None,
            layer: Layer::new(),
            ids: 0,
            version: Version::EMPTY,
            events: EventMask::DEFAULT,
            station: Station::new(),
            frame: [0; FRAME_BUF],
            frame_taken: false,
            data: [0; TRANSMIT_BUF],
            staged: 0,
            refusal: None,
        }
    }

    /// Record the attach request: bring the bus up, identify the chip and
    /// request its ALP clock. Accepted from the initial state and after a
    /// refusal; returns `false` otherwise.
    pub fn attach(&mut self) -> bool {
        match self.state {
            State::Off | State::Parked => {
                self.window = Window::new();
                self.chip_id = 0;
                self.download = None;
                self.opening = None;
                self.layer = Layer::new();
                self.ids = 0;
                self.version = Version::EMPTY;
                self.station = Station::new();
                self.frame_taken = false;
                self.staged = 0;
                self.refusal = None;
                self.state = State::BusAttach;
                true
            }
            _ => false,
        }
    }

    /// Record the upload request: write the firmware image and the
    /// settings image into the chip's RAM, start the firmware, and bring
    /// the packet channel up. The settings image is the converted form,
    /// NUL-separated `key=value` strings. Accepted while attached; returns
    /// `false` otherwise.
    pub fn upload(&mut self, firmware: &'b [u8], settings: &'b [u8]) -> bool {
        if self.state != State::Attached {
            return false;
        }
        self.download = Some(Download::new(firmware, settings));
        self.state = State::Downloading;
        true
    }

    /// Record the open request: set the packet channel's interrupts up,
    /// push the regulatory blob (empty: none), configure the running
    /// firmware, query its version and push the event mask. `version` is
    /// the version string expected of the firmware, recorded beside the
    /// images by whoever packaged them; the reply must contain it, or the
    /// driver refuses by name. `None` declines the check; an empty string
    /// is refused at the first poll. Accepted while the firmware runs;
    /// returns `false` otherwise.
    pub fn open(&mut self, regulatory: &'b [u8], version: Option<&'b [u8]>) -> bool {
        if self.state != State::Running {
            return false;
        }
        self.opening = Some(Opening::new(regulatory, version, self.events));
        self.state = State::Opening;
        true
    }

    /// Record the events the firmware is to deliver, pushed as the last
    /// transaction of the opening; the default asks for the events a join,
    /// a scan and the link's upkeep need. Accepted until the opening
    /// starts; returns `false` while opening or ready, where the mask
    /// pushed is the one in force. A further attach keeps it.
    pub fn subscribe(&mut self, events: EventMask) -> bool {
        if matches!(self.state, State::Opening | State::Ready) {
            return false;
        }
        self.events = events;
        true
    }

    /// The events the firmware is asked to deliver.
    pub fn event_mask(&self) -> EventMask {
        self.events
    }

    /// Record the radio-up request: bring the radio up, wait for its event
    /// and read the chip's own address back. Accepted once per attach,
    /// while ready with nothing in flight; returns `false` otherwise.
    pub fn up(&mut self) -> bool {
        self.state == State::Ready && self.station.up()
    }

    /// Record a scan request: list the networks in range, one record per
    /// outcome, then the scan's end. Accepted while ready with nothing in
    /// flight, after the radio up, with no network held or the link up;
    /// returns `false` otherwise.
    pub fn scan(&mut self) -> bool {
        self.state == State::Ready && self.station.scan()
    }

    /// Record a join request: associate with the network named by `ssid`
    /// (1 to 32 bytes), with `passphrase` (8 to 64 bytes) on a WPA2-PSK
    /// network or `None` on an open one, and keep the link joined. The
    /// slices are held by reference until `disconnect` or `attach`; the
    /// passphrase is re-supplied on every recovery attempt and copied
    /// nowhere else. Accepted while ready with nothing in flight, after the
    /// radio up, with no network held; returns `false` otherwise, and for a
    /// slice outside its bounds.
    pub fn join(&mut self, ssid: &'b [u8], passphrase: Option<&'b [u8]>) -> bool {
        self.state == State::Ready && self.station.join(ssid, passphrase)
    }

    /// Record a join request with a credential of any kind:
    /// `Credential::Open`, `Credential::Passphrase` (WPA2-PSK, 8 to 64
    /// bytes) or `Credential::SaePassword` (WPA3-SAE, 1 to 128 bytes: the
    /// firmware runs the SAE exchange and the driver asks it to protect
    /// management frames). Accepted as `join` is; a WPA3 password is
    /// refused once the capability query has answered without the word
    /// `sae`, and a slice outside its bounds is refused.
    pub fn join_with(&mut self, ssid: &'b [u8], credential: Credential<'b>) -> bool {
        self.state == State::Ready && self.station.join_with(ssid, credential)
    }

    /// Record the capability query: the firmware's capability string read
    /// and searched for the words `sae` and `mfp`. Accepted while ready
    /// with nothing in flight; returns `false` otherwise. A reply with a
    /// non-zero status is handed back as an unanswered outcome, never a
    /// refusal: a question about the firmware is not a configuration.
    pub fn capabilities(&mut self) -> bool {
        self.state == State::Ready && self.station.capabilities()
    }

    /// The firmware's capability string as the last poll handed it back,
    /// in the driver's buffer; empty otherwise. Readable until the next
    /// poll.
    pub fn capability_string(&self) -> &[u8] {
        match self.station.capability() {
            Some((at, len)) => &self.frame[at..at + len],
            None => &[],
        }
    }

    /// Record a disconnect: drop the network and send the disassociation.
    /// Accepted while ready with a network held; returns `false` otherwise.
    /// A frame staged for transmission is dropped with the network.
    pub fn disconnect(&mut self) -> bool {
        let accepted = self.state == State::Ready && self.station.disconnect();
        if accepted {
            self.staged = 0;
        }
        accepted
    }

    /// The slot for an Ethernet frame of `len` bytes (14 to 1,514) to
    /// send, in the driver's transmit buffer behind the frame's headers:
    /// the frame is staged from this call and written by [`Driver::flush`]
    /// or by the next polls, when the link is up, the credit window is open
    /// and the chip accepts it. Accepted while ready with the link up and no
    /// frame staged; `None` otherwise -- while the link is not up the caller's
    /// stack keeps its own frames.
    pub fn stage(&mut self, len: usize) -> Option<&mut [u8]> {
        if !can_stage(self.state, self.station.link(), self.staged, len) {
            return None;
        }
        self.staged = len;
        Some(&mut self.data[data::HEAD..data::HEAD + len])
    }

    /// Write the staged frame now, when the link is up, the credit window
    /// is open and no control request is half sent: `Ok(true)` when the
    /// chip took it, `Ok(false)` when it stays staged (nothing staged, the
    /// window closed, a request half sent, the link not up, or the chip's
    /// receive side not ready -- in which case the sequence number is
    /// given back and the next attempt takes a fresh one). A bus refusal
    /// parks the driver, as a poll's would, and is readable afterwards
    /// through [`Driver::refusal`].
    pub fn flush<T: Transport>(&mut self, bus: &mut T) -> Result<bool, Refusal> {
        let (_, mut sender) = self.split();
        sender.flush(bus)
    }

    /// Stage `frame` and write it: `Ok(true)` when the frame was taken
    /// (written, or held for the next polls), `Ok(false)` when it was not
    /// (the link not up, a frame already staged, the length outside 14 to
    /// 1,514).
    pub fn send<T: Transport>(&mut self, bus: &mut T, frame: &[u8]) -> Result<bool, Refusal> {
        let Some(slot) = self.stage(frame.len()) else {
            return Ok(false);
        };
        slot.copy_from_slice(frame);
        self.flush(bus)?;
        Ok(true)
    }

    /// Whether a frame is staged and not yet written.
    pub fn sending(&self) -> bool {
        self.staged != 0
    }

    /// The Ethernet frame of the data frame the last poll handed back, in
    /// the driver's buffer; empty otherwise. Readable until the next poll
    /// that services the packet channel.
    pub fn frame_payload(&self) -> &[u8] {
        match self.layer.last_data() {
            Some((at, len)) => &self.frame[at..at + len],
            None => &[],
        }
    }

    /// The refusal that parked the driver, while it is parked.
    pub fn refusal(&self) -> Option<Refusal> {
        self.refusal
    }

    /// The received frame not yet taken, with its taken mark, and the
    /// transmit side, borrowed apart.
    pub(crate) fn split(&mut self) -> (Option<(&[u8], &mut bool)>, Sender<'_, 'b>) {
        let Driver {
            frame,
            frame_taken,
            data,
            staged,
            layer,
            state,
            refusal,
            station,
            ..
        } = self;
        let received = match layer.last_data() {
            Some((at, len)) if !*frame_taken => Some((&frame[at..at + len], frame_taken)),
            _ => None,
        };
        (
            received,
            Sender {
                data,
                staged,
                layer,
                state,
                refusal,
                station,
            },
        )
    }

    /// The link's state.
    pub fn link(&self) -> LinkState {
        self.station.link()
    }

    /// The chip's own address, once the radio-up request read it back.
    pub fn address(&self) -> Option<[u8; ADDRESS_LEN]> {
        self.station.address()
    }

    /// The payload of the event the last poll handed back, in the driver's
    /// buffer; empty when the last poll handed back no event. Readable
    /// until the next poll that services the packet channel; a poll that
    /// hands back a scan record or a decision touches no frame.
    pub fn event_payload(&self) -> &[u8] {
        match self.layer.last_event() {
            Some((at, len)) => &self.frame[at..at + len],
            None => &[],
        }
    }

    /// The bytes of the scan record the last poll handed back, in the
    /// driver's buffer; empty otherwise. Readable until the next poll that
    /// services the packet channel.
    pub fn record_bytes(&self) -> &[u8] {
        match (self.layer.last_event(), self.station.record()) {
            (Some(_), Some((at, end))) => &self.frame[at..end],
            _ => &[],
        }
    }

    /// Whether the machine is parked at a refusal.
    pub fn is_parked(&self) -> bool {
        self.state == State::Parked
    }

    /// Whether the chip is attached and awaiting the upload.
    pub fn is_attached(&self) -> bool {
        self.state == State::Attached
    }

    /// Whether the firmware is running.
    pub fn is_running(&self) -> bool {
        self.state == State::Running
    }

    /// Whether the control path answers, the firmware is configured and
    /// the driver services the packet channel on every poll.
    pub fn is_ready(&self) -> bool {
        self.state == State::Ready
    }

    /// The packet channel's counters.
    pub fn frames(&self) -> Frames {
        self.layer.frames
    }

    /// The credit window: the next transmit sequence number and the
    /// sequence number the chip has said the host may not reach.
    pub fn credit(&self) -> (u8, u8) {
        self.layer.credit.window()
    }

    /// The firmware's version string as it last answered the version
    /// query; empty before one.
    pub fn version(&self) -> &[u8] {
        self.version.as_bytes()
    }

    /// One bounded step. An event or a scan record the last poll handed
    /// back is no longer readable after a call that services the packet
    /// channel.
    pub fn poll<T: Transport, C: Clock>(&mut self, bus: &mut T, clock: &mut C) -> Wake {
        let now = clock.now_us();
        match self.step(bus, clock, now) {
            Ok(wake) => wake,
            Err(refusal) => {
                self.park(refusal);
                Wake::Done(Outcome::Refused(refusal))
            }
        }
    }

    /// Park the machine at a refusal: the state, the window forgotten, the
    /// requests in flight dropped, the refusal recorded.
    fn park(&mut self, refusal: Refusal) {
        self.state = State::Parked;
        self.window = Window::new();
        self.download = None;
        self.opening = None;
        self.refusal = Some(refusal);
    }

    fn step<T: Transport, C: Clock>(
        &mut self,
        bus: &mut T,
        clock: &mut C,
        now: Micros,
    ) -> Result<Wake, Refusal> {
        match self.state {
            State::Off | State::Parked | State::Attached | State::Running => Ok(Wake::Idle),
            State::Ready => {
                let writable = self.staged != 0
                    && can_write(
                        self.station.link(),
                        self.station.is_sending(),
                        self.layer.credit.open(),
                    );
                if writable && data::write(bus, &mut self.data, self.staged, &mut self.layer)? {
                    self.staged = 0;
                    return Ok(Wake::Again);
                }
                let wake = self.station.step(
                    bus,
                    &mut self.window,
                    &mut self.frame,
                    &mut self.layer,
                    &mut self.ids,
                    now,
                )?;
                if matches!(wake, Wake::Done(Outcome::Frame { .. })) {
                    self.frame_taken = false;
                }
                Ok(wake)
            }
            State::BusAttach => match bus.attach(now)? {
                Attach::Pending { until } => Ok(wait(now, until)),
                Attach::Ready => {
                    self.state = State::ChipId;
                    Ok(Wake::Again)
                }
            },
            State::ChipId => {
                let word = self.window.read32(bus, CHIPCOMMON)?;
                if (word & 0xFFFF) as u16 != bus.chip_id() {
                    return Err(Refusal::new(STAGE_CHIP_IDENTITY, word));
                }
                self.chip_id = word;
                self.state = State::AlpRequest;
                Ok(Wake::Again)
            }
            State::AlpRequest => {
                bus.write_direct(Func::F1, f1::CHIPCLKCSR, ALP_REQUEST)?;
                let next = now + ALP_RETRY_US;
                self.state = State::AlpPoll { tries: 0, next };
                Ok(Wake::At(next))
            }
            State::AlpPoll { tries, next } => {
                if now < next {
                    return Ok(Wake::At(next));
                }
                let csr = bus.read_direct(Func::F1, f1::CHIPCLKCSR)?;
                if csr & clkcsr::ALP_AVAIL != 0 {
                    self.state = State::AlpRelease;
                    return Ok(Wake::Again);
                }
                let tries = tries + 1;
                if tries >= ALP_TRIES {
                    return Err(Refusal::new(STAGE_ALP_CLOCK, u32::from(csr)));
                }
                let next = now + ALP_RETRY_US;
                self.state = State::AlpPoll { tries, next };
                Ok(Wake::At(next))
            }
            State::AlpRelease => {
                bus.write_direct(Func::F1, f1::CHIPCLKCSR, 0)?;
                let until = now + ALP_SETTLE_US;
                self.state = State::Settle { until };
                Ok(Wake::At(until))
            }
            State::Settle { until } => {
                if now < until {
                    return Ok(Wake::At(until));
                }
                self.state = State::Attached;
                Ok(Wake::Done(Outcome::Attached {
                    chip_id: self.chip_id,
                }))
            }
            State::Downloading => {
                let Some(download) = &mut self.download else {
                    self.state = State::Attached;
                    return Ok(Wake::Idle);
                };
                match download.step(bus, clock, now, &mut self.window)? {
                    Progress::Wake(wake) => Ok(wake),
                    Progress::Done { save_restore } => {
                        self.download = None;
                        self.state = State::Running;
                        Ok(Wake::Done(Outcome::Uploaded { save_restore }))
                    }
                }
            }
            State::Opening => {
                let Some(opening) = &mut self.opening else {
                    self.state = State::Running;
                    return Ok(Wake::Idle);
                };
                let step = opening.step(
                    bus,
                    &mut self.window,
                    &mut self.frame,
                    &mut self.layer,
                    &mut self.ids,
                    now,
                    &mut self.version,
                )?;
                match step {
                    OpenStep::Wake(wake) => Ok(wake),
                    OpenStep::Event(event) => Ok(Wake::Done(Outcome::Event(event))),
                    OpenStep::Ready(version) => {
                        self.opening = None;
                        self.state = State::Ready;
                        Ok(Wake::Done(Outcome::Ready { version }))
                    }
                }
            }
        }
    }
}

impl Default for Driver<'_> {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for Driver<'_> {
    /// The state, the chip identity, the window, whether a download or an
    /// opening is in flight, the frame layer, the request id counter, the
    /// version, the event mask, the station, the length of the frame
    /// staged for transmission and the refusal that parked the driver; the
    /// two buffers are omitted.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Driver")
            .field("state", &self.state)
            .field("chip_id", &self.chip_id)
            .field("window", &self.window)
            .field("downloading", &self.download.is_some())
            .field("opening", &self.opening.is_some())
            .field("layer", &self.layer)
            .field("ids", &self.ids)
            .field("version", &self.version)
            .field("events", &self.events)
            .field("station", &self.station)
            .field("staged", &self.staged)
            .field("refusal", &self.refusal)
            .finish_non_exhaustive()
    }
}

/// A wait until `until`, or "again at once" when the instant has passed.
fn wait(now: Micros, until: Micros) -> Wake {
    if until > now {
        Wake::At(until)
    } else {
        Wake::Again
    }
}
