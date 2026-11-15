//! The radio as a language runtime reaches it: a [`Station`] behind the Wi-Fi seam
//! ([`WifiControl`]), with the stored network beside it.
//!
//! The radio is off until something asks for it ([`RadioState`]): a join, or the board's start when
//! its stored network joins in the background. Reading the state, the stored network or whether a
//! radio is fitted, and writing the stored network, never power it.
//!
//! A join is a short sequence the controller walks one step at a time while the station is pumped:
//! bring the radio up when it is off, and wait for it while it starts; drop the network held, ask
//! the firmware whether it runs WPA3 when that is not yet known, scan for
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
use lamella_wifi_cyw4343x::station::CYCLE_REST_US;
use lamella_wifi_cyw4343x::{Advertised, Clock, Credential, JoinFailure, LinkState, Micros, Network, Security};
use smoltcp::phy::{Device, DeviceCapabilities};
use smoltcp::time::Instant;

use crate::record::{self, Record, RecordStore};
use crate::{BringUp, Control, Images, Receive, Station, Transmit};

/// What the radio is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RadioState {
    /// Fitted and not running: nothing has asked for it yet.
    Off,
    /// Being brought up.
    Starting,
    /// Running.
    Up,
    /// It could not be brought up, or its driver stopped; a join fails at once.
    Failed,
}

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
    /// The radio is being brought up for the join, which goes on once it is up.
    Starting,
    /// The capability query, then the join. `asked` once the driver has taken the query.
    Capabilities { asked: bool },
    /// A scan for the network, then the join. `scans` is the journal's count when it was taken.
    Scanning { asked: bool, scans: u32 },
    /// The driver released a mismatched network; wait before looking for it again.
    Resting { until: Micros },
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
    /// A replacement waiting for the driver to release the slot. Its secret is erased on drop.
    pending: Option<Record>,
    phase: Phase,
    /// Whether a disconnect waits for the driver to take it.
    dropping: bool,
    ssid: [u8; SSID_MAX],
    ssid_len: usize,
    request: u8,
    in_force: u8,
    source: CredentialSource,
    reconnection: Reconnection,
    /// A board-start join waits through refused secrets; a program's request reports one at once.
    background: bool,
    rssi: Option<i16>,
    channel: Option<u8>,
    bssid: Option<[u8; 6]>,
    join: u32,
    join_outcome: Option<JoinStatus>,
    /// The failure the join in progress last met.
    join_failure: Option<Failure>,
    last_failure: Option<Failure>,
    /// The images the radio is brought up with, and how long its bring-up may take; `None` for a
    /// controller that cannot start the radio itself.
    images: Option<(Images<'b>, Micros)>,
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
            pending: None,
            phase: Phase::Idle,
            dropping: false,
            ssid: [0; SSID_MAX],
            ssid_len: 0,
            request: 0,
            in_force: 0,
            source: CredentialSource::None,
            reconnection: Reconnection::Automatic,
            background: false,
            rssi: None,
            channel: None,
            bssid: None,
            join: 0,
            join_outcome: None,
            join_failure: None,
            last_failure: None,
            images: None,
        }
    }

    /// The controller, bringing its radio up from `images`, within `timeout_us`, when something
    /// asks for the radio.
    #[must_use]
    pub fn with_images(mut self, images: Images<'b>, timeout_us: Micros) -> Self {
        self.images = Some((images, timeout_us));
        self
    }

    /// The station.
    pub fn station(&self) -> &Station<R, C> {
        &self.station
    }

    /// What the radio is doing.
    pub fn radio_state(&self) -> RadioState {
        if self.station.refusal().is_some() {
            return RadioState::Failed;
        }
        if self.station.address().is_some() {
            return RadioState::Up;
        }
        match self.station.bring_up_state() {
            BringUp::Off if self.images.is_some() => RadioState::Off,
            BringUp::Off | BringUp::Stopped(_) => RadioState::Failed,
            BringUp::Starting { .. } => RadioState::Starting,
            BringUp::Up => RadioState::Up,
        }
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
    /// Pumps the station and moves the radio's bring-up and the join forward. The stack's own calls
    /// do all three; a caller with no stack running calls this.
    pub fn service(&mut self) {
        self.station.service();
        if let Some((images, _)) = self.images {
            self.station.advance_bring_up(images);
        }
        self.tick();
    }

    /// Starts the radio when it is off, and returns at once: the passes that follow bring it up.
    /// Returns whether it is starting or up.
    pub fn power_on(&mut self) -> bool {
        match self.radio_state() {
            RadioState::Up | RadioState::Starting => true,
            RadioState::Failed => false,
            RadioState::Off => match self.images {
                Some((images, timeout_us)) => self.station.start_bring_up(images, timeout_us),
                None => false,
            },
        }
    }

    /// Starts the board's background join of `ssid` with `secret`, accepting `kinds`, from `source`.
    /// Refused secrets leave the driver's retries running. Returns the join's number.
    pub fn begin(
        &mut self,
        ssid: &[u8],
        secret: &[u8],
        kinds: u8,
        reconnection: Reconnection,
        source: CredentialSource,
    ) -> u32 {
        self.begin_request(ssid, secret, kinds, reconnection, source, true)
    }

    fn begin_request(
        &mut self,
        ssid: &[u8],
        secret: &[u8],
        kinds: u8,
        reconnection: Reconnection,
        source: CredentialSource,
        background: bool,
    ) -> u32 {
        self.join = self.join.wrapping_add(1).max(1);
        self.join_outcome = None;
        self.join_failure = None;
        self.background = background;
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
        if !self.power_on() {
            self.fail(not_running());
            return self.join;
        }
        if self.dropping {
            self.pending = Record::new(ssid, secret, kinds, reconnection, BootConnection::OnConnect);
            if self.pending.is_some() {
                self.phase = Phase::Starting;
            } else {
                self.fail(Failure {
                    status: JoinStatus::UnspecifiedFailure,
                    detail: String::from("the network name, secret or security kinds are outside their bounds"),
                });
            }
            return self.join;
        }
        self.parked = Some(self.slot.park(ssid, secret));
        self.phase = if self.radio_state() == RadioState::Up { self.next_step(false) } else { Phase::Starting };
        self.tick();
        self.join
    }

    /// Starts the board's background join of the stored network; `None` when none is stored.
    pub fn begin_stored(&mut self) -> Option<u32> {
        self.stored_request(true)
    }

    fn stored_request(&mut self, background: bool) -> Option<u32> {
        let (_, stored) = record::load(&mut self.records)?;
        Some(self.begin_request(
            stored.ssid(),
            stored.secret(),
            stored.security,
            stored.reconnection,
            CredentialSource::StoredRecord,
            background,
        ))
    }

    /// The boot setting of the stored network as the record holds it, the retired
    /// [`BootConnection::BeforeMain`] included; `None` when none is stored that this firmware can use.
    pub fn stored_boot(&mut self) -> Option<BootConnection> {
        record::load(&mut self.records).map(|(_, stored)| stored.boot)
    }

    /// Whether the current stored copy is one this firmware cannot use: undamaged, but holding a
    /// setting a newer version stored. The board then joins no stored network, and the copy is kept.
    pub fn stored_unusable(&mut self) -> bool {
        matches!(record::load_held(&mut self.records), record::Held::Unusable(_))
    }

    /// Whether this controller's storage keeps records at all. A firmware that keeps no board settings
    /// is built over a store that keeps none, and then reads no stored network whatever the board
    /// holds: a different start from a board that stores none.
    pub fn keeps_records(&self) -> bool {
        self.records.keeps_records()
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
        self.pending = None;
        self.dropping = self.station.scan_pending()
            || (self.station.link() != LinkState::Detached && !self.station.disconnect());
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
        let usable = [Security::Open, Security::Wpa2Psk, Security::Wpa3Sae].into_iter().any(|kind| {
            self.request & security_bit(kind) != 0 && self.credential_fits(kind)
        });
        if !usable {
            self.fail(invalid_length());
            return Phase::Idle;
        }
        let journal = *self.station.journal();
        let asked = journal.capabilities.is_some();
        let sae = journal.capabilities.and_then(|c| c.answered.then_some(c.sae));
        if scanned && self.background {
            if let Some(seen) = journal.sighting {
                self.rssi = (seen.strongest.rssi != 0).then_some(seen.strongest.rssi);
                self.channel = (seen.strongest.channel != 0).then_some(seen.strongest.channel);
                let kind = match self.request {
                    security::OPEN => seen.open.then_some(Security::Open),
                    security::WPA2 => seen.wpa2.then_some(Security::Wpa2Psk),
                    security::WPA3 => (seen.wpa3 && sae != Some(false)).then_some(Security::Wpa3Sae),
                    _ => chosen_kind(seen.wpa2, seen.wpa3, seen.sae, sae),
                };
                if let Some(kind) = kind {
                    return self.joining(kind);
                }
                self.last_failure = Some(failure_of(
                    Some(JoinFailure::Mismatch { advertised: seen.advertised }),
                    security_kind(self.request),
                ));
            } else {
                self.last_failure = Some(failure_of(Some(JoinFailure::NotFound), security_kind(self.request)));
            }
            self.join_failure = self.last_failure.clone();
            return self.rest_before_scan();
        }
        match self.request {
            security::OPEN => self.joining(Security::Open),
            security::WPA2 => self.joining(Security::Wpa2Psk),
            security::WPA3 if !asked => Phase::Capabilities { asked: false },
            security::WPA3 if sae == Some(false) => {
                self.fail(Failure {
                    status: JoinStatus::UnsupportedAuthenticationProtocol,
                    detail: String::from("the radio's firmware does not run WPA3 (SAE)"),
                });
                Phase::Idle
            }
            security::WPA3 => self.joining(Security::Wpa3Sae),
            _ if !asked => Phase::Capabilities { asked: false },
            _ if sae == Some(false) => self.joining(Security::Wpa2Psk),
            _ if !scanned => Phase::Scanning { asked: false, scans: 0 },
            _ => match journal.sighting {
                // A program's scan saw no matching network; try WPA2 and let the driver's join
                // and recovery scans determine whether it is available.
                None => self.joining(Security::Wpa2Psk),
                Some(seen) => {
                    self.rssi = (seen.strongest.rssi != 0).then_some(seen.strongest.rssi);
                    self.channel = (seen.strongest.channel != 0).then_some(seen.strongest.channel);
                    if let Some(kind) = chosen_kind(seen.wpa2, seen.wpa3, seen.sae, sae) {
                        self.joining(kind)
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

    fn credential_fits(&self, kind: Security) -> bool {
        self.credential(kind).is_some_and(|(ssid, credential)| Network::with(ssid, credential).is_some())
    }

    fn joining(&mut self, kind: Security) -> Phase {
        if !self.credential_fits(kind) {
            self.fail(invalid_length());
            return Phase::Idle;
        }
        Phase::Joining { kind, asked: false, joins: 0, failures: 0 }
    }

    fn rest_before_scan(&mut self) -> Phase {
        Phase::Resting { until: self.station.clock.now_us().saturating_add(CYCLE_REST_US) }
    }

    fn background_mismatch(&mut self, journal: &crate::Journal) -> Phase {
        match self.kind_after_mismatch(journal) {
            Some(kind) => self.joining(kind),
            None => self.rest_before_scan(),
        }
    }

    /// The kind to join with again when the driver let the held network go because its recovery scan
    /// saw it advertising no suite the kind in force can use: for a join that accepts both secured
    /// kinds and joins again by itself, the kind the network now offers, chosen as a fresh join
    /// chooses it. `None` for any other join, or when the network offers neither kind.
    fn kind_after_mismatch(&self, journal: &crate::Journal) -> Option<Security> {
        let Some(JoinFailure::Mismatch { advertised }) = journal.failure else { return None };
        if self.request != security::WPA2 | security::WPA3
            || (self.reconnection != Reconnection::Automatic && !self.background)
        {
            return None;
        }
        let sae = journal.capabilities.and_then(|c| c.answered.then_some(c.sae));
        chosen_kind(
            advertised.compatible(Security::Wpa2Psk),
            advertised.compatible(Security::Wpa3Sae),
            advertised.rsne && advertised.sae,
            sae,
        )
        .filter(|&kind| security_bit(kind) != self.in_force)
    }

    /// Joins the held network again with `kind`, from the credential still parked. Returns whether
    /// the driver took the join.
    fn join_again(&mut self, kind: Security) -> bool {
        let Some((ssid, credential)) = self.credential(kind) else { return false };
        if !self.station.start_join(ssid, credential) {
            return false;
        }
        self.in_force = security_bit(kind);
        true
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
        if self.station.scan_pending() && self.phase != Phase::Idle {
            if let Some(refusal) = self.station.refusal() {
                self.fail(Failure {
                    status: JoinStatus::UnspecifiedFailure,
                    detail: format!("the radio stopped at {} (status {:#010x})", refusal.stage, refusal.status),
                });
                return;
            }
        }
        if self.dropping {
            if self.station.scan_pending() {
                return;
            }
            self.dropping = self.station.link() != LinkState::Detached && !self.station.disconnect();
            if self.dropping {
                return;
            }
            self.wipe();
            if let Some(pending) = self.pending.take() {
                self.parked = Some(self.slot.park(pending.ssid(), pending.secret()));
                self.phase = if self.radio_state() == RadioState::Up { self.next_step(false) } else { Phase::Starting };
            }
        }
        for _ in 0..4 {
            let journal = *self.station.journal();
            let next = match self.phase {
                Phase::Idle => return,
                Phase::Starting => match self.radio_state() {
                    RadioState::Up => self.next_step(false),
                    RadioState::Failed => {
                        self.fail(not_running());
                        Phase::Idle
                    }
                    RadioState::Off | RadioState::Starting => return,
                },
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
                    let Some((ssid, _)) = self.parked else { return };
                    if !self.station.start_scan(ssid) {
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
                Phase::Resting { until } => {
                    if self.station.clock.now_us() < until {
                        return;
                    }
                    Phase::Scanning { asked: false, scans: 0 }
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
                            if self.background && matches!(journal.failure, Some(JoinFailure::Mismatch { .. })) {
                                self.background_mismatch(&journal)
                            } else {
                                // The driver let the network go; no later attempt will come.
                                self.join_outcome = Some(failure.status);
                                self.wipe();
                                Phase::Idle
                            }
                        } else if failure.status == JoinStatus::InvalidCredential && !self.background {
                            // A program's own join gets the refusal at once. A board-start join
                            // leaves the driver's rest, refusal count and credential intact.
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
                        if self.background && matches!(journal.failure, Some(JoinFailure::Mismatch { .. })) {
                            self.background_mismatch(&journal)
                        } else {
                            match self.kind_after_mismatch(&journal) {
                                Some(kind) if self.join_again(kind) => {
                                    Phase::Held { joins: journal.joins, failures: journal.failures, losses: lost }
                                }
                                _ => {
                                    self.wipe();
                                    self.source = CredentialSource::None;
                                    Phase::Idle
                                }
                            }
                        }
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
    /// The radio is present while it is fitted and has not failed: off until a join, starting, or up.
    fn radio(&mut self) -> RadioInfo {
        let capabilities = self.station.journal().capabilities;
        RadioInfo {
            present: self.radio_state() != RadioState::Failed,
            wpa3: capabilities.and_then(|c| c.answered.then_some(c.sae)),
        }
    }

    fn link(&self) -> WifiLink {
        self.link_now()
    }

    fn hardware_address(&mut self) -> Option<[u8; 6]> {
        self.station.address()
    }

    fn join_start(&mut self, request: JoinRequest<'_>) -> u32 {
        self.begin_request(
            request.ssid, request.secret, request.security, request.reconnection, CredentialSource::Program, false,
        )
    }

    fn join_stored(&mut self) -> Option<u32> {
        self.stored_request(false)
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
        let mut last_failure = self.last_failure.clone();
        if let (Some(failure), Some(rest)) = (&mut last_failure, self.station.radio().backoff()) {
            // Read the driver's current rest each time; never append it to the saved failure.
            failure.detail.push_str(&format!("; retry rest {} s", rest.div_ceil(1_000_000)));
        }
        WifiState {
            link,
            ssid: if held { self.ssid[..self.ssid_len].to_vec() } else { Vec::new() },
            security: if held { self.in_force } else { 0 },
            source: if held { self.source } else { CredentialSource::None },
            rssi: if held { self.rssi } else { None },
            channel: if held { self.channel } else { None },
            bssid: if held { self.bssid } else { None },
            last_failure,
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
            boot: stored.boot.in_effect(),
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

/// The failure of a join on a radio that could not be brought up, or whose driver stopped.
fn not_running() -> Failure {
    Failure { status: JoinStatus::UnspecifiedFailure, detail: String::from("the radio is not running") }
}

/// The kind a join that accepts both secured kinds takes from what the network offers: WPA3 when the
/// network names SAE and may take a WPA3 join, unless the radio has said its firmware does not run
/// SAE; else WPA2 when the network may take a WPA2 join; `None` when it offers neither. A fresh
/// join's scan and a recovery scan's mismatch choose by this one rule.
fn chosen_kind(wpa2: bool, wpa3: bool, names_sae: bool, radio_sae: Option<bool>) -> Option<Security> {
    if names_sae && wpa3 && radio_sae != Some(false) {
        Some(Security::Wpa3Sae)
    } else if wpa2 {
        Some(Security::Wpa2Psk)
    } else {
        None
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

fn invalid_length() -> Failure {
    Failure {
        status: JoinStatus::InvalidCredential,
        detail: String::from("the network name or secret has an invalid length for the selected security kind"),
    }
}

/// The kind and the plain line for a failed attempt of a `kind` join, as the driver reported it.
pub fn failure_of(failure: Option<JoinFailure>, kind: Security) -> Failure {
    let (status, detail) = match failure {
        Some(failure) if failure.refused_secret(kind) => (
            JoinStatus::InvalidCredential,
            match failure {
                JoinFailure::Association { auth_status, .. } => {
                    format!("WPA3 authentication failed (authentication status {auth_status})")
                }
                _ => String::from("the WPA2 handshake timed out (supplicant reason 15), the usual sign of a wrong passphrase"),
            },
        ),
        None => (JoinStatus::UnspecifiedFailure, String::from("the join failed, and the radio gave no reason")),
        Some(JoinFailure::NotFound) => (
            JoinStatus::NetworkNotAvailable,
            String::from("the network was not found: a scan did not see its name"),
        ),
        Some(JoinFailure::Association { status: 3, .. }) => (
            JoinStatus::NetworkNotAvailable,
            String::from("the network was not found: the radio's join scan saw no network of that name (association status 3)"),
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
