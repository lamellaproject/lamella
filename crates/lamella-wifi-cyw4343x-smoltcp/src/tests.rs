//! The station against a scripted radio: which wake it honors, when it hands a frame and when it
//! holds one, what it counts, what a refusal stops, how a bring-up and a join end -- and a real
//! `smoltcp` interface answering an ARP request through it.

use super::*;
use smoltcp::iface::{Config, Interface, SocketSet, SocketStorage};
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr};
use std::collections::VecDeque;
use std::vec;
use std::vec::Vec;

/// Microseconds the test clock moves on every read, so a wait loop always reaches its deadline.
const TICK: Micros = 10;

/// A clock that moves forward on every read.
struct TestClock {
    now: Micros,
}

impl Clock for TestClock {
    fn now_us(&mut self) -> Micros {
        self.now += TICK;
        self.now
    }

    fn delay_us(&mut self, us: u32) {
        self.now += Micros::from(us);
    }
}

/// One poll's answer: the wake, the frame it delivers, and the driver state it leaves.
struct Step {
    wake: Wake,
    frame: Vec<u8>,
    link: Option<LinkState>,
    ready: Option<bool>,
}

fn step(wake: Wake) -> Step {
    Step { wake, frame: Vec::new(), link: None, ready: None }
}

fn frame_step(frame: &[u8]) -> Step {
    Step { frame: frame.to_vec(), ..step(Wake::Done(Outcome::Frame { len: frame.len() })) }
}

fn link_step(wake: Wake, link: LinkState) -> Step {
    Step { link: Some(link), ..step(wake) }
}

/// A radio that answers each poll from a script and records what it was asked. Past the script's
/// end it answers as a ready driver with nothing to do: wait for the interrupt, or ten milliseconds.
struct Script {
    steps: VecDeque<Step>,
    polls: Vec<Micros>,
    interrupt: bool,
    frame: Vec<u8>,
    staged: Option<Vec<u8>>,
    written: Vec<Vec<u8>>,
    window_open: bool,
    ready: bool,
    link: LinkState,
    requests: Vec<&'static str>,
    accept: bool,
}

impl Script {
    fn new(steps: Vec<Step>) -> Self {
        Script {
            steps: steps.into(),
            polls: Vec::new(),
            interrupt: false,
            frame: Vec::new(),
            staged: None,
            written: Vec::new(),
            window_open: true,
            ready: false,
            link: LinkState::Detached,
            requests: Vec::new(),
            accept: true,
        }
    }

    /// A ready driver with the link up.
    fn joined(steps: Vec<Step>) -> Self {
        Script { ready: true, link: LinkState::Up, ..Script::new(steps) }
    }

    /// Write the staged frame when the window is open, as the driver's poll and flush do.
    fn write_held(&mut self) -> bool {
        if self.window_open && self.staged.is_some() {
            self.written.push(self.staged.take().unwrap());
            return true;
        }
        false
    }

    fn request(&mut self, name: &'static str) -> bool {
        self.requests.push(name);
        self.accept
    }
}

impl Radio for Script {
    fn poll<C: Clock>(&mut self, clock: &mut C) -> Wake {
        let now = clock.now_us();
        self.polls.push(now);
        // A poll services the chip's interrupt, and the chip lowers the line.
        self.interrupt = false;
        self.write_held();
        let Some(step) = self.steps.pop_front() else {
            return Wake::Irq(now + SERVICE_POLL_US);
        };
        if let Some(link) = step.link {
            self.link = link;
        }
        if let Some(ready) = step.ready {
            self.ready = ready;
        }
        self.frame = step.frame;
        step.wake
    }

    fn interrupt(&mut self) -> bool {
        self.interrupt
    }

    fn frame(&self) -> &[u8] {
        &self.frame
    }

    fn stage(&mut self, len: usize) -> Option<&mut [u8]> {
        let can = self.ready && self.link == LinkState::Up && self.staged.is_none() && (14..=ETHERNET_MAX).contains(&len);
        if !can {
            return None;
        }
        self.staged = Some(vec![0; len]);
        self.staged.as_deref_mut()
    }

    fn flush(&mut self) -> Result<bool, Refusal> {
        Ok(self.write_held())
    }

    fn sending(&self) -> bool {
        self.staged.is_some()
    }

    fn ready(&self) -> bool {
        self.ready
    }

    fn link(&self) -> LinkState {
        self.link
    }
}

impl<'b> Control<'b> for Script {
    fn attach(&mut self) -> bool {
        self.request("attach")
    }

    fn upload(&mut self, _firmware: &'b [u8], _settings: &'b [u8]) -> bool {
        self.request("upload")
    }

    fn open(&mut self, _regulatory: &'b [u8], _version: &'b [u8]) -> bool {
        self.request("open")
    }

    fn up(&mut self) -> bool {
        self.request("up")
    }

    fn join(&mut self, _ssid: &'b [u8], _credential: Credential<'b>) -> bool {
        self.request("join")
    }

    fn disconnect(&mut self) -> bool {
        self.request("disconnect")
    }
}

fn station(script: Script) -> Station<Script, TestClock> {
    Station::new(script, TestClock { now: 0 })
}

const OURS: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x00, 0x01];
const PEER: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x00, 0x02];

/// An ARP request from the peer at 192.168.1.1 for 192.168.1.2, broadcast.
fn arp_request() -> Vec<u8> {
    let mut frame = Vec::new();
    frame.extend_from_slice(&[0xff; 6]);
    frame.extend_from_slice(&PEER);
    frame.extend_from_slice(&[0x08, 0x06]);
    frame.extend_from_slice(&[0x00, 0x01, 0x08, 0x00, 6, 4, 0x00, 0x01]);
    frame.extend_from_slice(&PEER);
    frame.extend_from_slice(&[192, 168, 1, 1]);
    frame.extend_from_slice(&[0; 6]);
    frame.extend_from_slice(&[192, 168, 1, 2]);
    frame
}

/// A loss event: the chip's deauthentication, number 5.
const LOSS: lamella_wifi_cyw4343x::Event =
    lamella_wifi_cyw4343x::Event { number: 5, status: 0, reason: 0, auth_type: 0, address: PEER, len: 0 };

const IMAGES: Images<'static> = Images { firmware: b"fw", settings: b"s\0\0", regulatory: b"clm", version: b"7.95.49" };

#[test]
fn a_frame_is_copied_out_and_handed_with_a_token_that_writes_the_reply() {
    let mut s = station(Script::joined(vec![frame_step(&arp_request())]));
    let (rx, tx) = s.receive(Instant::ZERO).expect("the frame");
    assert_eq!(rx.consume(|frame| frame.to_vec()), arp_request());
    tx.consume(60, |slot| slot.fill(0xab));
    assert_eq!(s.radio().written, vec![vec![0xab; 60]], "written at once: the window was open");
    assert!(s.receive(Instant::ZERO).is_none(), "taken once");
    assert_eq!(s.counters(), Counters { received: 1, dropped: 0, sent: 1, link_losses: 0 });
}

#[test]
fn an_interrupt_wait_ends_at_its_instant_or_at_the_interrupt_and_a_timed_wait_only_at_its_instant() {
    let mut s = station(Script::joined(vec![step(Wake::Irq(1_000_000))]));
    s.service();
    assert_eq!(s.radio().polls.len(), 1);
    for _ in 0..50 {
        s.service();
    }
    assert_eq!(s.radio().polls.len(), 1, "not before the instant with the interrupt low");
    s.radio.interrupt = true;
    s.service();
    assert_eq!(s.radio().polls.len(), 2, "the interrupt ends an interrupt wait at once");

    let mut s = station(Script::joined(vec![step(Wake::At(1_000_000))]));
    s.radio.interrupt = true;
    s.service();
    for _ in 0..50 {
        s.service();
    }
    assert_eq!(s.radio().polls.len(), 1, "a timed wait is the driver's own: the interrupt does not end it");
    s.clock.now = 1_000_000;
    s.service();
    assert_eq!(s.radio().polls.len(), 2, "at its instant");
}

#[test]
fn a_frame_held_by_a_closed_window_is_polled_at_once_and_its_poll_writes_it() {
    let mut script = Script::joined(vec![step(Wake::Irq(1_000_000))]);
    script.window_open = false;
    let mut s = station(script);
    s.transmit(Instant::ZERO).expect("nothing staged").consume(42, |slot| slot.fill(1));
    assert!(s.radio().sending(), "held: the window is closed");
    assert!(s.transmit(Instant::ZERO).is_none(), "no second frame while one is held");
    let polls = s.radio().polls.len();
    s.service();
    assert_eq!(s.radio().polls.len(), polls + POLLS_PER_CALL as usize, "due on every poll while held");
    s.radio.window_open = true;
    s.service();
    assert_eq!(s.radio().written, vec![vec![1; 42]]);
    assert!(s.transmit(Instant::ZERO).is_some());
}

#[test]
fn a_frame_waits_while_the_reply_could_not_be_staged_and_a_second_one_is_dropped() {
    let mut script = Script::joined(vec![frame_step(&arp_request()), frame_step(&[7; 60])]);
    script.window_open = false;
    script.staged = Some(vec![9; 42]);
    let mut s = station(script);
    assert!(s.receive(Instant::ZERO).is_none(), "the reply could not be staged");
    assert!(s.receive(Instant::ZERO).is_none(), "still held");
    assert_eq!(s.counters().dropped, 1, "the second frame came while the first waited");
    s.radio.window_open = true;
    let (rx, _) = s.receive(Instant::ZERO).expect("handed once the held frame is written");
    assert_eq!(rx.consume(|frame| frame.to_vec()), arp_request(), "the older frame, kept in order");
    assert_eq!(s.radio().written, vec![vec![9; 42]]);
}

#[test]
fn the_link_is_the_drivers_and_a_loss_is_counted() {
    let mut s = station(Script::joined(vec![
        link_step(Wake::Done(Outcome::LinkLost(LOSS)), LinkState::Down),
    ]));
    assert!(s.link_up());
    s.service();
    assert!(!s.link_up());
    assert_eq!(s.counters().link_losses, 1);
    assert!(s.transmit(Instant::ZERO).is_none(), "no token while the link is down");
}

#[test]
fn a_refusal_stops_the_pump() {
    let refusal = Refusal::new("a stage", 0x1234);
    let mut s = station(Script::joined(vec![step(Wake::Done(Outcome::Refused(refusal)))]));
    s.service();
    assert_eq!(s.refusal(), Some(refusal));
    let polls = s.radio().polls.len();
    s.clock.now = 10_000_000;
    s.service();
    assert!(s.receive(Instant::ZERO).is_none());
    assert_eq!(s.radio().polls.len(), polls, "a refused driver is polled no further");
}

#[test]
fn a_bring_up_requests_each_step_in_order_and_returns_the_chips_address() {
    let mut s = station(Script::new(vec![
        step(Wake::Again),
        step(Wake::Done(Outcome::Attached { chip_id: 0x1515_a9af })),
        step(Wake::At(500)),
        step(Wake::Done(Outcome::Uploaded { save_restore: true })),
        link_step(Wake::Done(Outcome::Ready { version: lamella_wifi_cyw4343x::Version::EMPTY }), LinkState::Detached),
        step(Wake::Done(Outcome::Up { address: OURS })),
    ]));
    let mut seen = Vec::new();
    let address = s.bring_up(IMAGES, 1_000_000, &mut |outcome, _| seen.push(outcome));
    assert_eq!(address, Ok(OURS));
    assert_eq!(s.address(), Some(OURS));
    assert_eq!(s.radio().requests, ["attach", "upload", "open", "up"]);
    assert_eq!(seen.len(), 4, "every outcome shown: {seen:?}");
}

#[test]
fn a_bring_up_stops_at_a_refusal_a_rejected_request_and_its_deadline() {
    let refusal = Refusal::new("chip identity", 0x4373);
    let mut s = station(Script::new(vec![step(Wake::Done(Outcome::Refused(refusal)))]));
    assert_eq!(s.bring_up(IMAGES, 1_000_000, &mut |_, _| {}), Err(Stop::Refused(refusal)));

    let mut script = Script::new(Vec::new());
    script.accept = false;
    assert_eq!(station(script).bring_up(IMAGES, 1_000_000, &mut |_, _| {}), Err(Stop::Rejected("attach")));

    let mut s = station(Script::new(vec![step(Wake::At(u64::MAX))]));
    assert_eq!(s.bring_up(IMAGES, 5_000, &mut |_, _| {}), Err(Stop::TimedOut));
}

#[test]
fn a_join_waits_through_a_failure_the_driver_retries_and_ends_at_one_it_does_not() {
    let retried = JoinFailure::Association { status: 3, assoc_status: 0, auth_status: 0 };
    let final_failure = JoinFailure::Supplicant { reason: 15 };
    let mut s = station(Script::joined(vec![
        link_step(Wake::Done(Outcome::JoinFailed(retried)), LinkState::Down),
        link_step(Wake::Done(Outcome::Joined { bssid: PEER }), LinkState::Up),
    ]));
    let mut seen = Vec::new();
    assert_eq!(s.join(b"net", Credential::Open, 1_000_000, &mut |o, _| seen.push(o)), Ok(PEER));
    assert_eq!(seen.len(), 2, "the retried failure was shown: {seen:?}");

    let mut s = station(Script::joined(vec![link_step(
        Wake::Done(Outcome::JoinFailed(final_failure)),
        LinkState::Detached,
    )]));
    assert_eq!(
        s.join(b"net", Credential::Passphrase(b"wrong-pass"), 1_000_000, &mut |_, _| {}),
        Err(Stop::JoinFailed(final_failure))
    );
}

#[test]
fn a_smoltcp_interface_answers_an_arp_request_through_the_station() {
    let mut s = station(Script::joined(vec![frame_step(&arp_request())]));
    let mut config = Config::new(HardwareAddress::Ethernet(EthernetAddress(OURS)));
    config.random_seed = 1;
    let mut iface = Interface::new(config, &mut s, smoltcp::time::Instant::ZERO);
    iface.update_ip_addrs(|addrs| {
        addrs.push(IpCidr::new(IpAddress::v4(192, 168, 1, 2), 24)).unwrap();
    });
    let mut storage = [SocketStorage::EMPTY; 1];
    let mut sockets = SocketSet::new(&mut storage[..]);
    iface.poll(smoltcp::time::Instant::ZERO, &mut s, &mut sockets);
    let written = &s.radio().written;
    assert_eq!(written.len(), 1, "one reply");
    let reply = &written[0];
    assert_eq!(&reply[0..6], &PEER, "to the asker");
    assert_eq!(&reply[6..12], &OURS, "from the chip's address");
    assert_eq!(&reply[12..14], &[0x08, 0x06], "ARP");
    assert_eq!(&reply[20..22], &[0x00, 0x02], "a reply");
    assert_eq!(&reply[28..32], &[192, 168, 1, 2], "for our address");
}
