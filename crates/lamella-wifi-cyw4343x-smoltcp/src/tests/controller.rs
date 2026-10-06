//! The controller against the scripted radio, run as the driver runs: each outcome is fed after the
//! request it answers, so the order of the controller's questions is part of what is tested.

use super::*;
use crate::controller::{Controller, CredentialSlot};
use crate::record::{RecordStore, SLOTS, SLOT_SPAN};
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

/// Two erase units in memory.
struct Flash {
    units: [[u8; SLOT_SPAN]; SLOTS],
}

impl RecordStore for Flash {
    fn read(&self, slot: usize) -> &[u8] {
        &self.units[slot]
    }

    fn erase(&mut self, slot: usize) -> bool {
        self.units[slot] = [0xFF; SLOT_SPAN];
        true
    }

    fn program(&mut self, slot: usize, bytes: &[u8]) -> bool {
        self.units[slot][..bytes.len()].copy_from_slice(bytes);
        true
    }
}

type Under = Controller<'static, Script, TestClock, Flash, Slot>;

/// A controller over a radio that is up, ready and holds no network.
fn controller() -> Under {
    let mut script = Script::new(Vec::new());
    script.ready = true;
    script.driver_like = true;
    let mut station = Station::new(script, TestClock { now: 0 });
    station.address = Some(OURS);
    Controller::new(station, Flash { units: [[0xFF; SLOT_SPAN]; SLOTS] }, Slot::default())
}

/// Feeds the radio `steps` and pumps until they are taken.
fn feed(c: &mut Under, steps: Vec<Step>) {
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

/// The driver's joins, as (name, kind, secret).
fn driver_joins(c: &Under) -> Vec<(Vec<u8>, &'static str, Vec<u8>)> {
    c.station().radio().joins.clone()
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

/// A failure the driver does not retry ends the join with its kind, and the credential is wiped.
#[test]
fn a_wrong_wpa2_passphrase_ends_the_join_as_an_invalid_credential_and_wipes_the_slot() {
    let mut c = controller();
    c.join_start(request(b"lab-net", b"wrong-pass", security::WPA2));
    feed(
        &mut c,
        vec![link_step(Wake::Done(Outcome::JoinFailed(JoinFailure::Supplicant { reason: 15 })), LinkState::Detached)],
    );
    let state = c.state();
    assert_eq!((state.link, state.join_outcome), (WifiLink::Disconnected, Some(JoinStatus::InvalidCredential)));
    assert!(state.last_failure.unwrap().detail.contains("supplicant reason 15"));
    assert_eq!(slot_counts(&c), (1, 1), "parked once, wiped once");
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
    assert_eq!(c.record_set_boot(BootConnection::BeforeMain), RecordWrite::Written);
    assert_eq!(c.record_write(request(b"home-net", b"stored-pass", security::WPA2)), RecordWrite::Unchanged);
    c.record_write(request(b"new-net", b"other-pass", security::WPA2));
    assert_eq!(c.record_read().unwrap().boot, BootConnection::BeforeMain);
    assert!(c.record_clear());
    assert!(c.record_read().is_none());
    assert_eq!(c.record_set_boot(BootConnection::Background), RecordWrite::NoRecord);
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
    assert_eq!(kind(JoinFailure::Supplicant { reason: 14 }, Security::Wpa2Psk), JoinStatus::InvalidCredential);
    assert_eq!(kind(JoinFailure::Supplicant { reason: 13 }, Security::Wpa2Psk), JoinStatus::UnspecifiedFailure);
}

/// A wrong passphrase answered with a deauthentication: the driver would try again, but the same
/// secret cannot succeed, so the join ends at once as an invalid credential and the slot is wiped.
#[test]
fn a_deauthentication_during_the_handshake_ends_the_join_as_an_invalid_credential() {
    let mut c = controller();
    c.join_start(request(b"lab-net", b"wrong-pass", security::WPA2));
    feed(
        &mut c,
        vec![link_step(Wake::Done(Outcome::JoinFailed(JoinFailure::Supplicant { reason: 14 })), LinkState::Down)],
    );
    let state = c.state();
    assert_eq!((state.link, state.join_outcome), (WifiLink::Disconnected, Some(JoinStatus::InvalidCredential)));
    assert!(state.last_failure.unwrap().detail.contains("supplicant reason 14"));
    assert_eq!(requests(&c).last(), Some(&"disconnect"), "the driver's retry is stopped");
    assert_eq!(slot_counts(&c), (1, 1));
}
