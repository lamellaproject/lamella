//! The control path at the transport level: the interrupt setup, the credit
//! window opened by the boot event frames, the regulatory download in two
//! chunks, the five configuration settings, the version query and the event
//! mask as one recorded exchange of the trait's own operations, with its
//! controls; then the channel serviced at `Ready`, two events handed back
//! and three empty passes, with the event path's controls.

use super::download::{FIRMWARE, SETTINGS, full_script, re, wd, we};
use super::download_tie::leak;
use crate::clock::Micros;
use crate::control::{
    STAGE_CREDIT, STAGE_EVENT_MASK, STAGE_EXPECTED_VERSION, STAGE_REGULATORY, STAGE_REPLY_TIMEOUT,
    STAGE_VERSION_MISMATCH, Version,
};
use crate::driver::{Driver, Outcome};
use crate::error::Refusal;
use crate::event::{Event, EventMask, Link, number};
use crate::fixture::{End, FakeClock, FakeTransport, Op, Run, run};
use crate::frame::Frames;
use crate::transport::Part;
use std::vec::Vec;

const CAP: u32 = 100_000;

/// The opening's first row in the full script: after the attach's eight
/// and the download's 65.
pub(super) const OPEN: usize = 8 + 65;

const fn blob() -> [u8; 1500] {
    let mut b = [0u8; 1500];
    let mut i = 0;
    while i < 1500 {
        b[i] = ((i * 0x17) as u8) ^ 0x5A;
        i += 1;
    }
    b
}

/// A synthetic regulatory blob of 1,500 bytes: two chunks.
pub(super) static BLOB: [u8; 1500] = blob();
/// The version string expected of the firmware.
pub(super) const EXPECTED: &[u8] = b"7.95.49";
/// The firmware's synthetic reply to the version query.
pub(super) const VERSION_REPLY: &[u8] = b"wl0: Sep 10 2026 00:00:00 version 7.95.49 (fixture CY)\n";
/// The reply once stripped.
pub(super) const VERSION_STRIPPED: &[u8] =
    b"wl0: Sep 10 2026 00:00:00 version 7.95.49 (fixture CY)";

/// A frame header by the design's rules: the size and its complement, the
/// sequence, the channel, the payload's offset, the credit.
pub(super) fn header(size: u16, seq: u8, channel: u8, doff: u8, credit: u8) -> Vec<u8> {
    let mut h = std::vec![0u8; 12];
    h[0..2].copy_from_slice(&size.to_le_bytes());
    h[2..4].copy_from_slice(&(!size).to_le_bytes());
    h[4] = seq;
    h[5] = channel;
    h[7] = doff;
    h[9] = credit;
    h
}

/// A request frame by the design's rules.
pub(super) fn request_frame(
    seq: u8,
    id: u16,
    set: bool,
    cmd: u32,
    name: &[u8],
    value: &[u8],
) -> Vec<u8> {
    let name_len = if name.is_empty() { 0 } else { name.len() + 1 };
    let mut f = header((28 + name_len + value.len()) as u16, seq, 0, 12, 0);
    f.extend_from_slice(&cmd.to_le_bytes());
    f.extend_from_slice(&((name_len + value.len()) as u32).to_le_bytes());
    let flags = (u32::from(id) << 16) | (u32::from(set) * 2);
    f.extend_from_slice(&flags.to_le_bytes());
    f.extend_from_slice(&0u32.to_le_bytes());
    if name_len > 0 {
        f.extend_from_slice(name);
        f.push(0);
    }
    f.extend_from_slice(value);
    f
}

/// A reply frame: the header with its credit, the control header echoing
/// the command and the id with the status, the payload.
pub(super) fn reply_frame(
    seq: u8,
    credit: u8,
    cmd: u32,
    id: u16,
    status: u32,
    payload: &[u8],
) -> Vec<u8> {
    let mut f = header((28 + payload.len()) as u16, seq, 0, 12, credit);
    f.extend_from_slice(&cmd.to_le_bytes());
    f.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    f.extend_from_slice(&(u32::from(id) << 16).to_le_bytes());
    f.extend_from_slice(&status.to_le_bytes());
    f.extend_from_slice(payload);
    f
}

/// A boot event frame: 256 bytes on channel 1, the body zero (the walk
/// drops it at the ethertype, as the observation record says the boot
/// frames are rejected above the bus).
pub(super) fn event_frame(seq: u8, credit: u8) -> Vec<u8> {
    let mut f = header(256, seq, 1, 12, credit);
    f.resize(256, 0);
    f
}

/// The access point's address in the synthetic events.
pub(super) const ADDRESS: [u8; 6] = [0x00, 0x11, 0x22, 0x33, 0x44, 0x55];
/// The synthetic link event's payload.
pub(super) const LINK_PAYLOAD: [u8; 8] = [0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02, 0x03, 0x04];

/// An event frame by the design's rules: the frame header on channel 1
/// with its credit, the BDC header with `bdc_words` words of padding, the
/// Ethernet header with the vendor ethertype, the vendor header with its
/// OUI, the 48-byte event header with its fields big-endian, the payload.
#[allow(clippy::too_many_arguments)]
pub(super) fn event_of(
    seq: u8,
    credit: u8,
    bdc_words: u8,
    number: u32,
    status: u32,
    reason: u32,
    auth_type: u32,
    address: [u8; 6],
    payload: &[u8],
) -> Vec<u8> {
    let size = 12 + 4 + 4 * usize::from(bdc_words) + 14 + 10 + 48 + payload.len();
    let mut f = header(size as u16, seq, 1, 12, credit);
    f.extend_from_slice(&[0x20, 0x00, 0x00, bdc_words]);
    f.resize(f.len() + 4 * usize::from(bdc_words), 0);
    f.extend_from_slice(&[0x00, 0xA0, 0x50, 0x00, 0x00, 0x01]);
    f.extend_from_slice(&address);
    f.extend_from_slice(&[0x88, 0x6C]);
    f.extend_from_slice(&[0x00, 0x01]);
    f.extend_from_slice(&((48 + payload.len()) as u16).to_be_bytes());
    f.push(0x01);
    f.extend_from_slice(&[0x00, 0x10, 0x18]);
    f.extend_from_slice(&[0x00, 0x01]);
    f.extend_from_slice(&[0x00, 0x01, 0x00, 0x00]);
    f.extend_from_slice(&number.to_be_bytes());
    f.extend_from_slice(&status.to_be_bytes());
    f.extend_from_slice(&reason.to_be_bytes());
    f.extend_from_slice(&auth_type.to_be_bytes());
    f.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    f.extend_from_slice(&address);
    let mut name = [0u8; 16];
    name[..3].copy_from_slice(b"wl0");
    f.extend_from_slice(&name);
    f.extend_from_slice(&[0x00, 0x00]);
    f.extend_from_slice(payload);
    assert_eq!(f.len(), size);
    f
}

/// The synthetic radio event delivered after `Ready`: 88 bytes.
pub(super) fn radio_frame() -> Vec<u8> {
    event_of(11, 26, 0, number::RADIO, 0, 0, 0, ADDRESS, &[])
}

/// The synthetic link event delivered after it: 100 bytes, one word of
/// BDC padding, an eight-byte payload.
pub(super) fn link_frame() -> Vec<u8> {
    event_of(12, 27, 1, number::LINK, 0, 0, 0, ADDRESS, &LINK_PAYLOAD)
}

/// The radio event as the driver hands it back.
pub(super) const RADIO_EVENT: Event = Event {
    number: number::RADIO,
    status: 0,
    reason: 0,
    auth_type: 0,
    address: ADDRESS,
    len: 0,
};

/// The link event as the driver hands it back.
pub(super) const LINK_EVENT: Event = Event {
    number: number::LINK,
    status: 0,
    reason: 0,
    auth_type: 0,
    address: ADDRESS,
    len: LINK_PAYLOAD.len(),
};

/// A frame cut to `size` bytes with its header's size and complement made
/// to agree.
fn cut_to(mut frame: Vec<u8>, size: u16) -> Vec<u8> {
    frame.truncate(usize::from(size));
    frame[0..2].copy_from_slice(&size.to_le_bytes());
    frame[2..4].copy_from_slice(&(!size).to_le_bytes());
    frame
}

/// A regulatory chunk's value: the download header, the chunk, the zero
/// padding to a multiple of eight.
pub(super) fn chunk_value(blob: &[u8], at: usize) -> Vec<u8> {
    let n = (blob.len() - at).min(1400);
    let mut flag: u16 = 0x1000;
    if at == 0 {
        flag |= 0x0002;
    }
    if at + n == blob.len() {
        flag |= 0x0004;
    }
    let mut v = Vec::new();
    v.extend_from_slice(&flag.to_le_bytes());
    v.extend_from_slice(&2u16.to_le_bytes());
    v.extend_from_slice(&(n as u32).to_le_bytes());
    v.extend_from_slice(&0u32.to_le_bytes());
    v.extend_from_slice(&blob[at..at + n]);
    v.resize((12 + n + 7) & !7, 0);
    v
}

/// The transactions of the opening, in order: a set or a get, the command,
/// the name, the value; the event mask last.
pub(super) fn transactions(
    regulatory: bool,
    mask: EventMask,
) -> Vec<(bool, u32, &'static [u8], Vec<u8>)> {
    let mut t = Vec::new();
    if regulatory {
        t.push((true, 263, &b"clmload"[..], chunk_value(&BLOB, 0)));
        t.push((true, 263, &b"clmload"[..], chunk_value(&BLOB, 1400)));
    }
    t.push((true, 263, &b"bus:txglom"[..], std::vec![0, 0, 0, 0]));
    t.push((true, 86, &b""[..], std::vec![0, 0, 0, 0]));
    t.push((true, 110, &b""[..], std::vec![1, 0, 0, 0]));
    t.push((true, 263, &b"roam_off"[..], std::vec![1, 0, 0, 0]));
    t.push((
        true,
        263,
        &b"bsscfg:sup_wpa2_eapver"[..],
        std::vec![0, 0, 0, 0, 0xFF, 0xFF, 0xFF, 0xFF],
    ));
    t.push((false, 262, &b"ver"[..], std::vec![0u8; 64]));
    t.push((true, 263, &b"event_msgs"[..], mask.as_bytes().to_vec()));
    t
}

/// The version reply's payload: the string padded to 64 bytes.
pub(super) fn version_payload() -> Vec<u8> {
    let mut p = VERSION_REPLY.to_vec();
    p.resize(64, 0);
    p
}

static ACK: [u8; 4] = [0x40, 0, 0, 0];

/// A service pass delivering `frame`: the latch, the mailbox status
/// acknowledged when the bus routes it, the availability, the read.
pub(super) fn pass(v: &mut Vec<Op>, mailbox: bool, frame: &'static [u8]) {
    v.push(Op::TakeInterrupt { value: 0x0020 });
    if mailbox {
        v.push(re(0xA020, &ACK));
        v.push(we(0xA020, &ACK));
    }
    v.push(Op::F2Available {
        len: Some(frame.len()),
    });
    v.push(Op::F2Read { data: frame });
}

/// A service pass finding nothing.
pub(super) static EMPTY_PASS: [Op; 2] = [
    Op::TakeInterrupt { value: 0 },
    Op::F2Available { len: None },
];

/// The opening's rows: the setup, the credit window opened by the two boot
/// event frames, then each transaction's send and reply pass, with one
/// empty pass after the first send; the mask pushed is `mask`.
pub(super) fn open_ops_with(mailbox: bool, regulatory: bool, mask: EventMask) -> Vec<Op> {
    let mut v = std::vec![Op::F2InterruptSetup { mailbox }];
    if mailbox {
        v.push(we(0xA024, &[0xF0, 0x00, 0x00, 0x20]));
        v.push(wd(0x2034, 0x02));
    }
    pass(&mut v, mailbox, leak(event_frame(0, 0)));
    pass(&mut v, mailbox, leak(event_frame(1, 16)));
    for (i, (set, cmd, name, value)) in transactions(regulatory, mask).into_iter().enumerate() {
        let seq = i as u8;
        let id = i as u16 + 1;
        v.push(Op::F2Write {
            data: leak(request_frame(seq, id, set, cmd, name, &value)),
            accepted: true,
        });
        if i == 0 {
            v.extend_from_slice(&EMPTY_PASS);
        }
        let payload = if set { Vec::new() } else { version_payload() };
        let (reply_seq, credit) = (2 + i as u8, 17 + i as u8);
        pass(
            &mut v,
            mailbox,
            leak(reply_frame(reply_seq, credit, cmd, id, 0, &payload)),
        );
    }
    v
}

/// The opening's rows with the default mask.
pub(super) fn open_ops(mailbox: bool, regulatory: bool) -> Vec<Op> {
    open_ops_with(mailbox, regulatory, EventMask::DEFAULT)
}

/// The rows at `Ready`: the radio event's pass, the link event's pass,
/// three empty passes.
pub(super) fn ready_ops(mailbox: bool) -> Vec<Op> {
    let mut v = Vec::new();
    pass(&mut v, mailbox, leak(radio_frame()));
    pass(&mut v, mailbox, leak(link_frame()));
    for _ in 0..3 {
        v.extend_from_slice(&EMPTY_PASS);
    }
    v
}

/// The attach, the download at 64 and the opening.
pub(super) fn open_script(mailbox: bool, regulatory: bool) -> Vec<Op> {
    let mut s = full_script(64, Part::Cyw43439);
    s.extend(open_ops(mailbox, regulatory));
    s
}

/// The attach, the download at 64, the opening and the rows at `Ready`
/// with `first` delivered in the radio event's place.
fn ready_script_with_first(first: Vec<u8>) -> Vec<Op> {
    let mut s = open_script(true, true);
    pass(&mut s, true, leak(first));
    pass(&mut s, true, leak(link_frame()));
    s
}

/// A driver attached and uploaded over `script`, the clock at 411,036.
fn running_over<'s>(script: &'s [Op]) -> (Driver<'static>, FakeTransport<'s>, FakeClock) {
    let mut bus = FakeTransport::new(script, Part::Cyw43439);
    let mut clock = FakeClock::new(0);
    let mut driver = Driver::new();
    assert!(driver.attach());
    let attached = run(&mut driver, &mut bus, &mut clock, CAP);
    assert_eq!(
        attached.outcome,
        Some(Outcome::Attached {
            chip_id: 0x1545_A9AF
        })
    );
    assert!(driver.upload(&FIRMWARE, &SETTINGS));
    let uploaded = run(&mut driver, &mut bus, &mut clock, CAP);
    assert_eq!(
        uploaded.outcome,
        Some(Outcome::Uploaded { save_restore: true })
    );
    assert_eq!(clock.now(), 411_036);
    (driver, bus, clock)
}

/// What an opening left: the record, the rows left, the fake's fault, the
/// counters, the window, the state, the version string, the clock.
struct Opened {
    record: Run,
    remaining: usize,
    fault: Option<Refusal>,
    frames: Frames,
    credit: (u8, u8),
    ready: bool,
    version: Vec<u8>,
    now: Micros,
}

fn open_over(script: &[Op], regulatory: &'static [u8], version: Option<&'static [u8]>) -> Opened {
    let (mut driver, mut bus, mut clock) = running_over(script);
    assert!(driver.open(regulatory, version));
    let record = run(&mut driver, &mut bus, &mut clock, CAP);
    Opened {
        record,
        remaining: bus.remaining(),
        fault: bus.fault(),
        frames: driver.frames(),
        credit: driver.credit(),
        ready: driver.is_ready(),
        version: driver.version().to_vec(),
        now: clock.now(),
    }
}

/// A driver opened to `Ready` over `script`, the clock at 421,036.
pub(super) fn ready_over<'s>(script: &'s [Op]) -> (Driver<'static>, FakeTransport<'s>, FakeClock) {
    let (mut driver, mut bus, mut clock) = running_over(script);
    assert!(driver.open(&BLOB, Some(EXPECTED)));
    let opened = run(&mut driver, &mut bus, &mut clock, CAP);
    assert_eq!(opened.outcome, ready(VERSION_REPLY));
    assert!(driver.is_ready());
    assert_eq!(clock.now(), 421_036);
    (driver, bus, clock)
}

fn ready(reply: &[u8]) -> Option<Outcome> {
    Some(Outcome::Ready {
        version: Version::from_reply(reply),
    })
}

fn refused(stage: &'static str, status: u32) -> Option<Outcome> {
    Some(Outcome::Refused(Refusal::new(stage, status)))
}

#[test]
fn the_opening_replays_the_recorded_exchange_to_a_ready_driver() {
    let script = open_script(true, true);
    assert_eq!(script.len(), OPEN + 69);
    assert_eq!(open_ops(false, true).len(), 45);
    let o = open_over(&script, &BLOB, Some(EXPECTED));
    assert_eq!(o.fault, None);
    assert_eq!(o.record.end, End::Done);
    assert_eq!(o.record.outcome, ready(VERSION_REPLY));
    assert_eq!(o.remaining, 0, "every row consumed");
    assert!(o.ready);
    assert_eq!(o.record.deadlines(), &[421_036], "the one empty pass");
    assert_eq!(
        o.frames,
        Frames {
            received: 11,
            sent: 9,
            events: 2,
            malformed: 2,
            dropped: 0,
            aborted: 0,
            empty: 0,
            data_received: 0,
            data_sent: 0,
            data_dropped: 0
        }
    );
    assert_eq!(o.credit, (9, 25));
    assert_eq!(o.version, VERSION_STRIPPED);
    assert_eq!(o.now, 421_036);
    assert_eq!(o.record.polls, 22);
}

#[test]
fn the_driver_services_the_channel_at_ready_and_hands_each_event_back() {
    let mut script = open_script(true, true);
    script.extend(ready_ops(true));
    assert_eq!(script.len(), OPEN + 69 + 16);
    assert_eq!(ready_ops(false).len(), 12);
    let (mut driver, mut bus, mut clock) = ready_over(&script);
    let first = run(&mut driver, &mut bus, &mut clock, CAP);
    assert_eq!(first.outcome, Some(Outcome::Event(RADIO_EVENT)));
    assert_eq!(first.polls, 1);
    assert!(first.deadlines().is_empty());
    assert!(driver.event_payload().is_empty());
    let second = run(&mut driver, &mut bus, &mut clock, CAP);
    assert_eq!(second.outcome, Some(Outcome::Event(LINK_EVENT)));
    assert_eq!(second.polls, 1);
    assert_eq!(driver.event_payload(), &LINK_PAYLOAD);
    assert_eq!(LINK_EVENT.link(), Link::Unchanged);
    let third = run(&mut driver, &mut bus, &mut clock, 3);
    assert_eq!(third.end, End::Cap, "the channel is serviced, never idle");
    assert_eq!(third.outcome, None);
    assert_eq!(third.deadlines(), &[431_036, 441_036, 451_036]);
    assert!(
        driver.event_payload().is_empty(),
        "no event on the last poll"
    );
    assert_eq!(bus.remaining(), 0, "every row consumed");
    assert_eq!(bus.fault(), None);
    assert_eq!(
        driver.frames(),
        Frames {
            received: 13,
            sent: 9,
            events: 4,
            malformed: 2,
            dropped: 0,
            aborted: 0,
            empty: 0,
            data_received: 0,
            data_sent: 0,
            data_dropped: 0
        }
    );
    assert_eq!(driver.credit(), (9, 27));
    assert!(driver.is_ready());
    assert!(
        !driver.subscribe(EventMask::EMPTY),
        "the pushed mask is the one in force"
    );
}

#[test]
fn an_event_frame_with_the_wrong_ethertype_is_dropped_and_the_next_taken_at_once() {
    let mut wrong = radio_frame();
    wrong[29] = 0x6D;
    let script = ready_script_with_first(wrong);
    let (mut driver, mut bus, mut clock) = ready_over(&script);
    let record = run(&mut driver, &mut bus, &mut clock, CAP);
    assert_eq!(record.outcome, Some(Outcome::Event(LINK_EVENT)));
    assert_eq!(record.polls, 2, "the dropped frame's pass, then the next");
    assert_eq!((driver.frames().events, driver.frames().malformed), (4, 3));
    assert_eq!(bus.remaining(), 0);
    assert_eq!(bus.fault(), None);
}

#[test]
fn an_event_frame_with_the_wrong_oui_is_dropped() {
    let mut wrong = radio_frame();
    wrong[37] = 0x19;
    let script = ready_script_with_first(wrong);
    let (mut driver, mut bus, mut clock) = ready_over(&script);
    let record = run(&mut driver, &mut bus, &mut clock, CAP);
    assert_eq!(record.outcome, Some(Outcome::Event(LINK_EVENT)));
    assert_eq!(record.polls, 2);
    assert_eq!((driver.frames().events, driver.frames().malformed), (4, 3));
}

#[test]
fn an_empty_event_is_ignored_and_its_credit_taken() {
    let mut empty = header(16, 11, 1, 16, 26);
    empty.resize(16, 0);
    let script = ready_script_with_first(empty);
    let (mut driver, mut bus, mut clock) = ready_over(&script);
    let record = run(&mut driver, &mut bus, &mut clock, CAP);
    assert_eq!(record.outcome, Some(Outcome::Event(LINK_EVENT)));
    assert_eq!(record.polls, 2);
    assert_eq!((driver.frames().events, driver.frames().malformed), (4, 2));
    assert_eq!(driver.credit(), (9, 27));
}

#[test]
fn an_event_frame_cut_inside_its_header_is_dropped() {
    let script = ready_script_with_first(cut_to(radio_frame(), 80));
    let (mut driver, mut bus, mut clock) = ready_over(&script);
    let record = run(&mut driver, &mut bus, &mut clock, CAP);
    assert_eq!(record.outcome, Some(Outcome::Event(LINK_EVENT)));
    assert_eq!(record.polls, 2);
    assert_eq!(
        (
            driver.frames().events,
            driver.frames().malformed,
            driver.frames().aborted
        ),
        (4, 3, 0)
    );
}

#[test]
fn an_event_number_past_the_count_is_dropped() {
    let mut wrong = radio_frame();
    wrong[44..48].copy_from_slice(&[0, 0, 0, 200]);
    let script = ready_script_with_first(wrong);
    let (mut driver, mut bus, mut clock) = ready_over(&script);
    let record = run(&mut driver, &mut bus, &mut clock, CAP);
    assert_eq!(record.outcome, Some(Outcome::Event(LINK_EVENT)));
    assert_eq!(record.polls, 2);
    assert_eq!((driver.frames().events, driver.frames().malformed), (4, 3));
}

#[test]
fn the_mask_refused_by_the_firmware_is_refused_by_name() {
    let mut script = open_script(true, true);
    script[OPEN + 68] = Op::F2Read {
        data: leak(reply_frame(10, 25, 263, 9, 0xFFFF_FFFF, &[])),
    };
    let o = open_over(&script, &BLOB, Some(EXPECTED));
    assert_eq!(o.fault, None);
    assert_eq!(o.record.outcome, refused(STAGE_EVENT_MASK, 0xFFFF_FFFF));
    assert_eq!(o.remaining, 0);
    assert!(!o.ready);
    assert_eq!(
        o.version, VERSION_STRIPPED,
        "the gate passed before the mask"
    );
    assert_eq!(o.frames.sent, 9);
}

#[test]
fn an_event_during_the_openings_wait_is_handed_back_and_the_opening_continues() {
    let mut script = open_script(true, true);
    let mut early = Vec::new();
    pass(
        &mut early,
        true,
        leak(event_of(9, 17, 0, number::RADIO, 0, 0, 0, ADDRESS, &[])),
    );
    script.splice(OPEN + 16..OPEN + 16, early);
    let (mut driver, mut bus, mut clock) = running_over(&script);
    assert!(driver.open(&BLOB, Some(EXPECTED)));
    let first = run(&mut driver, &mut bus, &mut clock, CAP);
    assert_eq!(first.outcome, Some(Outcome::Event(RADIO_EVENT)));
    assert!(!driver.is_ready(), "the request is still outstanding");
    assert!(!driver.open(&BLOB, Some(EXPECTED)), "still opening");
    let second = run(&mut driver, &mut bus, &mut clock, CAP);
    assert_eq!(second.outcome, ready(VERSION_REPLY));
    assert!(driver.is_ready());
    assert_eq!(bus.remaining(), 0);
    assert_eq!(bus.fault(), None);
    assert_eq!(
        (
            driver.frames().events,
            driver.frames().malformed,
            driver.frames().received
        ),
        (3, 2, 12)
    );
    assert_eq!(driver.credit(), (9, 25));
}

#[test]
fn a_payload_shorter_than_its_count_is_delivered_as_the_bytes_present() {
    let mut short = link_frame();
    short[64..68].copy_from_slice(&[0, 0, 0, 20]);
    let mut script = open_script(true, true);
    pass(&mut script, true, leak(short));
    let (mut driver, mut bus, mut clock) = ready_over(&script);
    let record = run(&mut driver, &mut bus, &mut clock, CAP);
    assert_eq!(record.outcome, Some(Outcome::Event(LINK_EVENT)));
    assert_eq!(driver.event_payload(), &LINK_PAYLOAD);
    assert_eq!(bus.remaining(), 0);
}

#[test]
fn subscribe_is_a_setting_before_the_opening_and_its_numbers_reach_the_wire() {
    let mut mask = EventMask::DEFAULT;
    assert!(mask.add(number::SCAN_COMPLETE));
    let mut script = full_script(64, Part::Cyw43439);
    script.extend(open_ops_with(true, true, mask));
    let Op::F2Write { data, .. } = script[OPEN + 63] else {
        panic!("the mask's send");
    };
    assert_eq!(data.len(), 56);
    assert_eq!(&data[39..56], mask.as_bytes());
    assert_eq!(data[42], 0x04, "byte 3 of the mask carries event 26");
    let mut driver = Driver::new();
    assert_eq!(driver.event_mask(), EventMask::DEFAULT);
    assert!(driver.subscribe(mask), "on a fresh driver");
    let mut bus = FakeTransport::new(&script, Part::Cyw43439);
    let mut clock = FakeClock::new(0);
    assert!(driver.attach());
    let attached = run(&mut driver, &mut bus, &mut clock, CAP);
    assert!(matches!(attached.outcome, Some(Outcome::Attached { .. })));
    assert!(driver.subscribe(mask), "while attached");
    assert!(driver.upload(&FIRMWARE, &SETTINGS));
    let uploaded = run(&mut driver, &mut bus, &mut clock, CAP);
    assert!(matches!(uploaded.outcome, Some(Outcome::Uploaded { .. })));
    assert!(driver.subscribe(mask), "while running");
    assert!(driver.open(&BLOB, Some(EXPECTED)));
    assert!(!driver.subscribe(mask), "not while opening");
    let opened = run(&mut driver, &mut bus, &mut clock, CAP);
    assert_eq!(opened.outcome, ready(VERSION_REPLY));
    assert!(!driver.subscribe(mask), "not while ready");
    assert_eq!(driver.event_mask(), mask);
    assert_eq!(bus.remaining(), 0, "the amended mask is what went out");
    assert_eq!(bus.fault(), None);
}

#[test]
fn a_control_frame_with_no_request_outstanding_is_dropped_at_ready() {
    let mut script = open_script(true, true);
    pass(&mut script, true, leak(reply_frame(13, 26, 263, 1, 0, &[])));
    script.extend(ready_ops(true));
    let (mut driver, mut bus, mut clock) = ready_over(&script);
    let record = run(&mut driver, &mut bus, &mut clock, CAP);
    assert_eq!(record.outcome, Some(Outcome::Event(RADIO_EVENT)));
    assert_eq!(record.polls, 2, "the stranger's pass, then the event's");
    assert_eq!((driver.frames().dropped, driver.frames().received), (1, 13));
    assert_eq!(bus.fault(), None);
}

#[test]
fn a_reply_with_the_wrong_id_is_dropped_and_the_right_one_taken() {
    let mut script = open_script(true, true);
    let mut stranger = Vec::new();
    pass(
        &mut stranger,
        true,
        leak(reply_frame(9, 17, 263, 2, 0, &[])),
    );
    script.splice(OPEN + 16..OPEN + 16, stranger);
    let o = open_over(&script, &BLOB, Some(EXPECTED));
    assert_eq!(o.fault, None);
    assert_eq!(o.record.outcome, ready(VERSION_REPLY));
    assert_eq!(o.remaining, 0);
    assert_eq!((o.frames.dropped, o.frames.received), (1, 12));
}

#[test]
fn a_status_that_refuses_is_refused_by_the_transactions_name() {
    let mut script = open_script(true, true);
    script[OPEN + 20] = Op::F2Read {
        data: leak(reply_frame(2, 17, 263, 1, 0xFFFF_FFFC, &[])),
    };
    let o = open_over(&script, &BLOB, Some(EXPECTED));
    assert_eq!(o.fault, None);
    assert_eq!(o.record.outcome, refused(STAGE_REGULATORY, 0xFFFF_FFFC));
    assert_eq!(o.remaining, 48, "the rows after the reply left");
    assert!(!o.ready);
}

#[test]
fn a_complement_that_disagrees_is_aborted_and_survived() {
    let mut script = open_script(true, true);
    let mut bad = event_frame(1, 16);
    bad[2] = 0xFE;
    bad[3] = 0xFE;
    script[OPEN + 12] = Op::F2Read { data: leak(bad) };
    let mut after = std::vec![Op::AbortF2, wd(0x1000D, 0x01)];
    pass(&mut after, true, leak(event_frame(2, 16)));
    script.splice(OPEN + 13..OPEN + 13, after);
    let o = open_over(&script, &BLOB, Some(EXPECTED));
    assert_eq!(o.fault, None);
    assert_eq!(o.record.outcome, ready(VERSION_REPLY));
    assert_eq!(o.remaining, 0);
    assert_eq!(
        (o.frames.aborted, o.frames.events, o.frames.received),
        (1, 2, 12)
    );
    assert_eq!(
        o.frames.malformed, 2,
        "the doctored frame never reached the walk"
    );
    assert_eq!(
        o.record.deadlines(),
        &[421_036, 431_036],
        "the wait after the abort, then the empty pass"
    );
}

#[test]
fn a_data_offset_outside_the_frame_is_aborted_and_the_reply_retaken() {
    let mut script = open_script(true, true);
    let mut bad = reply_frame(2, 17, 263, 1, 0, &[]);
    bad[7] = 29;
    script[OPEN + 20] = Op::F2Read { data: leak(bad) };
    let mut after = std::vec![Op::AbortF2, wd(0x1000D, 0x01)];
    pass(&mut after, true, leak(reply_frame(3, 17, 263, 1, 0, &[])));
    script.splice(OPEN + 21..OPEN + 21, after);
    let o = open_over(&script, &BLOB, Some(EXPECTED));
    assert_eq!(o.fault, None);
    assert_eq!(o.record.outcome, ready(VERSION_REPLY));
    assert_eq!(o.remaining, 0);
    assert_eq!((o.frames.aborted, o.frames.received), (1, 12));
}

#[test]
fn a_credit_that_never_comes_is_refused_at_the_deadline() {
    let mut script = open_script(true, true);
    script.splice(
        OPEN + 3..OPEN + 13,
        [Op::Repeat {
            ops: &EMPTY_PASS,
            times: 300,
        }],
    );
    let o = open_over(&script, &BLOB, Some(EXPECTED));
    assert_eq!(o.fault, None);
    assert_eq!(o.record.outcome, refused(STAGE_CREDIT, 0));
    let deadlines = o.record.deadlines();
    assert_eq!(
        deadlines.len(),
        300,
        "three hundred passes ten milliseconds apart"
    );
    assert_eq!(deadlines[0], 421_036);
    assert_eq!(deadlines[299], 3_411_036);
    assert_eq!(o.now, 3_411_036);
    assert_eq!(o.remaining, 56, "the rows from the first send on");
    assert_eq!(o.frames.sent, 0);
}

#[test]
fn a_reply_that_never_comes_is_refused_at_the_deadline() {
    let mut script = open_script(true, true);
    script.splice(
        OPEN + 14..OPEN + 21,
        [Op::Repeat {
            ops: &EMPTY_PASS,
            times: 1000,
        }],
    );
    let o = open_over(&script, &BLOB, Some(EXPECTED));
    assert_eq!(o.fault, None);
    assert_eq!(o.record.outcome, refused(STAGE_REPLY_TIMEOUT, 0x0001_0107));
    let deadlines = o.record.deadlines();
    assert_eq!(
        deadlines.len(),
        1000,
        "one thousand passes ten milliseconds apart"
    );
    assert_eq!(deadlines[0], 421_036);
    assert_eq!(deadlines[999], 10_411_036);
    assert_eq!(o.remaining, 48);
    assert_eq!(o.frames.sent, 1);
}

#[test]
fn a_chunk_refused_stops_the_download() {
    let mut script = open_script(true, true);
    script[OPEN + 26] = Op::F2Read {
        data: leak(reply_frame(3, 18, 263, 2, 0xFFFF_FFFF, &[])),
    };
    let o = open_over(&script, &BLOB, Some(EXPECTED));
    assert_eq!(o.record.outcome, refused(STAGE_REGULATORY, 0xFFFF_FFFF));
    assert_eq!(o.remaining, 42, "the settings never sent");
    assert_eq!(o.frames.sent, 2);
}

#[test]
fn an_empty_blob_skips_the_download() {
    let script = open_script(true, false);
    assert_eq!(script.len(), OPEN + 57);
    let o = open_over(&script, &BLOB[..0], Some(EXPECTED));
    assert_eq!(o.fault, None);
    assert_eq!(o.record.outcome, ready(VERSION_REPLY));
    assert_eq!(o.remaining, 0);
    assert_eq!((o.frames.sent, o.frames.received), (7, 9));
    assert_eq!(o.credit, (7, 23));
}

#[test]
fn a_version_without_the_expected_string_is_refused_and_none_declines_the_check() {
    let script = open_script(true, true);
    let o = open_over(&script, &BLOB, Some(b"9.99.99"));
    assert_eq!(o.fault, None);
    assert_eq!(o.record.outcome, refused(STAGE_VERSION_MISMATCH, 54));
    assert_eq!(
        o.version, VERSION_STRIPPED,
        "the string readable after the refusal"
    );
    assert_eq!(o.remaining, 6, "the mask never pushed");
    assert_eq!(o.frames.sent, 8);
    assert!(!o.ready);
    let o = open_over(&script, &BLOB, None);
    assert_eq!(o.record.outcome, ready(VERSION_REPLY));
    assert_eq!(o.remaining, 0);
    assert!(o.ready);
}

#[test]
fn a_data_frame_during_the_opening_is_dropped_and_counted_and_its_credit_taken() {
    let mut script = open_script(true, true);
    let mut data = event_frame(1, 16);
    data[5] = 2;
    script[OPEN + 12] = Op::F2Read { data: leak(data) };
    let o = open_over(&script, &BLOB, Some(EXPECTED));
    assert_eq!(o.fault, None);
    assert_eq!(o.record.outcome, ready(VERSION_REPLY));
    assert_eq!(
        o.remaining, 0,
        "the dropped frame's valid header opened the window"
    );
    assert_eq!(
        (
            o.frames.data_received,
            o.frames.data_dropped,
            o.frames.events,
            o.frames.malformed,
            o.frames.received
        ),
        (1, 1, 1, 1, 11)
    );
}

#[test]
fn open_is_refused_where_the_state_forbids_it_and_an_empty_expected_string_by_name() {
    let script = open_script(true, true);
    let mut fresh = Driver::new();
    assert!(!fresh.open(&BLOB, Some(EXPECTED)), "not before the attach");
    let mut bus = FakeTransport::new(&script, Part::Cyw43439);
    let mut clock = FakeClock::new(0);
    let mut driver = Driver::new();
    assert!(driver.attach());
    let attached = run(&mut driver, &mut bus, &mut clock, CAP);
    assert!(matches!(attached.outcome, Some(Outcome::Attached { .. })));
    assert!(!driver.open(&BLOB, Some(EXPECTED)), "not while attached");
    assert!(driver.upload(&FIRMWARE, &SETTINGS));
    assert!(!driver.open(&BLOB, Some(EXPECTED)), "not while downloading");
    let uploaded = run(&mut driver, &mut bus, &mut clock, CAP);
    assert!(matches!(uploaded.outcome, Some(Outcome::Uploaded { .. })));
    assert!(driver.open(&BLOB, Some(EXPECTED)));
    assert!(!driver.open(&BLOB, Some(EXPECTED)), "not while opening");
    let opened = run(&mut driver, &mut bus, &mut clock, CAP);
    assert_eq!(opened.outcome, ready(VERSION_REPLY));
    assert!(!driver.open(&BLOB, Some(EXPECTED)), "not while ready");
    assert!(
        !driver.upload(&FIRMWARE, &SETTINGS),
        "no upload while ready"
    );
    assert!(!driver.attach(), "attach is refused while ready");

    let (mut driver, mut bus, mut clock) = running_over(&script);
    assert!(
        driver.open(&BLOB, Some(&BLOB[..0])),
        "the request is recorded"
    );
    let record = run(&mut driver, &mut bus, &mut clock, CAP);
    assert_eq!(record.outcome, refused(STAGE_EXPECTED_VERSION, 0));
    assert_eq!(record.polls, 1, "refused at the first poll");
    assert_eq!(bus.remaining(), 69, "nothing reached the bus");
    assert_eq!(bus.fault(), None);
    assert!(driver.is_parked());
}

#[test]
fn a_write_not_accepted_is_retried_with_the_sequence_and_the_id_spent_once() {
    let mut script = open_script(true, true);
    let Op::F2Write { data, .. } = script[OPEN + 13] else {
        panic!("the first send");
    };
    script.insert(
        OPEN + 13,
        Op::F2Write {
            data,
            accepted: false,
        },
    );
    let o = open_over(&script, &BLOB, Some(EXPECTED));
    assert_eq!(o.fault, None);
    assert_eq!(o.record.outcome, ready(VERSION_REPLY));
    assert_eq!(
        o.record.deadlines(),
        &[421_036, 431_036],
        "the retry, then the empty pass"
    );
    assert_eq!(o.frames.sent, 9);
    assert_eq!(o.credit, (9, 25));
}

#[test]
fn a_zero_length_availability_is_counted_and_passed_over() {
    let mut script = open_script(true, true);
    script.splice(
        OPEN + 14..OPEN + 14,
        [
            Op::TakeInterrupt { value: 0 },
            Op::F2Available { len: Some(0) },
        ],
    );
    let o = open_over(&script, &BLOB, Some(EXPECTED));
    assert_eq!(o.fault, None);
    assert_eq!(o.record.outcome, ready(VERSION_REPLY));
    assert_eq!(o.frames.empty, 1);
    assert_eq!(o.record.deadlines(), &[421_036, 431_036]);
}

#[test]
fn the_credit_only_frame_opens_the_window() {
    static CREDIT_ONLY: [u8; 12] = [
        0x0C, 0x00, 0xF3, 0xFF, 0x01, 0x00, 0x00, 0x0C, 0x00, 0x10, 0x00, 0x00,
    ];
    let mut script = open_script(true, true);
    script[OPEN + 11] = Op::F2Available { len: Some(12) };
    script[OPEN + 12] = Op::F2Read { data: &CREDIT_ONLY };
    let o = open_over(&script, &BLOB, Some(EXPECTED));
    assert_eq!(o.fault, None);
    assert_eq!(o.record.outcome, ready(VERSION_REPLY));
    assert_eq!(
        (o.frames.events, o.frames.malformed, o.frames.received),
        (1, 1, 11)
    );
}

#[test]
fn an_unknown_channel_is_dropped_and_counted_and_its_credit_taken() {
    let mut script = open_script(true, true);
    let mut odd = event_frame(1, 16);
    odd[5] = 3;
    script[OPEN + 12] = Op::F2Read { data: leak(odd) };
    let o = open_over(&script, &BLOB, Some(EXPECTED));
    assert_eq!(o.fault, None);
    assert_eq!(o.record.outcome, ready(VERSION_REPLY));
    assert_eq!(
        o.remaining, 0,
        "the dropped frame's valid header opened the window"
    );
    assert_eq!(
        (
            o.frames.dropped,
            o.frames.events,
            o.frames.malformed,
            o.frames.received
        ),
        (1, 1, 1, 11)
    );
}

#[test]
fn malformed_replies_are_dropped() {
    let mut script = open_script(true, true);
    let mut short = header(24, 9, 0, 12, 17);
    short.resize(24, 0);
    let mut long = reply_frame(10, 17, 263, 1, 0, &[]);
    long[16] = 8;
    let mut before = Vec::new();
    pass(&mut before, true, leak(short));
    pass(&mut before, true, leak(long));
    script.splice(OPEN + 16..OPEN + 16, before);
    let o = open_over(&script, &BLOB, Some(EXPECTED));
    assert_eq!(o.fault, None);
    assert_eq!(o.record.outcome, ready(VERSION_REPLY));
    assert_eq!((o.frames.dropped, o.frames.received), (2, 13));
}

#[test]
fn a_control_frame_with_no_request_outstanding_is_dropped() {
    let mut script = open_script(true, true);
    let mut stranger = Vec::new();
    pass(&mut stranger, true, leak(reply_frame(5, 0, 263, 1, 0, &[])));
    script.splice(OPEN + 8..OPEN + 8, stranger);
    let o = open_over(&script, &BLOB, Some(EXPECTED));
    assert_eq!(o.fault, None);
    assert_eq!(o.record.outcome, ready(VERSION_REPLY));
    assert_eq!((o.frames.dropped, o.frames.received), (1, 12));
}
