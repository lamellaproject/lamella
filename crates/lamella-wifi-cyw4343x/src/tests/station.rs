//! The control plane at the transport level: the radio up with its event
//! and the address read back, a scan with its records and its end, a
//! secured join by the observed sequence, the observed loss burst, the
//! recovery cycle with a network-name-first completion, and the disconnect
//! -- as one recorded exchange of the trait's own operations after the
//! opening, with its controls; then the WPA3 join beside it, the
//! capability query, and the mismatch class in the recovery cycle.

use super::control::{
    ADDRESS, EMPTY_PASS, OPEN, event_of, open_script, pass, ready_over, reply_frame, request_frame,
};
use super::download_tie::leak;
use super::link::{
    PASSPHRASE, SSID, open_values, open_values_for, sae_values, secured_values, secured_values_for,
    secured_values_with_reset,
};
use super::scan::{
    RECORD_A, RECORD_B, RECORD_C, RECORD_D, RECORD_E, RECORD_F, SECURITY_D, SECURITY_F, SSID_B,
    record_a, record_b, record_c, record_d, record_e, record_f, result,
};
use crate::clock::Micros;
use crate::control::STATUS_NOT_UP;
use crate::driver::{Driver, Outcome};
use crate::error::Refusal;
use crate::event::{Event, EventMask, number, status};
use crate::fixture::{End, FakeClock, FakeTransport, Op, Run, run};
use crate::frame::Frames;
use crate::link::{Credential, JoinFailure, LinkState, STAGE_AUTH_MODE};
use crate::scan::{ScanEnd, params};
use crate::station::{Capabilities, STAGE_ADDRESS, STAGE_SCAN_REGULATORY};
use crate::transport::{Part, Transport};
use std::vec::Vec;

const CAP: u32 = 100_000;

/// The station's own address in the fixtures: locally administered.
pub(super) const STATION: [u8; 6] = [0x02, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE];
/// The eight bytes standing for an association event's elements.
pub(super) const IES: [u8; 8] = [0x30, 0x06, 0x01, 0x00, 0x00, 0x0F, 0xAC, 0x04];
/// No address, as scan result events carry.
const NONE: [u8; 6] = [0; 6];

/// The cursor through the host's sequences and ids and the chip's
/// sequences and credits.
#[derive(Clone, Copy, Debug)]
pub(super) struct Cursor {
    pub seq: u8,
    pub id: u16,
    pub chip: u8,
    pub credit: u8,
}

impl Cursor {
    /// The cursor after the opening of the recorded exchange.
    pub(super) const AFTER_OPENING: Cursor = Cursor {
        seq: 9,
        id: 10,
        chip: 11,
        credit: 26,
    };
}

/// One request sent: the frame the driver must write.
pub(super) fn send(
    v: &mut Vec<Op>,
    c: &mut Cursor,
    set: bool,
    cmd: u32,
    name: &[u8],
    value: &[u8],
) {
    v.push(Op::F2Write {
        data: leak(request_frame(c.seq, c.id, set, cmd, name, value)),
        accepted: true,
    });
    c.seq = c.seq.wrapping_add(1);
    c.id = c.id.wrapping_add(1);
}

/// The reply to the last request sent, in a service pass.
pub(super) fn reply(
    v: &mut Vec<Op>,
    c: &mut Cursor,
    mailbox: bool,
    cmd: u32,
    status: u32,
    payload: &[u8],
) {
    let id = c.id.wrapping_sub(1);
    pass(
        v,
        mailbox,
        leak(reply_frame(c.chip, c.credit, cmd, id, status, payload)),
    );
    c.chip = c.chip.wrapping_add(1);
    c.credit = c.credit.wrapping_add(1);
}

/// A request and its reply.
#[allow(clippy::too_many_arguments)]
pub(super) fn exchange(
    v: &mut Vec<Op>,
    c: &mut Cursor,
    mailbox: bool,
    set: bool,
    cmd: u32,
    name: &[u8],
    value: &[u8],
    status: u32,
    payload: &[u8],
) {
    send(v, c, set, cmd, name, value);
    reply(v, c, mailbox, cmd, status, payload);
}

/// An event frame in a service pass.
#[allow(clippy::too_many_arguments)]
pub(super) fn event(
    v: &mut Vec<Op>,
    c: &mut Cursor,
    mailbox: bool,
    number: u32,
    status: u32,
    reason: u32,
    address: [u8; 6],
    payload: &[u8],
) {
    pass(
        v,
        mailbox,
        leak(event_of(
            c.chip, c.credit, 0, number, status, reason, 0, address, payload,
        )),
    );
    c.chip = c.chip.wrapping_add(1);
    c.credit = c.credit.wrapping_add(1);
}

/// An event frame in a service pass carrying the credit the previous
/// frame carried: the chip advances its sequence and not its credit when
/// no host frame was consumed between two of its own frames -- the shape
/// of the firmware's join events on the wire.
#[allow(clippy::too_many_arguments)]
pub(super) fn event_held_credit(
    v: &mut Vec<Op>,
    c: &mut Cursor,
    mailbox: bool,
    number: u32,
    status: u32,
    reason: u32,
    address: [u8; 6],
    payload: &[u8],
) {
    let credit = c.credit.wrapping_sub(1);
    pass(
        v,
        mailbox,
        leak(event_of(
            c.chip, credit, 0, number, status, reason, 0, address, payload,
        )),
    );
    c.chip = c.chip.wrapping_add(1);
}

/// `n` empty passes, explicit.
pub(super) fn empty(v: &mut Vec<Op>, n: usize) {
    for _ in 0..n {
        v.extend_from_slice(&EMPTY_PASS);
    }
}

/// `n` empty passes as one repeated group.
fn repeat(v: &mut Vec<Op>, n: u32) {
    v.push(Op::Repeat {
        ops: &EMPTY_PASS,
        times: n,
    });
}

/// An event frame in a service pass, with an authentication type in its
/// header.
#[allow(clippy::too_many_arguments)]
pub(super) fn event_auth(
    v: &mut Vec<Op>,
    c: &mut Cursor,
    mailbox: bool,
    number: u32,
    status: u32,
    reason: u32,
    auth_type: u32,
    address: [u8; 6],
    payload: &[u8],
) {
    pass(
        v,
        mailbox,
        leak(event_of(
            c.chip, c.credit, 0, number, status, reason, auth_type, address, payload,
        )),
    );
    c.chip = c.chip.wrapping_add(1);
    c.credit = c.credit.wrapping_add(1);
}

/// The event as the driver hands it back.
pub(super) const fn ev(
    number: u32,
    status: u32,
    reason: u32,
    address: [u8; 6],
    len: usize,
) -> Event {
    ev_auth(number, status, reason, 0, address, len)
}

/// The event as the driver hands it back, with its authentication type.
pub(super) const fn ev_auth(
    number: u32,
    status: u32,
    reason: u32,
    auth_type: u32,
    address: [u8; 6],
    len: usize,
) -> Event {
    Event {
        number,
        status,
        reason,
        auth_type,
        address,
        len,
    }
}

/// The firmware's synthetic capability string, 35 bytes.
pub(super) const CAP_STRING: &[u8] = b"ap sta wme 802.11d mfp sae wowl tko";

/// The capability query and its reply carrying `string` then zeros to the
/// capacity, with `status`.
pub(super) fn cap_ops(c: &mut Cursor, mailbox: bool, string: &[u8], status: u32) -> Vec<Op> {
    let mut v = Vec::new();
    send(&mut v, c, false, 262, b"cap", &[0; 768]);
    let mut payload = string.to_vec();
    payload.resize(768, 0);
    reply(&mut v, c, mailbox, 262, status, &payload);
    v
}

/// The radio up: the command and its reply, an empty pass, the radio event,
/// the address query and its reply.
pub(super) fn up_ops(c: &mut Cursor, mailbox: bool) -> Vec<Op> {
    let mut v = Vec::new();
    send(&mut v, c, true, 2, b"", &[]);
    reply(&mut v, c, mailbox, 2, 0, &[]);
    empty(&mut v, 1);
    event(&mut v, c, mailbox, number::RADIO, 0, 0, ADDRESS, &[]);
    send(&mut v, c, false, 262, b"cur_etheraddr", &[0; 6]);
    reply(&mut v, c, mailbox, 262, 0, &STATION);
    v
}

/// A scan's two requests and their replies.
pub(super) fn scan_requests(v: &mut Vec<Op>, c: &mut Cursor, mailbox: bool, sync: u16) {
    exchange(v, c, mailbox, true, 49, b"", &[0, 0, 0, 0], 0, &[]);
    exchange(v, c, mailbox, true, 263, b"escan", &params(sync), 0, &[]);
}

/// A scan result event of `sync` with `status` carrying `recs`.
pub(super) fn scan_event(
    v: &mut Vec<Op>,
    c: &mut Cursor,
    mailbox: bool,
    sync: u16,
    status: u32,
    recs: &[Vec<u8>],
) {
    event(
        v,
        c,
        mailbox,
        number::ESCAN_RESULT,
        status,
        0,
        NONE,
        &result(sync, recs),
    );
}

/// The scan: the two requests, a partial result with records A and B, a
/// partial result with C, an empty pass, the done event with no records.
pub(super) fn scan_ops(c: &mut Cursor, mailbox: bool) -> Vec<Op> {
    let mut v = Vec::new();
    scan_requests(&mut v, c, mailbox, 1);
    scan_event(
        &mut v,
        c,
        mailbox,
        1,
        status::PARTIAL,
        &[record_a(), record_b()],
    );
    scan_event(&mut v, c, mailbox, 1, status::PARTIAL, &[record_c()]);
    empty(&mut v, 1);
    scan_event(&mut v, c, mailbox, 1, status::SUCCESS, &[]);
    v
}

/// The join's requests and their replies for `values`.
pub(super) fn join_requests(
    v: &mut Vec<Op>,
    c: &mut Cursor,
    mailbox: bool,
    values: &[(u32, &[u8], Vec<u8>)],
) {
    for (cmd, name, value) in values {
        exchange(v, c, mailbox, true, *cmd, name, value, 0, &[]);
    }
}

/// The secured join: the eight requests, then the observed sequence of
/// events to the handshake's success, then the firmware's own association
/// success after it (the order the WPA2 flights delivered; its frame
/// carrying the credit the supplicant's frame carried) and the hold's five
/// empty passes, at whose end the join completes.
pub(super) fn join_ops(c: &mut Cursor, mailbox: bool) -> Vec<Op> {
    let mut v = Vec::new();
    join_requests(&mut v, c, mailbox, &secured_values());
    event(&mut v, c, mailbox, number::AUTH, 0, 0, ADDRESS, &[]);
    event(&mut v, c, mailbox, number::ASSOC, 0, 0, ADDRESS, &IES);
    event(&mut v, c, mailbox, number::JOIN, 0, 0, ADDRESS, &[]);
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
    event_held_credit(&mut v, c, mailbox, number::SET_SSID, 0, 0, ADDRESS, &[]);
    empty(&mut v, 5);
    v
}

/// The WPA3 join: the nine requests, then the events in the order the two
/// flights delivered them -- the authentication event carrying the SAE
/// algorithm number, the association, the link indication, the
/// supplicant's word, then the firmware's own join and association
/// success, which completes the join.
pub(super) fn sae_join_ops(c: &mut Cursor, mailbox: bool) -> Vec<Op> {
    let mut v = Vec::new();
    join_requests(&mut v, c, mailbox, &sae_values());
    event_auth(&mut v, c, mailbox, number::AUTH, 0, 0, 3, ADDRESS, &[]);
    event(&mut v, c, mailbox, number::ASSOC, 0, 0, ADDRESS, &IES);
    event(&mut v, c, mailbox, number::LINK, 0, 0, ADDRESS, &[]);
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
    event(&mut v, c, mailbox, number::JOIN, 0, 0, ADDRESS, &[]);
    event(&mut v, c, mailbox, number::SET_SSID, 0, 0, ADDRESS, &[]);
    // The hold after the supplicant's word: five empty passes at the
    // cadence, then the join completes at its expiry.
    empty(&mut v, 5);
    v
}

/// The WPA3 plane after the opening: the capability query, the radio up,
/// the scan, the WPA3 join.
pub(super) fn sae_plane_ops(mailbox: bool) -> Vec<Op> {
    let mut c = Cursor::AFTER_OPENING;
    let mut v = cap_ops(&mut c, mailbox, CAP_STRING, 0);
    v.extend(up_ops(&mut c, mailbox));
    v.extend(scan_ops(&mut c, mailbox));
    v.extend(sae_join_ops(&mut c, mailbox));
    v
}

/// The WPA3 plane's outcomes in order over a driver at `Ready`.
pub(super) fn drive_sae<T: Transport>(
    driver: &mut Driver<'static>,
    bus: &mut T,
    clock: &mut FakeClock,
) {
    assert!(driver.capabilities());
    let (o, r) = outcome(driver, bus, clock);
    assert_eq!(
        o,
        Some(Outcome::Capabilities(Capabilities {
            answered: true,
            status: 0,
            len: 35,
            sae: true,
            mfp: true
        }))
    );
    assert_eq!(r.polls, 2);
    assert_eq!(driver.capability_string(), CAP_STRING);
    assert!(driver.up());
    assert_eq!(
        outcome(driver, bus, clock).0,
        Some(Outcome::Up { address: STATION })
    );
    assert!(driver.scan());
    for _ in 0..4 {
        outcome(driver, bus, clock);
    }
    assert!(driver.join_with(SSID, Credential::SaePassword(PASSPHRASE)));
    assert_eq!(driver.link(), LinkState::Joining);
    let (o, r) = outcome(driver, bus, clock);
    assert_eq!(
        o,
        Some(Outcome::Event(ev_auth(number::AUTH, 0, 0, 3, ADDRESS, 0)))
    );
    assert_eq!(r.polls, 19, "nine exchanges and the event");
    assert_eq!(
        outcome(driver, bus, clock).0,
        Some(Outcome::Event(ev(number::ASSOC, 0, 0, ADDRESS, 8)))
    );
    assert_eq!(
        outcome(driver, bus, clock).0,
        Some(Outcome::Event(ev(number::LINK, 0, 0, ADDRESS, 0)))
    );
    assert_eq!(
        outcome(driver, bus, clock).0,
        Some(Outcome::Event(ev(number::PSK_SUP, 6, 0, ADDRESS, 0))),
        "the supplicant's word before the association success: a plain event"
    );
    assert_eq!(driver.link(), LinkState::Joining);
    assert_eq!(
        outcome(driver, bus, clock).0,
        Some(Outcome::Event(ev(number::JOIN, 0, 0, ADDRESS, 0)))
    );
    assert_eq!(
        outcome(driver, bus, clock).0,
        Some(Outcome::Event(ev(number::SET_SSID, 0, 0, ADDRESS, 0))),
        "the association word inside the hold: held"
    );
    assert_eq!(driver.link(), LinkState::Joining);
    let (o, r) = outcome(driver, bus, clock);
    assert_eq!(
        o,
        Some(Outcome::Joined { bssid: ADDRESS }),
        "the hold's expiry completes"
    );
    assert_eq!(
        r.polls, 6,
        "five empty passes at the cadence, then the expiry"
    );
    assert_eq!(r.deadlines().len(), 5);
    assert_eq!(driver.link(), LinkState::Up);
}

/// The observed loss burst.
pub(super) fn burst_ops(c: &mut Cursor, mailbox: bool) -> Vec<Op> {
    let mut v = Vec::new();
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
    event(&mut v, c, mailbox, number::DISASSOC_IND, 0, 2, ADDRESS, &[]);
    event(&mut v, c, mailbox, number::DEAUTH, 0, 6, ADDRESS, &[]);
    event(&mut v, c, mailbox, number::DEAUTH, 0, 7, ADDRESS, &[]);
    event(&mut v, c, mailbox, number::LINK, 0, 1, ADDRESS, &[]);
    v
}

/// The recovery cycle: the settle (a hundred empty passes, explicit or as
/// one group), the scan seeing A alone, the re-entry's three requests, the
/// events to a network-name-first completion.
pub(super) fn recovery_ops(c: &mut Cursor, mailbox: bool, explicit: bool) -> Vec<Op> {
    let mut v = Vec::new();
    if explicit {
        empty(&mut v, 100);
    } else {
        repeat(&mut v, 100);
    }
    scan_requests(&mut v, c, mailbox, 2);
    scan_event(&mut v, c, mailbox, 2, status::PARTIAL, &[record_a()]);
    scan_event(&mut v, c, mailbox, 2, status::SUCCESS, &[]);
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
    // The hold after the supplicant's word: five empty passes, then the
    // join completes at its expiry.
    empty(&mut v, 5);
    v
}

/// The disconnect and its reply, then a loss-shaped event after it.
pub(super) fn disconnect_ops(c: &mut Cursor, mailbox: bool) -> Vec<Op> {
    let mut v = Vec::new();
    exchange(&mut v, c, mailbox, true, 52, b"", &[0; 12], 0, &[]);
    event(&mut v, c, mailbox, number::DEAUTH_IND, 0, 8, ADDRESS, &[]);
    v
}

/// The whole plane after the opening, the settle explicit for the ties.
pub(super) fn plane_ops(mailbox: bool, explicit: bool) -> Vec<Op> {
    let mut c = Cursor::AFTER_OPENING;
    let mut v = up_ops(&mut c, mailbox);
    v.extend(scan_ops(&mut c, mailbox));
    v.extend(join_ops(&mut c, mailbox));
    v.extend(burst_ops(&mut c, mailbox));
    v.extend(recovery_ops(&mut c, mailbox, explicit));
    v.extend(disconnect_ops(&mut c, mailbox));
    v
}

/// The attach, the download, the opening and the plane.
fn plane_script() -> Vec<Op> {
    let mut s = open_script(true, true);
    s.extend(plane_ops(true, false));
    s
}

/// One run to its outcome.
fn outcome<T: Transport>(
    driver: &mut Driver,
    bus: &mut T,
    clock: &mut FakeClock,
) -> (Option<Outcome>, Run) {
    let record = run(driver, bus, clock, CAP);
    (record.outcome, record)
}

/// The plane's outcomes in order over a driver at `Ready` with the clock at
/// `base`: every outcome, poll count and deadline asserted.
pub(super) fn drive_plane<T: Transport>(
    driver: &mut Driver<'static>,
    bus: &mut T,
    clock: &mut FakeClock,
    base: Micros,
) {
    let step = 10_000;
    assert!(driver.up());
    let (o, r) = outcome(driver, bus, clock);
    assert_eq!(o, Some(Outcome::Up { address: STATION }));
    assert_eq!(r.deadlines(), &[base + step]);
    assert_eq!(r.polls, 6);
    assert_eq!(driver.address(), Some(STATION));
    assert_eq!(driver.link(), LinkState::Detached);
    assert!(!driver.up(), "once per attach");

    assert!(driver.scan());
    let (o, r) = outcome(driver, bus, clock);
    assert_eq!(o, Some(Outcome::ScanRecord(RECORD_A)));
    assert_eq!(r.polls, 5);
    assert_eq!(driver.record_bytes(), &record_a()[..]);
    assert_eq!(
        driver.event_payload(),
        &result(1, &[record_a(), record_b()])[..]
    );
    let (o, r) = outcome(driver, bus, clock);
    assert_eq!(o, Some(Outcome::ScanRecord(RECORD_B)));
    assert_eq!(r.polls, 1);
    assert_eq!(driver.record_bytes(), &record_b()[..]);
    let (o, r) = outcome(driver, bus, clock);
    assert_eq!(o, Some(Outcome::ScanRecord(RECORD_C)));
    assert_eq!(r.polls, 1);
    assert_eq!(driver.record_bytes(), &record_c()[..]);
    let (o, r) = outcome(driver, bus, clock);
    assert_eq!(
        o,
        Some(Outcome::ScanDone {
            end: ScanEnd::Complete,
            records: 3
        })
    );
    assert_eq!(r.polls, 2);
    assert_eq!(r.deadlines(), &[base + 2 * step]);
    assert!(driver.record_bytes().is_empty());

    assert!(driver.join(SSID, Some(PASSPHRASE)));
    assert_eq!(driver.link(), LinkState::Joining);
    let (o, r) = outcome(driver, bus, clock);
    assert_eq!(o, Some(Outcome::Event(ev(number::AUTH, 0, 0, ADDRESS, 0))));
    assert_eq!(r.polls, 17);
    let (o, r) = outcome(driver, bus, clock);
    assert_eq!(o, Some(Outcome::Event(ev(number::ASSOC, 0, 0, ADDRESS, 8))));
    assert_eq!(r.polls, 1);
    assert_eq!(driver.event_payload(), &IES);
    let (o, _) = outcome(driver, bus, clock);
    assert_eq!(o, Some(Outcome::Event(ev(number::JOIN, 0, 0, ADDRESS, 0))));
    assert_eq!(driver.link(), LinkState::Joining);
    let (o, r) = outcome(driver, bus, clock);
    assert_eq!(
        o,
        Some(Outcome::Event(ev(number::PSK_SUP, 6, 0, ADDRESS, 0))),
        "the supplicant's word starts the hold"
    );
    assert_eq!(r.polls, 1);
    let supplicant_at = clock.now();
    assert_eq!(driver.link(), LinkState::Joining);
    let (o, r) = outcome(driver, bus, clock);
    assert_eq!(
        o,
        Some(Outcome::Event(ev(number::SET_SSID, 0, 0, ADDRESS, 0))),
        "the association word inside the hold: held"
    );
    assert_eq!(r.polls, 1);
    assert_eq!(driver.link(), LinkState::Joining);
    let (o, r) = outcome(driver, bus, clock);
    assert_eq!(
        o,
        Some(Outcome::Joined { bssid: ADDRESS }),
        "the hold's expiry completes"
    );
    assert_eq!(
        r.polls, 6,
        "five empty passes at the cadence, then the expiry"
    );
    let hold: Vec<Micros> = (1..=5).map(|i| supplicant_at + i * step).collect();
    assert_eq!(r.deadlines(), &hold[..]);
    assert_eq!(clock.now(), supplicant_at + 50_000);
    assert_eq!(driver.link(), LinkState::Up);

    let (o, r) = outcome(driver, bus, clock);
    assert_eq!(
        o,
        Some(Outcome::LinkLost(ev(number::PSK_SUP, 6, 14, ADDRESS, 0)))
    );
    assert_eq!(r.polls, 1);
    assert_eq!(driver.link(), LinkState::Down);
    for (n, reason) in [
        (number::DISASSOC_IND, 2),
        (number::DEAUTH, 6),
        (number::DEAUTH, 7),
        (number::LINK, 1),
    ] {
        let (o, r) = outcome(driver, bus, clock);
        assert_eq!(
            o,
            Some(Outcome::Event(ev(n, 0, reason, ADDRESS, 0))),
            "the burst's event {n}"
        );
        assert_eq!(r.polls, 1);
        assert_eq!(driver.link(), LinkState::Down);
    }
    let lost_at = clock.now();

    let (o, r) = outcome(driver, bus, clock);
    assert_eq!(o, Some(Outcome::ScanRecord(RECORD_A)));
    assert_eq!(
        r.polls, 106,
        "a hundred empty passes, the cycle's start, the two exchanges, the result"
    );
    let settle: Vec<Micros> = (1..=100).map(|i| lost_at + i * step).collect();
    assert_eq!(r.deadlines(), &settle[..]);
    assert_eq!(driver.link(), LinkState::Down);
    let (o, r) = outcome(driver, bus, clock);
    assert_eq!(
        o,
        Some(Outcome::ScanDone {
            end: ScanEnd::Complete,
            records: 1
        })
    );
    assert_eq!(r.polls, 1);
    assert_eq!(
        driver.link(),
        LinkState::Joining,
        "the network seen: the re-entry"
    );
    let (o, r) = outcome(driver, bus, clock);
    assert_eq!(o, Some(Outcome::Event(ev(number::AUTH, 0, 0, ADDRESS, 0))));
    assert_eq!(r.polls, 7, "the re-entry's three exchanges and the event");
    let (o, _) = outcome(driver, bus, clock);
    assert_eq!(o, Some(Outcome::Event(ev(number::ASSOC, 0, 0, ADDRESS, 8))));
    let (o, _) = outcome(driver, bus, clock);
    assert_eq!(
        o,
        Some(Outcome::Event(ev(number::SET_SSID, 0, 0, ADDRESS, 0)))
    );
    assert_eq!(
        driver.link(),
        LinkState::Joining,
        "associated, not up: held until the handshake"
    );
    let (o, r) = outcome(driver, bus, clock);
    assert_eq!(
        o,
        Some(Outcome::Event(ev(number::PSK_SUP, 6, 0, ADDRESS, 0))),
        "the handshake's word starts the hold"
    );
    assert_eq!(r.polls, 1);
    assert_eq!(driver.link(), LinkState::Joining);
    let (o, r) = outcome(driver, bus, clock);
    assert_eq!(
        o,
        Some(Outcome::Joined { bssid: ADDRESS }),
        "the hold's expiry completes the re-entry"
    );
    assert_eq!(r.polls, 6);
    assert_eq!(r.deadlines().len(), 5);
    assert_eq!(driver.link(), LinkState::Up);

    assert!(driver.disconnect());
    assert_eq!(driver.link(), LinkState::Detached, "dropped at once");
    let (o, r) = outcome(driver, bus, clock);
    assert_eq!(o, Some(Outcome::Disconnected));
    assert_eq!(r.polls, 2);
    let (o, r) = outcome(driver, bus, clock);
    assert_eq!(
        o,
        Some(Outcome::Event(ev(number::DEAUTH_IND, 0, 8, ADDRESS, 0))),
        "a plain event once detached"
    );
    assert_eq!(r.polls, 1);
    assert_eq!(driver.link(), LinkState::Detached);
    assert!(!driver.disconnect(), "nothing held");
}

fn frames(received: u32, sent: u32, events: u32) -> Frames {
    Frames {
        received,
        sent,
        events,
        malformed: 2,
        dropped: 0,
        aborted: 0,
        empty: 0,
        data_received: 0,
        data_sent: 0,
        data_dropped: 0,
    }
}

fn refused(stage: &'static str, status: u32) -> Option<Outcome> {
    Some(Outcome::Refused(Refusal::new(stage, status)))
}

#[test]
fn the_control_plane_replays_the_recorded_exchange_to_a_joined_network_and_back() {
    let mut c = Cursor::AFTER_OPENING;
    let phases = [
        (up_ops(&mut c, true), 19, "the radio up"),
        (scan_ops(&mut c, true), 29, "the scan"),
        (join_ops(&mut c, true), 83, "the join"),
        (burst_ops(&mut c, true), 25, "the burst"),
        (
            recovery_ops(&mut c, true, false),
            71,
            "the recovery cycle with the settle as one group",
        ),
        (disconnect_ops(&mut c, true), 11, "the disconnect"),
    ];
    let mut script = open_script(true, true);
    for (ops, rows, name) in &phases {
        assert_eq!(ops.len(), *rows, "{name}");
        script.extend_from_slice(ops);
    }
    assert_eq!(script.len(), OPEN + 69 + 238);
    let mut c = Cursor::AFTER_OPENING;
    assert_eq!(up_ops(&mut c, false).len(), 13);
    assert_eq!(scan_ops(&mut c, false).len(), 19);
    assert_eq!(join_ops(&mut c, false).len(), 57);
    assert_eq!(burst_ops(&mut c, false).len(), 15);
    assert_eq!(recovery_ops(&mut c, false, true).len(), 248);
    assert_eq!(disconnect_ops(&mut c, false).len(), 7);

    let (mut driver, mut bus, mut clock) = ready_over(&script);
    drive_plane(&mut driver, &mut bus, &mut clock, 421_036);
    assert_eq!(bus.remaining(), 0, "every row consumed");
    assert_eq!(bus.fault(), None);
    assert_eq!(driver.frames(), frames(50, 27, 23));
    assert_eq!(driver.credit(), (27, 63));
    assert!(driver.is_ready());
}

#[test]
fn the_counters_at_each_phases_end() {
    let mut script = open_script(true, true);
    let mut c = Cursor::AFTER_OPENING;
    script.extend(up_ops(&mut c, true));
    script.extend(scan_ops(&mut c, true));
    script.extend(join_ops(&mut c, true));
    script.extend(burst_ops(&mut c, true));
    let (mut driver, mut bus, mut clock) = ready_over(&script);
    assert!(driver.up());
    assert_eq!(
        outcome(&mut driver, &mut bus, &mut clock).0,
        Some(Outcome::Up { address: STATION })
    );
    assert_eq!(driver.frames(), frames(14, 11, 3));
    assert_eq!(driver.credit(), (11, 28));
    assert!(driver.scan());
    for _ in 0..4 {
        outcome(&mut driver, &mut bus, &mut clock);
    }
    assert_eq!(driver.frames(), frames(19, 13, 6));
    assert_eq!(driver.credit(), (13, 33));
    assert!(driver.join(SSID, Some(PASSPHRASE)));
    for _ in 0..6 {
        outcome(&mut driver, &mut bus, &mut clock);
    }
    assert_eq!(driver.link(), LinkState::Up);
    assert_eq!(driver.frames(), frames(32, 21, 11));
    assert_eq!(driver.credit(), (21, 45));
    for _ in 0..5 {
        outcome(&mut driver, &mut bus, &mut clock);
    }
    assert_eq!(driver.link(), LinkState::Down);
    assert_eq!(driver.frames(), frames(37, 21, 16));
    assert_eq!(driver.credit(), (21, 50));
    assert_eq!(bus.remaining(), 0);
}

/// A driver at `Ready` with the radio up over `script`, the clock at
/// 431,036 and the cursor past the radio up.
fn up_over(script: &[Op]) -> (Driver<'static>, FakeTransport<'_>, FakeClock) {
    let (mut driver, mut bus, mut clock) = ready_over(script);
    assert!(driver.up());
    let (o, _) = outcome(&mut driver, &mut bus, &mut clock);
    assert_eq!(o, Some(Outcome::Up { address: STATION }));
    assert_eq!(clock.now(), 431_036);
    (driver, bus, clock)
}

/// The opening then the radio up: the script and the cursor after them.
fn up_script() -> (Vec<Op>, Cursor) {
    let mut script = open_script(true, true);
    let mut c = Cursor::AFTER_OPENING;
    script.extend(up_ops(&mut c, true));
    (script, c)
}

#[test]
fn the_radio_event_never_coming_is_not_a_failure() {
    let mut script = open_script(true, true);
    let mut c = Cursor::AFTER_OPENING;
    send(&mut script, &mut c, true, 2, b"", &[]);
    reply(&mut script, &mut c, true, 2, 0, &[]);
    repeat(&mut script, 100);
    send(&mut script, &mut c, false, 262, b"cur_etheraddr", &[0; 6]);
    reply(&mut script, &mut c, true, 262, 0, &STATION);
    let (mut driver, mut bus, mut clock) = ready_over(&script);
    assert!(driver.up());
    let (o, r) = outcome(&mut driver, &mut bus, &mut clock);
    assert_eq!(o, Some(Outcome::Up { address: STATION }));
    assert_eq!(r.deadlines().len(), 100, "a second of empty passes");
    assert_eq!(r.deadlines()[0], 431_036);
    assert_eq!(r.deadlines()[99], 1_421_036);
    assert_eq!(r.polls, 105);
    assert_eq!(driver.frames().sent, 11);
    assert_eq!(bus.remaining(), 0);
    assert_eq!(bus.fault(), None);
}

#[test]
fn a_not_up_answer_is_retried_with_a_fresh_id_and_the_third_answers() {
    let mut script = open_script(true, true);
    let mut c = Cursor::AFTER_OPENING;
    send(&mut script, &mut c, true, 2, b"", &[]);
    reply(&mut script, &mut c, true, 2, 0, &[]);
    empty(&mut script, 1);
    event(&mut script, &mut c, true, number::RADIO, 0, 0, ADDRESS, &[]);
    for _ in 0..2 {
        send(&mut script, &mut c, false, 262, b"cur_etheraddr", &[0; 6]);
        reply(&mut script, &mut c, true, 262, STATUS_NOT_UP, &[]);
    }
    send(&mut script, &mut c, false, 262, b"cur_etheraddr", &[0; 6]);
    reply(&mut script, &mut c, true, 262, 0, &STATION);
    assert_eq!(c.id, 14, "the ids 11, 12 and 13 on the wire");
    let (mut driver, mut bus, mut clock) = ready_over(&script);
    assert!(driver.up());
    let (o, r) = outcome(&mut driver, &mut bus, &mut clock);
    assert_eq!(o, Some(Outcome::Up { address: STATION }));
    assert_eq!(r.deadlines(), &[431_036, 531_036, 631_036]);
    assert_eq!(r.polls, 12);
    assert_eq!(driver.frames().sent, 13);
    assert_eq!(bus.remaining(), 0);
    assert_eq!(bus.fault(), None);
}

#[test]
fn the_not_up_retries_exhausted_refuse_by_name() {
    let mut script = open_script(true, true);
    let mut c = Cursor::AFTER_OPENING;
    send(&mut script, &mut c, true, 2, b"", &[]);
    reply(&mut script, &mut c, true, 2, 0, &[]);
    empty(&mut script, 1);
    event(&mut script, &mut c, true, number::RADIO, 0, 0, ADDRESS, &[]);
    for _ in 0..6 {
        send(&mut script, &mut c, false, 262, b"cur_etheraddr", &[0; 6]);
        reply(&mut script, &mut c, true, 262, STATUS_NOT_UP, &[]);
    }
    let (mut driver, mut bus, mut clock) = ready_over(&script);
    assert!(driver.up());
    let (o, r) = outcome(&mut driver, &mut bus, &mut clock);
    assert_eq!(o, refused(STAGE_ADDRESS, STATUS_NOT_UP));
    assert_eq!(
        r.deadlines(),
        &[431_036, 531_036, 631_036, 731_036, 831_036, 931_036]
    );
    assert_eq!(driver.frames().sent, 16);
    assert!(driver.is_parked());
    assert_eq!(driver.address(), None);
    assert_eq!(bus.remaining(), 0);
}

#[test]
fn a_short_address_is_refused_by_name() {
    let mut script = open_script(true, true);
    let mut c = Cursor::AFTER_OPENING;
    send(&mut script, &mut c, true, 2, b"", &[]);
    reply(&mut script, &mut c, true, 2, 0, &[]);
    event(&mut script, &mut c, true, number::RADIO, 0, 0, ADDRESS, &[]);
    send(&mut script, &mut c, false, 262, b"cur_etheraddr", &[0; 6]);
    reply(&mut script, &mut c, true, 262, 0, &STATION[..2]);
    let (mut driver, mut bus, mut clock) = ready_over(&script);
    assert!(driver.up());
    let (o, _) = outcome(&mut driver, &mut bus, &mut clock);
    assert_eq!(o, refused(STAGE_ADDRESS, 2));
    assert_eq!(bus.remaining(), 0);
}

#[test]
fn a_scan_timing_out_ends_at_the_deadline_and_leaves_the_driver_ready() {
    let (mut script, mut c) = up_script();
    scan_requests(&mut script, &mut c, true, 1);
    repeat(&mut script, 500);
    let (mut driver, mut bus, mut clock) = up_over(&script);
    assert!(driver.scan());
    let (o, r) = outcome(&mut driver, &mut bus, &mut clock);
    assert_eq!(
        o,
        Some(Outcome::ScanDone {
            end: ScanEnd::TimedOut,
            records: 0
        })
    );
    assert_eq!(r.deadlines().len(), 500);
    assert_eq!(r.deadlines()[0], 441_036);
    assert_eq!(r.deadlines()[499], 5_431_036);
    assert_eq!(r.polls, 505);
    assert_eq!(driver.link(), LinkState::Detached);
    assert!(driver.is_ready());
    assert!(driver.scan(), "a scan is accepted again");
    assert_eq!(bus.remaining(), 0);
}

#[test]
fn a_done_event_with_no_records_ends_the_scan_at_once() {
    let (mut script, mut c) = up_script();
    scan_requests(&mut script, &mut c, true, 1);
    scan_event(&mut script, &mut c, true, 1, status::SUCCESS, &[]);
    let (mut driver, mut bus, mut clock) = up_over(&script);
    assert!(driver.scan());
    let (o, r) = outcome(&mut driver, &mut bus, &mut clock);
    assert_eq!(
        o,
        Some(Outcome::ScanDone {
            end: ScanEnd::Complete,
            records: 0
        })
    );
    assert_eq!(r.polls, 5);
    assert!(r.deadlines().is_empty());
    assert_eq!(bus.remaining(), 0);
}

#[test]
fn a_records_length_past_the_payload_delivers_it_clipped_and_ends_the_walk() {
    let (mut script, mut c) = up_script();
    scan_requests(&mut script, &mut c, true, 1);
    let mut long = record_a();
    long[4..8].copy_from_slice(&500u32.to_le_bytes());
    scan_event(
        &mut script,
        &mut c,
        true,
        1,
        status::PARTIAL,
        &[long, record_b()],
    );
    scan_event(&mut script, &mut c, true, 1, status::PARTIAL, &[record_c()]);
    scan_event(&mut script, &mut c, true, 1, status::SUCCESS, &[]);
    let (mut driver, mut bus, mut clock) = up_over(&script);
    assert!(driver.scan());
    let (o, _) = outcome(&mut driver, &mut bus, &mut clock);
    let Some(Outcome::ScanRecord(a)) = o else {
        panic!("record A: {o:?}");
    };
    assert_eq!((a.channel, a.length, a.ssid()), (11, 500, SSID));
    assert_eq!(
        driver.record_bytes().len(),
        310,
        "the bytes present: the rest of the payload"
    );
    let (o, _) = outcome(&mut driver, &mut bus, &mut clock);
    assert_eq!(
        o,
        Some(Outcome::ScanRecord(RECORD_C)),
        "B lost behind the length"
    );
    let (o, _) = outcome(&mut driver, &mut bus, &mut clock);
    assert_eq!(
        o,
        Some(Outcome::ScanDone {
            end: ScanEnd::Complete,
            records: 2
        })
    );
    assert_eq!(bus.remaining(), 0);
}

#[test]
fn a_result_of_another_scan_is_a_plain_event() {
    let (mut script, mut c) = up_script();
    scan_requests(&mut script, &mut c, true, 1);
    scan_event(
        &mut script,
        &mut c,
        true,
        7,
        status::PARTIAL,
        &[record_a(), record_b()],
    );
    scan_event(&mut script, &mut c, true, 1, status::PARTIAL, &[record_c()]);
    scan_event(&mut script, &mut c, true, 1, status::SUCCESS, &[]);
    let (mut driver, mut bus, mut clock) = up_over(&script);
    assert!(driver.scan());
    let (o, _) = outcome(&mut driver, &mut bus, &mut clock);
    assert_eq!(
        o,
        Some(Outcome::Event(ev(number::ESCAN_RESULT, 8, 0, NONE, 322)))
    );
    assert_eq!(driver.event_payload().len(), 322);
    assert!(driver.record_bytes().is_empty(), "never walked");
    let (o, _) = outcome(&mut driver, &mut bus, &mut clock);
    assert_eq!(o, Some(Outcome::ScanRecord(RECORD_C)));
    let (o, _) = outcome(&mut driver, &mut bus, &mut clock);
    assert_eq!(
        o,
        Some(Outcome::ScanDone {
            end: ScanEnd::Complete,
            records: 1
        })
    );
    assert_eq!(bus.remaining(), 0);
}

#[test]
fn the_scan_start_refused_not_up_past_the_retries_names_the_regulatory_data() {
    let (mut script, mut c) = up_script();
    exchange(
        &mut script,
        &mut c,
        true,
        true,
        49,
        b"",
        &[0, 0, 0, 0],
        0,
        &[],
    );
    for _ in 0..6 {
        exchange(
            &mut script,
            &mut c,
            true,
            true,
            263,
            b"escan",
            &params(1),
            STATUS_NOT_UP,
            &[],
        );
    }
    let (mut driver, mut bus, mut clock) = up_over(&script);
    assert!(driver.scan());
    let (o, r) = outcome(&mut driver, &mut bus, &mut clock);
    assert_eq!(o, refused(STAGE_SCAN_REGULATORY, STATUS_NOT_UP));
    assert_eq!(
        r.deadlines(),
        &[531_036, 631_036, 731_036, 831_036, 931_036]
    );
    assert_eq!(driver.frames().sent, 18);
    assert!(driver.is_parked());
    assert_eq!(bus.remaining(), 0);
}

#[test]
fn a_scan_ended_by_another_status_carries_the_status() {
    let (mut script, mut c) = up_script();
    scan_requests(&mut script, &mut c, true, 1);
    scan_event(
        &mut script,
        &mut c,
        true,
        1,
        status::PARTIAL,
        &[record_a(), record_b()],
    );
    scan_event(&mut script, &mut c, true, 1, status::ABORT, &[record_c()]);
    let (mut driver, mut bus, mut clock) = up_over(&script);
    assert!(driver.scan());
    let mut got = Vec::new();
    for _ in 0..4 {
        got.push(outcome(&mut driver, &mut bus, &mut clock).0);
    }
    assert_eq!(
        got,
        std::vec![
            Some(Outcome::ScanRecord(RECORD_A)),
            Some(Outcome::ScanRecord(RECORD_B)),
            Some(Outcome::ScanRecord(RECORD_C)),
            Some(Outcome::ScanDone {
                end: ScanEnd::Ended(4),
                records: 3
            })
        ]
    );
    assert_eq!(bus.remaining(), 0);
    assert!(driver.is_ready());
}

#[test]
fn the_requests_are_refused_where_the_state_forbids_them() {
    let script = plane_script();
    let (mut driver, bus, _) = ready_over(&script);
    assert!(!driver.scan(), "before the radio up");
    assert!(!driver.join(SSID, Some(PASSPHRASE)), "before the radio up");
    assert!(!driver.disconnect(), "nothing held");
    assert!(
        !driver.subscribe(EventMask::EMPTY),
        "the pushed mask is in force"
    );
    assert_eq!(driver.link(), LinkState::Detached);
    assert_eq!(driver.address(), None);
    assert_eq!(bus.remaining(), 238, "no row consumed");

    let mut fresh: Driver<'static> = Driver::new();
    assert!(!fresh.up(), "not before ready");
    assert!(!fresh.scan());
    assert!(!fresh.join(SSID, None));
    assert!(!fresh.disconnect());

    let (mut driver, mut bus, mut clock) = up_over(&script);
    assert!(!driver.up(), "the radio is up");
    assert!(!driver.join(b"", Some(PASSPHRASE)), "an empty SSID");
    assert!(!driver.join(&[b'a'; 33], Some(PASSPHRASE)), "33 bytes");
    assert!(!driver.join(SSID, Some(&[b'p'; 7])), "a 7-byte passphrase");
    assert!(
        !driver.join(SSID, Some(&[b'p'; 65])),
        "a 65-byte passphrase"
    );
    assert_eq!(driver.link(), LinkState::Detached, "nothing recorded");
    assert!(driver.scan());
    assert!(!driver.scan(), "one request at a time");
    assert!(!driver.join(SSID, Some(PASSPHRASE)), "a scan in flight");
    for _ in 0..4 {
        outcome(&mut driver, &mut bus, &mut clock);
    }
    assert!(driver.join(SSID, Some(PASSPHRASE)));
    assert!(!driver.join(SSID, Some(PASSPHRASE)), "while joining");
    assert!(!driver.scan(), "while joining");
    for _ in 0..6 {
        outcome(&mut driver, &mut bus, &mut clock);
    }
    assert_eq!(driver.link(), LinkState::Up);
    assert!(!driver.join(SSID, Some(PASSPHRASE)), "while joined");
    assert!(!driver.up());
}

#[test]
fn the_joins_timeout_fails_the_attempt_and_the_machine_holds_the_network() {
    let (mut script, mut c) = up_script();
    join_requests(&mut script, &mut c, true, &secured_values());
    repeat(&mut script, 1500);
    let (mut driver, mut bus, mut clock) = up_over(&script);
    assert!(driver.join(SSID, Some(PASSPHRASE)));
    let (o, r) = outcome(&mut driver, &mut bus, &mut clock);
    assert_eq!(o, Some(Outcome::JoinFailed(JoinFailure::Timeout)));
    assert_eq!(
        r.deadlines().len(),
        1500,
        "fifteen seconds at ten milliseconds"
    );
    assert_eq!(r.deadlines()[1499], 15_431_036);
    assert_eq!(clock.now(), 15_431_036);
    assert_eq!(driver.link(), LinkState::Down);
    assert!(driver.is_ready());
    assert_eq!(bus.remaining(), 0);
}

#[test]
fn no_networks_fails_the_attempt_with_no_association_status() {
    let (mut script, mut c) = up_script();
    join_requests(&mut script, &mut c, true, &secured_values());
    event(
        &mut script,
        &mut c,
        true,
        number::SET_SSID,
        status::NO_NETWORKS,
        0,
        ADDRESS,
        &[],
    );
    let (mut driver, mut bus, mut clock) = up_over(&script);
    assert!(driver.join(SSID, Some(PASSPHRASE)));
    let (o, r) = outcome(&mut driver, &mut bus, &mut clock);
    assert_eq!(
        o,
        Some(Outcome::JoinFailed(JoinFailure::Association {
            status: 3,
            assoc_status: 0,
            auth_status: 0
        }))
    );
    assert_eq!(r.polls, 17);
    assert_eq!(driver.link(), LinkState::Down);
    assert_eq!(bus.remaining(), 0);
}

#[test]
fn a_no_ack_association_is_latched_into_the_failure() {
    let (mut script, mut c) = up_script();
    join_requests(&mut script, &mut c, true, &secured_values());
    event(&mut script, &mut c, true, number::AUTH, 0, 0, ADDRESS, &[]);
    event(
        &mut script,
        &mut c,
        true,
        number::ASSOC,
        status::NO_ACK,
        0,
        ADDRESS,
        &[],
    );
    event(
        &mut script,
        &mut c,
        true,
        number::SET_SSID,
        status::FAIL,
        0,
        ADDRESS,
        &[],
    );
    let (mut driver, mut bus, mut clock) = up_over(&script);
    assert!(driver.join(SSID, Some(PASSPHRASE)));
    assert_eq!(
        outcome(&mut driver, &mut bus, &mut clock).0,
        Some(Outcome::Event(ev(number::AUTH, 0, 0, ADDRESS, 0)))
    );
    assert_eq!(
        outcome(&mut driver, &mut bus, &mut clock).0,
        Some(Outcome::Event(ev(number::ASSOC, 5, 0, ADDRESS, 0)))
    );
    assert_eq!(
        outcome(&mut driver, &mut bus, &mut clock).0,
        Some(Outcome::JoinFailed(JoinFailure::Association {
            status: 1,
            assoc_status: 5,
            auth_status: 0
        }))
    );
    assert_eq!(driver.link(), LinkState::Down);
    assert_eq!(bus.remaining(), 0);
}

#[test]
fn the_supplicants_timeout_detaches_the_network_and_a_later_join_is_accepted() {
    let (mut script, mut c) = up_script();
    join_requests(&mut script, &mut c, true, &secured_values());
    event(&mut script, &mut c, true, number::AUTH, 0, 0, ADDRESS, &[]);
    event(
        &mut script,
        &mut c,
        true,
        number::ASSOC,
        0,
        0,
        ADDRESS,
        &IES,
    );
    event(
        &mut script,
        &mut c,
        true,
        number::SET_SSID,
        0,
        0,
        ADDRESS,
        &[],
    );
    event(
        &mut script,
        &mut c,
        true,
        number::PSK_SUP,
        status::UNSOLICITED,
        15,
        ADDRESS,
        &[],
    );
    let (mut driver, mut bus, mut clock) = up_over(&script);
    assert!(driver.join(SSID, Some(PASSPHRASE)));
    for _ in 0..3 {
        assert!(matches!(
            outcome(&mut driver, &mut bus, &mut clock).0,
            Some(Outcome::Event(_))
        ));
    }
    assert_eq!(driver.link(), LinkState::Joining, "associated, held");
    assert_eq!(
        outcome(&mut driver, &mut bus, &mut clock).0,
        Some(Outcome::JoinFailed(JoinFailure::Supplicant { reason: 15 }))
    );
    assert_eq!(driver.link(), LinkState::Detached, "not retried");
    assert!(
        driver.join(SSID, Some(PASSPHRASE)),
        "the caller joins again"
    );
    assert_eq!(bus.remaining(), 0);
}

#[test]
fn a_loss_during_the_attempt_fails_it_and_the_machine_holds_the_network() {
    let (mut script, mut c) = up_script();
    join_requests(&mut script, &mut c, true, &secured_values());
    event(&mut script, &mut c, true, number::AUTH, 0, 0, ADDRESS, &[]);
    event(
        &mut script,
        &mut c,
        true,
        number::DEAUTH_IND,
        0,
        2,
        ADDRESS,
        &[],
    );
    let (mut driver, mut bus, mut clock) = up_over(&script);
    assert!(driver.join(SSID, Some(PASSPHRASE)));
    assert!(matches!(
        outcome(&mut driver, &mut bus, &mut clock).0,
        Some(Outcome::Event(_))
    ));
    assert_eq!(
        outcome(&mut driver, &mut bus, &mut clock).0,
        Some(Outcome::JoinFailed(JoinFailure::Lost {
            number: 6,
            reason: 2
        }))
    );
    assert_eq!(driver.link(), LinkState::Down);
    assert_eq!(bus.remaining(), 0);
}

#[test]
fn a_recovery_scan_not_seeing_the_network_rests_and_the_next_cycle_joins() {
    let mut script = open_script(true, true);
    let mut c = Cursor::AFTER_OPENING;
    script.extend(up_ops(&mut c, true));
    script.extend(join_ops(&mut c, true));
    script.extend(burst_ops(&mut c, true));
    repeat(&mut script, 100);
    scan_requests(&mut script, &mut c, true, 1);
    scan_event(&mut script, &mut c, true, 1, status::PARTIAL, &[record_b()]);
    scan_event(&mut script, &mut c, true, 1, status::SUCCESS, &[]);
    repeat(&mut script, 500);
    scan_requests(&mut script, &mut c, true, 2);
    scan_event(&mut script, &mut c, true, 2, status::PARTIAL, &[record_a()]);
    scan_event(&mut script, &mut c, true, 2, status::SUCCESS, &[]);
    join_requests(&mut script, &mut c, true, &secured_values()[5..]);
    event(&mut script, &mut c, true, number::AUTH, 0, 0, ADDRESS, &[]);
    event(
        &mut script,
        &mut c,
        true,
        number::PSK_SUP,
        status::UNSOLICITED,
        0,
        ADDRESS,
        &[],
    );
    event_held_credit(
        &mut script,
        &mut c,
        true,
        number::SET_SSID,
        0,
        0,
        ADDRESS,
        &[],
    );
    empty(&mut script, 5);
    let (mut driver, mut bus, mut clock) = up_over(&script);
    assert!(driver.join(SSID, Some(PASSPHRASE)));
    for _ in 0..6 {
        outcome(&mut driver, &mut bus, &mut clock);
    }
    assert_eq!(driver.link(), LinkState::Up);
    for _ in 0..5 {
        outcome(&mut driver, &mut bus, &mut clock);
    }
    assert_eq!(driver.link(), LinkState::Down);
    let lost_at = clock.now();
    let (o, r) = outcome(&mut driver, &mut bus, &mut clock);
    assert_eq!(o, Some(Outcome::ScanRecord(RECORD_B)));
    assert_eq!(r.deadlines().len(), 100);
    let (o, _) = outcome(&mut driver, &mut bus, &mut clock);
    assert_eq!(
        o,
        Some(Outcome::ScanDone {
            end: ScanEnd::Complete,
            records: 1
        })
    );
    let (o, r) = outcome(&mut driver, &mut bus, &mut clock);
    assert_eq!(o, Some(Outcome::JoinFailed(JoinFailure::NotFound)));
    assert_eq!(r.polls, 1);
    assert_eq!(driver.link(), LinkState::Down);
    let (o, r) = outcome(&mut driver, &mut bus, &mut clock);
    assert_eq!(o, Some(Outcome::ScanRecord(RECORD_A)));
    assert_eq!(r.deadlines().len(), 500, "the five-second rest");
    assert_eq!(r.deadlines()[0], lost_at + 1_010_000);
    assert_eq!(r.deadlines()[499], lost_at + 6_000_000);
    assert_eq!(
        outcome(&mut driver, &mut bus, &mut clock).0,
        Some(Outcome::ScanDone {
            end: ScanEnd::Complete,
            records: 1
        })
    );
    assert_eq!(driver.link(), LinkState::Joining);
    for _ in 0..3 {
        assert!(matches!(
            outcome(&mut driver, &mut bus, &mut clock).0,
            Some(Outcome::Event(_))
        ));
    }
    assert_eq!(
        outcome(&mut driver, &mut bus, &mut clock).0,
        Some(Outcome::Joined { bssid: ADDRESS })
    );
    assert_eq!(driver.link(), LinkState::Up);
    assert_eq!(bus.remaining(), 0);
    assert_eq!(bus.fault(), None);
}

#[test]
fn an_open_join_completes_on_the_association() {
    let mut script = open_script(true, true);
    let mut c = Cursor::AFTER_OPENING;
    script.extend(up_ops(&mut c, true));
    script.extend(scan_ops(&mut c, true));
    join_requests(&mut script, &mut c, true, &open_values());
    event(&mut script, &mut c, true, number::AUTH, 0, 0, ADDRESS, &[]);
    event(
        &mut script,
        &mut c,
        true,
        number::ASSOC,
        0,
        0,
        ADDRESS,
        &IES,
    );
    event(
        &mut script,
        &mut c,
        true,
        number::SET_SSID,
        0,
        0,
        ADDRESS,
        &[],
    );
    let (mut driver, mut bus, mut clock) = up_over(&script);
    assert!(driver.scan());
    for _ in 0..4 {
        outcome(&mut driver, &mut bus, &mut clock);
    }
    assert!(driver.join(SSID_B, None));
    let (o, r) = outcome(&mut driver, &mut bus, &mut clock);
    assert_eq!(o, Some(Outcome::Event(ev(number::AUTH, 0, 0, ADDRESS, 0))));
    assert_eq!(r.polls, 15, "seven exchanges and the event");
    assert!(matches!(
        outcome(&mut driver, &mut bus, &mut clock).0,
        Some(Outcome::Event(_))
    ));
    assert_eq!(
        outcome(&mut driver, &mut bus, &mut clock).0,
        Some(Outcome::Joined { bssid: ADDRESS })
    );
    assert_eq!(driver.link(), LinkState::Up);
    assert_eq!(driver.frames().sent, 20);
    assert_eq!(bus.remaining(), 0);
    assert_eq!(bus.fault(), None);
}

#[test]
fn a_loss_during_a_scan_moves_the_machine_and_the_scan_ends_first() {
    let mut script = open_script(true, true);
    let mut c = Cursor::AFTER_OPENING;
    script.extend(up_ops(&mut c, true));
    script.extend(join_ops(&mut c, true));
    scan_requests(&mut script, &mut c, true, 1);
    event(
        &mut script,
        &mut c,
        true,
        number::DEAUTH_IND,
        0,
        2,
        ADDRESS,
        &[],
    );
    scan_event(&mut script, &mut c, true, 1, status::PARTIAL, &[record_a()]);
    scan_event(&mut script, &mut c, true, 1, status::SUCCESS, &[]);
    let (mut driver, mut bus, mut clock) = up_over(&script);
    assert!(driver.join(SSID, Some(PASSPHRASE)));
    for _ in 0..6 {
        outcome(&mut driver, &mut bus, &mut clock);
    }
    assert_eq!(driver.link(), LinkState::Up);
    assert!(driver.scan(), "a scan while up");
    let (o, _) = outcome(&mut driver, &mut bus, &mut clock);
    assert_eq!(
        o,
        Some(Outcome::LinkLost(ev(number::DEAUTH_IND, 0, 2, ADDRESS, 0)))
    );
    assert_eq!(driver.link(), LinkState::Down);
    assert_eq!(
        outcome(&mut driver, &mut bus, &mut clock).0,
        Some(Outcome::ScanRecord(RECORD_A))
    );
    assert_eq!(
        outcome(&mut driver, &mut bus, &mut clock).0,
        Some(Outcome::ScanDone {
            end: ScanEnd::Complete,
            records: 1
        })
    );
    assert_eq!(
        driver.link(),
        LinkState::Down,
        "the cycle waits for the rest"
    );
    assert_eq!(bus.remaining(), 0);
}

#[test]
fn a_disconnect_from_the_joins_wait_sends_the_disassociation_at_once() {
    let (mut script, mut c) = up_script();
    join_requests(&mut script, &mut c, true, &secured_values());
    exchange(&mut script, &mut c, true, true, 52, b"", &[0; 12], 0, &[]);
    event(
        &mut script,
        &mut c,
        true,
        number::PSK_SUP,
        status::UNSOLICITED,
        0,
        ADDRESS,
        &[],
    );
    let (mut driver, mut bus, mut clock) = up_over(&script);
    assert!(driver.join(SSID, Some(PASSPHRASE)));
    let r = run(&mut driver, &mut bus, &mut clock, 16);
    assert_eq!(r.end, End::Cap, "the eight exchanges done, the wait open");
    assert_eq!(driver.link(), LinkState::Joining);
    assert!(driver.disconnect());
    assert_eq!(driver.link(), LinkState::Detached);
    let (o, r) = outcome(&mut driver, &mut bus, &mut clock);
    assert_eq!(o, Some(Outcome::Disconnected));
    assert_eq!(r.polls, 2);
    let (o, _) = outcome(&mut driver, &mut bus, &mut clock);
    assert_eq!(
        o,
        Some(Outcome::Event(ev(number::PSK_SUP, 6, 0, ADDRESS, 0))),
        "the handshake's word after the drop is a plain event"
    );
    assert_eq!(driver.link(), LinkState::Detached);
    assert_eq!(bus.remaining(), 0);
    assert_eq!(bus.fault(), None);
}

#[test]
fn an_event_during_a_join_steps_exchange_is_handed_back_and_the_step_continues() {
    let (mut script, mut c) = up_script();
    let values = secured_values();
    join_requests(&mut script, &mut c, true, &values[..2]);
    send(
        &mut script,
        &mut c,
        true,
        values[2].0,
        values[2].1,
        &values[2].2,
    );
    event(&mut script, &mut c, true, number::LINK, 0, 0, ADDRESS, &[]);
    reply(&mut script, &mut c, true, values[2].0, 0, &[]);
    join_requests(&mut script, &mut c, true, &values[3..]);
    event(
        &mut script,
        &mut c,
        true,
        number::PSK_SUP,
        status::UNSOLICITED,
        0,
        ADDRESS,
        &[],
    );
    event_held_credit(
        &mut script,
        &mut c,
        true,
        number::SET_SSID,
        0,
        0,
        ADDRESS,
        &[],
    );
    empty(&mut script, 5);
    let (mut driver, mut bus, mut clock) = up_over(&script);
    assert!(driver.join(SSID, Some(PASSPHRASE)));
    let (o, r) = outcome(&mut driver, &mut bus, &mut clock);
    assert_eq!(o, Some(Outcome::Event(ev(number::LINK, 0, 0, ADDRESS, 0))));
    assert_eq!(
        r.polls, 6,
        "two exchanges, the third's send, its wait's first pass"
    );
    assert_eq!(driver.link(), LinkState::Joining);
    for _ in 0..2 {
        assert!(matches!(
            outcome(&mut driver, &mut bus, &mut clock).0,
            Some(Outcome::Event(_))
        ));
    }
    let (o, _) = outcome(&mut driver, &mut bus, &mut clock);
    assert_eq!(o, Some(Outcome::Joined { bssid: ADDRESS }));
    assert_eq!(bus.remaining(), 0);
    assert_eq!(bus.fault(), None);
}

#[test]
fn a_disconnect_during_a_join_steps_exchange_runs_the_reply_then_disconnects() {
    let (mut script, mut c) = up_script();
    let values = secured_values();
    join_requests(&mut script, &mut c, true, &values[..2]);
    send(
        &mut script,
        &mut c,
        true,
        values[2].0,
        values[2].1,
        &values[2].2,
    );
    reply(&mut script, &mut c, true, values[2].0, 0, &[]);
    exchange(&mut script, &mut c, true, true, 52, b"", &[0; 12], 0, &[]);
    let (mut driver, mut bus, mut clock) = up_over(&script);
    assert!(driver.join(SSID, Some(PASSPHRASE)));
    let r = run(&mut driver, &mut bus, &mut clock, 5);
    assert_eq!(
        r.end,
        End::Cap,
        "two exchanges done, the third's request sent, its reply awaited"
    );
    assert_eq!(driver.link(), LinkState::Joining);
    assert!(driver.disconnect());
    assert_eq!(driver.link(), LinkState::Detached, "dropped at once");
    let (o, r) = outcome(&mut driver, &mut bus, &mut clock);
    assert_eq!(o, Some(Outcome::Disconnected));
    assert_eq!(
        r.polls, 3,
        "the reply's pass, the disassociation's send, its reply's pass"
    );
    assert_eq!(
        driver.frames().dropped,
        0,
        "the reply was the step's, never a stranger"
    );
    assert_eq!(bus.remaining(), 0, "the disassociation went out");
    assert_eq!(bus.fault(), None);
    assert!(driver.is_ready());
    assert_eq!(driver.link(), LinkState::Detached);
    assert!(driver.scan(), "a scan is accepted after the disconnect");
}

#[test]
fn a_disconnect_during_a_join_steps_exchange_still_judges_the_reply() {
    let (mut script, mut c) = up_script();
    let values = secured_values();
    join_requests(&mut script, &mut c, true, &values[..2]);
    send(
        &mut script,
        &mut c,
        true,
        values[2].0,
        values[2].1,
        &values[2].2,
    );
    reply(&mut script, &mut c, true, values[2].0, 0xFFFF_FFFF, &[]);
    let (mut driver, mut bus, mut clock) = up_over(&script);
    assert!(driver.join(SSID, Some(PASSPHRASE)));
    let r = run(&mut driver, &mut bus, &mut clock, 5);
    assert_eq!(r.end, End::Cap);
    assert!(driver.disconnect());
    let (o, r) = outcome(&mut driver, &mut bus, &mut clock);
    assert_eq!(
        o,
        refused(STAGE_AUTH_MODE, 0xFFFF_FFFF),
        "the step's refusal parks the driver, as without the disconnect"
    );
    assert_eq!(r.polls, 1);
    assert!(driver.is_parked());
    assert_eq!(bus.remaining(), 0, "the disassociation never sent");
    assert_eq!(bus.fault(), None);
}

#[test]
fn a_records_length_near_the_word_limit_is_handed_out_once_and_the_walk_ends() {
    let (mut script, mut c) = up_script();
    scan_requests(&mut script, &mut c, true, 1);
    let mut long = record_a();
    long[4..8].copy_from_slice(&0xFFFF_FFF0u32.to_le_bytes());
    scan_event(
        &mut script,
        &mut c,
        true,
        1,
        status::PARTIAL,
        &[long, record_b()],
    );
    scan_event(&mut script, &mut c, true, 1, status::PARTIAL, &[record_c()]);
    scan_event(&mut script, &mut c, true, 1, status::SUCCESS, &[]);
    let (mut driver, mut bus, mut clock) = up_over(&script);
    assert!(driver.scan());
    let (o, _) = outcome(&mut driver, &mut bus, &mut clock);
    let Some(Outcome::ScanRecord(a)) = o else {
        panic!("record A: {o:?}");
    };
    assert_eq!((a.length, a.ssid(), a.channel), (0xFFFF_FFF0, SSID, 11));
    assert_eq!(driver.record_bytes().len(), 310, "the bytes present");
    let (o, r) = outcome(&mut driver, &mut bus, &mut clock);
    assert_eq!(
        o,
        Some(Outcome::ScanRecord(RECORD_C)),
        "handed out once; B lost behind the length; the walk ended"
    );
    assert_eq!(r.polls, 1);
    let (o, _) = outcome(&mut driver, &mut bus, &mut clock);
    assert_eq!(
        o,
        Some(Outcome::ScanDone {
            end: ScanEnd::Complete,
            records: 2
        })
    );
    assert_eq!(bus.remaining(), 0);
    assert_eq!(bus.fault(), None);
}

#[test]
fn the_driver_printed_after_the_passphrase_step_carries_no_byte_of_it() {
    let (mut script, mut c) = up_script();
    let values = secured_values();
    join_requests(&mut script, &mut c, true, &values[..6]);
    send(
        &mut script,
        &mut c,
        true,
        values[6].0,
        values[6].1,
        &values[6].2,
    );
    let (mut driver, mut bus, mut clock) = up_over(&script);
    assert!(driver.join(SSID, Some(PASSPHRASE)));
    let r = run(&mut driver, &mut bus, &mut clock, 13);
    assert_eq!(r.end, End::Cap, "six exchanges done, the key frame sent");
    assert_eq!(bus.remaining(), 0, "the key frame went out");
    assert_eq!(bus.fault(), None);
    let printed = std::format!("{driver:?}");
    assert!(printed.starts_with("Driver { state: Ready"), "{printed}");
    assert!(!printed.contains("frame:"), "no buffer field: {printed}");
    assert!(
        !printed.contains("115, 121, 110, 116, 104"),
        "no byte of the passphrase: {printed}"
    );
    assert!(!printed.contains("synthetic"), "{printed}");
    assert!(
        printed.contains("Passphrase { len: 20 }"),
        "the station's request prints the length alone: {printed}"
    );
    assert!(
        printed.ends_with(".. }"),
        "the omitted-fields mark: {printed}"
    );
    assert!(printed.len() < 3_000, "{}", printed.len());
}

#[test]
fn the_plane_fits_the_other_part_and_the_transport_fake_reports_no_fault() {
    let mut script: Vec<Op> = super::download::full_script(64, Part::Cyw4343w);
    script.extend(super::control::open_ops(true, true));
    script.extend(plane_ops(true, false));
    let mut bus = FakeTransport::new(&script, Part::Cyw4343w);
    let mut clock = FakeClock::new(0);
    let mut driver: Driver<'static> = Driver::new();
    assert!(driver.attach());
    assert!(matches!(
        run(&mut driver, &mut bus, &mut clock, CAP).outcome,
        Some(Outcome::Attached { .. })
    ));
    assert!(driver.upload(&super::download::FIRMWARE, &super::download::SETTINGS));
    assert!(matches!(
        run(&mut driver, &mut bus, &mut clock, CAP).outcome,
        Some(Outcome::Uploaded { .. })
    ));
    assert!(driver.open(&super::control::BLOB, Some(super::control::EXPECTED)));
    assert!(matches!(
        run(&mut driver, &mut bus, &mut clock, CAP).outcome,
        Some(Outcome::Ready { .. })
    ));
    drive_plane(&mut driver, &mut bus, &mut clock, 421_036);
    assert_eq!(bus.remaining(), 0);
    assert_eq!(bus.fault(), None);
}

#[test]
fn a_wpa3_join_replays_the_nine_requests_and_completes_at_the_holds_expiry_after_both_words() {
    let mut script = open_script(true, true);
    let mut c = Cursor::AFTER_OPENING;
    script.extend(up_ops(&mut c, true));
    script.extend(scan_ops(&mut c, true));
    let join = sae_join_ops(&mut c, true);
    assert_eq!(join.len(), 94);
    script.extend_from_slice(&join);
    let mut c = Cursor::AFTER_OPENING;
    let _ = up_ops(&mut c, false);
    let _ = scan_ops(&mut c, false);
    assert_eq!(sae_join_ops(&mut c, false).len(), 64);
    let (mut driver, mut bus, mut clock) = up_over(&script);
    assert!(driver.scan());
    for _ in 0..4 {
        outcome(&mut driver, &mut bus, &mut clock);
    }
    assert!(driver.join_with(SSID, Credential::SaePassword(PASSPHRASE)));
    assert_eq!(driver.link(), LinkState::Joining);
    let (o, r) = outcome(&mut driver, &mut bus, &mut clock);
    assert_eq!(
        o,
        Some(Outcome::Event(ev_auth(number::AUTH, 0, 0, 3, ADDRESS, 0)))
    );
    assert_eq!(r.polls, 19, "nine exchanges and the event");
    assert_eq!(
        outcome(&mut driver, &mut bus, &mut clock).0,
        Some(Outcome::Event(ev(number::ASSOC, 0, 0, ADDRESS, 8)))
    );
    assert_eq!(
        outcome(&mut driver, &mut bus, &mut clock).0,
        Some(Outcome::Event(ev(number::LINK, 0, 0, ADDRESS, 0)))
    );
    assert_eq!(
        outcome(&mut driver, &mut bus, &mut clock).0,
        Some(Outcome::Event(ev(number::PSK_SUP, 6, 0, ADDRESS, 0))),
        "the supplicant's word before the association success: a plain event, the link still joining"
    );
    assert_eq!(driver.link(), LinkState::Joining);
    assert_eq!(
        outcome(&mut driver, &mut bus, &mut clock).0,
        Some(Outcome::Event(ev(number::JOIN, 0, 0, ADDRESS, 0)))
    );
    assert_eq!(driver.link(), LinkState::Joining);
    assert_eq!(
        outcome(&mut driver, &mut bus, &mut clock).0,
        Some(Outcome::Event(ev(number::SET_SSID, 0, 0, ADDRESS, 0))),
        "the association word inside the hold: held"
    );
    assert_eq!(driver.link(), LinkState::Joining);
    let held_at = clock.now();
    let (o, r) = outcome(&mut driver, &mut bus, &mut clock);
    assert_eq!(
        o,
        Some(Outcome::Joined { bssid: ADDRESS }),
        "the hold's expiry completes"
    );
    assert_eq!(
        r.polls, 6,
        "five empty passes at the cadence, then the expiry"
    );
    assert_eq!(r.deadlines().len(), 5);
    assert_eq!(
        clock.now(),
        held_at + 50_000,
        "fifty milliseconds after the supplicant's word"
    );
    assert_eq!(driver.link(), LinkState::Up);
    assert_eq!(driver.frames(), frames(34, 22, 12));
    assert_eq!(driver.credit(), (22, 48));
    assert_eq!(bus.remaining(), 0);
    assert_eq!(bus.fault(), None);
}

#[test]
fn a_wpa2_credential_offered_to_a_wpa3_only_network_ends_detached_with_the_advertisement() {
    let (mut script, mut c) = up_script();
    join_requests(&mut script, &mut c, true, &secured_values());
    event(&mut script, &mut c, true, number::AUTH, 0, 0, ADDRESS, &[]);
    event(
        &mut script,
        &mut c,
        true,
        number::ASSOC,
        status::FAIL,
        0,
        ADDRESS,
        &[],
    );
    event(
        &mut script,
        &mut c,
        true,
        number::SET_SSID,
        status::FAIL,
        0,
        ADDRESS,
        &[],
    );
    repeat(&mut script, 500);
    scan_requests(&mut script, &mut c, true, 1);
    scan_event(&mut script, &mut c, true, 1, status::PARTIAL, &[record_d()]);
    scan_event(&mut script, &mut c, true, 1, status::SUCCESS, &[]);
    let (mut driver, mut bus, mut clock) = up_over(&script);
    assert!(driver.join(SSID, Some(PASSPHRASE)));
    assert_eq!(
        outcome(&mut driver, &mut bus, &mut clock).0,
        Some(Outcome::Event(ev(number::AUTH, 0, 0, ADDRESS, 0)))
    );
    assert_eq!(
        outcome(&mut driver, &mut bus, &mut clock).0,
        Some(Outcome::Event(ev(number::ASSOC, 1, 0, ADDRESS, 0)))
    );
    assert_eq!(
        outcome(&mut driver, &mut bus, &mut clock).0,
        Some(Outcome::JoinFailed(JoinFailure::Association {
            status: 1,
            assoc_status: 1,
            auth_status: 0
        }))
    );
    assert_eq!(driver.link(), LinkState::Down);
    let (o, r) = outcome(&mut driver, &mut bus, &mut clock);
    assert_eq!(o, Some(Outcome::ScanRecord(RECORD_D)));
    assert_eq!(r.deadlines().len(), 500, "the five-second rest");
    assert_eq!(
        outcome(&mut driver, &mut bus, &mut clock).0,
        Some(Outcome::ScanDone {
            end: ScanEnd::Complete,
            records: 1
        })
    );
    assert_eq!(
        driver.link(),
        LinkState::Down,
        "seen, advertising nothing the passphrase can use"
    );
    let (o, r) = outcome(&mut driver, &mut bus, &mut clock);
    assert_eq!(
        o,
        Some(Outcome::JoinFailed(JoinFailure::Mismatch {
            advertised: SECURITY_D
        }))
    );
    assert_eq!(r.polls, 1);
    assert_eq!(driver.link(), LinkState::Detached, "not retried");
    assert!(
        driver.join_with(SSID, Credential::SaePassword(PASSPHRASE)),
        "the caller joins again with the right kind"
    );
    assert_eq!(bus.remaining(), 0);
    assert_eq!(bus.fault(), None);
}

#[test]
fn a_wpa3_credential_offered_to_a_wpa2_only_network_ends_detached_with_the_authentication_status_latched()
 {
    let (mut script, mut c) = up_script();
    join_requests(&mut script, &mut c, true, &sae_values());
    event_auth(
        &mut script,
        &mut c,
        true,
        number::AUTH,
        status::TIMEOUT,
        0,
        3,
        ADDRESS,
        &[],
    );
    event(
        &mut script,
        &mut c,
        true,
        number::SET_SSID,
        status::FAIL,
        0,
        ADDRESS,
        &[],
    );
    repeat(&mut script, 500);
    scan_requests(&mut script, &mut c, true, 1);
    scan_event(&mut script, &mut c, true, 1, status::PARTIAL, &[record_f()]);
    scan_event(&mut script, &mut c, true, 1, status::SUCCESS, &[]);
    let (mut driver, mut bus, mut clock) = up_over(&script);
    assert!(driver.join_with(SSID, Credential::SaePassword(PASSPHRASE)));
    let (o, r) = outcome(&mut driver, &mut bus, &mut clock);
    assert_eq!(
        o,
        Some(Outcome::Event(ev_auth(number::AUTH, 2, 0, 3, ADDRESS, 0)))
    );
    assert_eq!(r.polls, 19);
    assert_eq!(
        outcome(&mut driver, &mut bus, &mut clock).0,
        Some(Outcome::JoinFailed(JoinFailure::Association {
            status: 1,
            assoc_status: 0,
            auth_status: 2
        }))
    );
    assert_eq!(driver.link(), LinkState::Down);
    assert_eq!(
        outcome(&mut driver, &mut bus, &mut clock).0,
        Some(Outcome::ScanRecord(RECORD_F))
    );
    assert_eq!(
        outcome(&mut driver, &mut bus, &mut clock).0,
        Some(Outcome::ScanDone {
            end: ScanEnd::Complete,
            records: 1
        })
    );
    assert_eq!(
        outcome(&mut driver, &mut bus, &mut clock).0,
        Some(Outcome::JoinFailed(JoinFailure::Mismatch {
            advertised: SECURITY_F
        }))
    );
    assert_eq!(driver.link(), LinkState::Detached);
    assert!(
        driver.join(SSID, Some(PASSPHRASE)),
        "the caller joins again with the right kind"
    );
    assert_eq!(bus.remaining(), 0);
    assert_eq!(bus.fault(), None);
}

#[test]
fn a_transition_network_re_enters_the_join_for_either_kind() {
    for sae in [false, true] {
        let (mut script, mut c) = up_script();
        let values = if sae { sae_values() } else { secured_values() };
        join_requests(&mut script, &mut c, true, &values);
        event(
            &mut script,
            &mut c,
            true,
            number::SET_SSID,
            status::NO_NETWORKS,
            0,
            ADDRESS,
            &[],
        );
        repeat(&mut script, 500);
        scan_requests(&mut script, &mut c, true, 1);
        scan_event(&mut script, &mut c, true, 1, status::PARTIAL, &[record_e()]);
        scan_event(&mut script, &mut c, true, 1, status::SUCCESS, &[]);
        let reentry = if sae { 6 } else { 5 };
        join_requests(&mut script, &mut c, true, &values[reentry..]);
        event(&mut script, &mut c, true, number::AUTH, 0, 0, ADDRESS, &[]);
        event(
            &mut script,
            &mut c,
            true,
            number::PSK_SUP,
            status::UNSOLICITED,
            0,
            ADDRESS,
            &[],
        );
        event(
            &mut script,
            &mut c,
            true,
            number::SET_SSID,
            0,
            0,
            ADDRESS,
            &[],
        );
        empty(&mut script, 5);
        let (mut driver, mut bus, mut clock) = up_over(&script);
        let credential = if sae {
            Credential::SaePassword(PASSPHRASE)
        } else {
            Credential::Passphrase(PASSPHRASE)
        };
        assert!(driver.join_with(SSID, credential));
        assert_eq!(
            outcome(&mut driver, &mut bus, &mut clock).0,
            Some(Outcome::JoinFailed(JoinFailure::Association {
                status: 3,
                assoc_status: 0,
                auth_status: 0
            }))
        );
        assert_eq!(
            outcome(&mut driver, &mut bus, &mut clock).0,
            Some(Outcome::ScanRecord(RECORD_E))
        );
        assert_eq!(
            outcome(&mut driver, &mut bus, &mut clock).0,
            Some(Outcome::ScanDone {
                end: ScanEnd::Complete,
                records: 1
            })
        );
        assert_eq!(
            driver.link(),
            LinkState::Joining,
            "the network seen advertising the kind: the re-entry (sae {sae})"
        );
        let (o, r) = outcome(&mut driver, &mut bus, &mut clock);
        assert_eq!(o, Some(Outcome::Event(ev(number::AUTH, 0, 0, ADDRESS, 0))));
        assert_eq!(
            r.polls, 7,
            "the re-entry's three exchanges and the event (sae {sae})"
        );
        assert_eq!(
            outcome(&mut driver, &mut bus, &mut clock).0,
            Some(Outcome::Event(ev(number::PSK_SUP, 6, 0, ADDRESS, 0))),
            "the re-entry's supplicant word before the association success: held (sae {sae})"
        );
        assert_eq!(driver.link(), LinkState::Joining);
        assert_eq!(
            outcome(&mut driver, &mut bus, &mut clock).0,
            Some(Outcome::Event(ev(number::SET_SSID, 0, 0, ADDRESS, 0))),
            "the re-entry's association word inside the hold: held (sae {sae})"
        );
        assert_eq!(driver.link(), LinkState::Joining);
        assert_eq!(
            outcome(&mut driver, &mut bus, &mut clock).0,
            Some(Outcome::Joined { bssid: ADDRESS })
        );
        assert_eq!(driver.link(), LinkState::Up);
        assert_eq!(bus.remaining(), 0, "sae {sae}");
        assert_eq!(bus.fault(), None);
    }
}

#[test]
fn an_open_credential_against_a_secured_network_and_a_passphrase_against_an_open_one_mismatch() {
    let (mut script, mut c) = up_script();
    join_requests(&mut script, &mut c, true, &open_values_for(SSID));
    event(
        &mut script,
        &mut c,
        true,
        number::SET_SSID,
        status::FAIL,
        0,
        ADDRESS,
        &[],
    );
    repeat(&mut script, 500);
    scan_requests(&mut script, &mut c, true, 1);
    scan_event(&mut script, &mut c, true, 1, status::PARTIAL, &[record_a()]);
    scan_event(&mut script, &mut c, true, 1, status::SUCCESS, &[]);
    let (mut driver, mut bus, mut clock) = up_over(&script);
    assert!(driver.join(SSID, None));
    assert_eq!(
        outcome(&mut driver, &mut bus, &mut clock).0,
        Some(Outcome::JoinFailed(JoinFailure::Association {
            status: 1,
            assoc_status: 0,
            auth_status: 0
        }))
    );
    assert_eq!(
        outcome(&mut driver, &mut bus, &mut clock).0,
        Some(Outcome::ScanRecord(RECORD_A))
    );
    assert!(matches!(
        outcome(&mut driver, &mut bus, &mut clock).0,
        Some(Outcome::ScanDone { .. })
    ));
    assert_eq!(
        outcome(&mut driver, &mut bus, &mut clock).0,
        Some(Outcome::JoinFailed(JoinFailure::Mismatch {
            advertised: RECORD_A.security
        })),
        "an open credential against a secured network"
    );
    assert_eq!(driver.link(), LinkState::Detached);
    assert_eq!(bus.remaining(), 0);
    assert_eq!(bus.fault(), None);

    let (mut script, mut c) = up_script();
    join_requests(&mut script, &mut c, true, &secured_values_for(SSID_B));
    event(
        &mut script,
        &mut c,
        true,
        number::SET_SSID,
        status::FAIL,
        0,
        ADDRESS,
        &[],
    );
    repeat(&mut script, 500);
    scan_requests(&mut script, &mut c, true, 1);
    scan_event(&mut script, &mut c, true, 1, status::PARTIAL, &[record_b()]);
    scan_event(&mut script, &mut c, true, 1, status::SUCCESS, &[]);
    let (mut driver, mut bus, mut clock) = up_over(&script);
    assert!(driver.join(SSID_B, Some(PASSPHRASE)));
    assert!(matches!(
        outcome(&mut driver, &mut bus, &mut clock).0,
        Some(Outcome::JoinFailed(_))
    ));
    assert_eq!(
        outcome(&mut driver, &mut bus, &mut clock).0,
        Some(Outcome::ScanRecord(RECORD_B))
    );
    assert!(matches!(
        outcome(&mut driver, &mut bus, &mut clock).0,
        Some(Outcome::ScanDone { .. })
    ));
    assert_eq!(
        outcome(&mut driver, &mut bus, &mut clock).0,
        Some(Outcome::JoinFailed(JoinFailure::Mismatch {
            advertised: RECORD_B.security
        })),
        "a passphrase against an open network"
    );
    assert_eq!(driver.link(), LinkState::Detached);
    assert_eq!(bus.remaining(), 0);
    assert_eq!(bus.fault(), None);
}

#[test]
fn the_protection_setting_is_returned_to_none_by_a_later_wpa2_join_on_the_same_attach() {
    let (mut script, mut c) = up_script();
    script.extend(sae_join_ops(&mut c, true));
    exchange(&mut script, &mut c, true, true, 52, b"", &[0; 12], 0, &[]);
    join_requests(&mut script, &mut c, true, &secured_values_with_reset());
    event(&mut script, &mut c, true, number::AUTH, 0, 0, ADDRESS, &[]);
    event(
        &mut script,
        &mut c,
        true,
        number::PSK_SUP,
        status::UNSOLICITED,
        0,
        ADDRESS,
        &[],
    );
    event_held_credit(
        &mut script,
        &mut c,
        true,
        number::SET_SSID,
        0,
        0,
        ADDRESS,
        &[],
    );
    empty(&mut script, 5);
    exchange(&mut script, &mut c, true, true, 52, b"", &[0; 12], 0, &[]);
    join_requests(&mut script, &mut c, true, &secured_values());
    event(&mut script, &mut c, true, number::AUTH, 0, 0, ADDRESS, &[]);
    event(
        &mut script,
        &mut c,
        true,
        number::PSK_SUP,
        status::UNSOLICITED,
        0,
        ADDRESS,
        &[],
    );
    event_held_credit(
        &mut script,
        &mut c,
        true,
        number::SET_SSID,
        0,
        0,
        ADDRESS,
        &[],
    );
    empty(&mut script, 5);
    let (mut driver, mut bus, mut clock) = up_over(&script);
    assert!(driver.join_with(SSID, Credential::SaePassword(PASSPHRASE)));
    for _ in 0..7 {
        outcome(&mut driver, &mut bus, &mut clock);
    }
    assert_eq!(driver.link(), LinkState::Up);
    assert!(driver.disconnect());
    assert_eq!(
        outcome(&mut driver, &mut bus, &mut clock).0,
        Some(Outcome::Disconnected)
    );
    assert!(driver.join(SSID, Some(PASSPHRASE)));
    let (o, r) = outcome(&mut driver, &mut bus, &mut clock);
    assert_eq!(o, Some(Outcome::Event(ev(number::AUTH, 0, 0, ADDRESS, 0))));
    assert_eq!(
        r.polls, 19,
        "nine exchanges: the protection returned to none among them"
    );
    for _ in 0..2 {
        assert!(matches!(
            outcome(&mut driver, &mut bus, &mut clock).0,
            Some(Outcome::Event(_))
        ));
    }
    assert_eq!(
        outcome(&mut driver, &mut bus, &mut clock).0,
        Some(Outcome::Joined { bssid: ADDRESS })
    );
    assert!(driver.disconnect());
    assert_eq!(
        outcome(&mut driver, &mut bus, &mut clock).0,
        Some(Outcome::Disconnected)
    );
    assert!(driver.join(SSID, Some(PASSPHRASE)));
    let (o, r) = outcome(&mut driver, &mut bus, &mut clock);
    assert_eq!(o, Some(Outcome::Event(ev(number::AUTH, 0, 0, ADDRESS, 0))));
    assert_eq!(r.polls, 17, "eight exchanges: nothing to return");
    for _ in 0..2 {
        assert!(matches!(
            outcome(&mut driver, &mut bus, &mut clock).0,
            Some(Outcome::Event(_))
        ));
    }
    assert_eq!(
        outcome(&mut driver, &mut bus, &mut clock).0,
        Some(Outcome::Joined { bssid: ADDRESS })
    );
    assert_eq!(bus.remaining(), 0);
    assert_eq!(bus.fault(), None);
}

#[test]
fn the_capability_query_reads_the_firmwares_words_and_guards_the_wpa3_join() {
    let mut script = open_script(true, true);
    let mut c = Cursor::AFTER_OPENING;
    let cap = cap_ops(&mut c, true, CAP_STRING, 0);
    assert_eq!(cap.len(), 6);
    script.extend_from_slice(&cap);
    script.extend(up_ops(&mut c, true));
    let (mut driver, mut bus, mut clock) = ready_over(&script);
    assert!(driver.capabilities());
    assert!(!driver.capabilities(), "one request at a time");
    let (o, r) = outcome(&mut driver, &mut bus, &mut clock);
    assert_eq!(
        o,
        Some(Outcome::Capabilities(Capabilities {
            answered: true,
            status: 0,
            len: 35,
            sae: true,
            mfp: true
        }))
    );
    assert_eq!(r.polls, 2);
    assert_eq!(driver.capability_string(), CAP_STRING);
    assert_eq!(driver.frames().sent, 10);
    assert!(driver.up());
    assert_eq!(
        outcome(&mut driver, &mut bus, &mut clock).0,
        Some(Outcome::Up { address: STATION })
    );
    assert!(
        driver.capability_string().is_empty(),
        "readable until the next poll"
    );
    assert!(
        driver.join_with(SSID, Credential::SaePassword(PASSPHRASE)),
        "the firmware said sae"
    );
    assert_eq!(bus.remaining(), 0);
    assert_eq!(bus.fault(), None);

    for string in [&b"ap sta wme"[..], &b"ap sae_ext mfp"[..]] {
        let mut script = open_script(true, true);
        let mut c = Cursor::AFTER_OPENING;
        script.extend(cap_ops(&mut c, true, string, 0));
        script.extend(up_ops(&mut c, true));
        let (mut driver, mut bus, mut clock) = ready_over(&script);
        assert!(driver.capabilities());
        let (o, _) = outcome(&mut driver, &mut bus, &mut clock);
        let Some(Outcome::Capabilities(capabilities)) = o else {
            panic!("{o:?}");
        };
        assert!(capabilities.answered && !capabilities.sae, "{string:?}");
        assert_eq!(capabilities.mfp, string.ends_with(b"mfp"), "{string:?}");
        assert_eq!(capabilities.len as usize, string.len());
        assert_eq!(driver.capability_string(), string);
        assert!(driver.up());
        outcome(&mut driver, &mut bus, &mut clock);
        assert!(
            !driver.join_with(SSID, Credential::SaePassword(PASSPHRASE)),
            "refused: the firmware named no SAE offload ({string:?})"
        );
        assert_eq!(driver.link(), LinkState::Detached);
        assert!(
            driver.join(SSID, Some(PASSPHRASE)),
            "a WPA2 join is not the firmware's word's business"
        );
        assert_eq!(bus.remaining(), 0);
    }

    let mut script = open_script(true, true);
    let mut c = Cursor::AFTER_OPENING;
    script.extend(cap_ops(&mut c, true, &[], 0xFFFF_FFE9));
    script.extend(up_ops(&mut c, true));
    let (mut driver, mut bus, mut clock) = ready_over(&script);
    assert!(driver.capabilities());
    assert_eq!(
        outcome(&mut driver, &mut bus, &mut clock).0,
        Some(Outcome::Capabilities(Capabilities {
            answered: false,
            status: 0xFFFF_FFE9,
            len: 0,
            sae: false,
            mfp: false
        })),
        "an answer, not a refusal"
    );
    assert!(driver.is_ready(), "not parked");
    assert!(driver.capability_string().is_empty());
    assert!(driver.up());
    outcome(&mut driver, &mut bus, &mut clock);
    assert!(
        driver.join_with(SSID, Credential::SaePassword(PASSPHRASE)),
        "unknown: the firmware decides"
    );
    assert_eq!(bus.remaining(), 0);
    assert_eq!(bus.fault(), None);
}

#[test]
fn the_wpa3_request_guards() {
    let script = plane_script();
    let (mut driver, bus, _) = up_over(&script);
    assert!(
        !driver.join_with(SSID, Credential::SaePassword(&[])),
        "an empty password"
    );
    assert!(
        !driver.join_with(SSID, Credential::SaePassword(&[b'p'; 129])),
        "129 bytes"
    );
    assert!(
        !driver.join_with(b"", Credential::SaePassword(PASSPHRASE)),
        "an empty SSID"
    );
    assert_eq!(driver.link(), LinkState::Detached, "nothing recorded");
    assert!(
        driver.join_with(SSID, Credential::SaePassword(&[b'p'; 128])),
        "128 bytes"
    );
    assert_eq!(driver.link(), LinkState::Joining);
    assert!(!driver.capabilities(), "a request in flight");
    assert!(
        !driver.join_with(SSID, Credential::SaePassword(PASSPHRASE)),
        "while joining"
    );
    assert_eq!(bus.remaining(), 238 - 19, "no row consumed by the guards");
    let mut fresh: Driver<'static> = Driver::new();
    assert!(!fresh.capabilities(), "not before ready");
    assert!(!fresh.join_with(SSID, Credential::SaePassword(PASSPHRASE)));
    assert!(fresh.capability_string().is_empty());
}

#[test]
fn a_wpa3_join_whose_association_success_comes_first_completes_on_the_supplicants_word() {
    let (mut script, mut c) = up_script();
    join_requests(&mut script, &mut c, true, &sae_values());
    event_auth(
        &mut script,
        &mut c,
        true,
        number::AUTH,
        0,
        0,
        3,
        ADDRESS,
        &[],
    );
    event(
        &mut script,
        &mut c,
        true,
        number::ASSOC,
        0,
        0,
        ADDRESS,
        &IES,
    );
    event(
        &mut script,
        &mut c,
        true,
        number::SET_SSID,
        0,
        0,
        ADDRESS,
        &[],
    );
    event(
        &mut script,
        &mut c,
        true,
        number::PSK_SUP,
        status::UNSOLICITED,
        0,
        ADDRESS,
        &[],
    );
    empty(&mut script, 5);
    let (mut driver, mut bus, mut clock) = up_over(&script);
    assert!(driver.join_with(SSID, Credential::SaePassword(PASSPHRASE)));
    let (o, r) = outcome(&mut driver, &mut bus, &mut clock);
    assert_eq!(
        o,
        Some(Outcome::Event(ev_auth(number::AUTH, 0, 0, 3, ADDRESS, 0)))
    );
    assert_eq!(r.polls, 19);
    assert_eq!(
        outcome(&mut driver, &mut bus, &mut clock).0,
        Some(Outcome::Event(ev(number::ASSOC, 0, 0, ADDRESS, 8)))
    );
    assert_eq!(
        outcome(&mut driver, &mut bus, &mut clock).0,
        Some(Outcome::Event(ev(number::SET_SSID, 0, 0, ADDRESS, 0))),
        "the association success before the supplicant's word: held"
    );
    assert_eq!(
        driver.link(),
        LinkState::Joining,
        "associated, not up: the handshake still owed"
    );
    let (o, r) = outcome(&mut driver, &mut bus, &mut clock);
    assert_eq!(
        o,
        Some(Outcome::Event(ev(number::PSK_SUP, 6, 0, ADDRESS, 0))),
        "the supplicant's word starts the hold"
    );
    assert_eq!(r.polls, 1);
    assert_eq!(driver.link(), LinkState::Joining);
    let held_at = clock.now();
    let (o, r) = outcome(&mut driver, &mut bus, &mut clock);
    assert_eq!(
        o,
        Some(Outcome::Joined { bssid: ADDRESS }),
        "the hold's expiry completes"
    );
    assert_eq!(r.polls, 6);
    assert_eq!(
        clock.now(),
        held_at + 50_000,
        "fifty milliseconds after the supplicant's word"
    );
    assert_eq!(driver.link(), LinkState::Up);
    assert_eq!(bus.remaining(), 0);
    assert_eq!(bus.fault(), None);
}

#[test]
fn a_wpa3_join_whose_association_fails_after_the_supplicants_word_fails_with_the_statuses() {
    let (mut script, mut c) = up_script();
    join_requests(&mut script, &mut c, true, &sae_values());
    event_auth(
        &mut script,
        &mut c,
        true,
        number::AUTH,
        0,
        0,
        3,
        ADDRESS,
        &[],
    );
    event(
        &mut script,
        &mut c,
        true,
        number::ASSOC,
        0,
        0,
        ADDRESS,
        &IES,
    );
    event(
        &mut script,
        &mut c,
        true,
        number::PSK_SUP,
        status::UNSOLICITED,
        0,
        ADDRESS,
        &[],
    );
    event(
        &mut script,
        &mut c,
        true,
        number::SET_SSID,
        status::FAIL,
        0,
        ADDRESS,
        &[],
    );
    let (mut driver, mut bus, mut clock) = up_over(&script);
    assert!(driver.join_with(SSID, Credential::SaePassword(PASSPHRASE)));
    for _ in 0..2 {
        assert!(matches!(
            outcome(&mut driver, &mut bus, &mut clock).0,
            Some(Outcome::Event(_))
        ));
    }
    assert_eq!(
        outcome(&mut driver, &mut bus, &mut clock).0,
        Some(Outcome::Event(ev(number::PSK_SUP, 6, 0, ADDRESS, 0))),
        "the supplicant's word alone does not complete a WPA3 join"
    );
    assert_eq!(driver.link(), LinkState::Joining);
    assert_eq!(
        outcome(&mut driver, &mut bus, &mut clock).0,
        Some(Outcome::JoinFailed(JoinFailure::Association {
            status: 1,
            assoc_status: 0,
            auth_status: 0
        })),
        "the firmware's association failure after the supplicant's word fails the attempt"
    );
    assert_eq!(
        driver.link(),
        LinkState::Down,
        "the network held for the next cycle"
    );
    assert!(driver.is_ready());
    assert_eq!(bus.remaining(), 0);
    assert_eq!(bus.fault(), None);
}

/// Three synthetic addresses for the events of one attempt, so the
/// reported address says which event it came from.
const AP_ASSOC: [u8; 6] = [0x02, 0x11, 0x11, 0x11, 0x11, 0x11];
const AP_SET_SSID: [u8; 6] = [0x02, 0x22, 0x22, 0x22, 0x22, 0x22];
const AP_SUPPLICANT: [u8; 6] = [0x02, 0x33, 0x33, 0x33, 0x33, 0x33];

#[test]
fn a_wpa3_join_completed_on_the_supplicants_word_reports_the_association_words_address() {
    let (mut script, mut c) = up_script();
    join_requests(&mut script, &mut c, true, &sae_values());
    event_auth(
        &mut script,
        &mut c,
        true,
        number::AUTH,
        0,
        0,
        3,
        ADDRESS,
        &[],
    );
    event(
        &mut script,
        &mut c,
        true,
        number::ASSOC,
        0,
        0,
        AP_ASSOC,
        &IES,
    );
    event(
        &mut script,
        &mut c,
        true,
        number::SET_SSID,
        0,
        0,
        AP_SET_SSID,
        &[],
    );
    event(
        &mut script,
        &mut c,
        true,
        number::PSK_SUP,
        status::UNSOLICITED,
        0,
        AP_SUPPLICANT,
        &[],
    );
    empty(&mut script, 5);
    let (mut driver, mut bus, mut clock) = up_over(&script);
    assert!(driver.join_with(SSID, Credential::SaePassword(PASSPHRASE)));
    for _ in 0..4 {
        assert!(matches!(
            outcome(&mut driver, &mut bus, &mut clock).0,
            Some(Outcome::Event(_))
        ));
    }
    assert_eq!(
        outcome(&mut driver, &mut bus, &mut clock).0,
        Some(Outcome::Joined { bssid: AP_SET_SSID }),
        "the association word's address, never the supplicant event's"
    );
    assert_eq!(driver.link(), LinkState::Up);
    assert_eq!(bus.remaining(), 0);
    assert_eq!(bus.fault(), None);
}

#[test]
fn a_wpa2_join_reports_the_association_words_address_whichever_order_the_words_come() {
    // The association word after the supplicant's word.
    let (mut script, mut c) = up_script();
    join_requests(&mut script, &mut c, true, &secured_values());
    event(&mut script, &mut c, true, number::AUTH, 0, 0, ADDRESS, &[]);
    event(
        &mut script,
        &mut c,
        true,
        number::ASSOC,
        0,
        0,
        AP_ASSOC,
        &IES,
    );
    event(
        &mut script,
        &mut c,
        true,
        number::PSK_SUP,
        status::UNSOLICITED,
        0,
        AP_SUPPLICANT,
        &[],
    );
    event(
        &mut script,
        &mut c,
        true,
        number::SET_SSID,
        0,
        0,
        AP_SET_SSID,
        &[],
    );
    empty(&mut script, 5);
    let (mut driver, mut bus, mut clock) = up_over(&script);
    assert!(driver.join(SSID, Some(PASSPHRASE)));
    for _ in 0..4 {
        assert!(matches!(
            outcome(&mut driver, &mut bus, &mut clock).0,
            Some(Outcome::Event(_))
        ));
    }
    assert_eq!(
        outcome(&mut driver, &mut bus, &mut clock).0,
        Some(Outcome::Joined { bssid: AP_SET_SSID }),
        "the association word's address, never the supplicant event's nor the association event's"
    );
    assert_eq!(bus.remaining(), 0);
    assert_eq!(bus.fault(), None);

    // The association word before the supplicant's word.
    let (mut script, mut c) = up_script();
    join_requests(&mut script, &mut c, true, &secured_values());
    event(
        &mut script,
        &mut c,
        true,
        number::ASSOC,
        0,
        0,
        AP_ASSOC,
        &IES,
    );
    event(
        &mut script,
        &mut c,
        true,
        number::SET_SSID,
        0,
        0,
        AP_SET_SSID,
        &[],
    );
    event(
        &mut script,
        &mut c,
        true,
        number::PSK_SUP,
        status::UNSOLICITED,
        0,
        AP_SUPPLICANT,
        &[],
    );
    empty(&mut script, 5);
    let (mut driver, mut bus, mut clock) = up_over(&script);
    assert!(driver.join(SSID, Some(PASSPHRASE)));
    for _ in 0..3 {
        assert!(matches!(
            outcome(&mut driver, &mut bus, &mut clock).0,
            Some(Outcome::Event(_))
        ));
    }
    assert_eq!(
        outcome(&mut driver, &mut bus, &mut clock).0,
        Some(Outcome::Joined { bssid: AP_SET_SSID }),
        "the association word's address over the association event's"
    );
    assert_eq!(bus.remaining(), 0);
    assert_eq!(bus.fault(), None);
}

#[test]
fn a_wpa3_join_on_the_common_order_completes_at_the_holds_expiry_or_on_a_later_association_word() {
    // The association word 20 ms after the supplicant's word: held, the
    // join at the hold's expiry, 50 ms after the supplicant's word.
    let (mut script, mut c) = up_script();
    join_requests(&mut script, &mut c, true, &sae_values());
    event_auth(
        &mut script,
        &mut c,
        true,
        number::AUTH,
        0,
        0,
        3,
        ADDRESS,
        &[],
    );
    event(
        &mut script,
        &mut c,
        true,
        number::ASSOC,
        0,
        0,
        ADDRESS,
        &IES,
    );
    event(
        &mut script,
        &mut c,
        true,
        number::PSK_SUP,
        status::UNSOLICITED,
        0,
        ADDRESS,
        &[],
    );
    empty(&mut script, 2);
    event(
        &mut script,
        &mut c,
        true,
        number::SET_SSID,
        0,
        0,
        ADDRESS,
        &[],
    );
    empty(&mut script, 3);
    let (mut driver, mut bus, mut clock) = up_over(&script);
    assert!(driver.join_with(SSID, Credential::SaePassword(PASSPHRASE)));
    for _ in 0..2 {
        assert!(matches!(
            outcome(&mut driver, &mut bus, &mut clock).0,
            Some(Outcome::Event(_))
        ));
    }
    assert_eq!(
        outcome(&mut driver, &mut bus, &mut clock).0,
        Some(Outcome::Event(ev(number::PSK_SUP, 6, 0, ADDRESS, 0)))
    );
    let supplicant_at = clock.now();
    let (o, r) = outcome(&mut driver, &mut bus, &mut clock);
    assert_eq!(
        o,
        Some(Outcome::Event(ev(number::SET_SSID, 0, 0, ADDRESS, 0))),
        "the association word at 20 ms: held"
    );
    assert_eq!(r.polls, 3);
    assert_eq!(clock.now(), supplicant_at + 20_000);
    assert_eq!(driver.link(), LinkState::Joining);
    let (o, r) = outcome(&mut driver, &mut bus, &mut clock);
    assert_eq!(
        o,
        Some(Outcome::Joined { bssid: ADDRESS }),
        "the hold's expiry completes"
    );
    assert_eq!(r.polls, 4);
    assert_eq!(clock.now(), supplicant_at + 50_000);
    assert_eq!(driver.link(), LinkState::Up);
    assert_eq!(bus.remaining(), 0);
    assert_eq!(bus.fault(), None);

    // The association word 60 ms after the supplicant's word: the hold has
    // passed, the join completes on the word at once.
    let (mut script, mut c) = up_script();
    join_requests(&mut script, &mut c, true, &sae_values());
    event_auth(
        &mut script,
        &mut c,
        true,
        number::AUTH,
        0,
        0,
        3,
        ADDRESS,
        &[],
    );
    event(
        &mut script,
        &mut c,
        true,
        number::ASSOC,
        0,
        0,
        ADDRESS,
        &IES,
    );
    event(
        &mut script,
        &mut c,
        true,
        number::PSK_SUP,
        status::UNSOLICITED,
        0,
        ADDRESS,
        &[],
    );
    empty(&mut script, 6);
    event(
        &mut script,
        &mut c,
        true,
        number::SET_SSID,
        0,
        0,
        ADDRESS,
        &[],
    );
    let (mut driver, mut bus, mut clock) = up_over(&script);
    assert!(driver.join_with(SSID, Credential::SaePassword(PASSPHRASE)));
    for _ in 0..3 {
        assert!(matches!(
            outcome(&mut driver, &mut bus, &mut clock).0,
            Some(Outcome::Event(_))
        ));
    }
    let supplicant_at = clock.now();
    let (o, r) = outcome(&mut driver, &mut bus, &mut clock);
    assert_eq!(
        o,
        Some(Outcome::Joined { bssid: ADDRESS }),
        "the association word after the hold completes at once"
    );
    assert_eq!(r.polls, 7, "six empty passes, then the word");
    assert_eq!(clock.now(), supplicant_at + 60_000);
    assert_eq!(driver.link(), LinkState::Up);
    assert_eq!(bus.remaining(), 0);
    assert_eq!(bus.fault(), None);
}

#[test]
fn a_wpa3_join_on_the_rare_order_completes_fifty_milliseconds_after_the_supplicants_word_and_not_before()
 {
    let (mut script, mut c) = up_script();
    join_requests(&mut script, &mut c, true, &sae_values());
    event_auth(
        &mut script,
        &mut c,
        true,
        number::AUTH,
        0,
        0,
        3,
        ADDRESS,
        &[],
    );
    event(
        &mut script,
        &mut c,
        true,
        number::ASSOC,
        0,
        0,
        ADDRESS,
        &IES,
    );
    event(
        &mut script,
        &mut c,
        true,
        number::SET_SSID,
        0,
        0,
        ADDRESS,
        &[],
    );
    event(
        &mut script,
        &mut c,
        true,
        number::PSK_SUP,
        status::UNSOLICITED,
        0,
        ADDRESS,
        &[],
    );
    empty(&mut script, 5);
    let (mut driver, mut bus, mut clock) = up_over(&script);
    assert!(driver.join_with(SSID, Credential::SaePassword(PASSPHRASE)));
    for _ in 0..3 {
        assert!(matches!(
            outcome(&mut driver, &mut bus, &mut clock).0,
            Some(Outcome::Event(_))
        ));
    }
    assert_eq!(
        outcome(&mut driver, &mut bus, &mut clock).0,
        Some(Outcome::Event(ev(number::PSK_SUP, 6, 0, ADDRESS, 0)))
    );
    let supplicant_at = clock.now();
    let r = run(&mut driver, &mut bus, &mut clock, 3);
    assert_eq!(r.end, End::Cap, "three polls inside the hold: no outcome");
    assert_eq!(
        driver.link(),
        LinkState::Joining,
        "not before the hold's expiry"
    );
    assert_eq!(
        clock.now(),
        supplicant_at + 30_000,
        "three polls: the clock at the third deadline"
    );
    let (o, _) = outcome(&mut driver, &mut bus, &mut clock);
    assert_eq!(
        o,
        Some(Outcome::Joined { bssid: ADDRESS }),
        "the hold's expiry completes"
    );
    assert_eq!(
        clock.now(),
        supplicant_at + 50_000,
        "fifty milliseconds after the supplicant's word"
    );
    assert_eq!(driver.link(), LinkState::Up);
    assert_eq!(bus.remaining(), 0);
    assert_eq!(bus.fault(), None);
}

#[test]
fn a_wpa2_join_completes_at_the_holds_expiry_or_on_a_later_association_word() {
    // The association word 20 ms after the supplicant's word: held, the
    // join at the hold's expiry, 50 ms after the supplicant's word.
    let (mut script, mut c) = up_script();
    join_requests(&mut script, &mut c, true, &secured_values());
    event(&mut script, &mut c, true, number::AUTH, 0, 0, ADDRESS, &[]);
    event(
        &mut script,
        &mut c,
        true,
        number::ASSOC,
        0,
        0,
        ADDRESS,
        &IES,
    );
    event(
        &mut script,
        &mut c,
        true,
        number::PSK_SUP,
        status::UNSOLICITED,
        0,
        ADDRESS,
        &[],
    );
    empty(&mut script, 2);
    event(
        &mut script,
        &mut c,
        true,
        number::SET_SSID,
        0,
        0,
        ADDRESS,
        &[],
    );
    empty(&mut script, 3);
    let (mut driver, mut bus, mut clock) = up_over(&script);
    assert!(driver.join(SSID, Some(PASSPHRASE)));
    for _ in 0..2 {
        assert!(matches!(
            outcome(&mut driver, &mut bus, &mut clock).0,
            Some(Outcome::Event(_))
        ));
    }
    assert_eq!(
        outcome(&mut driver, &mut bus, &mut clock).0,
        Some(Outcome::Event(ev(number::PSK_SUP, 6, 0, ADDRESS, 0))),
        "the supplicant's word starts the hold"
    );
    let supplicant_at = clock.now();
    let (o, r) = outcome(&mut driver, &mut bus, &mut clock);
    assert_eq!(
        o,
        Some(Outcome::Event(ev(number::SET_SSID, 0, 0, ADDRESS, 0))),
        "the association word at 20 ms: held"
    );
    assert_eq!(r.polls, 3);
    assert_eq!(clock.now(), supplicant_at + 20_000);
    assert_eq!(driver.link(), LinkState::Joining);
    let (o, r) = outcome(&mut driver, &mut bus, &mut clock);
    assert_eq!(
        o,
        Some(Outcome::Joined { bssid: ADDRESS }),
        "the hold's expiry completes"
    );
    assert_eq!(r.polls, 4);
    assert_eq!(clock.now(), supplicant_at + 50_000);
    assert_eq!(driver.link(), LinkState::Up);
    assert_eq!(bus.remaining(), 0);
    assert_eq!(bus.fault(), None);

    // The association word 60 ms after the supplicant's word: the hold has
    // passed, the join completes on the word at once.
    let (mut script, mut c) = up_script();
    join_requests(&mut script, &mut c, true, &secured_values());
    event(&mut script, &mut c, true, number::AUTH, 0, 0, ADDRESS, &[]);
    event(
        &mut script,
        &mut c,
        true,
        number::ASSOC,
        0,
        0,
        ADDRESS,
        &IES,
    );
    event(
        &mut script,
        &mut c,
        true,
        number::PSK_SUP,
        status::UNSOLICITED,
        0,
        ADDRESS,
        &[],
    );
    empty(&mut script, 6);
    event(
        &mut script,
        &mut c,
        true,
        number::SET_SSID,
        0,
        0,
        ADDRESS,
        &[],
    );
    let (mut driver, mut bus, mut clock) = up_over(&script);
    assert!(driver.join(SSID, Some(PASSPHRASE)));
    for _ in 0..3 {
        assert!(matches!(
            outcome(&mut driver, &mut bus, &mut clock).0,
            Some(Outcome::Event(_))
        ));
    }
    let supplicant_at = clock.now();
    let (o, r) = outcome(&mut driver, &mut bus, &mut clock);
    assert_eq!(
        o,
        Some(Outcome::Joined { bssid: ADDRESS }),
        "the association word after the hold completes at once"
    );
    assert_eq!(r.polls, 7, "six empty passes, then the word");
    assert_eq!(clock.now(), supplicant_at + 60_000);
    assert_eq!(driver.link(), LinkState::Up);
    assert_eq!(bus.remaining(), 0);
    assert_eq!(bus.fault(), None);
}

#[test]
fn a_wpa2_join_on_the_rare_order_completes_fifty_milliseconds_after_the_supplicants_word_and_not_before()
 {
    let (mut script, mut c) = up_script();
    join_requests(&mut script, &mut c, true, &secured_values());
    event(&mut script, &mut c, true, number::AUTH, 0, 0, ADDRESS, &[]);
    event(
        &mut script,
        &mut c,
        true,
        number::ASSOC,
        0,
        0,
        ADDRESS,
        &IES,
    );
    event(
        &mut script,
        &mut c,
        true,
        number::SET_SSID,
        0,
        0,
        ADDRESS,
        &[],
    );
    event(
        &mut script,
        &mut c,
        true,
        number::PSK_SUP,
        status::UNSOLICITED,
        0,
        ADDRESS,
        &[],
    );
    empty(&mut script, 5);
    let (mut driver, mut bus, mut clock) = up_over(&script);
    assert!(driver.join(SSID, Some(PASSPHRASE)));
    for _ in 0..3 {
        assert!(matches!(
            outcome(&mut driver, &mut bus, &mut clock).0,
            Some(Outcome::Event(_))
        ));
    }
    assert_eq!(
        outcome(&mut driver, &mut bus, &mut clock).0,
        Some(Outcome::Event(ev(number::PSK_SUP, 6, 0, ADDRESS, 0)))
    );
    let supplicant_at = clock.now();
    let r = run(&mut driver, &mut bus, &mut clock, 3);
    assert_eq!(r.end, End::Cap, "three polls inside the hold: no outcome");
    assert_eq!(
        driver.link(),
        LinkState::Joining,
        "not before the hold's expiry"
    );
    assert_eq!(
        clock.now(),
        supplicant_at + 30_000,
        "three polls: the clock at the third deadline"
    );
    let (o, _) = outcome(&mut driver, &mut bus, &mut clock);
    assert_eq!(
        o,
        Some(Outcome::Joined { bssid: ADDRESS }),
        "the hold's expiry completes"
    );
    assert_eq!(
        clock.now(),
        supplicant_at + 50_000,
        "fifty milliseconds after the supplicant's word"
    );
    assert_eq!(driver.link(), LinkState::Up);
    assert_eq!(bus.remaining(), 0);
    assert_eq!(bus.fault(), None);
}
