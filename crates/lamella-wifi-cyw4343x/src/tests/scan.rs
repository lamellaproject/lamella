//! The scan's pure parts: the 74 parameter bytes against the hand
//! derivation, the walk through a result payload on three synthetic
//! version-109 records (the channel from the DSSS Parameter Set element
//! and from the control channel field), the advertised security read from
//! the RSN and RSN extension elements of three more, the walk's bounds,
//! and the ends.

use crate::link::Security;
use crate::scan::{
    Advertised, PARAMS_LEN, RECORD_FIXED_LEN, RECORD_HEAD_LEN, RESULT_HEAD_LEN, ScanEnd,
    ScanRecord, Walked, akm, capability, element, head, params, records, walk,
};
use std::vec::Vec;

/// The synthetic network's SSID.
pub(super) const SSID_A: &[u8] = b"synthetic-net";
/// An open synthetic network's SSID.
pub(super) const SSID_B: &[u8] = b"synthetic-open";
/// A far synthetic network's SSID.
pub(super) const SSID_C: &[u8] = b"synthetic-far";
/// The three access points' addresses.
pub(super) const BSSID_A: [u8; 6] = [0x00, 0x11, 0x22, 0x33, 0x44, 0x55];
pub(super) const BSSID_B: [u8; 6] = [0x00, 0x11, 0x22, 0x33, 0x44, 0x56];
pub(super) const BSSID_C: [u8; 6] = [0x00, 0x11, 0x22, 0x33, 0x44, 0x57];
/// The eight supported rates of the synthetic records.
const RATES: [u8; 8] = [0x82, 0x84, 0x8B, 0x96, 0x0C, 0x12, 0x18, 0x24];

/// The RSN element of a WPA3-only network: version 1, CCMP-128 as the
/// group and the pairwise cipher, one authentication suite (SAE), the
/// capabilities MFPR and MFPC.
pub(super) const RSN_SAE: [u8; 22] = [
    0x30, 0x14, 0x01, 0x00, 0x00, 0x0F, 0xAC, 0x04, 0x01, 0x00, 0x00, 0x0F, 0xAC, 0x04, 0x01, 0x00,
    0x00, 0x0F, 0xAC, 0x08, 0xC0, 0x00,
];
/// The RSN element of a transition network: PSK then SAE among the
/// suites, MFPC alone.
pub(super) const RSN_TRANSITION: [u8; 26] = [
    0x30, 0x18, 0x01, 0x00, 0x00, 0x0F, 0xAC, 0x04, 0x01, 0x00, 0x00, 0x0F, 0xAC, 0x04, 0x02, 0x00,
    0x00, 0x0F, 0xAC, 0x02, 0x00, 0x0F, 0xAC, 0x08, 0x80, 0x00,
];
/// The RSN element of a WPA2 network: PSK alone, no capabilities field.
pub(super) const RSN_PSK: [u8; 20] = [
    0x30, 0x12, 0x01, 0x00, 0x00, 0x0F, 0xAC, 0x04, 0x01, 0x00, 0x00, 0x0F, 0xAC, 0x04, 0x01, 0x00,
    0x00, 0x0F, 0xAC, 0x02,
];
/// The standard's own sample element (IEEE Std 802.11-2024, 9.4.2.23.1):
/// authentication over IEEE 802.1X with CCMP-128, no capabilities.
const RSN_SAMPLE: [u8; 22] = [
    0x30, 0x14, 0x01, 0x00, 0x00, 0x0F, 0xAC, 0x04, 0x01, 0x00, 0x00, 0x0F, 0xAC, 0x04, 0x01, 0x00,
    0x00, 0x0F, 0xAC, 0x01, 0x00, 0x00,
];
/// An RSN extension element with the hash-to-element bit set.
pub(super) const RSNXE_H2E: [u8; 3] = [0xF4, 0x01, 0x20];
/// Three more access points on the synthetic network's name: WPA3-only,
/// a transition network, WPA2-only.
pub(super) const BSSID_D: [u8; 6] = [0x00, 0x11, 0x22, 0x33, 0x44, 0x58];
pub(super) const BSSID_E: [u8; 6] = [0x00, 0x11, 0x22, 0x33, 0x44, 0x59];
pub(super) const BSSID_F: [u8; 6] = [0x00, 0x11, 0x22, 0x33, 0x44, 0x5A];

/// A version-109 record: the fixed part of 128 bytes with the fields at
/// the pinned offsets, then the element block -- the SSID, the supported
/// rates and, when `ds` names a channel, the DSSS Parameter Set element.
pub(super) fn record(
    ssid: &[u8],
    bssid: [u8; 6],
    cap: u16,
    rssi: i16,
    ctl_ch: u8,
    ds: Option<u8>,
) -> Vec<u8> {
    record_with(ssid, bssid, cap, rssi, ctl_ch, ds, &[])
}

/// A version-109 record whose element block ends with `extra`: the RSN
/// elements of the security records.
#[allow(clippy::too_many_arguments)]
pub(super) fn record_with(
    ssid: &[u8],
    bssid: [u8; 6],
    cap: u16,
    rssi: i16,
    ctl_ch: u8,
    ds: Option<u8>,
    extra: &[u8],
) -> Vec<u8> {
    let mut ies = Vec::new();
    ies.push(element::SSID);
    ies.push(ssid.len() as u8);
    ies.extend_from_slice(ssid);
    ies.push(element::SUPPORTED_RATES);
    ies.push(RATES.len() as u8);
    ies.extend_from_slice(&RATES);
    if let Some(channel) = ds {
        ies.extend_from_slice(&[element::DSSS_PARAMETER_SET, 1, channel]);
    }
    ies.extend_from_slice(extra);
    let length = RECORD_FIXED_LEN + ies.len();
    let mut r = std::vec![0u8; RECORD_FIXED_LEN];
    r[0..4].copy_from_slice(&109u32.to_le_bytes());
    r[4..8].copy_from_slice(&(length as u32).to_le_bytes());
    r[8..14].copy_from_slice(&bssid);
    r[14..16].copy_from_slice(&100u16.to_le_bytes());
    r[16..18].copy_from_slice(&cap.to_le_bytes());
    r[18] = ssid.len() as u8;
    r[19..19 + ssid.len()].copy_from_slice(ssid);
    r[52..56].copy_from_slice(&8u32.to_le_bytes());
    r[56..64].copy_from_slice(&RATES);
    r[76] = 1;
    r[78..80].copy_from_slice(&rssi.to_le_bytes());
    r[80] = 0xA6;
    r[81] = 1;
    r[88] = ctl_ch;
    r[116..118].copy_from_slice(&(RECORD_FIXED_LEN as u16).to_le_bytes());
    r[120..124].copy_from_slice(&(ies.len() as u32).to_le_bytes());
    r.extend_from_slice(&ies);
    r
}

/// Record A: the synthetic secured network on channel 11 by the element.
pub(super) fn record_a() -> Vec<u8> {
    record(SSID_A, BSSID_A, 0x0411, -40, 0, Some(11))
}

/// Record B: the synthetic open network on channel 6 by the control
/// channel field.
pub(super) fn record_b() -> Vec<u8> {
    record(SSID_B, BSSID_B, 0x0401, -72, 6, None)
}

/// Record C: the far synthetic network on channel 1 by the element.
pub(super) fn record_c() -> Vec<u8> {
    record(SSID_C, BSSID_C, 0x0411, -80, 0, Some(1))
}

/// Record D: the synthetic network's name on a WPA3-only access point.
pub(super) fn record_d() -> Vec<u8> {
    record_with(SSID_A, BSSID_D, 0x0411, -45, 0, Some(11), &RSN_SAE)
}

/// Record E: the synthetic network's name on a transition access point
/// that supports the hash-to-element method.
pub(super) fn record_e() -> Vec<u8> {
    let mut extra = RSN_TRANSITION.to_vec();
    extra.extend_from_slice(&RSNXE_H2E);
    record_with(SSID_A, BSSID_E, 0x0411, -50, 0, Some(6), &extra)
}

/// Record F: the synthetic network's name on a WPA2-only access point.
pub(super) fn record_f() -> Vec<u8> {
    record_with(SSID_A, BSSID_F, 0x0411, -55, 0, Some(1), &RSN_PSK)
}

/// A result payload: the head with `sync` and the count, then the records.
pub(super) fn result(sync: u16, recs: &[Vec<u8>]) -> Vec<u8> {
    let total = RESULT_HEAD_LEN + recs.iter().map(Vec::len).sum::<usize>();
    let mut p = Vec::new();
    p.extend_from_slice(&(total as u32).to_le_bytes());
    p.extend_from_slice(&1u32.to_le_bytes());
    p.extend_from_slice(&sync.to_le_bytes());
    p.extend_from_slice(&(recs.len() as u16).to_le_bytes());
    for r in recs {
        p.extend_from_slice(r);
    }
    p
}

/// An advertisement, the flags this battery varies named.
#[allow(clippy::too_many_arguments)]
const fn advertised(
    privacy: bool,
    rsne: bool,
    ccmp: bool,
    psk: bool,
    sae: bool,
    mfp_capable: bool,
    mfp_required: bool,
    h2e: bool,
) -> Advertised {
    Advertised {
        rsne,
        privacy,
        ccmp,
        psk,
        sae,
        ft_sae: false,
        dot1x: false,
        other_akm: false,
        mfp_capable,
        mfp_required,
        h2e,
    }
}

/// The advertisement of a record without a readable RSN element: the
/// Privacy bit alone.
pub(super) const fn privacy_only(cap: u16) -> Advertised {
    advertised(
        cap & 0x10 != 0,
        false,
        false,
        false,
        false,
        false,
        false,
        false,
    )
}

pub(super) const SECURITY_D: Advertised =
    advertised(true, true, true, false, true, true, true, false);
pub(super) const SECURITY_E: Advertised =
    advertised(true, true, true, true, true, true, false, true);
pub(super) const SECURITY_F: Advertised =
    advertised(true, true, true, true, false, false, false, false);

/// The record the crate is expected to hand back.
pub(super) const fn expected(
    ssid: &[u8],
    bssid: [u8; 6],
    cap: u16,
    channel: u8,
    rssi: i16,
    length: u32,
    security: Advertised,
) -> ScanRecord {
    let mut bytes = [0u8; 32];
    let mut i = 0;
    while i < ssid.len() {
        bytes[i] = ssid[i];
        i += 1;
    }
    ScanRecord {
        bssid,
        ssid: bytes,
        ssid_len: ssid.len() as u8,
        capability: cap,
        beacon_period: 100,
        channel,
        rssi,
        version: 109,
        length,
        security,
    }
}

pub(super) const RECORD_A: ScanRecord =
    expected(SSID_A, BSSID_A, 0x0411, 11, -40, 156, privacy_only(0x0411));
pub(super) const RECORD_B: ScanRecord =
    expected(SSID_B, BSSID_B, 0x0401, 6, -72, 154, privacy_only(0x0401));
pub(super) const RECORD_C: ScanRecord =
    expected(SSID_C, BSSID_C, 0x0411, 1, -80, 156, privacy_only(0x0411));
pub(super) const RECORD_D: ScanRecord = expected(SSID_A, BSSID_D, 0x0411, 11, -45, 178, SECURITY_D);
pub(super) const RECORD_E: ScanRecord = expected(SSID_A, BSSID_E, 0x0411, 6, -50, 185, SECURITY_E);
pub(super) const RECORD_F: ScanRecord = expected(SSID_A, BSSID_F, 0x0411, 1, -55, 176, SECURITY_F);

#[test]
fn the_advertised_security_is_read_from_the_rsn_and_rsnxe_elements() {
    let got: Vec<ScanRecord> = records(&result(1, &[record_d(), record_e(), record_f()])).collect();
    assert_eq!(got, std::vec![RECORD_D, RECORD_E, RECORD_F]);
    assert_eq!(
        (record_d().len(), record_e().len(), record_f().len()),
        (178, 185, 176)
    );
    assert_eq!(
        RECORD_A.security,
        privacy_only(0x0411),
        "no RSN element: the bit alone"
    );
    assert_eq!(RECORD_B.security, Advertised::default(), "open: nothing");
    // The standard's own sample element: authentication over IEEE 802.1X
    // with CCMP-128 and no capabilities field.
    let sample = record_with(SSID_C, BSSID_C, 0x0411, -80, 0, Some(1), &RSN_SAMPLE);
    let r = records(&result(1, &[sample])).next().expect("a record");
    assert_eq!(
        r.security,
        Advertised {
            rsne: true,
            privacy: true,
            ccmp: true,
            dot1x: true,
            ..Advertised::default()
        }
    );
    assert!(!r.security.compatible(Security::Wpa2Psk) && !r.security.compatible(Security::Wpa3Sae));
    // A count past the element's length: what was read stays, the fields
    // past the cut are not read.
    let mut cut = RSN_SAE.to_vec();
    cut[14] = 2;
    let r = records(&result(
        1,
        &[record_with(SSID_A, BSSID_D, 0x0411, -45, 0, Some(11), &cut)],
    ))
    .next()
    .expect("a record");
    assert_eq!(
        r.security,
        Advertised {
            rsne: true,
            privacy: true,
            ccmp: true,
            sae: true,
            ..Advertised::default()
        },
        "the capabilities past the cut unread"
    );
    // A version other than 1 is not read.
    let mut v2 = RSN_SAE.to_vec();
    v2[2] = 2;
    let r = records(&result(
        1,
        &[record_with(SSID_A, BSSID_D, 0x0411, -45, 0, Some(11), &v2)],
    ))
    .next()
    .expect("a record");
    assert_eq!(r.security, privacy_only(0x0411));
    // A suite under another OUI counts as other.
    let mut other = RSN_PSK.to_vec();
    other[16..19].copy_from_slice(&[0x00, 0x50, 0xF2]);
    let r = records(&result(
        1,
        &[record_with(
            SSID_A,
            BSSID_F,
            0x0411,
            -55,
            0,
            Some(1),
            &other,
        )],
    ))
    .next()
    .expect("a record");
    assert!(r.security.rsne && r.security.other_akm && !r.security.psk);
    // The RSN extension element alone: the bit read, no RSN element.
    let r = records(&result(
        1,
        &[record_with(
            SSID_A,
            BSSID_D,
            0x0411,
            -45,
            0,
            Some(11),
            &RSNXE_H2E,
        )],
    ))
    .next()
    .expect("a record");
    assert_eq!(
        r.security,
        Advertised {
            privacy: true,
            h2e: true,
            ..Advertised::default()
        }
    );
    // An RSN extension element of length 0 is passed over; the channel is
    // still read beside the security elements.
    let r = records(&result(
        1,
        &[record_with(
            SSID_A,
            BSSID_D,
            0x0411,
            -45,
            0,
            Some(11),
            &[0xF4, 0x00],
        )],
    ))
    .next()
    .expect("a record");
    assert_eq!((r.security, r.channel), (privacy_only(0x0411), 11));
    // Another record version reads no element block.
    let mut other_version = record_d();
    other_version[0..4].copy_from_slice(&110u32.to_le_bytes());
    let r = records(&result(1, &[other_version]))
        .next()
        .expect("a record");
    assert_eq!((r.security, r.channel), (privacy_only(0x0411), 0));
    // The compatibility rule.
    assert!(SECURITY_D.compatible(Security::Wpa3Sae));
    assert!(!SECURITY_D.compatible(Security::Wpa2Psk) && !SECURITY_D.compatible(Security::Open));
    assert!(SECURITY_E.compatible(Security::Wpa3Sae) && SECURITY_E.compatible(Security::Wpa2Psk));
    assert!(!SECURITY_E.compatible(Security::Open));
    assert!(SECURITY_F.compatible(Security::Wpa2Psk) && !SECURITY_F.compatible(Security::Wpa3Sae));
    assert!(
        RECORD_B.security.compatible(Security::Open)
            && !RECORD_B.security.compatible(Security::Wpa2Psk)
    );
    assert!(
        !RECORD_B.security.compatible(Security::Wpa3Sae),
        "no privacy: no secured kind"
    );
    assert!(
        !RECORD_A.security.compatible(Security::Open),
        "privacy: no open join"
    );
    assert!(
        RECORD_A.security.compatible(Security::Wpa2Psk)
            && RECORD_A.security.compatible(Security::Wpa3Sae),
        "privacy without a readable RSN element: the walk cannot say the join will fail"
    );
    assert!(
        r.security.compatible(Security::Wpa2Psk)
            && r.security.compatible(Security::Wpa3Sae)
            && !r.security.compatible(Security::Open),
        "another record version reads no element: the firmware decides for a secured kind, never for an open one"
    );
    assert_eq!(
        (
            element::RSN,
            element::RSNXE,
            akm::PSK,
            akm::PSK_SHA256,
            akm::SAE,
            akm::SAE_EXT,
            akm::FT_SAE,
            akm::CIPHER_CCMP
        ),
        (48, 244, 2, 6, 8, 24, 9, 4)
    );
    assert_eq!(akm::OUI, [0x00, 0x0F, 0xAC]);
}

#[test]
fn the_parameters_match_the_hand_derived_bytes() {
    let p = params(1);
    assert_eq!(p.len(), PARAMS_LEN);
    assert_eq!(&p[..8], &[0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00]);
    assert!(p[8..44].iter().all(|&b| b == 0), "the wildcard SSID");
    assert_eq!(
        &p[44..52],
        &[0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x02, 0x00]
    );
    assert_eq!(&p[52..68], &[0xFF; 16], "the four default words");
    assert_eq!(
        &p[68..74],
        &[0, 0, 0, 0, 0, 0],
        "every channel, the one-element list"
    );
    assert_eq!(&params(0xABCD)[6..8], &[0xCD, 0xAB]);
    assert_eq!(PARAMS_LEN, 74);
}

#[test]
fn the_walk_reads_the_three_synthetic_records() {
    let a = record_a();
    assert_eq!(a.len(), 156);
    assert_eq!(
        &a[..52],
        &[
            0x6D, 0x00, 0x00, 0x00, 0x9C, 0x00, 0x00, 0x00, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55,
            0x64, 0x00, 0x11, 0x04, 0x0D, b's', b'y', b'n', b't', b'h', b'e', b't', b'i', b'c',
            b'-', b'n', b'e', b't', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0
        ][..]
    );
    assert_eq!(&a[78..80], &[0xD8, 0xFF], "the RSSI -40");
    assert_eq!(
        &a[116..124],
        &[0x80, 0x00, 0x00, 0x00, 0x1C, 0x00, 0x00, 0x00]
    );
    assert_eq!(
        &a[128..],
        &[
            &[0x00, 0x0D][..],
            SSID_A,
            &[0x01, 0x08][..],
            &RATES[..],
            &[0x03, 0x01, 0x0B][..]
        ]
        .concat()[..]
    );
    let payload = result(1, &[a, record_b(), record_c()]);
    assert_eq!(payload.len(), 12 + 156 + 154 + 156);
    assert_eq!(
        &payload[..12],
        &[
            0xDE, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x03, 0x00
        ]
    );
    let h = head(&payload).expect("a head");
    assert_eq!((h.buflen, h.version, h.sync, h.count), (478, 1, 1, 3));
    assert_eq!(
        &result(1, &[record_a(), record_b()])[..12],
        &[
            0x42, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x02, 0x00
        ],
        "the first result event's head"
    );
    let got: Vec<ScanRecord> = records(&payload).collect();
    assert_eq!(got, std::vec![RECORD_A, RECORD_B, RECORD_C]);
    assert_eq!(RECORD_A.ssid(), SSID_A);
    assert!(RECORD_A.is_secured() && RECORD_A.is_ess());
    assert!(!RECORD_B.is_secured() && RECORD_B.is_ess());
    assert_eq!(RECORD_B.rssi, -72);
    assert_eq!(capability::PRIVACY, 0x10);
    assert_eq!(capability::ESS, 0x01);
    assert_eq!(capability::IBSS, 0x02);
    let first = walk(&payload, 12).expect("record A");
    assert_eq!(
        first,
        Walked {
            record: RECORD_A,
            end: 12 + 156,
            next: Some(12 + 156)
        }
    );
    let last = walk(&payload, 12 + 156 + 154).expect("record C");
    assert_eq!((last.end, last.next), (payload.len(), None));
}

#[test]
fn the_walks_bounds() {
    let (a, b, c) = (record_a(), record_b(), record_c());
    // A length past the payload: the record delivered with what is present
    // and the walk ends.
    let mut long = a.clone();
    long[4..8].copy_from_slice(&500u32.to_le_bytes());
    let payload = result(1, &[long, b.clone()]);
    let got: Vec<ScanRecord> = records(&payload).collect();
    assert_eq!(got.len(), 1);
    assert_eq!(
        (got[0].channel, got[0].length, got[0].ssid()),
        (11, 500, SSID_A)
    );
    let w = walk(&payload, 12).expect("the clipped record");
    assert_eq!((w.end, w.next), (payload.len(), None));
    // A length under the head ends the walk without the record.
    let mut short = a.clone();
    short[4..8].copy_from_slice(&50u32.to_le_bytes());
    assert_eq!(records(&result(1, &[short, b.clone()])).count(), 0);
    let mut zero = a.clone();
    zero[4..8].copy_from_slice(&0u32.to_le_bytes());
    assert_eq!(records(&result(1, &[zero])).count(), 0);
    // Fewer than 51 bytes present: no record.
    let cut = result(1, core::slice::from_ref(&a));
    assert_eq!(walk(&cut[..12 + 50], 12), None);
    assert!(
        walk(&cut[..12 + 51], 12).is_some(),
        "the head alone is a record"
    );
    // A count larger than the records present: the bytes decide.
    let mut counted = result(1, core::slice::from_ref(&a));
    counted[10..12].copy_from_slice(&9u16.to_le_bytes());
    assert_eq!(records(&counted).count(), 1);
    // Another version reads no tail: the RSSI 0, the channel 0.
    let mut other = a.clone();
    other[0..4].copy_from_slice(&110u32.to_le_bytes());
    let got: Vec<ScanRecord> = records(&result(1, &[other, c.clone()])).collect();
    assert_eq!(
        (got[0].version, got[0].rssi, got[0].channel, got[0].ssid()),
        (110, 0, 0, SSID_A)
    );
    assert_eq!(got[1], RECORD_C);
    // A version-109 record shorter than the fixed part reads no tail.
    let mut stub = a[..100].to_vec();
    stub[4..8].copy_from_slice(&100u32.to_le_bytes());
    let got: Vec<ScanRecord> = records(&result(1, &[stub])).collect();
    assert_eq!((got[0].rssi, got[0].channel, got[0].length), (0, 0, 100));
    // The done event with no records; a payload without a head.
    let done = result(1, &[]);
    assert_eq!(done.len(), 12);
    assert_eq!(head(&done).map(|h| h.count), Some(0));
    assert_eq!(records(&done).count(), 0);
    assert_eq!(head(&done[..11]), None);
    assert_eq!(records(&done[..11]).count(), 0);
    // An element block clipped by the record's length: the element found
    // inside the clip, not outside it.
    let mut clipped = a.clone();
    clipped[120..124].copy_from_slice(&1000u32.to_le_bytes());
    assert_eq!(
        records(&result(1, &[clipped])).next().map(|r| r.channel),
        Some(11)
    );
    let mut cut_short = a.clone();
    cut_short[4..8].copy_from_slice(&(128 + 25u32).to_le_bytes());
    cut_short.truncate(128 + 25);
    assert_eq!(
        records(&result(1, &[cut_short])).next().map(|r| r.channel),
        Some(0),
        "the element cut off: the field"
    );
    // A DSSS element of length 0 is passed over; an element block offset
    // past the record falls back to the field.
    let mut empty_ds = record(SSID_A, BSSID_A, 0x0411, -40, 9, None);
    let n = empty_ds.len();
    empty_ds.extend_from_slice(&[element::DSSS_PARAMETER_SET, 0]);
    empty_ds[4..8].copy_from_slice(&((n + 2) as u32).to_le_bytes());
    empty_ds[120..124].copy_from_slice(&((n + 2 - 128) as u32).to_le_bytes());
    assert_eq!(
        records(&result(1, &[empty_ds])).next().map(|r| r.channel),
        Some(9)
    );
    let mut far = a.clone();
    far[116..118].copy_from_slice(&900u16.to_le_bytes());
    far[88] = 3;
    assert_eq!(
        records(&result(1, &[far])).next().map(|r| r.channel),
        Some(3)
    );
    // An SSID length past 32 is clamped.
    let mut wide = a.clone();
    wide[18] = 40;
    let r = records(&result(1, &[wide])).next().expect("a record");
    assert_eq!(r.ssid_len, 32);
    assert_eq!(r.ssid().len(), 32);
    assert_eq!(
        (RECORD_HEAD_LEN, RECORD_FIXED_LEN, RESULT_HEAD_LEN),
        (51, 128, 12)
    );
}

/// The walk's step to the next record computed on a 32-bit word, as the
/// Cortex-M targets compute it: a checked add filtered under the payload.
fn step32(at: u32, length: u32, payload_len: usize) -> Option<u32> {
    at.checked_add(length)
        .filter(|&n| (n as usize) < payload_len)
}

#[test]
fn a_length_near_the_word_limit_is_delivered_once_and_the_walk_ends() {
    let (a, b) = (record_a(), record_b());
    for length in [0xFFFF_FFF0u32, u32::MAX] {
        let mut long = a.clone();
        long[4..8].copy_from_slice(&length.to_le_bytes());
        let payload = result(1, &[long, b.clone()]);
        let w = walk(&payload, 12).expect("the record");
        assert_eq!(
            (w.record.length, w.record.ssid(), w.record.channel),
            (length, SSID_A, 11)
        );
        assert_eq!(
            (w.end, w.next),
            (payload.len(), None),
            "clipped and last: {length:#x}"
        );
        assert_eq!(records(&payload).count(), 1, "handed out once: {length:#x}");
        assert_eq!(
            step32(12, length, payload.len()),
            None,
            "the 32-bit twin agrees: {length:#x}"
        );
    }
    let payload = result(1, &[a, b]);
    assert_eq!(walk(&payload, 12).map(|w| w.next), Some(Some(12 + 156)));
    assert_eq!(step32(12, 156, payload.len()), Some(12 + 156));
    assert_eq!(
        walk(&payload, 12 + 156).map(|w| w.next),
        Some(None),
        "the last record ends at the payload"
    );
    assert_eq!(step32(12 + 156, 154, payload.len()), None);
    assert_eq!(step32(12, 500, payload.len()), None, "past the payload");
}

#[test]
fn the_ends() {
    assert_eq!(ScanEnd::of_status(0), ScanEnd::Complete);
    assert_eq!(ScanEnd::of_status(4), ScanEnd::Ended(4));
    assert_eq!(
        ScanEnd::of_status(8),
        ScanEnd::Ended(8),
        "partial is never an end; the station never asks"
    );
    assert_ne!(ScanEnd::TimedOut, ScanEnd::Complete);
}
