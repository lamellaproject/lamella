//! The scan: the parameters of an enhanced scan, the walk through a scan
//! result to the networks it lists, and the record handed to the caller.
//!
//! A scan is started by a set of the `escan` variable carrying 74 bytes of
//! parameters; its results arrive as scan-result events whose payload is a
//! 12-byte head (the buffer length, the version, the sync id the request
//! carried, the record count) followed by one record per network seen,
//! each record stepped by its own length field. The fields at stable
//! offsets -- the version, the length, the BSSID, the beacon period, the
//! capability, the SSID -- are read from every record; the fields past the
//! natural-alignment padding -- the RSSI, the control channel, the offset
//! and length of the record's element block -- are read when the record's
//! version is 109, the layout this crate pins, and the channel is taken
//! from the DSSS Parameter Set element of that block (IEEE Std
//! 802.11-2024, 9.4.2.4, Figure 9-211) when it holds one, from the control
//! channel field otherwise. The same walk reads what the network
//! advertises about its security from the block's RSN element (9.4.2.23:
//! the authentication and key management suites, the pairwise cipher, the
//! protection of management frames) and its RSN extension element
//! (9.4.2.240: the hash-to-element method of SAE). A scan completes by the
//! event's status: partial means more results follow, success means done,
//! any other status ends it.

use crate::link::Security;

/// The parameters' length in bytes: the version, the action, the sync id
/// and the 66-byte scan parameters, whose one-element channel list is
/// sent.
pub const PARAMS_LEN: usize = 74;
/// The result head's length in bytes.
pub const RESULT_HEAD_LEN: usize = 12;
/// The bytes of a record through its SSID: the fields at stable offsets.
pub const RECORD_HEAD_LEN: usize = 51;
/// The fixed part of a version-109 record, before its element block.
pub const RECORD_FIXED_LEN: usize = 128;
/// The record version whose tail layout this crate reads.
pub const RECORD_VERSION: u32 = 109;
/// The longest SSID.
pub const SSID_MAX: usize = 32;

/// The scan request's fields.
pub mod request {
    /// The parameters' version.
    pub const VERSION: u32 = 1;
    /// The action: start a scan.
    pub const START: u16 = 1;
    /// The action: continue a scan.
    pub const CONTINUE: u16 = 2;
    /// The action: abort a scan.
    pub const ABORT: u16 = 3;
    /// The BSS type: any.
    pub const BSS_ANY: u8 = 2;
    /// The scan type: active.
    pub const ACTIVE: u8 = 0;
    /// The scan type: passive.
    pub const PASSIVE: u8 = 1;
    /// The firmware's default for a probe count or a dwell time.
    pub const DEFAULT: i32 = -1;
}

/// Bits of a record's capability field (IEEE Std 802.11-2024, 9.4.1.4,
/// Figure 9-140).
pub mod capability {
    /// An infrastructure network.
    pub const ESS: u16 = 1 << 0;
    /// An independent network.
    pub const IBSS: u16 = 1 << 1;
    /// Data confidentiality is required: a secured network.
    pub const PRIVACY: u16 = 1 << 4;
}

/// Element identifiers (IEEE Std 802.11-2024, 9.4.2.1, Table 9-130).
pub mod element {
    /// The SSID element (9.4.2.2).
    pub const SSID: u8 = 0;
    /// The Supported Rates and BSS Membership Selectors element (9.4.2.3).
    pub const SUPPORTED_RATES: u8 = 1;
    /// The DSSS Parameter Set element (9.4.2.4): its one byte is the
    /// current channel.
    pub const DSSS_PARAMETER_SET: u8 = 3;
    /// The RSN element (9.4.2.23): the network's security suites and its
    /// protection of management frames.
    pub const RSN: u8 = 48;
    /// The RSN extension element (9.4.2.240): bit 5 of its first byte says
    /// the network supports the hash-to-element method of SAE.
    pub const RSNXE: u8 = 244;
}

/// The suite selectors under the OUI `00-0F-AC` (IEEE Std 802.11-2024,
/// 9.4.2.23.3, Table 9-190 for the authentication and key management
/// suites; 9.4.2.23.2 for the cipher suites): the suffix byte of each.
pub mod akm {
    /// The suite selectors' OUI.
    pub const OUI: [u8; 3] = [0x00, 0x0F, 0xAC];
    /// Authentication over IEEE 802.1X.
    pub const DOT1X: u8 = 1;
    /// A pre-shared key.
    pub const PSK: u8 = 2;
    /// Fast transition over IEEE 802.1X.
    pub const FT_DOT1X: u8 = 3;
    /// Fast transition with a pre-shared key.
    pub const FT_PSK: u8 = 4;
    /// IEEE 802.1X with SHA-256.
    pub const DOT1X_SHA256: u8 = 5;
    /// A pre-shared key with SHA-256.
    pub const PSK_SHA256: u8 = 6;
    /// SAE: a password, the exchange of 12.4.
    pub const SAE: u8 = 8;
    /// Fast transition over SAE.
    pub const FT_SAE: u8 = 9;
    /// IEEE 802.1X with Suite B.
    pub const DOT1X_SUITE_B: u8 = 11;
    /// IEEE 802.1X with Suite B and 192-bit keys.
    pub const DOT1X_SUITE_B_192: u8 = 12;
    /// Fast transition over IEEE 802.1X with SHA-384.
    pub const FT_DOT1X_SHA384: u8 = 13;
    /// SAE with the group-dependent hash.
    pub const SAE_EXT: u8 = 24;
    /// Fast transition over SAE with the group-dependent hash.
    pub const FT_SAE_EXT: u8 = 25;
    /// The cipher suite selector's suffix for CCMP-128.
    pub const CIPHER_CCMP: u8 = 4;
}

/// What a scan record advertises about the network's security, read from
/// its RSN element (IEEE Std 802.11-2024, 9.4.2.23) and its RSN extension
/// element (9.4.2.240): the authentication suites, the pairwise cipher,
/// the protection of management frames, the hash-to-element method. All
/// false when the record carries no readable element block; `privacy` is
/// the capability field's bit restated.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Advertised {
    /// The record carries an RSN element of version 1.
    pub rsne: bool,
    /// The Privacy bit of the capability field.
    pub privacy: bool,
    /// CCMP-128 among the pairwise cipher suites.
    pub ccmp: bool,
    /// A pre-shared key among the suites: WPA2-Personal.
    pub psk: bool,
    /// SAE among the suites: WPA3-Personal.
    pub sae: bool,
    /// Fast transition over SAE among the suites.
    pub ft_sae: bool,
    /// Authentication over IEEE 802.1X among the suites.
    pub dot1x: bool,
    /// A suite this crate does not name among them.
    pub other_akm: bool,
    /// Protection of management frames enabled: the MFPC bit.
    pub mfp_capable: bool,
    /// Protection of management frames mandatory: the MFPR bit.
    pub mfp_required: bool,
    /// The hash-to-element method of SAE supported: the RSN extension
    /// element's bit.
    pub h2e: bool,
}

impl Advertised {
    /// Whether the network may take a join of `kind`: an open join needs
    /// no privacy; a WPA2 join needs privacy and, when an RSN element was
    /// read, a pre-shared key suite in it; a WPA3 join needs privacy and,
    /// when an RSN element was read, an SAE suite in it. Without a
    /// readable RSN element the walk cannot say the join will fail, so
    /// the firmware's attempt decides.
    pub const fn compatible(&self, kind: Security) -> bool {
        match kind {
            Security::Open => !self.privacy,
            Security::Wpa2Psk => self.privacy && (!self.rsne || self.psk),
            Security::Wpa3Sae => self.privacy && (!self.rsne || self.sae),
        }
    }
}

/// The 74 parameter bytes of an active scan of every channel for any
/// network, tagged with `sync`, which every result of the scan echoes.
pub fn params(sync: u16) -> [u8; PARAMS_LEN] {
    let mut p = [0u8; PARAMS_LEN];
    p[0..4].copy_from_slice(&request::VERSION.to_le_bytes());
    p[4..6].copy_from_slice(&request::START.to_le_bytes());
    p[6..8].copy_from_slice(&sync.to_le_bytes());
    p[44..50].fill(0xFF);
    p[50] = request::BSS_ANY;
    p[51] = request::ACTIVE;
    for at in [52, 56, 60, 64] {
        p[at..at + 4].copy_from_slice(&request::DEFAULT.to_le_bytes());
    }
    p
}

/// The head of a scan result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Head {
    /// The buffer length the firmware reports.
    pub buflen: u32,
    /// The result's version.
    pub version: u32,
    /// The sync id the scan request carried.
    pub sync: u16,
    /// The record count the firmware reports; the bytes present decide.
    pub count: u16,
}

/// The head of a scan result's payload, or `None` for fewer than twelve
/// bytes.
pub fn head(payload: &[u8]) -> Option<Head> {
    if payload.len() < RESULT_HEAD_LEN {
        return None;
    }
    let word = |at: usize| {
        u32::from_le_bytes([
            payload[at],
            payload[at + 1],
            payload[at + 2],
            payload[at + 3],
        ])
    };
    Some(Head {
        buflen: word(0),
        version: word(4),
        sync: u16::from_le_bytes([payload[8], payload[9]]),
        count: u16::from_le_bytes([payload[10], payload[11]]),
    })
}

/// One network a scan saw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanRecord {
    /// The network's BSSID.
    pub bssid: [u8; 6],
    /// The SSID's bytes, zero past `ssid_len`.
    pub ssid: [u8; SSID_MAX],
    /// The SSID's length.
    pub ssid_len: u8,
    /// The capability field.
    pub capability: u16,
    /// The beacon period, in units of 1,024 microseconds.
    pub beacon_period: u16,
    /// The channel: from the DSSS Parameter Set element when the record
    /// carries one, else the control channel field; 0 when unknown.
    pub channel: u8,
    /// The RSSI in dBm; 0 when the record's version is not the one whose
    /// tail this crate reads.
    pub rssi: i16,
    /// The record's version.
    pub version: u32,
    /// The record's length as it reports it.
    pub length: u32,
    /// What the network advertises about its security, from the record's
    /// RSN element and RSN extension element; the Privacy bit alone when
    /// the record carries no readable element block.
    pub security: Advertised,
}

impl ScanRecord {
    /// The SSID.
    pub fn ssid(&self) -> &[u8] {
        &self.ssid[..usize::from(self.ssid_len).min(SSID_MAX)]
    }

    /// Whether the network requires data confidentiality: the Privacy bit.
    pub const fn is_secured(&self) -> bool {
        self.capability & capability::PRIVACY != 0
    }

    /// Whether the network is an infrastructure network: the ESS bit.
    pub const fn is_ess(&self) -> bool {
        self.capability & capability::ESS != 0
    }
}

/// How a scan ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanEnd {
    /// The firmware reported the scan complete.
    Complete,
    /// The firmware ended the scan with this status.
    Ended(u32),
    /// No completing event came in time.
    TimedOut,
}

impl ScanEnd {
    /// The end a terminal event status means: success is complete, any
    /// other status ends the scan by its value.
    pub const fn of_status(status: u32) -> Self {
        if status == crate::event::status::SUCCESS {
            ScanEnd::Complete
        } else {
            ScanEnd::Ended(status)
        }
    }
}

/// One record walked: the record, the offset in the payload past its bytes
/// present, and the offset of the next record when one can follow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Walked {
    /// The record.
    pub record: ScanRecord,
    /// The offset past the record's bytes present.
    pub end: usize,
    /// The next record's offset, when the record's length ends inside the
    /// payload.
    pub next: Option<usize>,
}

/// A suite count field at `at` of an RSN element's body.
fn suite_count(body: &[u8], at: usize) -> Option<usize> {
    body.get(at..at + 2)
        .map(|c| usize::from(u16::from_le_bytes([c[0], c[1]])))
}

/// The RSN element's body read into `security`: the version must be 1;
/// then the group data cipher suite, the pairwise cipher suites, the
/// authentication and key management suites and the capabilities, each
/// optional, the reading stopping at the first field the length does not
/// cover and keeping what it read.
fn rsn(body: &[u8], security: &mut Advertised) {
    if body.len() < 2 || u16::from_le_bytes([body[0], body[1]]) != 1 {
        return;
    }
    security.rsne = true;
    let mut at = 2 + 4;
    let Some(count) = suite_count(body, at) else {
        return;
    };
    at += 2;
    for _ in 0..count {
        let Some(suite) = body.get(at..at + 4) else {
            return;
        };
        if suite[..3] == akm::OUI && suite[3] == akm::CIPHER_CCMP {
            security.ccmp = true;
        }
        at += 4;
    }
    let Some(count) = suite_count(body, at) else {
        return;
    };
    at += 2;
    for _ in 0..count {
        let Some(suite) = body.get(at..at + 4) else {
            return;
        };
        if suite[..3] == akm::OUI {
            match suite[3] {
                akm::PSK | akm::PSK_SHA256 => security.psk = true,
                akm::SAE | akm::SAE_EXT => security.sae = true,
                akm::FT_SAE | akm::FT_SAE_EXT => security.ft_sae = true,
                akm::DOT1X
                | akm::FT_DOT1X
                | akm::DOT1X_SHA256
                | akm::DOT1X_SUITE_B
                | akm::DOT1X_SUITE_B_192
                | akm::FT_DOT1X_SHA384 => security.dot1x = true,
                _ => security.other_akm = true,
            }
        } else {
            security.other_akm = true;
        }
        at += 4;
    }
    let Some(caps) = body.get(at..at + 2) else {
        return;
    };
    let caps = u16::from_le_bytes([caps[0], caps[1]]);
    security.mfp_required = caps & (1 << 6) != 0;
    security.mfp_capable = caps & (1 << 7) != 0;
}

/// The readings of an element block in one pass, each element bounded by
/// its own length and the block's: the channel from the first DSSS
/// Parameter Set element of at least one byte, and the security the RSN
/// element and the RSN extension element advertise.
fn elements(block: &[u8], privacy: bool) -> (Option<u8>, Advertised) {
    let mut channel = None;
    let mut security = Advertised {
        privacy,
        ..Advertised::default()
    };
    let mut at = 0usize;
    while at + 2 <= block.len() {
        let id = block[at];
        let len = usize::from(block[at + 1]);
        if at + 2 + len > block.len() {
            break;
        }
        let body = &block[at + 2..at + 2 + len];
        match id {
            element::DSSS_PARAMETER_SET if len >= 1 && channel.is_none() => channel = Some(body[0]),
            element::RSN => rsn(body, &mut security),
            element::RSNXE if len >= 1 => security.h2e = body[0] & (1 << 5) != 0,
            _ => {}
        }
        at += 2 + len;
    }
    (channel, security)
}

/// Walk the record at `at` of a result payload: the fields at stable
/// offsets read; on version 109 the RSSI, the control channel and the
/// element block read too, the channel from the DSSS Parameter Set element
/// when the block holds one and the advertised security from the RSN
/// elements; `None` when fewer than 51 bytes are present or
/// the record's length is under 51. A record whose length runs past the
/// payload is delivered with the bytes present and is the last: the step to
/// the next record is a checked add bounded by the payload's length, so a
/// length near the 32-bit limit ends the walk on a 32-bit target as it does
/// on a 64-bit one.
pub fn walk(payload: &[u8], at: usize) -> Option<Walked> {
    let bytes = payload.get(at..)?;
    if bytes.len() < RECORD_HEAD_LEN {
        return None;
    }
    let word = |o: usize| u32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]]);
    let half = |o: usize| u16::from_le_bytes([bytes[o], bytes[o + 1]]);
    let version = word(0);
    let length = word(4);
    let length_bytes = length as usize;
    if length_bytes < RECORD_HEAD_LEN {
        return None;
    }
    let extent = length_bytes.min(bytes.len());
    let mut bssid = [0u8; 6];
    bssid.copy_from_slice(&bytes[8..14]);
    let ssid_len = bytes[18].min(SSID_MAX as u8);
    let mut ssid = [0u8; SSID_MAX];
    ssid[..usize::from(ssid_len)].copy_from_slice(&bytes[19..19 + usize::from(ssid_len)]);
    let capability = half(16);
    let (mut rssi, mut channel) = (0i16, 0u8);
    let mut security = Advertised {
        privacy: capability & capability::PRIVACY != 0,
        ..Advertised::default()
    };
    if version == RECORD_VERSION && length_bytes >= RECORD_FIXED_LEN && extent >= RECORD_FIXED_LEN {
        rssi = i16::from_le_bytes([bytes[78], bytes[79]]);
        let control_channel = bytes[88];
        let block_at = usize::from(half(116));
        let block_len = word(120) as usize;
        let block_end = block_at.saturating_add(block_len).min(extent);
        let block = bytes.get(block_at..block_end).unwrap_or(&[]);
        let (found, advertised) = elements(block, security.privacy);
        channel = found.unwrap_or(control_channel);
        security = advertised;
    }
    let next = at.checked_add(length_bytes).filter(|&n| n < payload.len());
    Some(Walked {
        record: ScanRecord {
            bssid,
            ssid,
            ssid_len,
            capability,
            beacon_period: half(14),
            channel,
            rssi,
            version,
            length,
            security,
        },
        end: at + extent,
        next,
    })
}

/// The records of a result payload, in order, as an iterator.
#[derive(Clone, Copy, Debug)]
pub struct Records<'a> {
    payload: &'a [u8],
    at: Option<usize>,
}

/// The records of a result payload: none for a payload without a head.
pub fn records(payload: &[u8]) -> Records<'_> {
    Records {
        payload,
        at: head(payload).map(|_| RESULT_HEAD_LEN),
    }
}

impl Iterator for Records<'_> {
    type Item = ScanRecord;

    fn next(&mut self) -> Option<ScanRecord> {
        let at = self.at?;
        match walk(self.payload, at) {
            Some(walked) => {
                self.at = walked.next;
                Some(walked.record)
            }
            None => {
                self.at = None;
                None
            }
        }
    }
}
