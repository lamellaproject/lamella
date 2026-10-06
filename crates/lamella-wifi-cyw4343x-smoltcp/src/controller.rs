//! The radio as a language runtime reaches it: a [`Station`] behind the Wi-Fi seam
//! ([`WifiControl`]), with the stored network beside it.
//!
//! A join is a short sequence the controller walks one step at a time while the station is pumped:
//! drop the network held, ask the firmware whether it runs WPA3 when that is not yet known, scan for
//! the network when the request accepts both WPA2 and WPA3, then join with the kind chosen and
//! follow the attempts until one completes or the driver lets the network go. Each step that the
//! driver refuses because it is busy is asked again on the next pass, so no step waits on another.
//!
//! The name and secret of a join are copied into a [`CredentialSlot`], whose copies the driver holds
//! by reference until the network is dropped; the slot is wiped as soon as it is.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use lamella_net_core::wifi::{
    security, BootConnection, CredentialSource, Failure, JoinRequest, JoinStatus, RadioInfo,
    Reconnection, RecordWrite, StoredNetwork, WifiControl, WifiLink, WifiState, SSID_MAX,
};
use lamella_wifi_cyw4343x::{Advertised, Clock, Credential, JoinFailure, LinkState, Security};
use smoltcp::phy::{Device, DeviceCapabilities};
use smoltcp::time::Instant;

use crate::record::{self, Record, RecordStore};
use crate::{Control, Receive, Station, Transmit};

/// Storage for the name and secret of the network the driver holds, which it reads by reference
/// until the network is dropped.
pub trait CredentialSlot<'b> {
    /// Copies `ssid` and `secret` into the slot and returns views of the copies, valid until the next
    /// `park` or `wipe`. Called only while the driver holds no view of an earlier copy.
    fn park(&mut self, ssid: &[u8], secret: &[u8]) -> (&'b [u8], &'b [u8]);

    /// Overwrites the slot with zeros. Called only once the driver holds no view of it.
    fn wipe(&mut self);
}

/// Where a join is, between the request and its end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// No network is held and no join is in progress.
    Idle,
    /// The capability query, then the join. `asked` once the driver has taken the query.
    Capabilities { asked: bool },
    /// A scan for the network, then the join. `scans` is the journal's count when it was taken.
    Scanning { asked: bool, scans: u32 },
    /// The join of `kind`. The counts are the journal's when the driver took it.
    Joining { kind: Security, asked: bool, joins: u32, failures: u32 },
    /// A network is held: the link is up, or the driver is joining it again after a loss. The counts
    /// are the journal's and the station's when this phase was last brought up to date.
    Held { joins: u32, failures: u32, losses: u32 },
}

/// A [`Station`], the stored network's storage and the slot for the name and secret of the network
/// held, as the Wi-Fi seam.
pub struct Controller<'b, R, C, S, K> {
    station: Station<R, C>,
    records: S,
    slot: K,
    /// The views of the slot the driver was handed; `None` once it has let them go.
    parked: Option<(&'b [u8], &'b [u8])>,
    phase: Phase,
    /// Whether a disconnect waits for the driver to take it.
    dropping: bool,
    ssid: [u8; SSID_MAX],
    ssid_len: usize,
    request: u8,
    in_force: u8,
    source: CredentialSource,
    reconnection: Reconnection,
    rssi: Option<i16>,
    channel: Option<u8>,
    bssid: Option<[u8; 6]>,
    join: u32,
    join_outcome: Option<JoinStatus>,
    /// The failure the join in progress last met.
    join_failure: Option<Failure>,
    last_failure: Option<Failure>,
}

impl<'b, R, C, S, K> Controller<'b, R, C, S, K> {
    /// A controller over `station`, storing its network in `records` and parking the credential of
    /// the network held in `slot`.
    pub const fn new(station: Station<R, C>, records: S, slot: K) -> Self {
        Controller {
            station,
            records,
            slot,
            parked: None,
            phase: Phase::Idle,
            dropping: false,
            ssid: [0; SSID_MAX],
            ssid_len: 0,
            request: 0,
            in_force: 0,
            source: CredentialSource::None,
            reconnection: Reconnection::Automatic,
            rssi: None,
            channel: None,
            bssid: None,
            join: 0,
            join_outcome: None,
            join_failure: None,
            last_failure: None,
        }
    }

    /// The station.
    pub fn station(&self) -> &Station<R, C> {
        &self.station
    }

    /// The station, for the bring-up a board makes before any join.
    pub fn station_mut(&mut self) -> &mut Station<R, C> {
        &mut self.station
    }

    /// The credential slot, for a test to count what was asked of it.
    #[cfg(test)]
    pub(crate) fn slot(&self) -> &K {
        &self.slot
    }
}

impl<'b, R: Control<'b>, C: Clock, S: RecordStore, K: CredentialSlot<'b>> Controller<'b, R, C, S, K> {
    /// Pumps the station and moves the join forward. The stack's own calls do both; a caller with
    /// no stack running calls this.
    pub fn service(&mut self) {
        self.station.service();
        self.tick();
    }

    /// Starts a join of `ssid` with `secret`, accepting the kinds `kinds`, from `source`. Returns the
    /// join's number.
    pub fn begin(
        &mut self,
        ssid: &[u8],
        secret: &[u8],
        kinds: u8,
        reconnection: Reconnection,
        source: CredentialSource,
    ) -> u32 {
        self.join = self.join.wrapping_add(1).max(1);
        self.join_outcome = None;
        self.join_failure = None;
        if self.holds(ssid, secret, kinds) {
            self.source = source;
            self.reconnection = reconnection;
            self.join_outcome = Some(JoinStatus::Success);
            return self.join;
        }
        self.let_go();
        let len = ssid.len().min(SSID_MAX);
        self.ssid = [0; SSID_MAX];
        self.ssid[..len].copy_from_slice(&ssid[..len]);
        self.ssid_len = len;
        self.request = kinds;
        self.in_force = 0;
        self.source = source;
        self.reconnection = reconnection;
        self.rssi = None;
        self.channel = None;
        self.bssid = None;
        if self.station.address().is_none() || self.station.refusal().is_some() {
            self.fail(Failure {
                status: JoinStatus::UnspecifiedFailure,
                detail: String::from("the radio is not running"),
            });
            return self.join;
        }
        self.parked = Some(self.slot.park(ssid, secret));
        self.phase = self.next_step(false);
        self.tick();
        self.join
    }

    /// Starts a join of the stored network; `None` when none is stored.
    pub fn begin_stored(&mut self) -> Option<u32> {
        let (_, stored) = record::load(&mut self.records)?;
        Some(self.begin(
            stored.ssid(),
            stored.secret(),
            stored.security,
            stored.reconnection,
            CredentialSource::StoredRecord,
        ))
    }

    /// The boot setting of the stored network; `None` when none is stored.
    pub fn stored_boot(&mut self) -> Option<BootConnection> {
        record::load(&mut self.records).map(|(_, stored)| stored.boot)
    }

    /// Whether the link is up on the network `ssid`, joined with `secret` by a request accepting
    /// `kinds`.
    fn holds(&self, ssid: &[u8], secret: &[u8], kinds: u8) -> bool {
        let up = matches!(self.phase, Phase::Held { .. }) && self.station.link() == LinkState::Up;
        up && self.request == kinds
            && self.parked.is_some_and(|(held_ssid, held_secret)| held_ssid == ssid && held_secret == secret)
    }

    /// Drops the network held, if any, and wipes its credential once the driver has let it go.
    fn let_go(&mut self) {
        if self.station.link() != LinkState::Detached {
            self.dropping = !self.station.disconnect();
        }
        if !self.dropping {
            self.wipe();
        }
        self.phase = Phase::Idle;
    }

    /// Wipes the slot, once the driver holds no view of it.
    fn wipe(&mut self) {
        if self.parked.take().is_some() {
            self.slot.wipe();
        }
    }

    /// The step after the capability query and the scan (each done or not): another question for
    /// the driver, or the join of the kind chosen. A request that cannot succeed ends here.
    fn next_step(&mut self, scanned: bool) -> Phase {
        let journal = *self.station.journal();
        let asked = journal.capabilities.is_some();
        let sae = journal.capabilities.and_then(|c| c.answered.then_some(c.sae));
        let joining = |kind| Phase::Joining { kind, asked: false, joins: 0, failures: 0 };
        match self.request {
            security::OPEN => joining(Security::Open),
            security::WPA2 => joining(Security::Wpa2Psk),
            security::WPA3 if !asked => Phase::Capabilities { asked: false },
            security::WPA3 if sae == Some(false) => {
                self.fail(Failure {
                    status: JoinStatus::UnsupportedAuthenticationProtocol,
                    detail: String::from("the radio's firmware does not run WPA3 (SAE)"),
                });
                Phase::Idle
            }
            security::WPA3 => joining(Security::Wpa3Sae),
            _ if !asked => Phase::Capabilities { asked: false },
            _ if sae == Some(false) => joining(Security::Wpa2Psk),
            _ if !scanned => Phase::Scanning { asked: false, scans: 0 },
            _ => match journal.sighting {
                None => joining(Security::Wpa2Psk),
                Some(seen) => {
                    self.rssi = (seen.strongest.rssi != 0).then_some(seen.strongest.rssi);
                    self.channel = (seen.strongest.channel != 0).then_some(seen.strongest.channel);
                    if seen.sae && seen.wpa3 {
                        joining(Security::Wpa3Sae)
                    } else if seen.wpa2 {
                        joining(Security::Wpa2Psk)
                    } else {
                        let detail = format!(
                            "the network offers no security this join accepts: it advertises {}",
                            advertised_text(seen.advertised)
                        );
                        self.fail(Failure { status: JoinStatus::UnsupportedAuthenticationProtocol, detail });
                        Phase::Idle
                    }
                }
            },
        }
    }

    /// Ends the join in progress with `failure`, and lets the network go.
    fn fail(&mut self, failure: Failure) {
        self.join_outcome = Some(failure.status);
        self.last_failure = Some(failure.clone());
        self.join_failure = Some(failure);
        self.let_go();
    }

    /// The credential the driver joins with for `kind`, from the slot.
    fn credential(&self, kind: Security) -> Option<(&'b [u8], Credential<'b>)> {
        let (ssid, secret) = self.parked?;
        Some((ssid, match kind {
            Security::Open => Credential::Open,
            Security::Wpa2Psk => Credential::Passphrase(secret),
            Security::Wpa3Sae => Credential::SaePassword(secret),
        }))
    }

    /// Moves the join forward as far as the journal allows.
    fn tick(&mut self) {
        if self.dropping {
            self.dropping = self.station.link() != LinkState::Detached && !self.station.disconnect();
            if self.dropping {
                return;
            }
            self.wipe();
        }
        for _ in 0..4 {
            let journal = *self.station.journal();
            let next = match self.phase {
                Phase::Idle => return,
                Phase::Capabilities { asked: false } => {
                    if !self.station.start_capabilities() {
                        return;
                    }
                    Phase::Capabilities { asked: true }
                }
                Phase::Capabilities { asked: true } => {
                    if journal.capabilities.is_none() {
                        return;
                    }
                    self.next_step(false)
                }
                Phase::Scanning { asked: false, .. } => {
                    let ssid = self.ssid;
                    if !self.station.start_scan(&ssid[..self.ssid_len]) {
                        return;
                    }
                    Phase::Scanning { asked: true, scans: journal.scans }
                }
                Phase::Scanning { asked: true, scans } => {
                    if journal.scans == scans {
                        return;
                    }
                    self.next_step(true)
                }
                Phase::Joining { kind, asked: false, .. } => {
                    let Some((ssid, credential)) = self.credential(kind) else { return };
                    if !self.station.start_join(ssid, credential) {
                        if let Some(refusal) = self.station.refusal() {
                            self.fail(Failure {
                                status: JoinStatus::UnspecifiedFailure,
                                detail: format!("the radio stopped at {} (status {:#010x})", refusal.stage, refusal.status),
                            });
                        }
                        return;
                    }
                    self.in_force = security_bit(kind);
                    Phase::Joining { kind, asked: true, joins: journal.joins, failures: journal.failures }
                }
                Phase::Joining { kind, asked: true, joins, failures } => {
                    if journal.joins != joins {
                        self.bssid = journal.joined;
                        self.join_outcome = Some(JoinStatus::Success);
                        self.join_failure = None;
                        self.last_failure = None;
                        Phase::Held {
                            joins: journal.joins,
                            failures: journal.failures,
                            losses: self.station.counters().link_losses,
                        }
                    } else if journal.failures != failures {
                        let failure = failure_of(journal.failure, kind);
                        self.last_failure = Some(failure.clone());
                        self.join_failure = Some(failure.clone());
                        if journal.failure_link == Some(LinkState::Detached) {
                            self.join_outcome = Some(failure.status);
                            self.wipe();
                            Phase::Idle
                        } else if failure.status == JoinStatus::InvalidCredential {
                            self.fail(failure);
                            Phase::Idle
                        } else {
                            Phase::Joining { kind, asked: true, joins, failures: journal.failures }
                        }
                    } else if let Some(refusal) = self.station.refusal() {
                        self.fail(Failure {
                            status: JoinStatus::UnspecifiedFailure,
                            detail: format!("the radio stopped at {} (status {:#010x})", refusal.stage, refusal.status),
                        });
                        Phase::Idle
                    } else {
                        return;
                    }
                }
                Phase::Held { joins, failures, losses } => {
                    let lost = self.station.counters().link_losses;
                    if lost != losses && self.reconnection == Reconnection::Manual {
                        self.let_go();
                        self.source = CredentialSource::None;
                        return;
                    }
                    if journal.joins != joins {
                        self.bssid = journal.joined;
                        self.last_failure = None;
                    }
                    if journal.failures != failures {
                        let kind = security_kind(self.in_force);
                        self.last_failure = Some(failure_of(journal.failure, kind));
                    }
                    if self.station.link() == LinkState::Detached {
                        self.wipe();
                        self.source = CredentialSource::None;
                        Phase::Idle
                    } else {
                        let next = Phase::Held { joins: journal.joins, failures: journal.failures, losses: lost };
                        if next == self.phase {
                            return;
                        }
                        next
                    }
                }
            };
            self.phase = next;
        }
    }

    /// The link, from the phase and the driver.
    fn link_now(&self) -> WifiLink {
        match self.phase {
            Phase::Idle => WifiLink::Disconnected,
            Phase::Held { .. } => match self.station.link() {
                LinkState::Up => WifiLink::Connected,
                LinkState::Detached => WifiLink::Disconnected,
                LinkState::Down | LinkState::Joining => WifiLink::Connecting,
            },
            _ => WifiLink::Connecting,
        }
    }
}

impl<'b, R: Control<'b>, C: Clock, S: RecordStore, K: CredentialSlot<'b>> WifiControl
    for Controller<'b, R, C, S, K>
{
    fn radio(&mut self) -> RadioInfo {
        let capabilities = self.station.journal().capabilities;
        RadioInfo {
            present: self.station.address().is_some() && self.station.refusal().is_none(),
            wpa3: capabilities.and_then(|c| c.answered.then_some(c.sae)),
        }
    }

    fn link(&self) -> WifiLink {
        self.link_now()
    }

    fn join_start(&mut self, request: JoinRequest<'_>) -> u32 {
        self.begin(request.ssid, request.secret, request.security, request.reconnection, CredentialSource::Program)
    }

    fn join_stored(&mut self) -> Option<u32> {
        self.begin_stored()
    }

    fn disconnect(&mut self) {
        if self.join_outcome.is_none() && self.join != 0 {
            let failure = self.join_failure.clone().unwrap_or_else(|| Failure {
                status: JoinStatus::Timeout,
                detail: String::from("the join did not complete in the time allowed"),
            });
            self.join_outcome = Some(failure.status);
            self.last_failure = Some(failure);
        }
        self.let_go();
        self.source = CredentialSource::None;
        self.in_force = 0;
    }

    fn state(&mut self) -> WifiState {
        self.tick();
        let link = self.link_now();
        let held = link != WifiLink::Disconnected;
        WifiState {
            link,
            ssid: if held { self.ssid[..self.ssid_len].to_vec() } else { Vec::new() },
            security: if held { self.in_force } else { 0 },
            source: if held { self.source } else { CredentialSource::None },
            rssi: if held { self.rssi } else { None },
            channel: if held { self.channel } else { None },
            bssid: if held { self.bssid } else { None },
            last_failure: self.last_failure.clone(),
            join: self.join,
            join_outcome: self.join_outcome,
        }
    }

    fn record_read(&mut self) -> Option<StoredNetwork> {
        let (_, stored) = record::load(&mut self.records)?;
        Some(StoredNetwork {
            ssid: stored.ssid().to_vec(),
            security: stored.security,
            reconnection: stored.reconnection,
            boot: stored.boot,
        })
    }

    fn record_write(&mut self, network: JoinRequest<'_>) -> RecordWrite {
        let boot = self.stored_boot().unwrap_or(BootConnection::Background);
        let Some(fresh) = Record::new(network.ssid, network.secret, network.security, network.reconnection, boot) else {
            return RecordWrite::Failed;
        };
        record::store(&mut self.records, &fresh)
    }

    fn record_set_boot(&mut self, boot: BootConnection) -> RecordWrite {
        record::set_boot(&mut self.records, boot)
    }

    fn record_clear(&mut self) -> bool {
        record::clear(&mut self.records)
    }
}

/// The controller is the stack's device: each receive pumps the station and moves the join forward,
/// then hands over what the station holds.
impl<'b, R: Control<'b>, C: Clock, S: RecordStore, K: CredentialSlot<'b>> Device for Controller<'b, R, C, S, K> {
    type RxToken<'a>
        = Receive<'a>
    where
        Self: 'a;
    type TxToken<'a>
        = Transmit<'a, R>
    where
        Self: 'a;

    fn receive(&mut self, timestamp: Instant) -> Option<(Receive<'_>, Transmit<'_, R>)> {
        self.service();
        self.station.receive(timestamp)
    }

    fn transmit(&mut self, timestamp: Instant) -> Option<Transmit<'_, R>> {
        self.station.transmit(timestamp)
    }

    fn capabilities(&self) -> DeviceCapabilities {
        self.station.capabilities()
    }
}

/// A controller borrowed for as long as a backend holds it, which a firmware rebuilds for every
/// program while the controller lives on.
impl<'b, R: Control<'b>, C: Clock, S: RecordStore, K: CredentialSlot<'b>> Device
    for &mut Controller<'b, R, C, S, K>
{
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

/// The seam's bit for a driver kind.
fn security_bit(kind: Security) -> u8 {
    match kind {
        Security::Open => security::OPEN,
        Security::Wpa2Psk => security::WPA2,
        Security::Wpa3Sae => security::WPA3,
    }
}

/// The driver kind for a seam bit in force.
fn security_kind(bit: u8) -> Security {
    match bit {
        security::OPEN => Security::Open,
        security::WPA3 => Security::Wpa3Sae,
        _ => Security::Wpa2Psk,
    }
}

/// The suites a network advertises, as a reader would name them.
fn advertised_text(advertised: Advertised) -> String {
    let mut names: Vec<&str> = Vec::new();
    if !advertised.privacy {
        names.push("no security");
    }
    if advertised.psk {
        names.push("WPA2 (PSK)");
    }
    if advertised.sae || advertised.ft_sae {
        names.push("WPA3 (SAE)");
    }
    if advertised.dot1x {
        names.push("802.1X");
    }
    if advertised.other_akm {
        names.push("a suite this radio does not name");
    }
    if names.is_empty() {
        return String::from("a secured network whose suites the scan could not read");
    }
    names.join(", ")
}

/// The kind and the plain line for a failed attempt of a `kind` join, as the driver reported it.
pub fn failure_of(failure: Option<JoinFailure>, kind: Security) -> Failure {
    let (status, detail) = match failure {
        None => (JoinStatus::UnspecifiedFailure, String::from("the join failed, and the radio gave no reason")),
        Some(JoinFailure::NotFound) => (
            JoinStatus::NetworkNotAvailable,
            String::from("the network was not found: a scan did not see its name"),
        ),
        Some(JoinFailure::Association { status: 3, .. }) => (
            JoinStatus::NetworkNotAvailable,
            String::from("the network was not found: the radio's join scan saw no network of that name (association status 3)"),
        ),
        Some(JoinFailure::Supplicant { reason: 15 }) => (
            JoinStatus::InvalidCredential,
            String::from("the WPA2 handshake timed out (supplicant reason 15), the usual sign of a wrong passphrase"),
        ),
        Some(JoinFailure::Supplicant { reason: 14 }) if kind != Security::Open => (
            JoinStatus::InvalidCredential,
            String::from(
                "the network ended the handshake with a deauthentication (supplicant reason 14), the usual sign of a wrong passphrase",
            ),
        ),
        Some(JoinFailure::Association { auth_status, .. }) if auth_status != 0 && kind == Security::Wpa3Sae => (
            JoinStatus::InvalidCredential,
            format!("the network refused the WPA3 password (authentication status {auth_status})"),
        ),
        Some(JoinFailure::Mismatch { advertised }) => (
            JoinStatus::UnsupportedAuthenticationProtocol,
            format!(
                "the network offers no security this join accepts: it advertises {}",
                advertised_text(advertised)
            ),
        ),
        Some(JoinFailure::Timeout) => (
            JoinStatus::Timeout,
            String::from("no attempt completed within the radio's 15-second bound for one attempt"),
        ),
        Some(JoinFailure::Lost { number, reason }) => (
            JoinStatus::UnspecifiedFailure,
            format!("the link was lost during the join (event {number}, reason {reason})"),
        ),
        Some(JoinFailure::Association { status, assoc_status, auth_status }) => (
            JoinStatus::UnspecifiedFailure,
            format!(
                "the association failed (status {status}, association status {assoc_status}, authentication status {auth_status})"
            ),
        ),
        Some(JoinFailure::Supplicant { reason }) => (
            JoinStatus::UnspecifiedFailure,
            format!("the WPA handshake failed (supplicant reason {reason})"),
        ),
    };
    Failure { status, detail }
}
