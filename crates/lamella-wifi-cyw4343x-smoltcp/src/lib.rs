//! The CYW43439 and CYW4343W Wi-Fi driver as an owning `smoltcp` network device. A [`Station`]
//! owns the radio -- the driver over its bus, as a [`Chip`] -- and the clock it runs on, and the
//! station is the device: the stack's own receive and transmit calls pump the driver, so a backend
//! that owns its device runs the radio inside its drive loop with nothing else to schedule.
//! [`Station::bring_up`] and [`Station::join`] take the chip from power-up to a joined network
//! before the stack is built, and after that the station reports the link.

#![no_std]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

use lamella_wifi_cyw4343x::control::SERVICE_POLL_US;
use lamella_wifi_cyw4343x::data::ETHERNET_MAX;
use lamella_wifi_cyw4343x::gspi::{Gspi, GspiWire};
use lamella_wifi_cyw4343x::{
    Clock, Credential, Driver, JoinFailure, LinkState, Micros, Outcome, Refusal, Transport, Wake,
};
use smoltcp::phy::{ChecksumCapabilities, Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::time::Instant;

#[cfg(test)]
mod tests;

/// The most driver polls one receive or transmit call of the stack's makes, so a call never runs
/// the bus unbounded. A call that reaches it leaves the driver due, and the next call goes on.
pub const POLLS_PER_CALL: u32 = 16;

/// The radio's pump and data plane: what a [`Station`] asks of it on every call of the stack's.
pub trait Radio {
    /// One bounded step of the driver.
    fn poll<C: Clock>(&mut self, clock: &mut C) -> Wake;

    /// Whether the chip's interrupt is asserted now, read without a bus transfer; `false` when the
    /// bus cannot tell, in which case the station waits out every deadline.
    fn interrupt(&mut self) -> bool;

    /// The Ethernet frame the last poll handed back.
    fn frame(&self) -> &[u8];

    /// The slot for an Ethernet frame of `len` bytes, the frame staged; `None` while one cannot be.
    fn stage(&mut self, len: usize) -> Option<&mut [u8]>;

    /// Write the staged frame if it can be written now: `Ok(true)` written, `Ok(false)` held.
    fn flush(&mut self) -> Result<bool, Refusal>;

    /// Whether a staged frame waits to be written.
    fn sending(&self) -> bool;

    /// Whether the control path answers and the driver services the packet channel.
    fn ready(&self) -> bool;

    /// The link's state.
    fn link(&self) -> LinkState;
}

/// The radio's requests, each recorded at once and carried out by the polls that follow. `'b` is the
/// life of the images and the credentials the driver holds by reference.
pub trait Control<'b>: Radio {
    /// Bring the bus up and identify the chip.
    fn attach(&mut self) -> bool;

    /// Write the firmware and settings images and start the firmware.
    fn upload(&mut self, firmware: &'b [u8], settings: &'b [u8]) -> bool;

    /// Open the control path with the regulatory blob; the firmware's version reply must contain
    /// `version`.
    fn open(&mut self, regulatory: &'b [u8], version: &'b [u8]) -> bool;

    /// Bring the radio up and read the chip's own address.
    fn up(&mut self) -> bool;

    /// Join the network named `ssid` and keep it joined.
    fn join(&mut self, ssid: &'b [u8], credential: Credential<'b>) -> bool;

    /// Drop the network.
    fn disconnect(&mut self) -> bool;
}

/// A bus that can read the chip's interrupt without a transfer.
pub trait Interrupt {
    /// Whether the chip's interrupt is asserted now.
    fn asserted(&mut self) -> bool;
}

/// The gSPI bus reads the interrupt on its shared data line, which the chip drives high between
/// frames to ask for service.
impl<W: GspiWire> Interrupt for Gspi<W> {
    fn asserted(&mut self) -> bool {
        self.wire_mut().irq_asserted()
    }
}

/// The driver over its bus: the radio a board builds.
pub struct Chip<'b, T> {
    driver: Driver<'b>,
    bus: T,
}

impl<'b, T> Chip<'b, T> {
    /// The driver, in its power-up state, over `bus`.
    pub const fn new(bus: T) -> Self {
        Chip { driver: Driver::new(), bus }
    }

    /// The driver.
    pub fn driver(&self) -> &Driver<'b> {
        &self.driver
    }

    /// The bus.
    pub fn bus(&self) -> &T {
        &self.bus
    }
}

impl<T: Transport + Interrupt> Radio for Chip<'_, T> {
    fn poll<C: Clock>(&mut self, clock: &mut C) -> Wake {
        self.driver.poll(&mut self.bus, clock)
    }

    fn interrupt(&mut self) -> bool {
        self.bus.asserted()
    }

    fn frame(&self) -> &[u8] {
        self.driver.frame_payload()
    }

    fn stage(&mut self, len: usize) -> Option<&mut [u8]> {
        self.driver.stage(len)
    }

    fn flush(&mut self) -> Result<bool, Refusal> {
        self.driver.flush(&mut self.bus)
    }

    fn sending(&self) -> bool {
        self.driver.sending()
    }

    fn ready(&self) -> bool {
        self.driver.is_ready()
    }

    fn link(&self) -> LinkState {
        self.driver.link()
    }
}

impl<'b, T: Transport + Interrupt> Control<'b> for Chip<'b, T> {
    fn attach(&mut self) -> bool {
        self.driver.attach()
    }

    fn upload(&mut self, firmware: &'b [u8], settings: &'b [u8]) -> bool {
        self.driver.upload(firmware, settings)
    }

    fn open(&mut self, regulatory: &'b [u8], version: &'b [u8]) -> bool {
        self.driver.open(regulatory, Some(version))
    }

    fn up(&mut self) -> bool {
        self.driver.up()
    }

    fn join(&mut self, ssid: &'b [u8], credential: Credential<'b>) -> bool {
        self.driver.join_with(ssid, credential)
    }

    fn disconnect(&mut self) -> bool {
        self.driver.disconnect()
    }
}

/// The images the chip runs, and the version string the firmware's reply must contain. The station
/// holds them by reference for as long as the driver runs.
#[derive(Clone, Copy, Debug)]
pub struct Images<'b> {
    /// The firmware image.
    pub firmware: &'b [u8],
    /// The board-settings image in its converted form: NUL-separated `key=value` strings and a
    /// closing NUL.
    pub settings: &'b [u8],
    /// The regulatory blob.
    pub regulatory: &'b [u8],
    /// The version string the firmware's reply must contain.
    pub version: &'b [u8],
}

/// Why a bring-up or a join stopped short.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    /// The driver did not accept the named request in the state it was in.
    Rejected(&'static str),
    /// The driver stopped at a named stage.
    Refused(Refusal),
    /// The join failed in a way the driver does not retry -- a handshake that timed out, the usual
    /// sign of a wrong passphrase, or a network advertising no suite the credential can use.
    JoinFailed(JoinFailure),
    /// The time allowed ran out first. A join goes on in the driver, and the link comes up later if
    /// the network is found while the station is pumped.
    TimedOut,
}

/// What the station has counted since it was built.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    /// Frames the chip delivered and the station handed to the stack.
    pub received: u32,
    /// Frames the chip delivered while an earlier one still waited for the stack, and so dropped.
    pub dropped: u32,
    /// Frames the stack sent.
    pub sent: u32,
    /// Times the link was lost.
    pub link_losses: u32,
}

/// The radio and its clock as one `smoltcp` device: the driver pumped inside the stack's receive
/// and transmit calls, each received frame copied out of the driver and handed to the stack with a
/// token for its reply, the stack's frames staged in the driver and written under the chip's credit
/// window.
///
/// The driver is polled only when it is due: at the instant its last wake named, at once while a
/// staged frame waits to be written, and -- when its wake allows it -- as soon as the chip's
/// interrupt is asserted. The driver checks every deadline against its own records, so a poll that
/// comes early costs bus time and nothing else.
pub struct Station<R, C> {
    radio: R,
    clock: C,
    next_poll: Micros,
    on_interrupt: bool,
    rx: [u8; ETHERNET_MAX],
    rx_len: usize,
    address: Option<[u8; 6]>,
    refusal: Option<Refusal>,
    counters: Counters,
}

impl<R, C> Station<R, C> {
    /// A station over `radio`, timed by `clock`.
    pub const fn new(radio: R, clock: C) -> Self {
        Station {
            radio,
            clock,
            next_poll: 0,
            on_interrupt: false,
            rx: [0; ETHERNET_MAX],
            rx_len: 0,
            address: None,
            refusal: None,
            counters: Counters { received: 0, dropped: 0, sent: 0, link_losses: 0 },
        }
    }

    /// The radio.
    pub fn radio(&self) -> &R {
        &self.radio
    }

    /// The chip's own address, once the radio came up: the interface's hardware address.
    pub fn address(&self) -> Option<[u8; 6]> {
        self.address
    }

    /// The refusal that stopped the driver, once one has; the station polls it no further.
    pub fn refusal(&self) -> Option<Refusal> {
        self.refusal
    }

    /// What the station has counted.
    pub fn counters(&self) -> Counters {
        self.counters
    }
}

impl<R: Radio, C: Clock> Station<R, C> {
    /// Whether the link is up.
    pub fn link_up(&self) -> bool {
        self.radio.link() == LinkState::Up
    }

    /// The link's state.
    pub fn link(&self) -> LinkState {
        self.radio.link()
    }

    /// Poll the driver while it is due, for at most [`POLLS_PER_CALL`] polls, stopping at a frame.
    /// The stack's calls pump by themselves; a caller with no stack running -- between two
    /// programs, say -- calls this to keep the link serviced.
    pub fn service(&mut self) {
        for _ in 0..POLLS_PER_CALL {
            if !self.due() {
                return;
            }
            let wake = self.radio.poll(&mut self.clock);
            if let Some(Outcome::Frame { .. }) = self.settle(wake) {
                return;
            }
        }
    }

    /// Whether the driver should be polled now.
    fn due(&mut self) -> bool {
        if self.refusal.is_some() {
            return false;
        }
        if self.radio.sending() {
            return true;
        }
        self.clock.now_us() >= self.next_poll || (self.on_interrupt && self.radio.interrupt())
    }

    /// Record when `wake` asks to be polled next, and take its outcome in, if it carries one.
    fn settle(&mut self, wake: Wake) -> Option<Outcome> {
        let now = self.clock.now_us();
        let (next, on_interrupt) = match wake {
            Wake::Again | Wake::Done(_) => (now, false),
            Wake::At(at) => (at, false),
            Wake::Irq(at) => (at, true),
            Wake::Idle => (now.saturating_add(SERVICE_POLL_US), true),
        };
        self.next_poll = next;
        self.on_interrupt = on_interrupt;
        let Wake::Done(outcome) = wake else { return None };
        match outcome {
            Outcome::Frame { len } => {
                if self.rx_len == 0 {
                    let frame = self.radio.frame();
                    let len = len.min(frame.len());
                    self.rx[..len].copy_from_slice(&frame[..len]);
                    self.rx_len = len;
                } else {
                    self.counters.dropped = self.counters.dropped.wrapping_add(1);
                }
            }
            Outcome::Up { address } => self.address = Some(address),
            Outcome::LinkLost(_) => self.counters.link_losses = self.counters.link_losses.wrapping_add(1),
            Outcome::Refused(refusal) => self.refusal = Some(refusal),
            _ => {}
        }
        Some(outcome)
    }

    /// Whether the driver could take a frame the stack stages now.
    fn can_stage(&self) -> bool {
        self.radio.ready() && self.radio.link() == LinkState::Up && !self.radio.sending()
    }

    /// Pump the driver until it hands back an outcome `want` accepts, stopping at a refusal, at a
    /// failure `fails` names, or at `deadline`. Every outcome is shown to `seen` with the
    /// microseconds since `start`.
    fn await_outcome(
        &mut self,
        start: Micros,
        deadline: Micros,
        seen: &mut dyn FnMut(Outcome, Micros),
        want: impl Fn(&Outcome) -> bool,
        fails: impl Fn(&Outcome, LinkState) -> Option<Stop>,
    ) -> Result<Outcome, Stop> {
        loop {
            if let Some(refusal) = self.refusal {
                return Err(Stop::Refused(refusal));
            }
            if self.clock.now_us() >= deadline {
                return Err(Stop::TimedOut);
            }
            if !self.due() {
                continue;
            }
            let wake = self.radio.poll(&mut self.clock);
            let Some(outcome) = self.settle(wake) else { continue };
            seen(outcome, self.clock.now_us().saturating_sub(start));
            if want(&outcome) {
                return Ok(outcome);
            }
            if let Some(stop) = fails(&outcome, self.radio.link()) {
                return Err(stop);
            }
        }
    }
}

impl<'b, R: Control<'b>, C: Clock> Station<R, C> {
    /// Take the chip from power-up to a raised radio -- the bus attached and the chip identified, the
    /// images uploaded and the firmware started, the control path opened with the firmware's version
    /// checked, the radio up -- within `timeout_us`. Returns the chip's own address. Every outcome on
    /// the way is shown to `seen`, with the microseconds since the call began.
    pub fn bring_up(
        &mut self,
        images: Images<'b>,
        timeout_us: Micros,
        seen: &mut dyn FnMut(Outcome, Micros),
    ) -> Result<[u8; 6], Stop> {
        let start = self.clock.now_us();
        let deadline = start.saturating_add(timeout_us);
        let never = |_: &Outcome, _: LinkState| None;
        if !self.radio.attach() {
            return Err(Stop::Rejected("attach"));
        }
        self.next_poll = start;
        self.await_outcome(start, deadline, seen, |o| matches!(o, Outcome::Attached { .. }), never)?;
        if !self.radio.upload(images.firmware, images.settings) {
            return Err(Stop::Rejected("upload"));
        }
        self.await_outcome(start, deadline, seen, |o| matches!(o, Outcome::Uploaded { .. }), never)?;
        if !self.radio.open(images.regulatory, images.version) {
            return Err(Stop::Rejected("open"));
        }
        self.await_outcome(start, deadline, seen, |o| matches!(o, Outcome::Ready { .. }), never)?;
        if !self.radio.up() {
            return Err(Stop::Rejected("up"));
        }
        match self.await_outcome(start, deadline, seen, |o| matches!(o, Outcome::Up { .. }), never)? {
            Outcome::Up { address } => Ok(address),
            _ => Err(Stop::TimedOut),
        }
    }

    /// Join the network named `ssid` with `credential`, waiting up to `timeout_us` for the link.
    /// Returns the access point's address. A failure the driver retries is shown to `seen` and
    /// waited through; one it does not retry ends the join at once.
    pub fn join(
        &mut self,
        ssid: &'b [u8],
        credential: Credential<'b>,
        timeout_us: Micros,
        seen: &mut dyn FnMut(Outcome, Micros),
    ) -> Result<[u8; 6], Stop> {
        let start = self.clock.now_us();
        let deadline = start.saturating_add(timeout_us);
        if !self.radio.join(ssid, credential) {
            return Err(Stop::Rejected("join"));
        }
        self.next_poll = start;
        // A failed attempt that leaves the network held is retried by the driver's link machine;
        // one that detaches it is final.
        let final_failure = |outcome: &Outcome, link: LinkState| match outcome {
            Outcome::JoinFailed(failure) if link == LinkState::Detached => Some(Stop::JoinFailed(*failure)),
            _ => None,
        };
        match self.await_outcome(start, deadline, seen, |o| matches!(o, Outcome::Joined { .. }), final_failure)? {
            Outcome::Joined { bssid } => Ok(bssid),
            _ => Err(Stop::TimedOut),
        }
    }

    /// Drop the network. The disassociation goes out on the polls that follow.
    pub fn disconnect(&mut self) -> bool {
        self.radio.disconnect()
    }
}

/// The receive token: the frame the station copied out of the driver.
pub struct Receive<'a> {
    frame: &'a [u8],
}

impl RxToken for Receive<'_> {
    fn consume<T, F>(self, f: F) -> T
    where
        F: FnOnce(&[u8]) -> T,
    {
        f(self.frame)
    }
}

/// The transmit token: a slot in the driver's transmit buffer, staged when the stack fills it and
/// written then, or by the next poll while the chip's window is closed.
pub struct Transmit<'a, R> {
    radio: &'a mut R,
    sent: &'a mut u32,
    refusal: &'a mut Option<Refusal>,
}

impl<R: Radio> TxToken for Transmit<'_, R> {
    fn consume<T, F>(self, len: usize, f: F) -> T
    where
        F: FnOnce(&mut [u8]) -> T,
    {
        let slot = self
            .radio
            .stage(len)
            .expect("a transmit token is handed out only while a frame of the stated unit can be staged");
        let result = f(slot);
        *self.sent = self.sent.wrapping_add(1);
        if let Err(refusal) = self.radio.flush() {
            *self.refusal = Some(refusal);
        }
        result
    }
}

impl<R: Radio, C: Clock> Device for Station<R, C> {
    type RxToken<'a>
        = Receive<'a>
    where
        Self: 'a;
    type TxToken<'a>
        = Transmit<'a, R>
    where
        Self: 'a;

    /// Pump the driver, then hand the stack the frame it delivered with a token for the reply --
    /// while the driver could take that reply. A frame that waits keeps its place, and the pump
    /// that writes the staged frame ahead of it goes on.
    fn receive(&mut self, _timestamp: Instant) -> Option<(Receive<'_>, Transmit<'_, R>)> {
        self.service();
        if self.rx_len == 0 || !self.can_stage() {
            return None;
        }
        let len = core::mem::take(&mut self.rx_len);
        self.counters.received = self.counters.received.wrapping_add(1);
        let Station { radio, rx, counters, refusal, .. } = self;
        Some((
            Receive { frame: &rx[..len] },
            Transmit { radio, sent: &mut counters.sent, refusal },
        ))
    }

    /// A token while the driver could take a frame now, pumping it first if it could not.
    fn transmit(&mut self, _timestamp: Instant) -> Option<Transmit<'_, R>> {
        if !self.can_stage() {
            self.service();
            if !self.can_stage() {
                return None;
            }
        }
        let Station { radio, counters, refusal, .. } = self;
        Some(Transmit { radio, sent: &mut counters.sent, refusal })
    }

    /// Ethernet, a unit of 1,514 bytes, a burst of one, every checksum the stack's.
    fn capabilities(&self) -> DeviceCapabilities {
        let mut capabilities = DeviceCapabilities::default();
        capabilities.medium = Medium::Ethernet;
        capabilities.max_transmission_unit = ETHERNET_MAX;
        capabilities.max_burst_size = Some(1);
        capabilities.checksum = ChecksumCapabilities::default();
        capabilities
    }
}

/// A station borrowed for as long as a backend holds it: what a firmware hands a backend that the
/// firmware rebuilds for every program while the station lives on.
impl<R: Radio, C: Clock> Device for &mut Station<R, C> {
    type RxToken<'a>
        = Receive<'a>
    where
        Self: 'a;
    type TxToken<'a>
        = Transmit<'a, R>
    where
        Self: 'a;

    fn receive(&mut self, timestamp: Instant) -> Option<(Receive<'_>, Transmit<'_, R>)> {
        (**self).receive(timestamp)
    }

    fn transmit(&mut self, timestamp: Instant) -> Option<Transmit<'_, R>> {
        (**self).transmit(timestamp)
    }

    fn capabilities(&self) -> DeviceCapabilities {
        (**self).capabilities()
    }
}
