//! The controller against the scripted radio, run as the driver runs: each outcome is fed after the
//! request it answers, so the order of the controller's questions is part of what is tested.

use super::*;
use crate::controller::{Controller, CredentialSlot, RadioState};
use crate::record::{RecordStore, PAGE, SLOTS};
use lamella_net_core::wifi::{
    security, BootConnection, CredentialSource, JoinRequest, JoinStatus, Reconnection, RecordWrite,
    WifiControl, WifiLink,
};
use lamella_wifi_cyw4343x::{Capabilities, ScanEnd, ScanRecord};
use std::boxed::Box;
use std::format;

/// A slot that leaks a copy per park and counts the wipes, refusing a park over a live view.
#[derive(Default)]
struct Slot {
    parks: u32,
    wipes: u32,
    live: bool,
}

impl CredentialSlot<'static> for Slot {
    fn park(&mut self, ssid: &[u8], secret: &[u8]) -> (&'static [u8], &'static [u8]) {
        assert!(!self.live, "a park over a view the driver may still hold");
        self.parks += 1;
        self.live = true;
        (Box::leak(ssid.to_vec().into_boxed_slice()), Box::leak(secret.to_vec().into_boxed_slice()))
    }

    fn wipe(&mut self) {
        self.wipes += 1;
        self.live = false;
    }
}

/// The bytes of one erase unit in these tests: a sector, as the boards' flash has.
const UNIT: usize = 4096;

/// Two erase units in memory.
struct Flash {
    units: [[u8; UNIT]; SLOTS],
}

impl RecordStore for Flash {
    fn read(&self, slot: usize) -> &[u8] {
        &self.units[slot]
    }

    fn erase(&mut self, slot: usize) -> bool {
        self.units[slot] = [0xFF; UNIT];
        true
    }

    fn program(&mut self, slot: usize, offset: usize, page: &[u8; PAGE]) -> bool {
        self.units[slot][offset..offset + PAGE].copy_from_slice(page);
        true
    }
}

type Under = Controller<'static, Script, TestClock, Flash, Slot>;

/// A controller over a radio that is up, ready and holds no network.
fn controller() -> Under {
    controller_over(Flash { units: [[0xFF; UNIT]; SLOTS] })
}

/// A controller over a radio that is up and ready, with `flash` as the board's stored settings.
fn controller_over(flash: Flash) -> Under {
    let mut script = Script::new(Vec::new());
    script.ready = true;
    script.driver_like = true;
    let mut station = Station::new(script, TestClock { now: 0 });
    station.address = Some(OURS);
    Controller::new(station, flash, Slot::default())
}

/// CRC-32 as the record's check computes it: IEEE 802.3, reflected, polynomial `0xEDB88320`.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// Stored settings holding the network `home-net` with its byte `at` rewritten as `value`, the check
/// made again so the slot verifies.
fn flash_holding(at: usize, value: u8) -> Flash {
    let mut flash = Flash { units: [[0xFF; UNIT]; SLOTS] };
    let stored = crate::record::Record::new(
        b"home-net",
        b"stored-pass",
        security::WPA2,
        Reconnection::Automatic,
        BootConnection::Background,
    )
    .expect("a network");
    assert_eq!(crate::record::store(&mut flash, &stored), RecordWrite::Written);
    flash.units[0][at] = value;
    let check = crc32(&flash.units[0][..188]);
    flash.units[0][188..192].copy_from_slice(&check.to_le_bytes());
    flash
}

/// Stored settings holding the network `home-net` with boot byte 1, the retired "join before the
/// program starts", its check made again so the slot verifies.
fn flash_with_the_retired_boot_value() -> Flash {
    flash_holding(173, 1)
}

/// A current copy this firmware cannot use -- undamaged, with a setting a newer version stored or
/// in a newer format -- is no stored network: nothing is read, joined or changed from it, it is
/// kept, and a network stored over it replaces it. Reconnection 2 at offset 172, format 2 at 4.
#[test]
fn a_stored_copy_this_firmware_cannot_use_is_no_stored_network_and_is_kept_until_one_is_stored_over_it() {
    for (at, value) in [(172, 2), (4, 2)] {
        let mut c = controller_over(flash_holding(at, value));
        assert_eq!(c.stored_boot(), None, "byte {at} as {value}");
        assert!(c.stored_unusable(), "byte {at} as {value}: the current copy is one this firmware cannot use");
        assert!(c.record_read().is_none());
        assert_eq!(c.join_stored(), None, "no stored network is joined");
        assert_eq!(c.record_set_boot(BootConnection::OnConnect), RecordWrite::NoRecord);
        assert!(c.stored_unusable(), "and the copy is kept");
        assert_eq!(c.record_write(request(b"new-net", b"other-pass", security::WPA2)), RecordWrite::Written);
        assert!(!c.stored_unusable());
        assert_eq!(c.record_read().expect("the new network").ssid, b"new-net".to_vec());
    }
    let mut c = controller();
    assert!(!c.stored_unusable(), "nothing stored is not a copy this firmware cannot use");
}

/// A firmware that keeps no board settings is built over a store that keeps nothing, and the controller says so:
/// such a firmware reads no stored network, whatever the board's flash holds, which is not the same start as a
/// board that stores none. A board's own flash keeps records even while it holds none.
#[test]
fn a_store_that_keeps_nothing_is_told_apart_from_a_board_that_stores_no_network() {
    let mut script = Script::new(Vec::new());
    script.ready = true;
    let mut keeps_nothing = Controller::new(Station::new(script, TestClock { now: 0 }), crate::record::NoStore, Slot::default());
    assert!(!keeps_nothing.keeps_records(), "a firmware that keeps no settings reads no stored network");
    assert_eq!(keeps_nothing.stored_boot(), None);
    let mut empty = controller();
    assert!(empty.keeps_records(), "a board's flash keeps records while it holds none");
    assert_eq!(empty.stored_boot(), None, "and then it stores no network");
}

/// Feeds the radio `steps` and pumps until they are taken, whatever store holds the settings.
fn feed<S: RecordStore>(c: &mut Controller<'static, Script, TestClock, S, Slot>, steps: Vec<Step>) {
    c.station_mut().radio.steps.extend(steps);
    for _ in 0..8 {
        c.station_mut().next_poll = 0;
        c.service();
    }
}

/// How many times the slot was parked and wiped.
fn slot_counts(c: &Under) -> (u32, u32) {
    (c.slot().parks, c.slot().wipes)
}

fn requests(c: &Under) -> Vec<&'static str> {
    c.station().radio().requests.clone()
}

fn caps(sae: bool) -> Step {
    step(Wake::Done(Outcome::Capabilities(Capabilities { answered: true, status: 0, len: 16, sae, mfp: sae })))
}

/// A record of the network `name` from the access point `bssid`, heard at `rssi` on channel 6.
fn seen(name: &[u8], bssid: [u8; 6], rssi: i16, psk: bool, sae: bool) -> Step {
    let mut ssid = [0u8; 32];
    ssid[..name.len()].copy_from_slice(name);
    let security = Advertised { rsne: true, privacy: true, ccmp: true, psk, sae, ..Advertised::default() };
    step(Wake::Done(Outcome::ScanRecord(ScanRecord {
        bssid,
        ssid,
        ssid_len: name.len() as u8,
        capability: 0x0011,
        beacon_period: 100,
        channel: 6,
        rssi,
        version: 109,
        length: 0,
        security,
    })))
}

fn scan_done() -> Step {
    step(Wake::Done(Outcome::ScanDone { end: ScanEnd::Complete, records: 1 }))
}

fn joined() -> Step {
    link_step(Wake::Done(Outcome::Joined { bssid: PEER }), LinkState::Up)
}

fn request<'a>(ssid: &'a [u8], secret: &'a [u8], kinds: u8) -> JoinRequest<'a> {
    JoinRequest { ssid, secret, security: kinds, reconnection: Reconnection::Automatic }
}

/// The driver's joins, as (name, kind, secret), whatever store holds the settings.
fn driver_joins<S: RecordStore>(c: &Controller<'static, Script, TestClock, S, Slot>) -> Vec<(Vec<u8>, &'static str, Vec<u8>)> {
    c.station().radio().joins.clone()
}

/// A controller over a radio that is off, which it can bring up from the images, with `flash` as
/// the board's stored settings.
fn off_controller(flash: Flash) -> Under {
    let mut script = Script::new(Vec::new());
    script.driver_like = true;
    Controller::new(Station::new(script, TestClock { now: 0 }), flash, Slot::default()).with_images(IMAGES, 10_000_000)
}

/// The bring-up's outcomes, attach to up, each after the request it answers.
fn bring_up_steps() -> Vec<Step> {
    vec![
        step(Wake::Done(Outcome::Attached { chip_id: 0x1515_a9af })),
        step(Wake::Done(Outcome::Uploaded { save_restore: true })),
        Step { ready: Some(true), ..step(Wake::Done(Outcome::Ready { version: lamella_wifi_cyw4343x::Version::EMPTY })) },
        step(Wake::Done(Outcome::Up { address: OURS })),
    ]
}

fn empty_flash() -> Flash {
    Flash { units: [[0xFF; UNIT]; SLOTS] }
}

/// **THE RADIO IS OFF UNTIL A JOIN**: it is fitted, so it is present, and reading
/// the state, the stored network or the radio, or writing the stored network, starts nothing. A
/// join powers it and goes on once it is up, and the link reads as connecting meanwhile.
#[test]
fn a_radio_that_is_off_is_present_and_only_a_join_powers_it() {
    let mut c = off_controller(flash_holding(173, 0));
    assert_eq!(c.radio_state(), RadioState::Off);
    assert!(c.radio().present, "fitted, though off");
    assert_eq!(c.hardware_address(), None, "a radio that is off has not read its address");
    let _ = c.state();
    assert!(c.record_read().is_some());
    assert_eq!(c.record_set_boot(BootConnection::OnConnect), RecordWrite::Written);
    assert_eq!(c.link(), WifiLink::Disconnected);
    feed(&mut c, Vec::new());
    assert!(requests(&c).is_empty(), "nothing but a join starts the radio: {:?}", requests(&c));
    assert_eq!(c.radio_state(), RadioState::Off);

    let n = c.join_start(request(b"lab-net", b"passphrase-1", security::WPA2));
    assert_eq!(requests(&c), ["attach"], "the join starts the radio and returns");
    assert_eq!(c.radio_state(), RadioState::Starting);
    assert_eq!(c.link(), WifiLink::Connecting);
    assert_eq!(c.state().join_outcome, None, "the join waits for the radio");
    feed(&mut c, bring_up_steps());
    assert_eq!(c.radio_state(), RadioState::Up);
    assert_eq!(c.hardware_address(), Some(OURS), "the address the stack takes");
    assert_eq!(requests(&c), ["attach", "upload", "open", "up", "join"], "then joins");
    feed(&mut c, vec![joined()]);
    let state = c.state();
    assert_eq!((state.join, state.join_outcome, state.link), (n, Some(JoinStatus::Success), WifiLink::Connected));
}

/// A join asked while the radio starts waits for it, and does not start it again; the stored
/// network's join powers the radio as a program's does.
#[test]
fn a_join_asked_while_the_radio_starts_waits_for_it() {
    let mut c = off_controller(flash_holding(173, 0));
    assert!(c.power_on());
    assert_eq!(requests(&c), ["attach"]);
    assert!(c.join_stored().is_some());
    assert_eq!(requests(&c), ["attach"], "not started twice");
    feed(&mut c, bring_up_steps());
    assert_eq!(requests(&c), ["attach", "upload", "open", "up", "join"]);
    assert_eq!(driver_joins(&c), [(b"home-net".to_vec(), "wpa2", b"stored-pass".to_vec())]);
}

/// **A RADIO THAT FAILED FAILS A JOIN AT ONCE**: a bring-up that stopped short leaves the radio
/// failed and not present, and a join waiting for it ends with the radio's failure.
#[test]
fn a_radio_that_failed_fails_a_join_at_once() {
    let refusal = lamella_wifi_cyw4343x::Refusal::new("chip identity", 0x4373);
    let mut c = off_controller(empty_flash());
    let n = c.join_start(request(b"lab-net", b"passphrase-1", security::WPA2));
    feed(&mut c, vec![step(Wake::Done(Outcome::Refused(refusal)))]);
    assert_eq!(c.radio_state(), RadioState::Failed);
    assert!(!c.radio().present);
    let state = c.state();
    assert_eq!((state.join, state.join_outcome), (n, Some(JoinStatus::UnspecifiedFailure)));
    assert_eq!(state.last_failure.expect("the failure").detail, "the radio is not running");
    assert_eq!(slot_counts(&c), (1, 1), "the credential parked for it is wiped");
    let again = c.join_start(request(b"lab-net", b"passphrase-1", security::WPA2));
    assert_eq!(c.state().join_outcome, Some(JoinStatus::UnspecifiedFailure), "join {again} fails at once");
    assert_eq!(requests(&c), ["attach"], "a failed radio is not started again");
}

#[test]
fn a_wpa2_join_goes_straight_to_the_driver_and_reports_the_link_and_its_kind() {
    let mut c = controller();
    let n = c.join_start(request(b"lab-net", b"passphrase-1", security::WPA2));
    assert_eq!(driver_joins(&c), [(b"lab-net".to_vec(), "wpa2", b"passphrase-1".to_vec())]);
    let state = c.state();
    assert_eq!((state.link, state.join, state.join_outcome), (WifiLink::Connecting, n, None));
    feed(&mut c, vec![joined()]);
    let state = c.state();
    assert_eq!((state.link, state.join_outcome), (WifiLink::Connected, Some(JoinStatus::Success)));
    assert_eq!((state.ssid.as_slice(), state.security), (&b"lab-net"[..], security::WPA2));
    assert_eq!((state.source, state.bssid), (CredentialSource::Program, Some(PEER)));
}

/// "WPA2 or WPA3" asks the firmware, scans, and takes WPA3 only when the network names SAE and the
/// radio runs it; the strongest access point's signal and channel go into the report.
#[test]
fn a_wpa2_or_wpa3_join_takes_wpa3_when_the_network_offers_it_and_the_radio_runs_it() {
    let mut c = controller();
    c.join_start(request(b"lab-net", b"password-3", security::WPA2 | security::WPA3));
    assert_eq!(requests(&c), ["capabilities"], "the radio is asked first");
    feed(&mut c, vec![caps(true)]);
    assert_eq!(requests(&c), ["capabilities", "scan"]);
    feed(&mut c, vec![seen(b"lab-net", PEER, -61, true, true), seen(b"other", OURS, -30, true, true), scan_done()]);
    assert_eq!(driver_joins(&c), [(b"lab-net".to_vec(), "wpa3", b"password-3".to_vec())]);
    feed(&mut c, vec![joined()]);
    let state = c.state();
    assert_eq!((state.security, state.rssi, state.channel), (security::WPA3, Some(-61), Some(6)));
}

#[test]
fn a_wpa2_or_wpa3_join_takes_wpa2_when_the_network_offers_only_wpa2_or_is_not_seen() {
    let mut c = controller();
    feed(&mut c, vec![caps(true)]);
    c.join_start(request(b"lab-net", b"password-3", security::WPA2 | security::WPA3));
    feed(&mut c, vec![seen(b"lab-net", PEER, -50, true, false), scan_done()]);
    assert_eq!(driver_joins(&c)[0].1, "wpa2");

    let mut c = controller();
    feed(&mut c, vec![caps(true)]);
    c.join_start(request(b"hidden-net", b"password-3", security::WPA2 | security::WPA3));
    feed(&mut c, vec![seen(b"elsewhere", PEER, -50, true, true), scan_done()]);
    assert_eq!(driver_joins(&c)[0].1, "wpa2", "a network the scan did not see is tried as WPA2");
}

#[test]
fn a_hidden_networks_named_scan_can_choose_wpa3_from_the_parked_name() {
    let mut c = controller();
    feed(&mut c, vec![caps(true)]);
    c.join_start(request(b"concealed-lab", b"passphrase-7", security::WPA2 | security::WPA3));
    assert_eq!(c.station().radio().scan_names, [b"concealed-lab".to_vec()]);
    feed(&mut c, vec![seen(b"concealed-lab", PEER, -64, false, true), scan_done()]);
    assert_eq!(driver_joins(&c)[0].1, "wpa3");
    assert_eq!(slot_counts(&c), (1, 0), "the driver still holds the parked name");
}

#[test]
fn cancelling_a_named_scan_keeps_its_name_parked_until_the_scan_ends() {
    let mut c = controller();
    feed(&mut c, vec![caps(true)]);
    c.join_start(request(b"concealed-lab", b"passphrase-7", security::WPA2 | security::WPA3));
    c.disconnect();
    assert_eq!(c.state().link, WifiLink::Disconnected);
    assert_eq!(slot_counts(&c), (1, 0), "the scan still borrows the name");
    feed(&mut c, vec![seen(b"concealed-lab", PEER, -50, true, true)]);
    assert_eq!(slot_counts(&c), (1, 0));
    feed(&mut c, vec![scan_done()]);
    assert_eq!(slot_counts(&c), (1, 1));
    assert!(driver_joins(&c).is_empty(), "a cancelled scan never starts its join");
}

#[test]
fn replacing_a_named_scan_defers_slot_reuse_and_only_starts_the_latest_request() {
    let mut c = controller();
    feed(&mut c, vec![caps(true)]);
    c.join_start(request(b"concealed-lab", b"passphrase-7", security::WPA2 | security::WPA3));
    c.join_start(request(b"intermediate-lab", b"passphrase-8", security::WPA2));
    let last = c.join_start(request(b"final-lab", b"passphrase-9", security::WPA2));
    assert_eq!(slot_counts(&c), (1, 0), "the scan's borrowed name cannot be overwritten");
    assert!(driver_joins(&c).is_empty());
    feed(&mut c, vec![seen(b"concealed-lab", PEER, -50, false, true), scan_done()]);
    assert_eq!(slot_counts(&c), (2, 1));
    assert_eq!(driver_joins(&c), [(b"final-lab".to_vec(), "wpa2", b"passphrase-9".to_vec())]);
    feed(&mut c, vec![joined()]);
    assert_eq!(c.state().join, last);
    assert_eq!(c.state().ssid, b"final-lab");
}

#[test]
fn cancelling_a_replacement_during_a_named_scan_never_parks_that_replacement() {
    let mut c = controller();
    feed(&mut c, vec![caps(true)]);
    c.join_start(request(b"concealed-lab", b"passphrase-7", security::WPA2 | security::WPA3));
    c.join_start(request(b"next-lab", b"passphrase-8", security::WPA2));
    c.disconnect();
    assert_eq!(slot_counts(&c), (1, 0));
    feed(&mut c, vec![scan_done()]);
    assert_eq!(slot_counts(&c), (1, 1));
    assert!(driver_joins(&c).is_empty());
}

#[test]
fn a_radio_refusal_during_a_named_scan_ends_the_request_without_reusing_its_borrowed_name() {
    for replace in [false, true] {
        let mut c = controller();
        feed(&mut c, vec![caps(true)]);
        c.join_start(request(b"concealed-lab", b"passphrase-7", security::WPA2 | security::WPA3));
        if replace {
            c.join_start(request(b"next-lab", b"passphrase-8", security::WPA2));
        }
        feed(&mut c, vec![step(Wake::Done(Outcome::Refused(Refusal::new("scan reply", 7))))]);
        let state = c.state();
        assert_eq!((state.link, state.join_outcome), (WifiLink::Disconnected, Some(JoinStatus::UnspecifiedFailure)));
        assert!(state.last_failure.unwrap().detail.contains("scan reply"));
        assert_eq!(slot_counts(&c), (1, 0), "the stopped driver still owns its scan view until reset");
        assert!(driver_joins(&c).is_empty());
    }
}

/// On a radio without SAE, "WPA2 or WPA3" needs no scan: only WPA2 can be joined.
#[test]
fn a_wpa2_or_wpa3_join_on_a_radio_without_sae_joins_wpa2_without_scanning() {
    let mut c = controller();
    feed(&mut c, vec![caps(false)]);
    c.join_start(request(b"lab-net", b"password-3", security::WPA2 | security::WPA3));
    assert_eq!(requests(&c), ["join"], "the firmware's answer was already in, and no scan is made");
    assert_eq!(driver_joins(&c)[0].1, "wpa2");
}

/// WPA3 alone on a radio without SAE ends at once: no attempt is made, and none is downgraded.
#[test]
fn a_wpa3_join_on_a_radio_without_sae_ends_at_once_and_is_never_downgraded() {
    let mut c = controller();
    feed(&mut c, vec![caps(false)]);
    c.join_start(request(b"lab-net", b"password-3", security::WPA3));
    let state = c.state();
    assert_eq!(state.join_outcome, Some(JoinStatus::UnsupportedAuthenticationProtocol));
    assert_eq!(state.link, WifiLink::Disconnected);
    assert!(driver_joins(&c).is_empty(), "no attempt of any kind");
    assert!(state.last_failure.unwrap().detail.contains("WPA3"));
    assert!(!c.station().radio().requests.contains(&"join"));
}

#[test]
fn a_network_offering_no_kind_the_request_accepts_ends_the_join_after_the_scan() {
    let mut c = controller();
    feed(&mut c, vec![caps(true)]);
    c.join_start(request(b"corp-net", b"password-3", security::WPA2 | security::WPA3));
    let mut record = seen(b"corp-net", PEER, -50, false, false);
    if let Wake::Done(Outcome::ScanRecord(r)) = &mut record.wake {
        r.security.dot1x = true;
    }
    feed(&mut c, vec![record, scan_done()]);
    let state = c.state();
    assert_eq!(state.join_outcome, Some(JoinStatus::UnsupportedAuthenticationProtocol));
    assert!(state.last_failure.unwrap().detail.contains("802.1X"));
    assert!(driver_joins(&c).is_empty());
}

fn finish_background_rest(c: &mut Under) {
    let scans = c.station().radio().scan_names.len();
    c.station_mut().clock.now += 4_000_000;
    feed(c, Vec::new());
    assert_eq!(c.station().radio().scan_names.len(), scans, "no early retry");
    c.station_mut().clock.now += 1_100_000;
    feed(c, Vec::new());
    assert_eq!(c.station().radio().scan_names.len(), scans + 1, "scan again after the ordinary five-second rest");
}

#[test]
fn a_background_scan_keeps_looking_when_no_accepted_kind_is_seen() {
    let mut c = controller();
    feed(&mut c, vec![caps(true)]);
    c.begin(b"changing-lab", b"passphrase-7", security::WPA2 | security::WPA3,
        Reconnection::Automatic, CredentialSource::StoredRecord);
    for found in [true, false, true] {
        let mut answers = Vec::new();
        if found {
            answers.push(seen(b"changing-lab", PEER, -50, false, false));
        }
        answers.push(scan_done());
        feed(&mut c, answers);
        let state = c.state();
        assert_eq!((state.link, state.join_outcome), (WifiLink::Connecting, None));
        assert_eq!(slot_counts(&c), (1, 0));
        assert!(driver_joins(&c).is_empty());
        assert!(!requests(&c).contains(&"disconnect"));
        finish_background_rest(&mut c);
    }
    feed(&mut c, vec![seen(b"changing-lab", PEER, -50, false, true), scan_done()]);
    assert_eq!(driver_joins(&c)[0].1, "wpa3");
    feed(&mut c, vec![joined()]);
    assert_eq!(c.state().link, WifiLink::Connected);
}

#[test]
fn a_background_mismatch_retries_its_requested_kind_before_and_after_a_completed_join() {
    for kind in [security::WPA2, security::WPA3, security::OPEN] {
        for initially_joined in [false, true] {
            let mut c = controller();
            feed(&mut c, vec![caps(true)]);
            c.begin(b"changing-lab", b"passphrase-7", kind, Reconnection::Automatic, CredentialSource::StoredRecord);
            if initially_joined {
                feed(&mut c, vec![joined(), link_step(Wake::Done(Outcome::LinkLost(LOSS)), LinkState::Down)]);
            }
            feed(&mut c, vec![mismatch(false, false)]);
            assert_eq!(c.state().link, WifiLink::Connecting);
            assert_eq!(slot_counts(&c), (1, 0));
            finish_background_rest(&mut c);
            let mut answer = seen(b"changing-lab", PEER, -50, kind == security::WPA2, kind == security::WPA3);
            if kind == security::OPEN {
                if let Wake::Done(Outcome::ScanRecord(record)) = &mut answer.wake {
                    record.security = Advertised::default();
                }
            }
            feed(&mut c, vec![answer, scan_done()]);
            assert_eq!(driver_joins(&c).len(), 2);
            assert!(!requests(&c).contains(&"disconnect"), "changing kind cannot reset the driver's refusal count");
            feed(&mut c, vec![joined()]);
            assert_eq!(c.state().security, kind);
        }
    }
}

#[test]
fn a_background_first_join_can_change_kind_without_disconnecting_or_reparking() {
    let mut c = controller();
    feed(&mut c, vec![caps(true)]);
    c.begin(b"changing-lab", b"passphrase-7", security::WPA2 | security::WPA3,
        Reconnection::Automatic, CredentialSource::StoredRecord);
    feed(&mut c, vec![seen(b"changing-lab", PEER, -50, false, true), scan_done()]);
    feed(&mut c, vec![link_step(Wake::Done(Outcome::JoinFailed(
        JoinFailure::Association { status: 1, assoc_status: 0, auth_status: 7 },
    )), LinkState::Down)]);
    feed(&mut c, vec![mismatch(true, false)]);
    assert_eq!(driver_joins(&c).iter().map(|join| join.1).collect::<Vec<_>>(), ["wpa3", "wpa2"]);
    assert_eq!(slot_counts(&c), (1, 0));
    assert!(!requests(&c).contains(&"disconnect"));
    assert_eq!(c.state().join_outcome, None);
}

#[test]
fn a_background_secret_outside_the_chosen_kinds_bounds_stops_without_a_join() {
    for (kinds, secret) in [
        (security::WPA2, &b"short"[..]),
        (security::WPA3, &b""[..]),
        (security::WPA2 | security::WPA3, &b""[..]),
        (security::WPA2 | security::WPA3, &b"short"[..]),
    ] {
        let mut c = controller();
        feed(&mut c, vec![caps(true)]);
        c.begin(b"changing-lab", secret, kinds, Reconnection::Automatic, CredentialSource::StoredRecord);
        feed(&mut c, vec![seen(b"changing-lab", PEER, -50, true, false), scan_done()]);
        let state = c.state();
        assert_eq!((state.link, state.join_outcome), (WifiLink::Disconnected, Some(JoinStatus::InvalidCredential)));
        assert!(state.last_failure.unwrap().detail.contains("length"));
        assert!(driver_joins(&c).is_empty());
        assert_eq!(slot_counts(&c), (1, 1));
    }
}

/// A program's join stops at a refused secret and wipes its credential, ending the driver's retry.
#[test]
fn a_wrong_wpa2_passphrase_ends_the_join_as_an_invalid_credential_and_wipes_the_slot() {
    let mut c = controller();
    c.join_start(request(b"lab-net", b"wrong-pass", security::WPA2));
    feed(
        &mut c,
        vec![link_step(Wake::Done(Outcome::JoinFailed(JoinFailure::Supplicant { reason: 15 })), LinkState::Down)],
    );
    let state = c.state();
    assert_eq!((state.link, state.join_outcome), (WifiLink::Disconnected, Some(JoinStatus::InvalidCredential)));
    assert!(state.last_failure.unwrap().detail.contains("supplicant reason 15"));
    assert_eq!(slot_counts(&c), (1, 1), "parked once, wiped once");
}

#[test]
fn background_refusals_leave_every_retry_and_the_credential_with_the_driver() {
    for (kind, failure) in [
        (security::WPA2, JoinFailure::Supplicant { reason: 15 }),
        (security::WPA3, JoinFailure::Association { status: 1, assoc_status: 0, auth_status: 99 }),
    ] {
        let mut c = controller();
        feed(&mut c, vec![caps(true)]);
        c.record_write(request(b"retry-lab", b"temporary-pass", kind));
        c.begin_stored().unwrap();
        for _ in 0..10 {
            feed(&mut c, vec![link_step(Wake::Done(Outcome::JoinFailed(failure)), LinkState::Down)]);
            let state = c.state();
            assert_eq!((state.link, state.join_outcome), (WifiLink::Connecting, None));
            assert_eq!(state.last_failure.unwrap().status, JoinStatus::InvalidCredential);
            assert_eq!(slot_counts(&c), (1, 0));
            assert!(!requests(&c).contains(&"disconnect"));
            assert_eq!(driver_joins(&c).len(), 1, "the driver owns every retry");
        }
        feed(&mut c, vec![link_step(Wake::Done(Outcome::JoinFailed(JoinFailure::NotFound)), LinkState::Down)]);
        assert_eq!(c.state().link, WifiLink::Connecting);
        feed(&mut c, vec![joined()]);
        assert_eq!((c.state().link, c.state().join_outcome), (WifiLink::Connected, Some(JoinStatus::Success)));
        feed(&mut c, vec![link_step(Wake::Done(Outcome::LinkLost(LOSS)), LinkState::Down)]);
        feed(&mut c, vec![link_step(Wake::Done(Outcome::JoinFailed(failure)), LinkState::Down)]);
        assert_eq!(c.state().link, WifiLink::Connecting);
        assert_eq!(slot_counts(&c), (1, 0));
        c.disconnect();
        assert_eq!(c.state().link, WifiLink::Disconnected);
        assert_eq!(slot_counts(&c), (1, 1));
    }
}

#[test]
fn a_programs_stored_join_reports_a_refusal_immediately_for_either_reconnection_choice() {
    for reconnection in [Reconnection::Automatic, Reconnection::Manual] {
        let mut c = controller();
        c.record_write(JoinRequest { reconnection, ..request(b"retry-lab", b"temporary-pass", security::WPA2) });
        c.join_stored().unwrap();
        feed(&mut c, vec![link_step(
            Wake::Done(Outcome::JoinFailed(JoinFailure::Supplicant { reason: 15 })), LinkState::Down,
        )]);
        let state = c.state();
        assert_eq!((state.link, state.join_outcome), (WifiLink::Disconnected, Some(JoinStatus::InvalidCredential)));
        assert_eq!(requests(&c).last(), Some(&"disconnect"));
        assert_eq!(slot_counts(&c), (1, 1));
    }
}

#[test]
fn refusal_status_uses_the_drivers_kind_specific_evidence() {
    use crate::controller::failure_of;
    for kind in [Security::Open, Security::Wpa2Psk, Security::Wpa3Sae] {
        for failure in [
            JoinFailure::Supplicant { reason: 14 },
            JoinFailure::Supplicant { reason: 15 },
            JoinFailure::Association { status: 1, assoc_status: 0, auth_status: 7 },
            JoinFailure::Association { status: 3, assoc_status: 0, auth_status: 7 },
        ] {
            assert_eq!(failure_of(Some(failure), kind).status == JoinStatus::InvalidCredential,
                failure.refused_secret(kind), "{failure:?} with {kind:?}");
        }
    }
}

#[test]
fn a_wpa3_refusal_reports_authentication_failure_without_claiming_a_wrong_password() {
    let failure = JoinFailure::Association { status: 1, assoc_status: 0, auth_status: 7 };
    let detail = crate::controller::failure_of(Some(failure), Security::Wpa3Sae).detail;
    assert!(detail.contains("authentication failed"), "{detail}");
    assert!(!detail.contains("refused the WPA3 password"), "{detail}");
}

#[test]
fn wifi_state_reads_the_current_refusal_rest_and_rounds_up_without_retaining_it() {
    let mut c = controller();
    c.begin(b"retry-lab", b"temporary-pass", security::WPA2, Reconnection::Automatic, CredentialSource::BuildTime);
    feed(&mut c, vec![link_step(
        Wake::Done(Outcome::JoinFailed(JoinFailure::Supplicant { reason: 15 })), LinkState::Down,
    )]);
    for (micros, seconds) in [(5_000_000, 5), (5_000_001, 6), (10_950_000, 11), (359_999_999, 360)] {
        c.station_mut().radio.backoff = Some(micros);
        let detail = c.state().last_failure.unwrap().detail;
        assert!(detail.ends_with(&format!("; retry rest {seconds} s")), "{detail}");
        assert_eq!(detail.matches("retry rest").count(), 1);
        assert!(!detail.contains("temporary-pass") && !detail.contains("retry-lab"));
    }
    c.station_mut().radio.backoff = None;
    assert!(!c.state().last_failure.unwrap().detail.contains("retry rest"));
    feed(&mut c, vec![link_step(Wake::Done(Outcome::JoinFailed(JoinFailure::NotFound)), LinkState::Down)]);
    assert_eq!(c.state().last_failure.unwrap().status, JoinStatus::NetworkNotAvailable);
    feed(&mut c, vec![joined()]);
    assert!(c.state().last_failure.is_none());
}

/// A failure the driver retries leaves the join in progress, reporting the failure; ending it then
/// reports the kind of the failure it last met.
#[test]
fn a_retried_failure_keeps_the_join_and_ending_it_reports_that_failure() {
    let mut c = controller();
    c.join_start(request(b"far-net", b"passphrase-1", security::WPA2));
    feed(&mut c, vec![link_step(Wake::Done(Outcome::JoinFailed(JoinFailure::NotFound)), LinkState::Down)]);
    let state = c.state();
    assert_eq!((state.link, state.join_outcome), (WifiLink::Connecting, None), "the driver retries");
    assert_eq!(state.last_failure.as_ref().unwrap().status, JoinStatus::NetworkNotAvailable);
    c.disconnect();
    let state = c.state();
    assert_eq!((state.link, state.join_outcome), (WifiLink::Disconnected, Some(JoinStatus::NetworkNotAvailable)));
    assert!(requests(&c).contains(&"disconnect"));
    assert_eq!(slot_counts(&c), (1, 1));
}

#[test]
fn ending_a_join_that_met_no_failure_reports_a_timeout() {
    let mut c = controller();
    c.join_start(request(b"slow-net", b"passphrase-1", security::WPA2));
    c.disconnect();
    assert_eq!(c.state().join_outcome, Some(JoinStatus::Timeout));
}

/// Manual: a lost link is let go, so the driver's own recovery stops and the program decides.
#[test]
fn a_manual_network_is_let_go_when_the_link_is_lost() {
    let mut c = controller();
    c.join_start(JoinRequest { reconnection: Reconnection::Manual, ..request(b"lab-net", b"passphrase-1", security::WPA2) });
    feed(&mut c, vec![joined()]);
    feed(&mut c, vec![link_step(Wake::Done(Outcome::LinkLost(LOSS)), LinkState::Down)]);
    let state = c.state();
    assert_eq!((state.link, state.source), (WifiLink::Disconnected, CredentialSource::None));
    assert_eq!(requests(&c).last(), Some(&"disconnect"));
    assert_eq!(slot_counts(&c), (1, 1));
}

/// Automatic: a lost link is the driver's to recover; the report says so, then says it is back.
#[test]
fn an_automatic_network_reports_connecting_through_a_loss_and_connected_after_the_re_join() {
    let mut c = controller();
    c.join_start(request(b"lab-net", b"passphrase-1", security::WPA2));
    feed(&mut c, vec![joined()]);
    feed(&mut c, vec![link_step(Wake::Done(Outcome::LinkLost(LOSS)), LinkState::Down)]);
    assert_eq!(c.state().link, WifiLink::Connecting);
    assert!(!requests(&c).contains(&"disconnect"));
    feed(&mut c, vec![joined()]);
    assert_eq!(c.state().link, WifiLink::Connected);
    assert_eq!(slot_counts(&c), (1, 0), "the credential stays for the driver's re-joins");
}

/// The driver's recovery scan saw the held network advertising `psk` and `sae` as its suites and none
/// the kind in force can use, so it let the network go.
fn mismatch(psk: bool, sae: bool) -> Step {
    let advertised = Advertised { rsne: true, privacy: true, ccmp: true, psk, sae, ..Advertised::default() };
    link_step(Wake::Done(Outcome::JoinFailed(JoinFailure::Mismatch { advertised })), LinkState::Detached)
}

/// A join that accepts both kinds and joins again by itself: when the network's recovery scan sees
/// it offering only the other kind, the controller chooses again from what it offers, as a fresh
/// join chooses, and joins again with the credential it holds -- WPA3 to WPA2, and WPA2 to WPA3.
#[test]
fn a_lost_link_whose_network_now_offers_only_the_other_kind_is_joined_again_with_that_kind() {
    let mut c = controller();
    feed(&mut c, vec![caps(true)]);
    c.join_start(request(b"lab-net", b"password-3", security::WPA2 | security::WPA3));
    feed(&mut c, vec![seen(b"lab-net", PEER, -61, true, true), scan_done()]);
    feed(&mut c, vec![joined()]);
    assert_eq!(c.state().security, security::WPA3);
    feed(&mut c, vec![link_step(Wake::Done(Outcome::LinkLost(LOSS)), LinkState::Down), mismatch(true, false)]);
    assert_eq!(
        driver_joins(&c).last(),
        Some(&(b"lab-net".to_vec(), "wpa2", b"password-3".to_vec())),
        "joined again with WPA2, which the network now offers"
    );
    let state = c.state();
    assert_eq!((state.link, state.source), (WifiLink::Connecting, CredentialSource::Program));
    feed(&mut c, vec![joined()]);
    let state = c.state();
    assert_eq!((state.link, state.security, state.last_failure), (WifiLink::Connected, security::WPA2, None));
    assert_eq!(slot_counts(&c), (1, 0), "the credential stayed parked for the second join");

    let mut c = controller();
    feed(&mut c, vec![caps(true)]);
    c.join_start(request(b"lab-net", b"password-3", security::WPA2 | security::WPA3));
    feed(&mut c, vec![seen(b"lab-net", PEER, -61, true, false), scan_done()]);
    feed(&mut c, vec![joined()]);
    assert_eq!(c.state().security, security::WPA2);
    feed(&mut c, vec![link_step(Wake::Done(Outcome::LinkLost(LOSS)), LinkState::Down), mismatch(false, true)]);
    assert_eq!(driver_joins(&c).last().map(|join| join.1), Some("wpa3"), "joined again with WPA3");
    feed(&mut c, vec![joined()]);
    assert_eq!((c.state().link, c.state().security), (WifiLink::Connected, security::WPA3));
}

/// With no other kind to take, a mismatch lets the network go as before: a join that accepts one
/// kind, a radio without SAE facing a network that now offers WPA3 alone, and a network that now
/// offers neither kind. The credential is wiped and the failure says what the network advertises.
#[test]
fn a_mismatch_with_no_other_kind_to_take_lets_the_network_go() {
    let mut c = controller();
    feed(&mut c, vec![caps(true)]);
    c.join_start(request(b"lab-net", b"password-3", security::WPA3));
    feed(&mut c, vec![joined()]);
    feed(&mut c, vec![link_step(Wake::Done(Outcome::LinkLost(LOSS)), LinkState::Down), mismatch(true, false)]);
    let state = c.state();
    assert_eq!((state.link, state.source), (WifiLink::Disconnected, CredentialSource::None), "WPA3 alone");
    assert!(state.last_failure.unwrap().detail.contains("advertises"));
    assert_eq!((driver_joins(&c).len(), slot_counts(&c)), (1, (1, 1)), "no second join, and the credential is wiped");

    let mut c = controller();
    feed(&mut c, vec![caps(false)]);
    c.join_start(request(b"lab-net", b"password-3", security::WPA2 | security::WPA3));
    feed(&mut c, vec![joined()]);
    feed(&mut c, vec![link_step(Wake::Done(Outcome::LinkLost(LOSS)), LinkState::Down), mismatch(false, true)]);
    assert_eq!(c.state().link, WifiLink::Disconnected, "a radio without SAE cannot take WPA3");
    assert_eq!(driver_joins(&c).len(), 1);

    let mut c = controller();
    feed(&mut c, vec![caps(true)]);
    c.join_start(request(b"lab-net", b"password-3", security::WPA2 | security::WPA3));
    feed(&mut c, vec![seen(b"lab-net", PEER, -61, true, false), scan_done()]);
    feed(&mut c, vec![joined()]);
    feed(&mut c, vec![link_step(Wake::Done(Outcome::LinkLost(LOSS)), LinkState::Down), mismatch(false, false)]);
    assert_eq!(c.state().link, WifiLink::Disconnected, "a network offering neither kind");
    assert_eq!(driver_joins(&c).len(), 1);
}

/// Asking again for the network that is up, with the same secret, does not drop a working link.
#[test]
fn joining_the_network_already_up_succeeds_at_once_without_a_new_attempt() {
    let mut c = controller();
    c.join_start(request(b"lab-net", b"passphrase-1", security::WPA2));
    feed(&mut c, vec![joined()]);
    let n = c.join_start(request(b"lab-net", b"passphrase-1", security::WPA2));
    let state = c.state();
    assert_eq!((state.join, state.join_outcome, state.link), (n, Some(JoinStatus::Success), WifiLink::Connected));
    assert_eq!(driver_joins(&c).len(), 1);
    c.join_start(request(b"lab-net", b"passphrase-2", security::WPA2));
    assert_eq!(driver_joins(&c).len(), 2, "a different secret is a new join");
    assert_eq!(slot_counts(&c), (2, 1), "the first credential was wiped before the second was parked");
}

#[test]
fn a_join_from_the_stored_record_uses_its_values_and_says_where_they_came_from() {
    let mut c = controller();
    assert_eq!(c.join_stored(), None, "nothing stored");
    assert_eq!(c.record_write(request(b"home-net", b"stored-pass", security::WPA2)), RecordWrite::Written);
    let stored = c.record_read().unwrap();
    assert_eq!((stored.ssid.as_slice(), stored.boot), (&b"home-net"[..], BootConnection::Background));
    c.join_stored().unwrap();
    assert_eq!(driver_joins(&c), [(b"home-net".to_vec(), "wpa2", b"stored-pass".to_vec())]);
    feed(&mut c, vec![joined()]);
    assert_eq!(c.state().source, CredentialSource::StoredRecord);
}

/// The boot setting a network was stored with survives storing it again, and only a clear removes
/// the network.
#[test]
fn storing_a_network_again_keeps_its_boot_setting_and_a_clear_removes_it() {
    let mut c = controller();
    c.record_write(request(b"home-net", b"stored-pass", security::WPA2));
    assert_eq!(c.record_set_boot(BootConnection::OnConnect), RecordWrite::Written);
    assert_eq!(c.record_write(request(b"home-net", b"stored-pass", security::WPA2)), RecordWrite::Unchanged);
    c.record_write(request(b"new-net", b"other-pass", security::WPA2));
    assert_eq!(c.record_read().unwrap().boot, BootConnection::OnConnect);
    assert!(c.record_clear());
    assert!(c.record_read().is_none());
    assert_eq!(c.record_set_boot(BootConnection::Background), RecordWrite::NoRecord);
}

/// A stored network whose boot byte is 1, the retired "join before the program starts": a program
/// reads it as background, which is what the board does with it; the firmware still sees the value
/// itself, to say so; and a network stored over it is stored as background.
#[test]
fn a_record_holding_the_retired_boot_value_reads_as_background_and_is_stored_as_background() {
    let mut c = controller_over(flash_with_the_retired_boot_value());
    assert_eq!(c.stored_boot(), Some(BootConnection::BeforeMain), "the firmware sees the value, to say so");
    assert_eq!(c.record_read().unwrap().boot, BootConnection::Background, "a program reads what the board does");
    assert_eq!(c.record_write(request(b"new-net", b"other-pass", security::WPA2)), RecordWrite::Written);
    assert_eq!(c.stored_boot(), Some(BootConnection::Background), "a network stored over it is stored as background");
}

#[test]
fn a_join_on_a_radio_that_is_not_running_fails_at_once() {
    let mut c = controller();
    c.station_mut().address = None;
    c.join_start(request(b"lab-net", b"passphrase-1", security::WPA2));
    assert_eq!(c.state().join_outcome, Some(JoinStatus::UnspecifiedFailure));
    assert!(!c.radio().present);
}

#[test]
fn a_report_prints_the_names_length_and_never_the_name() {
    let mut c = controller();
    c.join_start(request(b"secret-place", b"passphrase-1", security::WPA2));
    let printed = format!("{:?} {:?}", c.state(), request(b"secret-place", b"passphrase-1", security::WPA2));
    assert!(!printed.contains("secret-place") && !printed.contains("passphrase-1"), "{printed}");
}

/// Every driver failure maps to its kind, as the seam documents them.
#[test]
fn each_driver_failure_maps_to_its_kind() {
    use crate::controller::failure_of;
    use lamella_wifi_cyw4343x::Security;
    let kind = |failure, sec| failure_of(Some(failure), sec).status;
    let association = |status, auth_status| JoinFailure::Association { status, assoc_status: 0, auth_status };
    assert_eq!(kind(JoinFailure::NotFound, Security::Wpa2Psk), JoinStatus::NetworkNotAvailable);
    assert_eq!(kind(association(3, 0), Security::Wpa2Psk), JoinStatus::NetworkNotAvailable);
    assert_eq!(kind(JoinFailure::Supplicant { reason: 15 }, Security::Wpa2Psk), JoinStatus::InvalidCredential);
    assert_eq!(kind(association(1, 1), Security::Wpa3Sae), JoinStatus::InvalidCredential);
    assert_eq!(kind(association(1, 1), Security::Wpa2Psk), JoinStatus::UnspecifiedFailure);
    assert_eq!(
        kind(JoinFailure::Mismatch { advertised: Advertised::default() }, Security::Wpa2Psk),
        JoinStatus::UnsupportedAuthenticationProtocol
    );
    assert_eq!(kind(JoinFailure::Timeout, Security::Wpa2Psk), JoinStatus::Timeout);
    assert_eq!(kind(JoinFailure::Lost { number: 5, reason: 7 }, Security::Wpa3Sae), JoinStatus::UnspecifiedFailure);
    assert_eq!(kind(JoinFailure::Supplicant { reason: 14 }, Security::Wpa2Psk), JoinStatus::UnspecifiedFailure);
    assert_eq!(kind(JoinFailure::Supplicant { reason: 13 }, Security::Wpa2Psk), JoinStatus::UnspecifiedFailure);
}

/// A deauthentication alone is not the driver's refused-secret signal; it keeps the ordinary retry.
#[test]
fn a_deauthentication_during_the_handshake_keeps_the_ordinary_retry() {
    let mut c = controller();
    c.join_start(request(b"lab-net", b"wrong-pass", security::WPA2));
    feed(
        &mut c,
        vec![link_step(Wake::Done(Outcome::JoinFailed(JoinFailure::Supplicant { reason: 14 })), LinkState::Down)],
    );
    let state = c.state();
    assert_eq!((state.link, state.join_outcome), (WifiLink::Connecting, None));
    assert_eq!(state.last_failure.as_ref().unwrap().status, JoinStatus::UnspecifiedFailure);
    assert!(state.last_failure.unwrap().detail.contains("supplicant reason 14"));
    assert!(!requests(&c).contains(&"disconnect"));
    assert_eq!(slot_counts(&c), (1, 0));
}

/// A store that reads the board's settings and never writes them: its erase and program answer
/// false, as the store of a firmware with no way to store, change or clear a network does.
struct ReadOnly(Flash);

impl RecordStore for ReadOnly {
    fn read(&self, slot: usize) -> &[u8] {
        self.0.read(slot)
    }

    fn erase(&mut self, _slot: usize) -> bool {
        false
    }

    fn program(&mut self, _slot: usize, _offset: usize, _page: &[u8; PAGE]) -> bool {
        false
    }
}

/// A replacement cut short leaves both slots holding an undamaged copy. The newer copy is the
/// stored network: a store that cannot erase the older one still reads past it, and the board joins
/// the newer network with the newer setting.
#[test]
fn a_store_that_cannot_write_reads_the_newer_of_two_copies_and_joins_it() {
    let older = crate::record::Record::new(
        b"older-net",
        b"older-pass",
        security::WPA2,
        Reconnection::Automatic,
        BootConnection::OnConnect,
    )
    .expect("a network");
    let newer = crate::record::Record::new(
        b"newer-net",
        b"newer-pass",
        security::WPA2,
        Reconnection::Automatic,
        BootConnection::Background,
    )
    .expect("a network");
    let mut flash = Flash { units: [[0xFF; UNIT]; SLOTS] };
    assert_eq!(crate::record::store(&mut flash, &older), RecordWrite::Written);
    let first = flash.units;
    assert_eq!(crate::record::store(&mut flash, &newer), RecordWrite::Written);
    // The older copy goes back where it was, as a cut between writing the newer copy and erasing the
    // older one leaves it.
    for slot in 0..SLOTS {
        if first[slot] != [0xFF; UNIT] {
            flash.units[slot] = first[slot];
        }
    }
    assert!(flash.units.iter().all(|unit| unit != &[0xFF; UNIT]), "both slots hold a copy");

    let mut script = Script::new(Vec::new());
    script.ready = true;
    script.driver_like = true;
    let mut station = Station::new(script, TestClock { now: 0 });
    station.address = Some(OURS);
    let mut c = Controller::new(station, ReadOnly(flash), Slot::default());

    assert_eq!(c.stored_boot(), Some(BootConnection::Background), "the newer copy's setting, not the older's");
    feed(&mut c, vec![caps(true)]);
    c.begin_stored().unwrap();
    feed(&mut c, vec![joined()]);
    let joins = driver_joins(&c);
    assert_eq!(joins.len(), 1, "one join");
    assert_eq!((joins[0].0.as_slice(), joins[0].2.as_slice()), (&b"newer-net"[..], &b"newer-pass"[..]));
    assert_eq!((c.state().link, c.state().join_outcome), (WifiLink::Connected, Some(JoinStatus::Success)));
}
