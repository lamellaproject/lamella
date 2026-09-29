//! The join: the credentials the caller stages, the request sequence that
//! configures the network's kind and triggers the association, the
//! failure classes, and the link's states.
//!
//! A join is ten steps in the firmware's order: the infrastructure mode,
//! the on-chip supplicant enabled or not, the authentication mode, the
//! cipher, the authentication type, the protection of management frames
//! on a WPA3 network, an unconditional disassociation, the secret on a
//! secured network (the passphrase of a WPA2 network; the password of a
//! WPA3 network, whose SAE exchange the firmware runs), the network name
//! that triggers the association, and the wait on the completing event.
//! The association and the usable link are two decisions: on a secured
//! network (WPA2 or WPA3) the link is usable once both the supplicant's
//! word and the firmware's own association success have arrived, in
//! either order, and a short hold after the supplicant's word has passed
//! (a frame written within a moment of that word is lost on this
//! firmware, on both kinds); on an open network the association
//! completes the join. A lost link and a failed
//! join are the same situation -- not associated, should be -- and the
//! driver returns to a scan and re-enters at the disassociation step with
//! the secret re-supplied, until the caller disconnects; a recovery scan
//! that finds the network advertising no suite the credential can use
//! ends the join with that reason. The caller's slices are held by
//! reference and copied into nothing but the frame being built; the
//! secret types print no byte of themselves.

use core::fmt;

use crate::control::{IFACE_STATION, Payload, Request, cmd, var};
use crate::scan::{Advertised, SSID_MAX};

/// The shortest passphrase.
pub const PASSPHRASE_MIN: usize = 8;
/// The longest passphrase.
pub const PASSPHRASE_MAX: usize = 64;
/// The shortest WPA3 password: one byte.
pub const SAE_PASSWORD_MIN: usize = 1;
/// The longest WPA3 password: the password structure's capacity.
pub const SAE_PASSWORD_MAX: usize = 128;
/// The password structure's length: the password's length as a 16-bit
/// word, then 128 bytes of password.
pub const SAE_PASSWORD_LEN: usize = 130;
/// The key structure's length: the key's length, the flags, 64 bytes of
/// key.
pub const KEY_LEN: usize = 68;
/// The SSID structure's length: the length word, 32 bytes of SSID.
pub const SSID_STRUCT_LEN: usize = 36;
/// The disassociation value's length, sent zeroed.
pub const DISASSOC_LEN: usize = 12;
/// The key structure's flag: the key is an ASCII passphrase the chip
/// derives the key from.
pub const KEY_FLAG_PASSPHRASE: u16 = 0x0001;
/// The infrastructure mode.
pub const INFRASTRUCTURE: u32 = 1;
/// The open-system authentication type (IEEE Std 802.11-2024, 9.4.1.1:
/// the authentication algorithm number 0).
pub const AUTH_OPEN_SYSTEM: u32 = 0;
/// The SAE authentication type (IEEE Std 802.11-2024, 9.4.1.1: the
/// authentication algorithm number 3).
pub const AUTH_SAE: u32 = 3;

/// Authentication modes.
pub mod auth_mode {
    /// None: an open network.
    pub const DISABLED: u32 = 0x0000;
    /// WPA with a pre-shared key.
    pub const WPA_PSK: u32 = 0x0004;
    /// WPA2 with a pre-shared key.
    pub const WPA2_PSK: u32 = 0x0080;
    /// WPA3 with a password: SAE, the firmware running the exchange.
    pub const WPA3_SAE: u32 = 0x0004_0000;
}

/// Ciphers.
pub mod cipher {
    /// None: an open network.
    pub const NONE: u32 = 0x0000;
    /// WEP.
    pub const WEP: u32 = 0x0001;
    /// TKIP.
    pub const TKIP: u32 = 0x0002;
    /// AES (CCMP).
    pub const AES: u32 = 0x0004;
}

/// The protection of management frames: the values of the `mfp` variable.
pub mod mfp {
    /// None.
    pub const NONE: u32 = 0;
    /// Capable: protected when the network protects them.
    pub const CAPABLE: u32 = 1;
    /// Required: a network that does not protect them is refused.
    pub const REQUIRED: u32 = 2;
}

/// The first step of the sequence.
pub const STEP_FIRST: u8 = 1;
/// The step of the protection setting.
pub const STEP_PROTECTION: u8 = 6;
/// The first step of a re-entry: the disassociation.
pub const STEP_REENTRY: u8 = 7;
/// The last step: the network name, the trigger.
pub const STEP_LAST: u8 = 9;

/// The stage name of the infrastructure mode refused.
pub const STAGE_INFRA: &str = "infrastructure mode";
/// The stage name of the supplicant setting refused.
pub const STAGE_SUPPLICANT: &str = "supplicant";
/// The stage name of the authentication mode refused.
pub const STAGE_AUTH_MODE: &str = "authentication mode";
/// The stage name of the cipher refused.
pub const STAGE_CIPHER: &str = "cipher";
/// The stage name of the authentication type refused.
pub const STAGE_AUTH_TYPE: &str = "authentication type";
/// The stage name of the protection setting refused.
pub const STAGE_PROTECTION: &str = "management frame protection";
/// The stage name of the disassociation refused.
pub const STAGE_DISASSOCIATE: &str = "disassociate";
/// The stage name of the passphrase refused.
pub const STAGE_PASSPHRASE: &str = "passphrase";
/// The stage name of the password refused.
pub const STAGE_PASSWORD: &str = "password";
/// The stage name of the network name refused.
pub const STAGE_NETWORK_NAME: &str = "network name";
/// The stage name of the disconnect refused.
pub const STAGE_DISCONNECT: &str = "disconnect";

/// The kind of security a join asks the firmware for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Security {
    /// No security: an open network.
    Open,
    /// WPA2 with a pre-shared key derived from a passphrase.
    Wpa2Psk,
    /// WPA3 with a password: SAE, the firmware running the exchange,
    /// management frames protected.
    Wpa3Sae,
}

/// The credential the caller stages for a join: none, a WPA2 passphrase
/// or a WPA3 password, each a slice held by reference. Printing it prints
/// the kind and the length, never a byte.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Credential<'b> {
    /// An open network.
    Open,
    /// A WPA2-PSK network's passphrase, 8 to 64 bytes.
    Passphrase(&'b [u8]),
    /// A WPA3-SAE network's password, 1 to 128 bytes.
    SaePassword(&'b [u8]),
}

impl fmt::Debug for Credential<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Credential::Open => f.write_str("Open"),
            Credential::Passphrase(bytes) => f
                .debug_struct("Passphrase")
                .field("len", &bytes.len())
                .finish(),
            Credential::SaePassword(bytes) => f
                .debug_struct("SaePassword")
                .field("len", &bytes.len())
                .finish(),
        }
    }
}

/// A passphrase the caller stages: 8 to 64 bytes, held by reference.
/// Printing it prints its length alone.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Passphrase<'b> {
    bytes: &'b [u8],
}

impl<'b> Passphrase<'b> {
    /// A passphrase of 8 to 64 bytes; `None` outside the bounds.
    pub const fn new(bytes: &'b [u8]) -> Option<Self> {
        if bytes.len() < PASSPHRASE_MIN || bytes.len() > PASSPHRASE_MAX {
            return None;
        }
        Some(Passphrase { bytes })
    }

    /// The passphrase's length.
    pub const fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Never: a passphrase has at least eight bytes.
    pub const fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    pub(crate) const fn as_bytes(&self) -> &'b [u8] {
        self.bytes
    }
}

impl fmt::Debug for Passphrase<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Passphrase")
            .field("len", &self.bytes.len())
            .finish()
    }
}

/// A WPA3 password the caller stages: 1 to 128 bytes, held by reference,
/// the octets of the password as the standard represents it (IEEE Std
/// 802.11-2024, 12.4.3: a UTF-8 string). Printing it prints its length
/// alone.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct SaePassword<'b> {
    bytes: &'b [u8],
}

impl<'b> SaePassword<'b> {
    /// A password of 1 to 128 bytes; `None` outside the bounds.
    pub const fn new(bytes: &'b [u8]) -> Option<Self> {
        if bytes.len() < SAE_PASSWORD_MIN || bytes.len() > SAE_PASSWORD_MAX {
            return None;
        }
        Some(SaePassword { bytes })
    }

    /// The password's length.
    pub const fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Never: a password has at least one byte.
    pub const fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    pub(crate) const fn as_bytes(&self) -> &'b [u8] {
        self.bytes
    }
}

impl fmt::Debug for SaePassword<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SaePassword")
            .field("len", &self.bytes.len())
            .finish()
    }
}

/// The secret a network holds, by its kind.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Secret<'b> {
    None,
    Passphrase(Passphrase<'b>),
    Sae(SaePassword<'b>),
}

/// A network the caller names: its SSID of 1 to 32 bytes and, on a
/// secured network, its passphrase or its password; all held by reference.
/// Printing it prints the SSID's length and the security's kind.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Network<'b> {
    ssid: &'b [u8],
    secret: Secret<'b>,
}

impl<'b> Network<'b> {
    /// A WPA2-PSK network with a passphrase, or an open network without;
    /// `None` for an SSID outside 1 to 32 bytes or a passphrase outside 8
    /// to 64.
    pub const fn new(ssid: &'b [u8], passphrase: Option<&'b [u8]>) -> Option<Self> {
        match passphrase {
            None => Self::with(ssid, Credential::Open),
            Some(bytes) => Self::with(ssid, Credential::Passphrase(bytes)),
        }
    }

    /// A network with a credential of any kind; `None` for an SSID outside
    /// 1 to 32 bytes, a passphrase outside 8 to 64 or a password outside 1
    /// to 128.
    pub const fn with(ssid: &'b [u8], credential: Credential<'b>) -> Option<Self> {
        if ssid.is_empty() || ssid.len() > SSID_MAX {
            return None;
        }
        let secret = match credential {
            Credential::Open => Secret::None,
            Credential::Passphrase(bytes) => match Passphrase::new(bytes) {
                Some(passphrase) => Secret::Passphrase(passphrase),
                None => return None,
            },
            Credential::SaePassword(bytes) => match SaePassword::new(bytes) {
                Some(password) => Secret::Sae(password),
                None => return None,
            },
        };
        Some(Network { ssid, secret })
    }

    /// The SSID.
    pub const fn ssid(&self) -> &'b [u8] {
        self.ssid
    }

    /// The passphrase, on a WPA2-PSK network.
    pub const fn passphrase(&self) -> Option<Passphrase<'b>> {
        match self.secret {
            Secret::Passphrase(passphrase) => Some(passphrase),
            _ => None,
        }
    }

    /// The password, on a WPA3-SAE network.
    pub const fn sae_password(&self) -> Option<SaePassword<'b>> {
        match self.secret {
            Secret::Sae(password) => Some(password),
            _ => None,
        }
    }

    /// The security's kind.
    pub const fn kind(&self) -> Security {
        match self.secret {
            Secret::None => Security::Open,
            Secret::Passphrase(_) => Security::Wpa2Psk,
            Secret::Sae(_) => Security::Wpa3Sae,
        }
    }

    /// Whether a secret is held: a WPA2-PSK or a WPA3-SAE join, whose
    /// usable link waits on the supplicant's word, on the firmware's
    /// association success and on a short hold after the supplicant's word.
    pub const fn is_secured(&self) -> bool {
        !matches!(self.secret, Secret::None)
    }
}

impl fmt::Debug for Network<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Network")
            .field("ssid_len", &self.ssid.len())
            .field("kind", &self.kind())
            .finish()
    }
}

/// The 68-byte key structure: the passphrase's length, the passphrase
/// flag, the passphrase then zeros.
pub fn key_structure(passphrase: Passphrase<'_>) -> [u8; KEY_LEN] {
    let bytes = passphrase.as_bytes();
    let mut key = [0u8; KEY_LEN];
    key[0..2].copy_from_slice(&(bytes.len() as u16).to_le_bytes());
    key[2..4].copy_from_slice(&KEY_FLAG_PASSPHRASE.to_le_bytes());
    key[4..4 + bytes.len()].copy_from_slice(bytes);
    key
}

/// The 130-byte password structure: the password's length as a 16-bit
/// word, the password then zeros.
pub fn sae_password_structure(password: SaePassword<'_>) -> [u8; SAE_PASSWORD_LEN] {
    let bytes = password.as_bytes();
    let mut s = [0u8; SAE_PASSWORD_LEN];
    s[0..2].copy_from_slice(&(bytes.len() as u16).to_le_bytes());
    s[2..2 + bytes.len()].copy_from_slice(bytes);
    s
}

/// The 36-byte SSID structure: the length as a 32-bit word, the SSID then
/// zeros; an SSID over 32 bytes is cut to 32.
pub fn ssid_structure(ssid: &[u8]) -> [u8; SSID_STRUCT_LEN] {
    let n = ssid.len().min(SSID_MAX);
    let mut s = [0u8; SSID_STRUCT_LEN];
    s[0..4].copy_from_slice(&(n as u32).to_le_bytes());
    s[4..4 + n].copy_from_slice(&ssid[..n]);
    s
}

/// The request of step `step` (1 to 9) of the join sequence for
/// `network`; `None` past the last step, for the protection step of a
/// network that is not WPA3 unless `protection_reset` says a WPA3 join
/// earlier on this attach left the setting on (then the word 0 returns it
/// to none), and for the secret step of an open network.
pub fn request<'b>(network: Network<'b>, step: u8, protection_reset: bool) -> Option<Request<'b>> {
    let kind = network.kind();
    let secured = network.is_secured();
    let sae = kind == Security::Wpa3Sae;
    let word = |stage: &'static str, command: u32, value: u32| Request {
        stage,
        set: true,
        cmd: command,
        name: b"",
        payload: Payload::Word(value),
    };
    Some(match step {
        1 => word(STAGE_INFRA, cmd::SET_INFRA, INFRASTRUCTURE),
        2 => Request {
            stage: STAGE_SUPPLICANT,
            set: true,
            cmd: cmd::SET_VAR,
            name: var::SUP_WPA,
            payload: Payload::Words(IFACE_STATION, u32::from(secured)),
        },
        3 => word(
            STAGE_AUTH_MODE,
            cmd::SET_WPA_AUTH,
            match kind {
                Security::Open => auth_mode::DISABLED,
                Security::Wpa2Psk => auth_mode::WPA2_PSK,
                Security::Wpa3Sae => auth_mode::WPA3_SAE,
            },
        ),
        4 => word(
            STAGE_CIPHER,
            cmd::SET_WSEC,
            if secured { cipher::AES } else { cipher::NONE },
        ),
        5 => word(
            STAGE_AUTH_TYPE,
            cmd::SET_AUTH,
            if sae { AUTH_SAE } else { AUTH_OPEN_SYSTEM },
        ),
        6 => {
            let value = if sae {
                mfp::REQUIRED
            } else if protection_reset {
                mfp::NONE
            } else {
                return None;
            };
            Request {
                stage: STAGE_PROTECTION,
                set: true,
                cmd: cmd::SET_VAR,
                name: var::MFP,
                payload: Payload::Word(value),
            }
        }
        7 => Request {
            stage: STAGE_DISASSOCIATE,
            set: true,
            cmd: cmd::DISASSOC,
            name: b"",
            payload: Payload::Zeros(DISASSOC_LEN),
        },
        8 => match network.secret {
            Secret::None => return None,
            Secret::Passphrase(passphrase) => Request {
                stage: STAGE_PASSPHRASE,
                set: true,
                cmd: cmd::SET_WSEC_PMK,
                name: b"",
                payload: Payload::Key(passphrase),
            },
            Secret::Sae(password) => Request {
                stage: STAGE_PASSWORD,
                set: true,
                cmd: cmd::SET_VAR,
                name: var::SAE_PASSWORD,
                payload: Payload::SaePassword(password),
            },
        },
        9 => Request {
            stage: STAGE_NETWORK_NAME,
            set: true,
            cmd: cmd::SET_SSID,
            name: b"",
            payload: Payload::Ssid(network.ssid()),
        },
        _ => return None,
    })
}

/// The disconnect: the disassociation with the zeroed value.
pub const DISCONNECT: Request<'static> = Request {
    stage: STAGE_DISCONNECT,
    set: true,
    cmd: cmd::DISASSOC,
    name: b"",
    payload: Payload::Zeros(DISASSOC_LEN),
};

/// Why a join attempt failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinFailure {
    /// The association failed: the network-name event's status (3: the
    /// firmware's own join scan found no matching network), the last
    /// association event's status (5: not acknowledged, the end of the
    /// firmware's own retries; 0 when none came) and the last
    /// authentication event's status (where a WPA3 password that does not
    /// match fails; 0 when none came).
    Association {
        /// The status of the network-name event.
        status: u32,
        /// The status of the last association event, or 0.
        assoc_status: u32,
        /// The status of the last authentication event, or 0.
        auth_status: u32,
    },
    /// The supplicant failed: the reason, bare (15: the handshake timed
    /// out, the usual sign of a wrong passphrase; 14: the supplicant saw a
    /// deauthentication).
    Supplicant {
        /// The supplicant's reason.
        reason: u32,
    },
    /// A loss event arrived during the attempt: its number and reason.
    Lost {
        /// The event number.
        number: u32,
        /// The event's reason.
        reason: u32,
    },
    /// No completing event came in time.
    Timeout,
    /// A recovery scan did not see the network.
    NotFound,
    /// A recovery scan saw the network advertising no suite the
    /// credential can use: the network is detached, since the join cannot
    /// succeed until the credential or the network changes.
    Mismatch {
        /// What the network advertised.
        advertised: Advertised,
    },
}

/// The link's state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkState {
    /// No network is held.
    Detached,
    /// A network is held and the link is not up: a recovery cycle runs
    /// after the rest.
    Down,
    /// A join attempt is in flight.
    Joining,
    /// The link is up.
    Up,
}
