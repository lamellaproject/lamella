//! The event path: the mask that tells the firmware which events to
//! deliver, the walk through an event frame to its header, the event
//! numbers, status codes and reason codes as the chip delivers them, and
//! the reading of one event for the link.
//!
//! An event frame is a function-2 frame on the event channel. After the
//! frame header come a four-byte BDC header whose last byte is a padding
//! length in 32-bit words, that padding, a 14-byte Ethernet header whose
//! ethertype must be the vendor's `0x886C`, a ten-byte vendor header whose
//! OUI must be `00:10:18`, and a 48-byte event header whose multi-byte
//! fields are big-endian -- the event number, the status, the reason, the
//! authentication type, the payload's length and the address -- then the
//! payload. The reason field carries four code spaces, selected by the
//! event's number; this firmware delivers the codes bare, and their offset
//! forms are accepted as well. A frame whose payload offset equals its
//! size is an empty event and is ignored; a payload shorter than the
//! header's count is delivered as the bytes present.

/// The number of event numbers: the enumeration runs from 0 to 128.
pub const EVENT_COUNT: u32 = 129;
/// The mask's length in bytes: one bit per event number.
pub const MASK_LEN: usize = 17;
/// The BDC header's length.
pub const BDC_HEADER_LEN: usize = 4;
/// The Ethernet header's length.
pub const ETHERNET_HEADER_LEN: usize = 14;
/// The vendor header's length.
pub const VENDOR_HEADER_LEN: usize = 10;
/// The event header's length.
pub const EVENT_HEADER_LEN: usize = 48;
/// The event ethertype as it travels, the first byte first: 0x886C.
pub const ETHERTYPE: [u8; 2] = [0x88, 0x6C];
/// The vendor's OUI in the vendor header.
pub const OUI: [u8; 3] = [0x00, 0x10, 0x18];

/// Event numbers.
pub mod number {
    /// The outcome of setting the SSID: the association.
    pub const SET_SSID: u32 = 0;
    /// A join started.
    pub const JOIN: u32 = 1;
    /// An 802.11 authentication request.
    pub const AUTH: u32 = 3;
    /// A deauthentication sent.
    pub const DEAUTH: u32 = 5;
    /// A deauthentication received.
    pub const DEAUTH_IND: u32 = 6;
    /// An association request.
    pub const ASSOC: u32 = 7;
    /// A disassociation sent.
    pub const DISASSOC: u32 = 11;
    /// A disassociation received.
    pub const DISASSOC_IND: u32 = 12;
    /// The generic link indication: a non-zero reason means down.
    pub const LINK: u32 = 16;
    /// A legacy scan completed.
    pub const SCAN_COMPLETE: u32 = 26;
    /// The radio's state changed.
    pub const RADIO: u32 = 40;
    /// The WPA handshake's outcome.
    pub const PSK_SUP: u32 = 46;
    /// A scan result.
    pub const ESCAN_RESULT: u32 = 69;
    /// An association indication.
    pub const ASSOC_IND_NDIS: u32 = 85;
    /// The number of event numbers: the range-check bound.
    pub const COUNT: u32 = super::EVENT_COUNT;
}

/// Event status codes.
pub mod status {
    /// Succeeded.
    pub const SUCCESS: u32 = 0;
    /// Failed.
    pub const FAIL: u32 = 1;
    /// Timed out.
    pub const TIMEOUT: u32 = 2;
    /// No matching network.
    pub const NO_NETWORKS: u32 = 3;
    /// Aborted.
    pub const ABORT: u32 = 4;
    /// Not acknowledged.
    pub const NO_ACK: u32 = 5;
    /// Unsolicited: the status a successful handshake reports.
    pub const UNSOLICITED: u32 = 6;
    /// An automatic authentication attempt.
    pub const ATTEMPT: u32 = 7;
    /// Scan results incomplete: more to come.
    pub const PARTIAL: u32 = 8;
    /// Aborted by another scan.
    pub const NEWSCAN: u32 = 9;
    /// Aborted by an association.
    pub const NEWASSOC: u32 = 10;
    /// An 802.11h quiet period.
    pub const QUIET_11H: u32 = 11;
    /// Scanning disabled.
    pub const SUPPRESS: u32 = 12;
    /// No allowable channels.
    pub const NO_CHANNELS: u32 = 13;
    /// Aborted for a fast roam.
    pub const CCX_FAST_ROAM: u32 = 14;
    /// A channel-select abort.
    pub const CHANNEL_SELECT_ABORT: u32 = 15;
}

/// Event reason codes: four code spaces on one field, selected by the
/// event's number. The firmware delivers them bare; a code may also arrive
/// with its space's offset added, which [`bare`](reason::bare) strips.
pub mod reason {
    /// The prune space's offset.
    pub const PRUNE_OFFSET: u32 = 256;
    /// The supplicant space's offset.
    pub const SUPPLICANT_OFFSET: u32 = 512;
    /// The 802.11 space's offset.
    pub const DOT11_OFFSET: u32 = 768;

    /// The reason without its space's offset: a value in `[offset, offset
    /// + 256)` loses the offset, any other value is returned as it is.
    pub const fn bare(reason: u32, offset: u32) -> u32 {
        if reason >= offset && reason < offset + 256 {
            reason - offset
        } else {
            reason
        }
    }

    /// The supplicant's reasons, on the WPA handshake event.
    pub mod supplicant {
        /// Other: the handshake succeeded.
        pub const OTHER: u32 = 0;
        /// Key-data decryption failed.
        pub const DECRYPT_KEY_DATA: u32 = 1;
        /// An illegal unicast WEP128 key.
        pub const BAD_UCAST_WEP128: u32 = 2;
        /// An illegal unicast WEP40 key.
        pub const BAD_UCAST_WEP40: u32 = 3;
        /// An unsupported key length.
        pub const UNSUP_KEY_LEN: u32 = 4;
        /// A pairwise cipher mismatch.
        pub const PW_KEY_CIPHER: u32 = 5;
        /// More than one RSN element in the third key message.
        pub const MSG3_TOO_MANY_IE: u32 = 6;
        /// A WPA element mismatch in the third key message.
        pub const MSG3_IE_MISMATCH: u32 = 7;
        /// The install flag was not set.
        pub const NO_INSTALL_FLAG: u32 = 8;
        /// The group key was missing from the third key message.
        pub const MSG3_NO_GTK: u32 = 9;
        /// A group cipher mismatch.
        pub const GRP_KEY_CIPHER: u32 = 10;
        /// The group key was missing from the first group message.
        pub const GRP_MSG1_NO_GTK: u32 = 11;
        /// The group key failed to decrypt.
        pub const GTK_DECRYPT_FAIL: u32 = 12;
        /// A message failed to send.
        pub const SEND_FAIL: u32 = 13;
        /// The supplicant received a deauthentication frame.
        pub const DEAUTH: u32 = 14;
        /// The four-way handshake timed out.
        pub const WPA_PSK_TIMEOUT: u32 = 15;
    }

    /// The 802.11 reason codes (IEEE Std 802.11-2024, 9.4.1.7, Table
    /// 9-79), on the deauthentication and disassociation family of
    /// events.
    pub mod dot11 {
        /// Unspecified.
        pub const UNSPECIFIED: u32 = 1;
        /// The previous authentication is no longer valid.
        pub const PREVIOUS_AUTH_INVALID: u32 = 2;
        /// Deauthenticated: the sending station is leaving.
        pub const DEAUTH_LEAVING: u32 = 3;
        /// Disassociated for inactivity.
        pub const DISASSOC_INACTIVITY: u32 = 4;
        /// The access point cannot handle all its associated stations.
        pub const DISASSOC_AP_FULL: u32 = 5;
        /// A class 2 frame from a station that is not authenticated.
        pub const CLASS2_FROM_NONAUTH: u32 = 6;
        /// A class 3 frame from a station that is not associated.
        pub const CLASS3_FROM_NONASSOC: u32 = 7;
        /// Disassociated: the sending station is leaving.
        pub const DISASSOC_LEAVING: u32 = 8;
        /// The station requesting association is not authenticated.
        pub const NOT_AUTHENTICATED: u32 = 9;
    }
}

/// The events the mask carries by default: the association outcome, the
/// join, the authentication, the deauthentication and disassociation
/// family, the association, the link indication, the radio's state, the
/// handshake's outcome, the scan results and the association indication.
const DEFAULT_NUMBERS: [u32; 13] = [
    number::SET_SSID,
    number::JOIN,
    number::AUTH,
    number::DEAUTH,
    number::DEAUTH_IND,
    number::ASSOC,
    number::DISASSOC,
    number::DISASSOC_IND,
    number::LINK,
    number::RADIO,
    number::PSK_SUP,
    number::ESCAN_RESULT,
    number::ASSOC_IND_NDIS,
];

/// The events the host asks the firmware to deliver: one bit per event
/// number, bit `n & 7` of byte `n >> 3`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventMask {
    bits: [u8; MASK_LEN],
}

impl EventMask {
    /// No events.
    pub const EMPTY: EventMask = EventMask {
        bits: [0; MASK_LEN],
    };

    /// The default: the thirteen events a join, a scan and the link's
    /// upkeep need.
    pub const DEFAULT: EventMask = {
        let mut mask = EventMask::EMPTY;
        let mut i = 0;
        while i < DEFAULT_NUMBERS.len() {
            let number = DEFAULT_NUMBERS[i];
            mask.bits[(number >> 3) as usize] |= 1 << (number & 7);
            i += 1;
        }
        mask
    };

    /// Ask for `number`; a number at or past the count is refused and the
    /// mask left as it is.
    pub fn add(&mut self, number: u32) -> bool {
        if number >= EVENT_COUNT {
            return false;
        }
        self.bits[(number >> 3) as usize] |= 1 << (number & 7);
        true
    }

    /// Whether `number` is asked for.
    pub const fn contains(&self, number: u32) -> bool {
        number < EVENT_COUNT && self.bits[(number >> 3) as usize] & (1 << (number & 7)) != 0
    }

    /// The mask's bytes as the firmware receives them.
    pub const fn as_bytes(&self) -> &[u8; MASK_LEN] {
        &self.bits
    }
}

impl Default for EventMask {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// One event as the firmware delivered it: the header's deciding fields
/// and the payload's length. The payload itself stays in the driver's
/// buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Event {
    /// The event number.
    pub number: u32,
    /// The status code.
    pub status: u32,
    /// The reason code, in the space the event number selects.
    pub reason: u32,
    /// The authentication type.
    pub auth_type: u32,
    /// The address the event concerns: the access point's.
    pub address: [u8; 6],
    /// The payload's length as delivered.
    pub len: usize,
}

/// What one event says about the link, read from its number, status and
/// reason alone. The carrier and the join's completion are two decisions
/// a link machine composes from these readings with the knowledge of
/// whether a key was set: a secured join (WPA2 or WPA3) completes once
/// both `SupplicantUp` and `Associated` have been read in either order
/// and a hold after `SupplicantUp` has passed, an open join on
/// `Associated`; any of
/// them fails on `JoinFailed`. An association event reads `Unchanged`
/// whatever its status: the status belongs to the join's failure report,
/// not to the link.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Link {
    /// Nothing about the link.
    Unchanged,
    /// The association succeeded; on a secured network the link is not
    /// usable until the supplicant has reported too.
    Associated,
    /// The join failed; the status is the failure code.
    JoinFailed,
    /// The supplicant completed the handshake: on a secured network the
    /// link is up once the association success has been read as well and
    /// the hold after this word has passed.
    SupplicantUp,
    /// The supplicant failed or lost the handshake: the link is down.
    SupplicantDown,
    /// The link is down: a deauthentication, a disassociation, or the
    /// link indication with a reason.
    Down,
}

impl Event {
    /// The event's reading for the link. The handshake's outcome is keyed
    /// on the reason, never on the status, which reads unsolicited on
    /// success; the association outcome is keyed on the status; the link
    /// indication is down on a non-zero reason and says nothing otherwise.
    pub const fn link(&self) -> Link {
        match self.number {
            number::PSK_SUP => {
                if reason::bare(self.reason, reason::SUPPLICANT_OFFSET) == reason::supplicant::OTHER
                {
                    Link::SupplicantUp
                } else {
                    Link::SupplicantDown
                }
            }
            number::SET_SSID => {
                if self.status == status::SUCCESS {
                    Link::Associated
                } else {
                    Link::JoinFailed
                }
            }
            number::DEAUTH | number::DEAUTH_IND | number::DISASSOC | number::DISASSOC_IND => {
                Link::Down
            }
            number::LINK => {
                if self.reason != 0 {
                    Link::Down
                } else {
                    Link::Unchanged
                }
            }
            _ => Link::Unchanged,
        }
    }
}

/// What is wrong with an event frame the walk dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// The frame ends before the event header does.
    Short,
    /// The Ethernet header's ethertype is not the vendor's.
    Ethertype,
    /// The vendor header's OUI is not the vendor's.
    Oui,
    /// The event number is at or past the count.
    Number,
}

/// The walk's result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Parsed {
    /// An event, and the offset of its payload in the frame.
    Event {
        /// The event.
        event: Event,
        /// The payload's offset in the frame.
        at: usize,
    },
    /// An empty event: the payload offset equals the frame's size.
    Empty,
    /// A frame the walk dropped.
    Dropped(Fault),
}

/// Walk an event frame from its frame header's payload offset: the BDC
/// header and its padding skipped, the ethertype and the OUI checked, the
/// event header read big-endian, the number range-checked, the payload's
/// length bounded by the bytes present.
pub fn parse(frame: &[u8], data_offset: usize) -> Parsed {
    let Some(body) = frame.get(data_offset..) else {
        return Parsed::Dropped(Fault::Short);
    };
    if body.is_empty() {
        return Parsed::Empty;
    }
    if body.len() < BDC_HEADER_LEN {
        return Parsed::Dropped(Fault::Short);
    }
    let ethernet_at = BDC_HEADER_LEN + 4 * usize::from(body[BDC_HEADER_LEN - 1]);
    let Some(ethernet) = body.get(ethernet_at..ethernet_at + ETHERNET_HEADER_LEN) else {
        return Parsed::Dropped(Fault::Short);
    };
    if ethernet[12..14] != ETHERTYPE {
        return Parsed::Dropped(Fault::Ethertype);
    }
    let vendor_at = ethernet_at + ETHERNET_HEADER_LEN;
    let Some(vendor) = body.get(vendor_at..vendor_at + VENDOR_HEADER_LEN) else {
        return Parsed::Dropped(Fault::Short);
    };
    if vendor[5..8] != OUI {
        return Parsed::Dropped(Fault::Oui);
    }
    let header_at = vendor_at + VENDOR_HEADER_LEN;
    let Some(header) = body.get(header_at..header_at + EVENT_HEADER_LEN) else {
        return Parsed::Dropped(Fault::Short);
    };
    let word = |at: usize| {
        u32::from_be_bytes([header[at], header[at + 1], header[at + 2], header[at + 3]])
    };
    let number = word(4);
    if number >= EVENT_COUNT {
        return Parsed::Dropped(Fault::Number);
    }
    let mut address = [0u8; 6];
    address.copy_from_slice(&header[24..30]);
    let payload_at = header_at + EVENT_HEADER_LEN;
    let present = body.len() - payload_at;
    let len = (word(20) as usize).min(present);
    Parsed::Event {
        event: Event {
            number,
            status: word(8),
            reason: word(12),
            auth_type: word(16),
            address,
            len,
        },
        at: data_offset + payload_at,
    }
}
