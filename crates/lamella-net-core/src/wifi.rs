//! The Wi-Fi seam: what a language runtime asks of a board's radio, and the plain values the answers
//! are made of. A program's Wi-Fi class library keeps every rule a program can see -- the argument
//! bounds, which kinds a join accepts, which exception a mistake raises -- and reaches the radio only
//! through [`WifiControl`], which a board's network backend lends through
//! [`NetBackend::wifi`](crate::NetBackend::wifi).
//!
//! Every call returns at once. A join is started, then followed through [`WifiControl::state`] until
//! it ends; the radio is serviced by the network backend meanwhile, so no call blocks the one thread
//! the runtime has.

use alloc::string::String;
use alloc::vec::Vec;

/// The longest network name: 32 bytes (IEEE Std 802.11-2024, 9.4.2.2).
pub const SSID_MAX: usize = 32;

/// The longest secret: a WPA3 password of 128 bytes. A WPA2 passphrase is at most 63 characters, or
/// 64 hexadecimal digits for the key itself.
pub const SECRET_MAX: usize = 128;

/// The security kinds, as bits. A join request carries the set of kinds it accepts; a report carries
/// the one kind in force.
pub mod security {
    /// No security: an open network, joined with no secret.
    pub const OPEN: u8 = 1;
    /// WPA2-Personal: a passphrase of 8 to 63 characters, or the 64-hexadecimal-digit key.
    pub const WPA2: u8 = 2;
    /// WPA3-Personal: a password of 1 to 128 bytes, used in the SAE exchange.
    pub const WPA3: u8 = 4;
    /// Every bit a request may carry.
    pub const ALL: u8 = OPEN | WPA2 | WPA3;
}

/// The link, as a program sees it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WifiLink {
    /// No network is held.
    Disconnected = 0,
    /// A join, or a re-join after the link was lost, is in progress.
    Connecting = 1,
    /// The link carries frames. Whether the board has an address on it is the network interface's
    /// report, not this one.
    Connected = 2,
}

/// How a join ended. The discriminants are those of `Windows.Devices.WiFi.WiFiConnectionStatus`, so
/// a program ported from Windows compares the same names and numbers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum JoinStatus {
    /// The join failed for a reason none of the other values names.
    UnspecifiedFailure = 0,
    /// The link is up.
    Success = 1,
    /// Access to the radio was withdrawn. No board reports it; it keeps the numbering.
    AccessRevoked = 2,
    /// The network refused the secret.
    InvalidCredential = 3,
    /// The network was not found.
    NetworkNotAvailable = 4,
    /// The join did not complete in the time allowed.
    Timeout = 5,
    /// The network offers no security kind the request accepts, or the radio cannot run the one it
    /// offers.
    UnsupportedAuthenticationProtocol = 6,
}

/// Where the credential of the network held came from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CredentialSource {
    /// No network is held.
    None = 0,
    /// The running program named the network.
    Program = 1,
    /// The network stored on the board.
    StoredRecord = 2,
    /// The network named when the firmware was built, which only a development build carries.
    BuildTime = 3,
}

/// What the radio does when the link is lost. The discriminants are those of
/// `Windows.Devices.WiFi.WiFiReconnectionKind`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Reconnection {
    /// Join the network again, resting between attempts, until the program disconnects.
    Automatic = 0,
    /// Let the network go: the program joins again when it chooses.
    Manual = 1,
}

impl Reconnection {
    /// The kind a seam integer names, or `None` for an integer that names none.
    #[must_use]
    pub fn from_code(code: i32) -> Option<Self> {
        match code {
            0 => Some(Reconnection::Automatic),
            1 => Some(Reconnection::Manual),
            _ => None,
        }
    }
}

/// What a stored network does when the board starts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BootConnection {
    /// Join in the background; the program starts at once.
    Background = 0,
    /// Join before the program starts, waiting a bounded time for the link and an address.
    BeforeMain = 1,
    /// Do not join; the program joins by calling for the stored network.
    OnConnect = 2,
}

impl BootConnection {
    /// The setting a seam integer names, or `None` for an integer that names none.
    #[must_use]
    pub fn from_code(code: i32) -> Option<Self> {
        match code {
            0 => Some(BootConnection::Background),
            1 => Some(BootConnection::BeforeMain),
            2 => Some(BootConnection::OnConnect),
            _ => None,
        }
    }
}

/// The radio, as far as a program needs to know it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RadioInfo {
    /// Whether a radio is present and running.
    pub present: bool,
    /// Whether the radio's firmware runs WPA3's SAE exchange; `None` until the firmware has said.
    pub wpa3: Option<bool>,
}

/// A join the program asks for. The name and the secret are borrowed for the call alone: the seam
/// copies what it keeps.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct JoinRequest<'a> {
    /// The network's name, 1 to [`SSID_MAX`] bytes.
    pub ssid: &'a [u8],
    /// The secret: empty for an open network.
    pub secret: &'a [u8],
    /// The kinds the join accepts, as [`security`] bits.
    pub security: u8,
    /// What the radio does if the link is lost.
    pub reconnection: Reconnection,
}

impl core::fmt::Debug for JoinRequest<'_> {
    /// The name's length and the secret's, never a byte of either.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("JoinRequest")
            .field("ssid_len", &self.ssid.len())
            .field("secret_len", &self.secret.len())
            .field("security", &self.security)
            .field("reconnection", &self.reconnection)
            .finish()
    }
}

/// A failed join: its kind, and one plain line saying what the radio reported.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Failure {
    /// The kind.
    pub status: JoinStatus,
    /// One line for a person to read. A program tests [`Failure::status`], never this.
    pub detail: String,
}

/// The radio's state at one moment.
#[derive(Clone, PartialEq, Eq)]
pub struct WifiState {
    /// The link.
    pub link: WifiLink,
    /// The name of the network held; empty when none is.
    pub ssid: Vec<u8>,
    /// The kind in force, as one [`security`] bit, once the join has chosen it; 0 before that and
    /// while no network is held.
    pub security: u8,
    /// Where the credential of the network held came from.
    pub source: CredentialSource,
    /// The signal strength in dBm, measured when the network was joined, where the radio reported it.
    pub rssi: Option<i16>,
    /// The channel, where the radio reported it.
    pub channel: Option<u8>,
    /// The access point's address, once the link has been up.
    pub bssid: Option<[u8; 6]>,
    /// The last join that failed, until a join succeeds.
    pub last_failure: Option<Failure>,
    /// The number of the last join started; 0 before the first.
    pub join: u32,
    /// How that join ended; `None` while it is still in progress.
    pub join_outcome: Option<JoinStatus>,
}

impl core::fmt::Debug for WifiState {
    /// The name's length, never the name.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("WifiState")
            .field("link", &self.link)
            .field("ssid_len", &self.ssid.len())
            .field("security", &self.security)
            .field("source", &self.source)
            .field("rssi", &self.rssi)
            .field("channel", &self.channel)
            .field("bssid", &self.bssid)
            .field("last_failure", &self.last_failure)
            .field("join", &self.join)
            .field("join_outcome", &self.join_outcome)
            .finish()
    }
}

/// The network stored on the board, as a program may read it: never the secret.
#[derive(Clone, PartialEq, Eq)]
pub struct StoredNetwork {
    /// The network's name.
    pub ssid: Vec<u8>,
    /// The kinds a join from the record accepts, as [`security`] bits.
    pub security: u8,
    /// What the radio does if the link is lost.
    pub reconnection: Reconnection,
    /// What the network does when the board starts.
    pub boot: BootConnection,
}

impl core::fmt::Debug for StoredNetwork {
    /// The name's length, never the name.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("StoredNetwork")
            .field("ssid_len", &self.ssid.len())
            .field("security", &self.security)
            .field("reconnection", &self.reconnection)
            .field("boot", &self.boot)
            .finish()
    }
}

/// What a write to the stored record did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RecordWrite {
    /// The record now holds the new values, and the values it held before are erased.
    Written,
    /// The record already held these values, so the storage was not written.
    Unchanged,
    /// No network is stored, so there was nothing to change.
    NoRecord,
    /// The storage did not take the write; the record is as it was.
    Failed,
}

/// A board's radio, as a language runtime reaches it.
///
/// Every method returns at once; nothing here waits on the radio. A join is followed through
/// [`WifiControl::state`], whose `join` and `join_outcome` say which join the report describes and
/// whether it has ended.
pub trait WifiControl {
    /// The radio.
    fn radio(&mut self) -> RadioInfo;

    /// The link, cheaply: what a network backend reads on every pass.
    fn link(&self) -> WifiLink;

    /// Starts a join of the network `request` names, dropping any network held first. Returns the
    /// join's number, which a later [`WifiControl::state`] names while it reports on this join.
    ///
    /// The request has been checked by the caller: a name of 1 to [`SSID_MAX`] bytes, a secret within
    /// the bounds of every kind it accepts, at least one kind, and no secured kind beside an open one.
    fn join_start(&mut self, request: JoinRequest<'_>) -> u32;

    /// Starts a join of the stored network, as [`WifiControl::join_start`] does; `None` when no
    /// network is stored.
    fn join_stored(&mut self) -> Option<u32>;

    /// Drops the network held, and ends a join in progress with the failure it last met, or
    /// [`JoinStatus::Timeout`] when it met none. The secret the radio held for it is erased.
    fn disconnect(&mut self);

    /// The radio's state now.
    fn state(&mut self) -> WifiState;

    /// The stored network, or `None` when none is stored.
    fn record_read(&mut self) -> Option<StoredNetwork>;

    /// Stores `network`, keeping the boot setting of a network already stored and taking
    /// [`BootConnection::Background`] otherwise. The values stored before are erased once the new ones
    /// are written; a write of the values already stored writes nothing.
    fn record_write(&mut self, network: JoinRequest<'_>) -> RecordWrite;

    /// Changes the boot setting of the stored network.
    fn record_set_boot(&mut self, boot: BootConnection) -> RecordWrite;

    /// Erases the stored network. Returns whether the storage now holds none.
    fn record_clear(&mut self) -> bool;
}
