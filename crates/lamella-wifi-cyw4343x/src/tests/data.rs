//! The data plane at the transport level: the ARP request's frame written
//! under the window, the ARP reply and a datagram received in place, a
//! burst of sends closing the window, a frame held across the loss burst
//! and the recovery cycle and written after the rejoin, frames dropped
//! outside the joined state -- as one recorded exchange after the radio up
//! and the join, with its controls; and the data frame's pure parts.

use super::control::{ADDRESS, EMPTY_PASS, header, open_script, pass, ready_over, request_frame};
use super::download_tie::leak;
use super::link::{PASSPHRASE, SSID, secured_values};
use super::scan::record_a;
use super::station::{
    Cursor, IES, STATION, burst_ops, empty, ev, event, exchange, join_ops, join_requests, reply,
    scan_event, scan_requests, send, up_ops,
};
use crate::data::{
    BDC_HEADER_LEN, BdcHeader, ETHERNET_HEADER_LEN, ETHERNET_MAX, Fault, HEAD, TRANSMIT_BUF,
    TRANSMIT_OFFSET, bdc, fits, head, parse,
};
use crate::driver::{Driver, Outcome};
use crate::event::{number, status};
use crate::fixture::{End, FakeClock, FakeTransport, Op, run};
use crate::frame::Frames;
use crate::link::LinkState;
use crate::scan::ScanEnd;
use crate::transport::Transport;
use std::vec::Vec;

const CAP: u32 = 100_000;

/// The station's address on its network.
pub(super) const OUR_IP: [u8; 4] = [192, 168, 4, 2];
/// The access point's address on the network.
pub(super) const AP_IP: [u8; 4] = [192, 168, 4, 1];
const BROADCAST: [u8; 6] = [0xFF; 6];
/// The sends that close the window after the join: the window (20, 42)
/// once the two frames were received.
pub(super) const BURST: usize = 22;

/// An ARP frame over Ethernet: the destination, the source, the operation,
/// the sender's and the target's addresses.
#[allow(clippy::too_many_arguments)]
pub(super) fn arp(
    dst: [u8; 6],
    src: [u8; 6],
    op: u16,
    sha: [u8; 6],
    spa: [u8; 4],
    tha: [u8; 6],
    tpa: [u8; 4],
) -> Vec<u8> {
    let mut f = Vec::new();
    f.extend_from_slice(&dst);
    f.extend_from_slice(&src);
    f.extend_from_slice(&[0x08, 0x06]);
    f.extend_from_slice(&[0x00, 0x01, 0x08, 0x00, 0x06, 0x04]);
    f.extend_from_slice(&op.to_be_bytes());
    f.extend_from_slice(&sha);
    f.extend_from_slice(&spa);
    f.extend_from_slice(&tha);
    f.extend_from_slice(&tpa);
    f
}

/// The station's ARP request for the access point's address, 42 bytes.
pub(super) fn arp_request() -> Vec<u8> {
    arp(BROADCAST, STATION, 1, STATION, OUR_IP, [0; 6], AP_IP)
}

/// The access point's reply to it, 42 bytes.
pub(super) fn arp_reply() -> Vec<u8> {
    arp(STATION, ADDRESS, 2, ADDRESS, AP_IP, STATION, OUR_IP)
}

/// The access point's request for the station's address.
#[cfg(feature = "smoltcp")]
pub(super) fn arp_request_from_ap() -> Vec<u8> {
    arp(BROADCAST, ADDRESS, 1, ADDRESS, AP_IP, [0; 6], OUR_IP)
}

/// The station's reply to it, as its stack builds it.
#[cfg(feature = "smoltcp")]
pub(super) fn arp_reply_from_us() -> Vec<u8> {
    arp(ADDRESS, STATION, 2, STATION, OUR_IP, ADDRESS, AP_IP)
}

/// A synthetic IPv4 datagram to the station, 64 bytes, its checksums
/// unset: the driver reads nothing past the Ethernet frame's start.
pub(super) fn datagram() -> Vec<u8> {
    let mut f = Vec::new();
    f.extend_from_slice(&STATION);
    f.extend_from_slice(&ADDRESS);
    f.extend_from_slice(&[0x08, 0x00]);
    f.extend_from_slice(&[
        0x45, 0x00, 0x00, 0x32, 0x00, 0x01, 0x00, 0x00, 0x40, 0x11, 0x00, 0x00,
    ]);
    f.extend_from_slice(&AP_IP);
    f.extend_from_slice(&OUR_IP);
    f.extend_from_slice(&[0x13, 0x88, 0x13, 0x88, 0x00, 0x1E, 0x00, 0x00]);
    f.extend_from_slice(b"synthetic-datagram-001");
    assert_eq!(f.len(), 64);
    f
}

/// A data frame as the driver writes it: the header with the offset 14 and
/// the sequence, the two alignment bytes, the BDC header, the Ethernet
/// frame.
pub(super) fn tx_frame(seq: u8, eth: &[u8]) -> Vec<u8> {
    let mut f = header((18 + eth.len()) as u16, seq, 2, 14, 0);
    f.extend_from_slice(&[0, 0, 0x20, 0, 0, 0]);
    f.extend_from_slice(eth);
    f
}

/// A data frame as the chip delivers it: the header with the chip's
/// payload offset and its credit, the BDC header at that offset with
/// `words` of padding, the Ethernet frame.
pub(super) fn rx_frame(seq: u8, credit: u8, doff: u8, words: u8, eth: &[u8]) -> Vec<u8> {
    let size = usize::from(doff) + 4 + 4 * usize::from(words) + eth.len();
    let mut f = header(size as u16, seq, 2, doff, credit);
    f.resize(usize::from(doff), 0);
    f.extend_from_slice(&[0x20, 0, 0, words]);
    f.resize(f.len() + 4 * usize::from(words), 0);
    f.extend_from_slice(eth);
    assert_eq!(f.len(), size);
    f
}

/// A credit-only frame: the twelve-byte header alone.
pub(super) fn credit_only(seq: u8, credit: u8) -> Vec<u8> {
    header(12, seq, 0, 12, credit)
}

/// A data frame sent: the frame the driver must write with the cursor's
/// sequence.
pub(super) fn data_send(v: &mut Vec<Op>, c: &mut Cursor, eth: &[u8]) {
    v.push(Op::F2Write {
        data: leak(tx_frame(c.seq, eth)),
        accepted: true,
    });
    c.seq = c.seq.wrapping_add(1);
}

/// A data frame received in a service pass, with the chip's offset and
/// padding.
pub(super) fn data_recv(
    v: &mut Vec<Op>,
    c: &mut Cursor,
    mailbox: bool,
    doff: u8,
    words: u8,
    eth: &[u8],
) {
    pass(
        v,
        mailbox,
        leak(rx_frame(c.chip, c.credit, doff, words, eth)),
    );
    c.chip = c.chip.wrapping_add(1);
    c.credit = c.credit.wrapping_add(1);
}

fn repeat(v: &mut Vec<Op>, n: u32) {
    v.push(Op::Repeat {
        ops: &EMPTY_PASS,
        times: n,
    });
}

/// The data plane's rows after the radio up and the join: the ARP request
/// sent, the reply and a datagram received, twenty-two sends closing the
/// window, the twenty-third held, the loss burst with a frame dropped
/// inside it, the settle and the recovery cycle, the held frame written
/// after the rejoin, a frame received, the disconnect, a frame dropped
/// after it.
pub(super) fn data_ops(c: &mut Cursor, mailbox: bool, explicit: bool) -> Vec<Op> {
    let mut v = Vec::new();
    data_send(&mut v, c, &arp_request());
    data_recv(&mut v, c, mailbox, 14, 0, &arp_reply());
    data_recv(&mut v, c, mailbox, 24, 1, &datagram());
    for _ in 0..BURST {
        data_send(&mut v, c, &arp_request());
    }
    event(
        &mut v,
        c,
        mailbox,
        number::PSK_SUP,
        status::UNSOLICITED,
        14,
        ADDRESS,
        &[],
    );
    data_recv(&mut v, c, mailbox, 14, 0, &arp_reply());
    event(&mut v, c, mailbox, number::DISASSOC_IND, 0, 2, ADDRESS, &[]);
    event(&mut v, c, mailbox, number::DEAUTH, 0, 6, ADDRESS, &[]);
    event(&mut v, c, mailbox, number::DEAUTH, 0, 7, ADDRESS, &[]);
    event(&mut v, c, mailbox, number::LINK, 0, 1, ADDRESS, &[]);
    if explicit {
        empty(&mut v, 100);
    } else {
        repeat(&mut v, 100);
    }
    scan_requests(&mut v, c, mailbox, 1);
    scan_event(&mut v, c, mailbox, 1, status::PARTIAL, &[record_a()]);
    scan_event(&mut v, c, mailbox, 1, status::SUCCESS, &[]);
    join_requests(&mut v, c, mailbox, &secured_values()[5..]);
    event(&mut v, c, mailbox, number::AUTH, 0, 0, ADDRESS, &[]);
    event(&mut v, c, mailbox, number::ASSOC, 0, 0, ADDRESS, &IES);
    event(&mut v, c, mailbox, number::SET_SSID, 0, 0, ADDRESS, &[]);
    event(
        &mut v,
        c,
        mailbox,
        number::PSK_SUP,
        status::UNSOLICITED,
        0,
        ADDRESS,
        &[],
    );
    empty(&mut v, 5);
    data_send(&mut v, c, &arp_request());
    data_recv(&mut v, c, mailbox, 14, 0, &arp_reply());
    exchange(&mut v, c, mailbox, true, 52, b"", &[0; 12], 0, &[]);
    data_recv(&mut v, c, mailbox, 14, 0, &arp_reply());
    v
}

/// The opening, the radio up and the join: the script and the cursor after
/// them.
pub(super) fn joined_script() -> (Vec<Op>, Cursor) {
    let mut s = open_script(true, true);
    let mut c = Cursor::AFTER_OPENING;
    s.extend(up_ops(&mut c, true));
    s.extend(join_ops(&mut c, true));
    (s, c)
}

/// The radio up and the secured join on a driver at `Ready`, over any
/// transport: the link up at the end.
pub(super) fn join_over<T: Transport>(
    driver: &mut Driver<'static>,
    bus: &mut T,
    clock: &mut FakeClock,
) {
    assert!(driver.up());
    assert_eq!(
        run(driver, bus, clock, CAP).outcome,
        Some(Outcome::Up { address: STATION })
    );
    assert!(driver.join(SSID, Some(PASSPHRASE)));
    for _ in 0..5 {
        assert!(matches!(
            run(driver, bus, clock, CAP).outcome,
            Some(Outcome::Event(_))
        ));
    }
    assert_eq!(
        run(driver, bus, clock, CAP).outcome,
        Some(Outcome::Joined { bssid: ADDRESS })
    );
    assert_eq!(driver.link(), LinkState::Up);
}

/// A driver joined over `script`: the clock at 481,036 (the hold's 50 ms
/// after the handshake's word), the window (19, 40) -- the credit the
/// handshake's event frame carried, repeated by the association word's.
pub(super) fn joined_over(script: &[Op]) -> (Driver<'static>, FakeTransport<'_>, FakeClock) {
    let (mut driver, mut bus, mut clock) = ready_over(script);
    join_over(&mut driver, &mut bus, &mut clock);
    assert_eq!(clock.now(), 481_036);
    assert_eq!(driver.credit(), (19, 40));
    (driver, bus, clock)
}

/// The data plane's outcomes in order over a joined driver: every outcome,
/// poll count and deadline asserted.
pub(super) fn drive_data_plane<T: Transport>(
    driver: &mut Driver<'static>,
    bus: &mut T,
    clock: &mut FakeClock,
) {
    let step = 10_000;
    assert_eq!(driver.send(bus, &arp_request()), Ok(true));
    assert!(!driver.sending(), "written at once");
    assert_eq!(driver.credit(), (20, 40));
    assert_eq!(driver.frames().data_sent, 1);

    let r = run(driver, bus, clock, CAP);
    assert_eq!(r.outcome, Some(Outcome::Frame { len: 42 }));
    assert_eq!(r.polls, 1);
    assert_eq!(driver.frame_payload(), &arp_reply()[..]);
    assert_eq!(driver.credit(), (20, 41));
    let r = run(driver, bus, clock, CAP);
    assert_eq!(r.outcome, Some(Outcome::Frame { len: 64 }));
    assert_eq!(r.polls, 1);
    assert_eq!(driver.frame_payload(), &datagram()[..]);
    assert_eq!(driver.credit(), (20, 42));
    assert_eq!(driver.frames().data_received, 2);

    for i in 0..BURST {
        assert_eq!(driver.send(bus, &arp_request()), Ok(true), "send {i}");
        assert!(!driver.sending(), "send {i} written");
    }
    assert_eq!(driver.credit(), (42, 42), "the window closed");
    assert_eq!(driver.frames().data_sent, 23);
    assert_eq!(driver.send(bus, &arp_request()), Ok(true), "taken and held");
    assert!(driver.sending());
    assert_eq!(driver.credit(), (42, 42), "no sequence spent");

    let r = run(driver, bus, clock, CAP);
    assert_eq!(
        r.outcome,
        Some(Outcome::LinkLost(ev(number::PSK_SUP, 6, 14, ADDRESS, 0)))
    );
    assert_eq!(r.polls, 1);
    assert_eq!(driver.link(), LinkState::Down);
    assert!(driver.sending(), "held: the link down");
    assert_eq!(
        driver.credit(),
        (42, 43),
        "the window open again, the frame not written"
    );
    let r = run(driver, bus, clock, CAP);
    assert_eq!(
        r.outcome,
        Some(Outcome::Event(ev(number::DISASSOC_IND, 0, 2, ADDRESS, 0)))
    );
    assert_eq!(r.polls, 2, "a frame dropped while down, then the event");
    assert_eq!(driver.frames().data_dropped, 1);
    assert!(
        driver.frame_payload().is_empty(),
        "the dropped frame forgotten"
    );
    for (n, reason) in [(number::DEAUTH, 6), (number::DEAUTH, 7), (number::LINK, 1)] {
        let r = run(driver, bus, clock, CAP);
        assert_eq!(
            r.outcome,
            Some(Outcome::Event(ev(n, 0, reason, ADDRESS, 0))),
            "the burst's event {n}"
        );
        assert_eq!(r.polls, 1);
    }
    let lost_at = clock.now();

    let r = run(driver, bus, clock, CAP);
    assert!(matches!(r.outcome, Some(Outcome::ScanRecord(_))));
    assert_eq!(r.polls, 106);
    let settle: Vec<u64> = (1..=100).map(|i| lost_at + i * step).collect();
    assert_eq!(r.deadlines(), &settle[..]);
    let r = run(driver, bus, clock, CAP);
    assert_eq!(
        r.outcome,
        Some(Outcome::ScanDone {
            end: ScanEnd::Complete,
            records: 1
        })
    );
    assert_eq!(driver.link(), LinkState::Joining);
    let r = run(driver, bus, clock, CAP);
    assert_eq!(
        r.outcome,
        Some(Outcome::Event(ev(number::AUTH, 0, 0, ADDRESS, 0)))
    );
    assert_eq!(r.polls, 7);
    assert_eq!(
        run(driver, bus, clock, CAP).outcome,
        Some(Outcome::Event(ev(number::ASSOC, 0, 0, ADDRESS, 8)))
    );
    assert_eq!(
        run(driver, bus, clock, CAP).outcome,
        Some(Outcome::Event(ev(number::SET_SSID, 0, 0, ADDRESS, 0)))
    );
    let r = run(driver, bus, clock, CAP);
    assert_eq!(
        r.outcome,
        Some(Outcome::Event(ev(number::PSK_SUP, 6, 0, ADDRESS, 0))),
        "the handshake's word starts the hold"
    );
    assert_eq!(r.polls, 1);
    assert_eq!(driver.link(), LinkState::Joining);
    let r = run(driver, bus, clock, CAP);
    assert_eq!(r.outcome, Some(Outcome::Joined { bssid: ADDRESS }));
    assert_eq!(
        r.polls, 6,
        "five empty passes at the cadence, then the hold's expiry"
    );
    assert_eq!(driver.link(), LinkState::Up);
    assert!(driver.sending(), "held through the cycle");

    let r = run(driver, bus, clock, CAP);
    assert_eq!(r.outcome, Some(Outcome::Frame { len: 42 }));
    assert_eq!(r.polls, 2, "the held frame's write, then the frame's pass");
    assert!(!driver.sending());
    assert_eq!(driver.frames().data_sent, 24);
    assert_eq!(driver.frame_payload(), &arp_reply()[..]);

    assert!(driver.disconnect());
    assert_eq!(driver.link(), LinkState::Detached);
    let r = run(driver, bus, clock, CAP);
    assert_eq!(r.outcome, Some(Outcome::Disconnected));
    assert_eq!(r.polls, 2);
    let r = run(driver, bus, clock, 1);
    assert_eq!(
        (r.end, r.outcome),
        (End::Cap, None),
        "a frame after the disconnect: dropped"
    );
    assert_eq!(driver.frames().data_dropped, 2);
    assert!(driver.frame_payload().is_empty());
}

#[test]
fn the_bdc_header_and_the_transmit_frame_match_the_hand_derived_bytes() {
    let mut bytes = [0u8; 4];
    BdcHeader::TRANSMIT.write(&mut bytes);
    assert_eq!(bytes, [0x20, 0, 0, 0]);
    let padded = BdcHeader {
        flags: 0x20,
        priority: 0,
        flags2: 0,
        data_offset: 1,
    };
    assert_eq!(BdcHeader::parse(&[0x20, 0, 0, 1]), Some(padded));
    assert_eq!(padded.padding(), 4);
    assert_eq!(BdcHeader::parse(&[0x20, 0, 0]), None);
    let eth = arp_request();
    assert_eq!(eth.len(), 42);
    assert_eq!(
        &eth[..],
        &[
            0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x02, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0x08, 0x06,
            0x00, 0x01, 0x08, 0x00, 0x06, 0x04, 0x00, 0x01, 0x02, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE,
            0xC0, 0xA8, 0x04, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xC0, 0xA8, 0x04, 0x01
        ][..]
    );
    let mut buf = [0u8; TRANSMIT_BUF];
    buf[HEAD..HEAD + 42].copy_from_slice(&eth);
    let n = head(&mut buf, 19, 42);
    assert_eq!(n, 60);
    assert_eq!(
        &buf[..18],
        &[
            0x3C, 0x00, 0xC3, 0xFF, 0x13, 0x02, 0x00, 0x0E, 0, 0, 0, 0, 0, 0, 0x20, 0, 0, 0
        ]
    );
    assert_eq!(&buf[..60], &tx_frame(19, &eth)[..]);
    assert_eq!(
        (
            BDC_HEADER_LEN,
            TRANSMIT_OFFSET,
            HEAD,
            ETHERNET_HEADER_LEN,
            ETHERNET_MAX,
            TRANSMIT_BUF
        ),
        (4, 14, 18, 14, 1514, 1532)
    );
    assert_eq!(
        (bdc::VERSION, bdc::PRIORITY, bdc::IFACE_STATION),
        (0x20, 0, 0)
    );
    assert!(fits(14) && fits(1514) && !fits(13) && !fits(1515) && !fits(0));
}

#[test]
fn the_receive_walk_reads_the_chips_offsets_and_names_its_faults() {
    let f = rx_frame(26, 41, 14, 0, &arp_reply());
    assert_eq!(f.len(), 60);
    assert_eq!(
        &f[..18],
        &[
            0x3C, 0x00, 0xC3, 0xFF, 0x1A, 0x02, 0x00, 0x0E, 0x00, 0x29, 0, 0, 0, 0, 0x20, 0, 0, 0
        ]
    );
    assert_eq!(parse(&f, 14), Ok((18, 42)));
    assert_eq!(&f[18..60], &arp_reply()[..]);
    let f = rx_frame(27, 42, 24, 1, &datagram());
    assert_eq!(f.len(), 96);
    assert_eq!(
        &f[..12],
        &[
            0x60, 0x00, 0x9F, 0xFF, 0x1B, 0x02, 0x00, 0x18, 0x00, 0x2A, 0, 0
        ]
    );
    assert!(f[12..24].iter().all(|&b| b == 0), "twelve bytes of padding");
    assert_eq!(&f[24..32], &[0x20, 0, 0, 1, 0, 0, 0, 0]);
    assert_eq!(parse(&f, 24), Ok((32, 64)));
    assert_eq!(&f[32..], &datagram()[..]);
    assert_eq!(
        parse(&f[..26], 24),
        Err(Fault::Short),
        "inside the BDC header"
    );
    let mut past = f.clone();
    past[27] = 100;
    assert_eq!(
        parse(&past, 24),
        Err(Fault::Short),
        "the padding past the frame"
    );
    assert_eq!(
        parse(&f, 100),
        Err(Fault::Short),
        "an offset past the frame"
    );
    let short = rx_frame(1, 1, 14, 0, &[0xAB; 13]);
    assert_eq!(
        parse(&short, 14),
        Err(Fault::Short),
        "no room for an Ethernet header"
    );
    assert_eq!(parse(&rx_frame(1, 1, 14, 0, &[0xAB; 14]), 14), Ok((18, 14)));
    assert_eq!(
        parse(&rx_frame(1, 1, 14, 0, &[0xCD; 1515]), 14),
        Err(Fault::Length)
    );
    assert_eq!(
        parse(&rx_frame(1, 1, 14, 0, &[0xCD; 1514]), 14),
        Ok((18, 1514))
    );
}

#[test]
fn the_data_plane_replays_the_recorded_exchange() {
    let (mut script, mut c) = joined_script();
    let ops = data_ops(&mut c, true, false);
    assert_eq!(ops.len(), 151);
    let mut c2 = Cursor::AFTER_OPENING;
    up_ops(&mut c2, false);
    join_ops(&mut c2, false);
    assert_eq!(data_ops(&mut c2, false, false).len(), 107);
    script.extend_from_slice(&ops);
    let (mut driver, mut bus, mut clock) = joined_over(&script);
    drive_data_plane(&mut driver, &mut bus, &mut clock);
    assert_eq!(bus.remaining(), 0, "every row consumed");
    assert_eq!(bus.fault(), None);
    assert_eq!(
        driver.frames(),
        Frames {
            received: 49,
            sent: 49,
            events: 19,
            malformed: 2,
            dropped: 0,
            aborted: 0,
            empty: 0,
            data_received: 5,
            data_sent: 24,
            data_dropped: 2
        }
    );
    assert_eq!(driver.credit(), (49, 62));
    assert!(driver.is_ready());
}

#[test]
fn no_frame_is_taken_while_the_link_is_not_up() {
    // At Ready before the radio up.
    let script = open_script(true, true);
    let (mut driver, mut bus, _) = ready_over(&script);
    assert!(driver.stage(42).is_none());
    assert_eq!(driver.send(&mut bus, &arp_request()), Ok(false));
    assert_eq!(driver.flush(&mut bus), Ok(false));
    assert!(!driver.sending());
    assert_eq!(bus.remaining(), 0, "no row consumed");
    // After the radio up, no network held.
    let mut script = open_script(true, true);
    let mut c = Cursor::AFTER_OPENING;
    script.extend(up_ops(&mut c, true));
    let (mut driver, mut bus, mut clock) = ready_over(&script);
    assert!(driver.up());
    assert_eq!(
        run(&mut driver, &mut bus, &mut clock, CAP).outcome,
        Some(Outcome::Up { address: STATION })
    );
    assert!(driver.stage(42).is_none(), "detached");
    assert_eq!(driver.send(&mut bus, &arp_request()), Ok(false));
    // During the attempt.
    join_requests(&mut script, &mut c, true, &secured_values());
    let (mut driver, mut bus, mut clock) = ready_over(&script);
    assert!(driver.up());
    assert_eq!(
        run(&mut driver, &mut bus, &mut clock, CAP).outcome,
        Some(Outcome::Up { address: STATION })
    );
    assert!(driver.join(SSID, Some(PASSPHRASE)));
    assert_eq!(
        run(&mut driver, &mut bus, &mut clock, 16).end,
        End::Cap,
        "the eight exchanges done"
    );
    assert_eq!(driver.link(), LinkState::Joining);
    assert!(driver.stage(42).is_none(), "joining");
    assert_eq!(driver.send(&mut bus, &arp_request()), Ok(false));
    assert_eq!(bus.remaining(), 0);
    // After a loss, and after a disconnect.
    let (mut script, mut c) = joined_script();
    script.extend(burst_ops(&mut c, true));
    exchange(&mut script, &mut c, true, true, 52, b"", &[0; 12], 0, &[]);
    let (mut driver, mut bus, mut clock) = joined_over(&script);
    assert!(matches!(
        run(&mut driver, &mut bus, &mut clock, CAP).outcome,
        Some(Outcome::LinkLost(_))
    ));
    assert_eq!(driver.link(), LinkState::Down);
    assert!(driver.stage(42).is_none(), "down");
    assert_eq!(driver.send(&mut bus, &arp_request()), Ok(false));
    for _ in 0..4 {
        assert!(matches!(
            run(&mut driver, &mut bus, &mut clock, CAP).outcome,
            Some(Outcome::Event(_))
        ));
    }
    assert!(driver.disconnect());
    assert_eq!(
        run(&mut driver, &mut bus, &mut clock, CAP).outcome,
        Some(Outcome::Disconnected)
    );
    assert!(driver.stage(42).is_none(), "detached");
    assert_eq!(driver.send(&mut bus, &arp_request()), Ok(false));
    assert!(!driver.sending());
    assert_eq!(bus.remaining(), 0);
    assert_eq!(bus.fault(), None);
}

#[test]
fn a_data_frame_received_before_the_join_is_dropped_and_counted() {
    let mut script = open_script(true, true);
    let mut c = Cursor::AFTER_OPENING;
    script.extend(up_ops(&mut c, true));
    data_recv(&mut script, &mut c, true, 14, 0, &arp_reply());
    let (mut driver, mut bus, mut clock) = ready_over(&script);
    assert!(driver.up());
    assert_eq!(
        run(&mut driver, &mut bus, &mut clock, CAP).outcome,
        Some(Outcome::Up { address: STATION })
    );
    assert_eq!(driver.credit(), (11, 28));
    let r = run(&mut driver, &mut bus, &mut clock, 1);
    assert_eq!((r.end, r.outcome), (End::Cap, None));
    assert_eq!(
        (driver.frames().data_received, driver.frames().data_dropped),
        (1, 1)
    );
    assert_eq!(
        driver.credit(),
        (11, 29),
        "the credit taken from the dropped frame's header"
    );
    assert!(driver.frame_payload().is_empty());
    assert_eq!(bus.remaining(), 0);
    assert_eq!(bus.fault(), None);
}

#[test]
fn a_malformed_data_frame_is_dropped_and_the_next_taken_at_once() {
    let (mut script, mut c) = joined_script();
    let mut past = rx_frame(c.chip, c.credit, 14, 0, &arp_reply());
    past[17] = 100;
    pass(&mut script, true, leak(past));
    c.chip += 1;
    c.credit += 1;
    data_recv(&mut script, &mut c, true, 14, 0, &[0xAB; 10]);
    data_recv(&mut script, &mut c, true, 14, 0, &[0xCD; 1515]);
    data_recv(&mut script, &mut c, true, 14, 0, &arp_reply());
    let (mut driver, mut bus, mut clock) = joined_over(&script);
    let r = run(&mut driver, &mut bus, &mut clock, CAP);
    assert_eq!(r.outcome, Some(Outcome::Frame { len: 42 }));
    assert_eq!(
        r.polls, 4,
        "the BDC offset past the frame, no room for a header, over the unit, then the frame"
    );
    assert_eq!(
        (driver.frames().data_received, driver.frames().data_dropped),
        (4, 3)
    );
    assert_eq!(driver.frame_payload(), &arp_reply()[..]);
    assert_eq!(bus.remaining(), 0);
    assert_eq!(bus.fault(), None);
}

#[test]
fn a_frame_held_at_a_closed_window_is_written_when_a_credit_reopens_it() {
    // The window (19, 40) after the join: twenty-one sends close it.
    let (mut script, mut c) = joined_script();
    for _ in 0..BURST - 1 {
        data_send(&mut script, &mut c, &arp_request());
    }
    pass(&mut script, true, leak(credit_only(c.chip, 60)));
    script.push(Op::F2Write {
        data: leak(tx_frame(c.seq, &arp_request())),
        accepted: true,
    });
    let (mut driver, mut bus, mut clock) = joined_over(&script);
    for _ in 0..BURST - 1 {
        assert_eq!(driver.send(&mut bus, &arp_request()), Ok(true));
    }
    assert_eq!(driver.credit(), (40, 40), "closed");
    assert_eq!(
        driver.send(&mut bus, &arp_request()),
        Ok(true),
        "taken and held"
    );
    assert!(driver.sending());
    assert_eq!(driver.credit(), (40, 40), "no sequence spent");
    assert_eq!(driver.flush(&mut bus), Ok(false), "still closed");
    let r = run(&mut driver, &mut bus, &mut clock, 1);
    assert_eq!(
        (r.end, r.outcome),
        (End::Cap, None),
        "the credit-only frame's pass"
    );
    assert_eq!(driver.credit(), (40, 60));
    assert!(driver.sending(), "the next poll writes");
    let r = run(&mut driver, &mut bus, &mut clock, 1);
    assert_eq!((r.end, r.outcome), (End::Cap, None));
    assert!(!driver.sending());
    assert_eq!(driver.credit(), (41, 60));
    assert_eq!(driver.frames().data_sent, 22);
    assert_eq!(bus.remaining(), 0);
    assert_eq!(bus.fault(), None);
}

#[test]
fn a_write_not_accepted_retracts_the_sequence_and_the_next_poll_writes_it() {
    let (mut script, c) = joined_script();
    let frame = leak(tx_frame(c.seq, &arp_request()));
    script.push(Op::F2Write {
        data: frame,
        accepted: false,
    });
    script.push(Op::F2Write {
        data: frame,
        accepted: true,
    });
    let (mut driver, mut bus, mut clock) = joined_over(&script);
    assert_eq!(
        driver.send(&mut bus, &arp_request()),
        Ok(true),
        "taken and held"
    );
    assert!(driver.sending());
    assert_eq!(driver.credit(), (19, 40), "the sequence retracted");
    assert_eq!(driver.frames().data_sent, 0);
    let r = run(&mut driver, &mut bus, &mut clock, 1);
    assert_eq!((r.end, r.outcome), (End::Cap, None));
    assert!(!driver.sending());
    assert_eq!(driver.credit(), (20, 40), "the same sequence, spent once");
    assert_eq!(driver.frames().data_sent, 1);
    assert_eq!(bus.remaining(), 0);
    assert_eq!(bus.fault(), None);
}

#[test]
fn the_unit_bounds_the_frame_and_the_guards_hold() {
    let (script, _) = joined_script();
    let (mut driver, mut bus, _) = joined_over(&script);
    assert!(driver.stage(1515).is_none(), "over the unit");
    assert!(driver.stage(13).is_none(), "under a header");
    assert!(driver.stage(0).is_none());
    assert_eq!(driver.send(&mut bus, &[0u8; 1515]), Ok(false));
    assert!(!driver.sending());
    {
        let slot = driver.stage(1514).expect("the unit");
        assert_eq!(slot.len(), 1514);
    }
    assert!(driver.sending());
    assert!(driver.stage(42).is_none(), "one frame at a time");
    assert_eq!(
        driver.send(&mut bus, &arp_request()),
        Ok(false),
        "one frame at a time"
    );
    assert_eq!(bus.remaining(), 0, "nothing on the bus");
    assert_eq!(bus.fault(), None);
    assert_eq!(driver.refusal(), None);
    let mut fresh: Driver<'static> = Driver::new();
    assert!(fresh.stage(42).is_none(), "not before ready");
    assert_eq!(fresh.flush(&mut bus), Ok(false));
    assert_eq!(fresh.send(&mut bus, &arp_request()), Ok(false));
    assert!(!fresh.sending());
    assert!(fresh.frame_payload().is_empty());
    assert_eq!(fresh.refusal(), None);
}

#[test]
fn a_request_half_sent_goes_out_before_a_data_frame() {
    let (mut script, mut c) = joined_script();
    let mode = leak(request_frame(c.seq, c.id, true, 49, b"", &[0, 0, 0, 0]));
    script.push(Op::F2Write {
        data: mode,
        accepted: false,
    });
    script.push(Op::F2Write {
        data: mode,
        accepted: true,
    });
    c.seq += 1;
    c.id += 1;
    data_send(&mut script, &mut c, &arp_request());
    reply(&mut script, &mut c, true, 49, 0, &[]);
    let (mut driver, mut bus, mut clock) = joined_over(&script);
    assert!(driver.scan(), "a scan while up");
    let r = run(&mut driver, &mut bus, &mut clock, 1);
    assert_eq!(
        r.deadlines(),
        &[491_036],
        "the request's write not accepted: retried"
    );
    assert_eq!(
        driver.credit(),
        (20, 40),
        "the request took the sequence 19"
    );
    assert_eq!(
        driver.send(&mut bus, &arp_request()),
        Ok(true),
        "taken and held behind the request"
    );
    assert!(driver.sending());
    assert_eq!(driver.credit(), (20, 40));
    let r = run(&mut driver, &mut bus, &mut clock, 1);
    assert_eq!(r.end, End::Cap);
    assert!(driver.sending(), "the request went out first");
    assert_eq!(
        bus.remaining(),
        6,
        "the data frame's write and the reply's pass left"
    );
    let r = run(&mut driver, &mut bus, &mut clock, 1);
    assert_eq!(r.end, End::Cap);
    assert!(
        !driver.sending(),
        "the data frame written with the sequence 20"
    );
    assert_eq!(driver.credit(), (21, 40));
    assert_eq!(bus.remaining(), 5);
    let r = run(&mut driver, &mut bus, &mut clock, 1);
    assert_eq!(r.end, End::Cap);
    assert_eq!(bus.remaining(), 0, "the request's reply taken");
    assert_eq!(bus.fault(), None);
    assert_eq!(driver.frames().data_sent, 1);
}

#[test]
fn a_data_frame_during_a_requests_wait_is_handed_back_and_the_request_continues() {
    let (mut script, mut c) = joined_script();
    send(&mut script, &mut c, true, 49, b"", &[0, 0, 0, 0]);
    data_recv(&mut script, &mut c, true, 14, 0, &arp_reply());
    reply(&mut script, &mut c, true, 49, 0, &[]);
    let (mut driver, mut bus, mut clock) = joined_over(&script);
    assert!(driver.scan());
    let r = run(&mut driver, &mut bus, &mut clock, CAP);
    assert_eq!(r.outcome, Some(Outcome::Frame { len: 42 }));
    assert_eq!(r.polls, 2, "the send, then the wait's pass");
    assert_eq!(driver.frame_payload(), &arp_reply()[..]);
    let r = run(&mut driver, &mut bus, &mut clock, 1);
    assert_eq!(r.end, End::Cap);
    assert_eq!(bus.remaining(), 0, "the reply taken after the frame");
    assert_eq!(bus.fault(), None);
}
