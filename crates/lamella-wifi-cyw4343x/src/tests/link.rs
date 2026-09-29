//! The join's pure parts: the key, password and SSID structures against
//! hand-derived bytes, the credentials' bounds and their printing, the
//! request frames of the three sequences against the test's own builder,
//! the command numbers and the names.

use super::control::request_frame;
use super::download_tie::leak;
use super::scan::SSID_B;
use crate::backplane::Window;
use crate::control::{Exchange, Payload, Progress, Request, STATUS_NOT_UP, cmd, var};
use crate::driver::Wake;
use crate::fixture::{FakeTransport, Op};
use crate::frame::{FRAME_BUF, Layer};
use crate::link::{
    AUTH_OPEN_SYSTEM, AUTH_SAE, Credential, DISASSOC_LEN, DISCONNECT, JoinFailure, KEY_LEN,
    LinkState, Network, PASSPHRASE_MAX, PASSPHRASE_MIN, Passphrase, SAE_PASSWORD_LEN,
    SAE_PASSWORD_MAX, SAE_PASSWORD_MIN, SSID_STRUCT_LEN, STAGE_AUTH_MODE, STAGE_AUTH_TYPE,
    STAGE_CIPHER, STAGE_DISASSOCIATE, STAGE_DISCONNECT, STAGE_INFRA, STAGE_NETWORK_NAME,
    STAGE_PASSPHRASE, STAGE_PASSWORD, STAGE_PROTECTION, STAGE_SUPPLICANT, STEP_FIRST, STEP_LAST,
    STEP_PROTECTION, STEP_REENTRY, SaePassword, Security, auth_mode, cipher, key_structure, mfp,
    request, sae_password_structure, ssid_structure,
};
use crate::scan::Advertised;
use crate::transport::Part;
use std::format;
use std::string::String;
use std::vec::Vec;

/// The synthetic passphrase, named as synthetic; the synthetic WPA3
/// password is the same bytes.
pub(super) const PASSPHRASE: &[u8] = b"synthetic-passphrase";
/// The synthetic network's SSID.
pub(super) const SSID: &[u8] = b"synthetic-net";

/// The 36-byte SSID structure's bytes for `ssid`.
fn name_of(ssid: &[u8]) -> Vec<u8> {
    let mut name = std::vec![ssid.len() as u8, 0, 0, 0];
    name.extend_from_slice(ssid);
    name.resize(36, 0);
    name
}

/// The eight values of a WPA2-PSK join's requests to `ssid`, by step (no
/// protection step).
pub(super) fn secured_values_for(ssid: &[u8]) -> Vec<(u32, &'static [u8], Vec<u8>)> {
    let mut key = std::vec![0x14, 0x00, 0x01, 0x00];
    key.extend_from_slice(PASSPHRASE);
    key.resize(68, 0);
    std::vec![
        (20, &b""[..], std::vec![1, 0, 0, 0]),
        (
            263,
            &b"bsscfg:sup_wpa"[..],
            std::vec![0, 0, 0, 0, 1, 0, 0, 0]
        ),
        (165, &b""[..], std::vec![0x80, 0, 0, 0]),
        (134, &b""[..], std::vec![4, 0, 0, 0]),
        (22, &b""[..], std::vec![0, 0, 0, 0]),
        (52, &b""[..], std::vec![0; 12]),
        (268, &b""[..], key),
        (26, &b""[..], name_of(ssid)),
    ]
}

/// The eight values of the secured join's requests to the synthetic
/// network, by step.
pub(super) fn secured_values() -> Vec<(u32, &'static [u8], Vec<u8>)> {
    secured_values_for(SSID)
}

/// The nine values of a WPA2-PSK join's requests after a WPA3 join on the
/// same attach: the protection returned to none at the sixth.
pub(super) fn secured_values_with_reset() -> Vec<(u32, &'static [u8], Vec<u8>)> {
    let mut v = secured_values();
    v.insert(5, (263, &b"mfp"[..], std::vec![0, 0, 0, 0]));
    v
}

/// The seven values of an open join's requests to `ssid`, by step (no
/// protection step, no secret).
pub(super) fn open_values_for(ssid: &[u8]) -> Vec<(u32, &'static [u8], Vec<u8>)> {
    let mut v = secured_values_for(ssid);
    v.remove(6);
    v[1].2 = std::vec![0, 0, 0, 0, 0, 0, 0, 0];
    v[2].2 = std::vec![0, 0, 0, 0];
    v[3].2 = std::vec![0, 0, 0, 0];
    v
}

/// The seven values of the open join's requests to the open synthetic
/// network, by step.
pub(super) fn open_values() -> Vec<(u32, &'static [u8], Vec<u8>)> {
    open_values_for(SSID_B)
}

/// The nine values of a WPA3-SAE join's requests to the synthetic network,
/// by step.
pub(super) fn sae_values() -> Vec<(u32, &'static [u8], Vec<u8>)> {
    let mut password = std::vec![0x14, 0x00];
    password.extend_from_slice(PASSPHRASE);
    password.resize(130, 0);
    std::vec![
        (20, &b""[..], std::vec![1, 0, 0, 0]),
        (
            263,
            &b"bsscfg:sup_wpa"[..],
            std::vec![0, 0, 0, 0, 1, 0, 0, 0]
        ),
        (165, &b""[..], std::vec![0, 0, 4, 0]),
        (134, &b""[..], std::vec![4, 0, 0, 0]),
        (22, &b""[..], std::vec![3, 0, 0, 0]),
        (263, &b"mfp"[..], std::vec![2, 0, 0, 0]),
        (52, &b""[..], std::vec![0; 12]),
        (263, &b"sae_password"[..], password),
        (26, &b""[..], name_of(SSID)),
    ]
}

#[test]
fn the_key_password_and_ssid_structures_match_the_hand_derived_bytes() {
    let passphrase = Passphrase::new(PASSPHRASE).expect("20 bytes");
    let key = key_structure(passphrase);
    assert_eq!(key.len(), KEY_LEN);
    assert_eq!(&key[..4], &[0x14, 0x00, 0x01, 0x00]);
    assert_eq!(&key[4..24], PASSPHRASE);
    assert!(key[24..].iter().all(|&b| b == 0));
    let password = sae_password_structure(SaePassword::new(PASSPHRASE).expect("20 bytes"));
    assert_eq!(password.len(), SAE_PASSWORD_LEN);
    assert_eq!(&password[..2], &[0x14, 0x00]);
    assert_eq!(&password[2..22], PASSPHRASE);
    assert!(password[22..].iter().all(|&b| b == 0));
    let full = sae_password_structure(SaePassword::new(&[b'p'; 128]).expect("128 bytes"));
    assert_eq!(&full[..2], &[0x80, 0x00]);
    assert_eq!(
        &full[2..],
        &[b'p'; 128],
        "the structure filled to its capacity"
    );
    let ssid = ssid_structure(SSID);
    assert_eq!(ssid.len(), SSID_STRUCT_LEN);
    assert_eq!(&ssid[..4], &[0x0D, 0x00, 0x00, 0x00]);
    assert_eq!(&ssid[4..17], SSID);
    assert!(ssid[17..].iter().all(|&b| b == 0));
    let long = ssid_structure(&[b'x'; 40]);
    assert_eq!(&long[..4], &[32, 0, 0, 0], "cut to 32");
    assert_eq!(&long[4..], &[b'x'; 32]);
    assert_eq!(
        (KEY_LEN, SAE_PASSWORD_LEN, SSID_STRUCT_LEN, DISASSOC_LEN),
        (68, 130, 36, 12)
    );
}

#[test]
fn the_credentials_bounds_and_their_printing() {
    assert!(
        Network::new(b"", Some(PASSPHRASE)).is_none(),
        "an empty SSID"
    );
    assert!(Network::new(&[b'a'; 33], None).is_none(), "33 bytes");
    assert!(Network::new(&[b'a'; 32], None).is_some());
    assert!(Network::new(SSID, Some(&[b'p'; 7])).is_none(), "7 bytes");
    assert!(Network::new(SSID, Some(&[b'p'; 8])).is_some());
    assert!(Network::new(SSID, Some(&[b'p'; 64])).is_some());
    assert!(Network::new(SSID, Some(&[b'p'; 65])).is_none(), "65 bytes");
    assert!(Passphrase::new(&[b'p'; 7]).is_none());
    assert_eq!(Passphrase::new(PASSPHRASE).map(|p| p.len()), Some(20));
    assert_eq!(
        Passphrase::new(PASSPHRASE).map(|p| p.is_empty()),
        Some(false)
    );
    assert_eq!((PASSPHRASE_MIN, PASSPHRASE_MAX), (8, 64));
    assert!(
        Network::with(SSID, Credential::SaePassword(&[])).is_none(),
        "an empty password"
    );
    assert!(
        Network::with(SSID, Credential::SaePassword(&[b'p'; 1])).is_some(),
        "one byte"
    );
    assert!(
        Network::with(SSID, Credential::SaePassword(&[b'p'; 128])).is_some(),
        "128 bytes"
    );
    assert!(
        Network::with(SSID, Credential::SaePassword(&[b'p'; 129])).is_none(),
        "129 bytes"
    );
    assert!(
        Network::with(b"", Credential::SaePassword(PASSPHRASE)).is_none(),
        "an empty SSID"
    );
    assert!(
        Network::with(SSID, Credential::Passphrase(&[b'p'; 7])).is_none(),
        "7 bytes through the credential"
    );
    assert!(SaePassword::new(&[]).is_none());
    assert_eq!(SaePassword::new(PASSPHRASE).map(|p| p.len()), Some(20));
    assert_eq!(
        SaePassword::new(PASSPHRASE).map(|p| p.is_empty()),
        Some(false)
    );
    assert_eq!((SAE_PASSWORD_MIN, SAE_PASSWORD_MAX), (1, 128));
    let secured = Network::new(SSID, Some(PASSPHRASE)).expect("the synthetic network");
    assert!(secured.is_secured());
    assert_eq!(secured.kind(), Security::Wpa2Psk);
    assert_eq!(secured.ssid(), SSID);
    assert_eq!(secured.passphrase().map(|p| p.len()), Some(20));
    assert_eq!(secured.sae_password(), None);
    let open = Network::new(SSID, None).expect("the open network");
    assert!(!open.is_secured());
    assert_eq!(open.kind(), Security::Open);
    assert_eq!(open.passphrase(), None);
    assert_eq!(open.sae_password(), None);
    let sae = Network::with(SSID, Credential::SaePassword(PASSPHRASE)).expect("the WPA3 network");
    assert!(sae.is_secured());
    assert_eq!(sae.kind(), Security::Wpa3Sae);
    assert_eq!(sae.passphrase(), None);
    assert_eq!(sae.sae_password().map(|p| p.len()), Some(20));
    assert_eq!(
        Network::with(SSID, Credential::Passphrase(PASSPHRASE)),
        Some(secured),
        "the same network by either constructor"
    );
    assert_eq!(Network::with(SSID, Credential::Open), Some(open));
    let printed: String = format!("{:?}", secured.passphrase().expect("held"));
    assert_eq!(printed, "Passphrase { len: 20 }");
    let printed: String = format!("{:?}", sae.sae_password().expect("held"));
    assert_eq!(printed, "SaePassword { len: 20 }");
    let printed: String = format!("{secured:?}");
    assert_eq!(printed, "Network { ssid_len: 13, kind: Wpa2Psk }");
    assert_eq!(format!("{open:?}"), "Network { ssid_len: 13, kind: Open }");
    assert_eq!(
        format!("{sae:?}"),
        "Network { ssid_len: 13, kind: Wpa3Sae }"
    );
    assert_eq!(format!("{:?}", Credential::Open), "Open");
    assert_eq!(
        format!("{:?}", Credential::Passphrase(PASSPHRASE)),
        "Passphrase { len: 20 }"
    );
    assert_eq!(
        format!("{:?}", Credential::SaePassword(PASSPHRASE)),
        "SaePassword { len: 20 }"
    );
    let printed: String = format!("{:?}", Payload::Key(secured.passphrase().expect("held")));
    assert_eq!(printed, "Key(Passphrase { len: 20 })");
    let printed: String = format!(
        "{:?}",
        Payload::SaePassword(sae.sae_password().expect("held"))
    );
    assert_eq!(printed, "SaePassword(SaePassword { len: 20 })");
    let printed: String = format!(
        "{:?}",
        request(secured, 8, false).expect("the passphrase step")
    );
    assert!(
        !printed.contains("synthetic"),
        "no byte of the passphrase in a request's printing: {printed}"
    );
    let printed: String = format!("{:?}", request(sae, 8, false).expect("the password step"));
    assert!(
        !printed.contains("synthetic"),
        "no byte of the password in a request's printing: {printed}"
    );
}

#[test]
fn the_sequences_requests_match_the_hand_derived_frames() {
    let secured = Network::new(SSID, Some(PASSPHRASE)).expect("the synthetic network");
    let mut buf = [0u8; FRAME_BUF];
    let stages = [
        STAGE_INFRA,
        STAGE_SUPPLICANT,
        STAGE_AUTH_MODE,
        STAGE_CIPHER,
        STAGE_AUTH_TYPE,
        STAGE_DISASSOCIATE,
        STAGE_PASSPHRASE,
        STAGE_NETWORK_NAME,
    ];
    let steps = [1u8, 2, 3, 4, 5, 7, 8, 9];
    for (i, (command, name, value)) in secured_values().into_iter().enumerate() {
        let step = steps[i];
        let r = request(secured, step, false).expect("a step");
        assert_eq!(
            (r.stage, r.set, r.cmd, r.name),
            (stages[i], true, command, name),
            "step {step}"
        );
        let seq = 13 + i as u8;
        let id = 14 + i as u16;
        let n = r.build(&mut buf, seq, id);
        assert_eq!(n, r.frame_len());
        assert_eq!(
            &buf[..n],
            &request_frame(seq, id, true, command, name, &value)[..],
            "step {step}"
        );
    }
    assert!(
        request(secured, 6, false).is_none(),
        "no protection step on a WPA2 network"
    );
    assert!(request(secured, 10, false).is_none());
    assert!(request(secured, 0, false).is_none());
    let reset =
        request(secured, 6, true).expect("the protection returned to none after a WPA3 join");
    assert_eq!(
        (reset.stage, reset.cmd, reset.name),
        (STAGE_PROTECTION, cmd::SET_VAR, var::MFP)
    );
    let n = reset.build(&mut buf, 0, 1);
    assert_eq!(
        &buf[..n],
        &request_frame(0, 1, true, 263, b"mfp", &[0, 0, 0, 0])[..]
    );
    let hand: Vec<Vec<u8>> = secured_values()
        .into_iter()
        .enumerate()
        .map(|(i, (command, name, value))| {
            request_frame(13 + i as u8, 14 + i as u16, true, command, name, &value)
        })
        .collect();
    assert_eq!(hand[0].len(), 32);
    assert_eq!(
        &hand[0][..],
        &[
            0x20, 0x00, 0xDF, 0xFF, 0x0D, 0x00, 0x00, 0x0C, 0x00, 0x00, 0x00, 0x00, 0x14, 0x00,
            0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x02, 0x00, 0x0E, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x01, 0x00, 0x00, 0x00
        ][..]
    );
    assert_eq!(hand[1].len(), 51);
    assert_eq!(
        &hand[1][..28],
        &[
            0x33, 0x00, 0xCC, 0xFF, 0x0E, 0x00, 0x00, 0x0C, 0x00, 0x00, 0x00, 0x00, 0x07, 0x01,
            0x00, 0x00, 0x17, 0x00, 0x00, 0x00, 0x02, 0x00, 0x0F, 0x00, 0x00, 0x00, 0x00, 0x00
        ][..]
    );
    assert_eq!(&hand[1][28..43], b"bsscfg:sup_wpa\0");
    assert_eq!(&hand[1][43..], &[0, 0, 0, 0, 1, 0, 0, 0]);
    assert_eq!(&hand[2][12..16], &[0xA5, 0, 0, 0]);
    assert_eq!(&hand[2][28..], &[0x80, 0, 0, 0]);
    assert_eq!(&hand[3][12..16], &[0x86, 0, 0, 0]);
    assert_eq!(&hand[4][12..16], &[0x16, 0, 0, 0]);
    assert_eq!(hand[5].len(), 40);
    assert_eq!(
        &hand[5][..28],
        &[
            0x28, 0x00, 0xD7, 0xFF, 0x12, 0x00, 0x00, 0x0C, 0x00, 0x00, 0x00, 0x00, 0x34, 0x00,
            0x00, 0x00, 0x0C, 0x00, 0x00, 0x00, 0x02, 0x00, 0x13, 0x00, 0x00, 0x00, 0x00, 0x00
        ][..]
    );
    assert_eq!(&hand[5][28..], &[0; 12]);
    assert_eq!(hand[6].len(), 96);
    assert_eq!(
        &hand[6][..32],
        &[
            0x60, 0x00, 0x9F, 0xFF, 0x13, 0x00, 0x00, 0x0C, 0x00, 0x00, 0x00, 0x00, 0x0C, 0x01,
            0x00, 0x00, 0x44, 0x00, 0x00, 0x00, 0x02, 0x00, 0x14, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x14, 0x00, 0x01, 0x00
        ][..]
    );
    assert_eq!(&hand[6][32..52], PASSPHRASE);
    assert!(hand[6][52..].iter().all(|&b| b == 0));
    assert_eq!(hand[7].len(), 64);
    assert_eq!(
        &hand[7][..32],
        &[
            0x40, 0x00, 0xBF, 0xFF, 0x14, 0x00, 0x00, 0x0C, 0x00, 0x00, 0x00, 0x00, 0x1A, 0x00,
            0x00, 0x00, 0x24, 0x00, 0x00, 0x00, 0x02, 0x00, 0x15, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x0D, 0x00, 0x00, 0x00
        ][..]
    );
    assert_eq!(&hand[7][32..45], SSID);
    assert!(hand[7][45..].iter().all(|&b| b == 0));

    let open = Network::new(SSID_B, None).expect("the open network");
    assert!(
        request(open, 8, false).is_none(),
        "no secret step on an open network"
    );
    assert!(
        request(open, 6, false).is_none(),
        "no protection step on an open network"
    );
    let steps = [1u8, 2, 3, 4, 5, 7, 9];
    for (i, (command, name, value)) in open_values().into_iter().enumerate() {
        let r = request(open, steps[i], false).expect("a step");
        let n = r.build(&mut buf, 0, 1);
        assert_eq!(
            &buf[..n],
            &request_frame(0, 1, true, command, name, &value)[..],
            "open step {}",
            steps[i]
        );
    }
    assert_eq!(
        (STEP_FIRST, STEP_PROTECTION, STEP_REENTRY, STEP_LAST),
        (1, 6, 7, 9)
    );

    let n = DISCONNECT.build(&mut buf, 26, 27);
    assert_eq!(
        &buf[..n],
        &request_frame(26, 27, true, 52, b"", &[0; 12])[..]
    );
    assert_eq!(
        (DISCONNECT.stage, DISCONNECT.cmd),
        (STAGE_DISCONNECT, cmd::DISASSOC)
    );
    let r: Request<'static> = DISCONNECT;
    assert_eq!(r.frame_len(), 40);
}

#[test]
fn the_wpa3_sequences_requests_match_the_hand_derived_frames() {
    let sae = Network::with(SSID, Credential::SaePassword(PASSPHRASE)).expect("the WPA3 network");
    let mut buf = [0u8; FRAME_BUF];
    let stages = [
        STAGE_INFRA,
        STAGE_SUPPLICANT,
        STAGE_AUTH_MODE,
        STAGE_CIPHER,
        STAGE_AUTH_TYPE,
        STAGE_PROTECTION,
        STAGE_DISASSOCIATE,
        STAGE_PASSWORD,
        STAGE_NETWORK_NAME,
    ];
    for (i, (command, name, value)) in sae_values().into_iter().enumerate() {
        let step = i as u8 + 1;
        let r = request(sae, step, false).expect("a step");
        assert_eq!(
            (r.stage, r.set, r.cmd, r.name),
            (stages[i], true, command, name),
            "step {step}"
        );
        let seq = 12 + step;
        let id = 13 + u16::from(step);
        let n = r.build(&mut buf, seq, id);
        assert_eq!(n, r.frame_len());
        assert_eq!(
            &buf[..n],
            &request_frame(seq, id, true, command, name, &value)[..],
            "step {step}"
        );
    }
    assert!(request(sae, 10, false).is_none());
    let protection = request(sae, 6, true).expect("the protection step");
    assert_eq!(
        protection.payload,
        Payload::Word(mfp::REQUIRED),
        "a WPA3 join requires it whatever went before"
    );
    let hand: Vec<Vec<u8>> = sae_values()
        .into_iter()
        .enumerate()
        .map(|(i, (command, name, value))| {
            request_frame(13 + i as u8, 14 + i as u16, true, command, name, &value)
        })
        .collect();
    assert_eq!(
        &hand[2][28..],
        &[0x00, 0x00, 0x04, 0x00],
        "the authentication mode: SAE with a password"
    );
    assert_eq!(
        &hand[4][28..],
        &[0x03, 0x00, 0x00, 0x00],
        "the authentication type: the SAE algorithm number"
    );
    assert_eq!(hand[5].len(), 36);
    assert_eq!(
        &hand[5][..28],
        &[
            0x24, 0x00, 0xDB, 0xFF, 0x12, 0x00, 0x00, 0x0C, 0x00, 0x00, 0x00, 0x00, 0x07, 0x01,
            0x00, 0x00, 0x08, 0x00, 0x00, 0x00, 0x02, 0x00, 0x13, 0x00, 0x00, 0x00, 0x00, 0x00
        ][..]
    );
    assert_eq!(&hand[5][28..32], b"mfp\0");
    assert_eq!(&hand[5][32..], &[0x02, 0x00, 0x00, 0x00]);
    assert_eq!(hand[7].len(), 171);
    assert_eq!(
        &hand[7][..28],
        &[
            0xAB, 0x00, 0x54, 0xFF, 0x14, 0x00, 0x00, 0x0C, 0x00, 0x00, 0x00, 0x00, 0x07, 0x01,
            0x00, 0x00, 0x8F, 0x00, 0x00, 0x00, 0x02, 0x00, 0x15, 0x00, 0x00, 0x00, 0x00, 0x00
        ][..]
    );
    assert_eq!(&hand[7][28..41], b"sae_password\0");
    assert_eq!(&hand[7][41..43], &[0x14, 0x00]);
    assert_eq!(&hand[7][43..63], PASSPHRASE);
    assert_eq!(hand[7][63..].len(), 108);
    assert!(hand[7][63..].iter().all(|&b| b == 0));
    assert_eq!(hand[8].len(), 64);
    assert_eq!(&hand[8][32..45], SSID);
}

#[test]
fn a_key_frames_bytes_are_cleared_once_the_bus_took_them() {
    let secured = Network::new(SSID, Some(PASSPHRASE)).expect("the synthetic network");
    let request = request(secured, 8, false).expect("the passphrase step");
    let frame = leak(request_frame(
        0,
        1,
        true,
        cmd::SET_WSEC_PMK,
        b"",
        &secured_values()[6].2,
    ));
    assert_eq!(frame.len(), 96);
    let script = [
        Op::F2Write {
            data: frame,
            accepted: false,
        },
        Op::F2Write {
            data: frame,
            accepted: true,
        },
    ];
    let mut bus = FakeTransport::new(&script, Part::Cyw43439);
    let mut window = Window::new();
    let mut layer = Layer::new();
    layer.credit.take(1);
    let mut ids = 0u16;
    let mut buf = [0u8; FRAME_BUF];
    let mut exchange = Exchange::new(request, 0);
    let p = exchange
        .step(&mut bus, &mut window, &mut buf, &mut layer, &mut ids, 0)
        .expect("a step");
    assert_eq!(p, Progress::Wake(Wake::At(10_000)), "not accepted: retried");
    assert_eq!(&buf[..96], frame, "the frame kept for the retry");
    assert_eq!(&buf[32..52], PASSPHRASE, "the passphrase inside it");
    let p = exchange
        .step(&mut bus, &mut window, &mut buf, &mut layer, &mut ids, 0)
        .expect("a step");
    assert_eq!(p, Progress::Wake(Wake::Again), "accepted");
    assert!(
        buf.iter().all(|&b| b == 0),
        "the frame cleared once the bus took it"
    );
    assert_eq!((layer.frames.sent, exchange.id()), (1, 1));
    assert_eq!(bus.remaining(), 0);
    assert_eq!(bus.fault(), None);
}

#[test]
fn a_password_frames_bytes_are_cleared_once_the_bus_took_them() {
    let sae = Network::with(SSID, Credential::SaePassword(PASSPHRASE)).expect("the WPA3 network");
    let request = request(sae, 8, false).expect("the password step");
    let frame = leak(request_frame(
        0,
        1,
        true,
        cmd::SET_VAR,
        b"sae_password",
        &sae_values()[7].2,
    ));
    assert_eq!(frame.len(), 171);
    let script = [
        Op::F2Write {
            data: frame,
            accepted: false,
        },
        Op::F2Write {
            data: frame,
            accepted: true,
        },
    ];
    let mut bus = FakeTransport::new(&script, Part::Cyw43439);
    let mut window = Window::new();
    let mut layer = Layer::new();
    layer.credit.take(1);
    let mut ids = 0u16;
    let mut buf = [0u8; FRAME_BUF];
    let mut exchange = Exchange::new(request, 0);
    let p = exchange
        .step(&mut bus, &mut window, &mut buf, &mut layer, &mut ids, 0)
        .expect("a step");
    assert_eq!(p, Progress::Wake(Wake::At(10_000)), "not accepted: retried");
    assert_eq!(&buf[..171], frame, "the frame kept for the retry");
    assert_eq!(&buf[43..63], PASSPHRASE, "the password inside it");
    let p = exchange
        .step(&mut bus, &mut window, &mut buf, &mut layer, &mut ids, 0)
        .expect("a step");
    assert_eq!(p, Progress::Wake(Wake::Again), "accepted");
    assert!(
        buf.iter().all(|&b| b == 0),
        "the frame cleared once the bus took it"
    );
    assert_eq!((layer.frames.sent, exchange.id()), (1, 1));
    assert_eq!(bus.remaining(), 0);
    assert_eq!(bus.fault(), None);
}

#[test]
fn the_numbers_the_names_and_the_classes() {
    assert_eq!(
        (
            cmd::UP,
            cmd::SET_INFRA,
            cmd::SET_AUTH,
            cmd::SET_SSID,
            cmd::SET_PASSIVE_SCAN,
            cmd::DISASSOC,
            cmd::SET_WSEC,
            cmd::SET_WPA_AUTH,
            cmd::SET_WSEC_PMK
        ),
        (2, 20, 22, 26, 49, 52, 134, 165, 268)
    );
    assert_eq!(var::ADDRESS, b"cur_etheraddr");
    assert_eq!(var::SCAN, b"escan");
    assert_eq!(var::SUP_WPA, b"bsscfg:sup_wpa");
    assert_eq!(var::MFP, b"mfp");
    assert_eq!(var::SAE_PASSWORD, b"sae_password");
    assert_eq!(var::CAPABILITIES, b"cap");
    assert_eq!(STATUS_NOT_UP, 0xFFFF_FFFC);
    assert_eq!(STATUS_NOT_UP as i32, -4);
    assert_eq!(
        (
            auth_mode::DISABLED,
            auth_mode::WPA_PSK,
            auth_mode::WPA2_PSK,
            auth_mode::WPA3_SAE
        ),
        (0, 4, 0x80, 0x4_0000)
    );
    assert_eq!((AUTH_OPEN_SYSTEM, AUTH_SAE), (0, 3));
    assert_eq!((mfp::NONE, mfp::CAPABLE, mfp::REQUIRED), (0, 1, 2));
    assert_eq!(
        (cipher::NONE, cipher::WEP, cipher::TKIP, cipher::AES),
        (0, 1, 2, 4)
    );
    assert_eq!(
        Payload::Key(Passphrase::new(PASSPHRASE).expect("held")).len(),
        68
    );
    assert_eq!(
        Payload::SaePassword(SaePassword::new(PASSPHRASE).expect("held")).len(),
        130
    );
    assert_eq!(Payload::Ssid(SSID).len(), 36);
    assert_eq!(Payload::Scan { sync: 1 }.len(), 74);
    let failure = JoinFailure::Association {
        status: 3,
        assoc_status: 0,
        auth_status: 0,
    };
    assert_ne!(failure, JoinFailure::Timeout);
    assert_ne!(
        JoinFailure::Mismatch {
            advertised: Advertised::default()
        },
        JoinFailure::NotFound
    );
    assert_ne!(LinkState::Down, LinkState::Detached);
    assert_ne!(Security::Wpa2Psk, Security::Wpa3Sae);
}
