//! The frame layer's and the control path's pure parts: the frame header's
//! round trip and its faults, the credit window, the control header and its
//! flags, the request frames against hand-derived bytes, the regulatory
//! download's chunk header and rounding, the reply's matching, the
//! version's strip and comparison, the command numbers and the names.

use super::control::{
    BLOB, EXPECTED, VERSION_REPLY, VERSION_STRIPPED, chunk_value, reply_frame, request_frame,
    transactions, version_payload,
};
use crate::control::{
    CONTROL_HEADER_LEN, ControlHeader, DOWNLOAD_HEADER_LEN, IFACE_STATION, Payload,
    REGULATORY_CHUNK, REPLY_WAIT_US, REQUEST_HEAD, Reply, Request, SEND_WAIT_US, SERVICE_POLL_US,
    VERSION_MAX, Version, chunk_len, cmd, download, download_header, flags, round8, take_reply,
    var,
};
use crate::event::EventMask;
use crate::frame::{Credit, FRAME_BUF, Fault, HEADER_LEN, Header, Inspection, channel, inspect};
use std::vec::Vec;

#[test]
fn the_header_round_trips() {
    let mut bytes = [0u8; 12];
    Header::transmit(1452, 0, channel::CONTROL, 12).write(&mut bytes);
    assert_eq!(bytes, [0xAC, 0x05, 0x53, 0xFA, 0, 0, 0, 12, 0, 0, 0, 0]);
    let mut event = std::vec![
        0x00, 0x01, 0xFF, 0xFE, 0x01, 0x01, 0x00, 0x0C, 0x00, 0x10, 0x00, 0x00
    ];
    event.resize(256, 0);
    let header = Header {
        size: 256,
        sequence: 1,
        channel: 1,
        next_length: 0,
        data_offset: 12,
        flow_control: 0,
        credit: 16,
    };
    assert_eq!(inspect(&event), Inspection::Frame(header));
    assert_eq!(header.channel_id(), channel::EVENT);
    assert_eq!(
        Header {
            channel: 0xF2,
            ..header
        }
        .channel_id(),
        channel::DATA
    );
    assert_eq!(HEADER_LEN, 12);
    assert_eq!(FRAME_BUF, 2048);
}

#[test]
fn the_faults_are_named() {
    assert_eq!(inspect(&[0u8; 12]), Inspection::Empty, "a zero size field");
    assert_eq!(inspect(&[0u8; 2]), Inspection::Empty);
    assert_eq!(
        inspect(&[0x1C, 0x00, 0xE3, 0xFF, 0, 0, 0, 12]),
        Inspection::Bad(Fault::Short)
    );
    assert_eq!(inspect(&[0x1C]), Inspection::Bad(Fault::Short));
    let mut frame = reply_frame(2, 17, 263, 1, 0, &[]);
    frame[2] = 0xE2;
    assert_eq!(inspect(&frame), Inspection::Bad(Fault::Checksum));
    let mut frame = reply_frame(2, 17, 263, 1, 0, &[]);
    frame[0] = 8;
    frame[2] = 0xF7;
    assert_eq!(
        inspect(&frame),
        Inspection::Bad(Fault::Size),
        "under the header"
    );
    let mut frame = reply_frame(2, 17, 263, 1, 0, &[]);
    frame[0] = 40;
    frame[2] = 0xD7;
    assert_eq!(
        inspect(&frame),
        Inspection::Bad(Fault::Size),
        "over the bytes read"
    );
    let mut frame = reply_frame(2, 17, 263, 1, 0, &[]);
    frame[7] = 8;
    assert_eq!(
        inspect(&frame),
        Inspection::Bad(Fault::Offset),
        "under the header"
    );
    let mut frame = reply_frame(2, 17, 263, 1, 0, &[]);
    frame[7] = 29;
    assert_eq!(
        inspect(&frame),
        Inspection::Bad(Fault::Offset),
        "over the size"
    );
    let credit_only = [
        0x0C, 0x00, 0xF3, 0xFF, 0x05, 0x00, 0x00, 0x00, 0x00, 0x19, 0x00, 0x00,
    ];
    let Inspection::Frame(header) = inspect(&credit_only) else {
        panic!("the twelve-byte frame's offset is not judged");
    };
    assert_eq!(
        (header.size, header.credit, header.data_offset),
        (12, 25, 0)
    );
}

#[test]
fn the_credit_window() {
    let mut credit = Credit::new();
    assert!(!credit.open(), "closed at the start");
    assert_eq!(credit.window(), (0, 0));
    credit.take(16);
    assert!(credit.open());
    assert_eq!(credit.issue(), 0);
    assert_eq!(credit.window(), (1, 16));
    assert_eq!(credit.word(), 0x1001);
    for _ in 0..255 {
        credit.issue();
    }
    assert_eq!(credit.window(), (0, 16), "the sequence wraps at eight bits");
    credit.take(0);
    assert!(!credit.open());
    assert_eq!(Credit::default(), Credit::new());
}

#[test]
fn the_control_header_and_its_flags() {
    let header = ControlHeader::request(cmd::SET_VAR, 1424, true, 1);
    assert_eq!(header.flags, 0x0001_0002);
    assert_eq!(header.id(), 1);
    assert!(!header.is_error());
    let mut bytes = [0u8; 16];
    header.write(&mut bytes);
    assert_eq!(
        bytes,
        [
            0x07, 0x01, 0x00, 0x00, 0x90, 0x05, 0x00, 0x00, 0x02, 0x00, 0x01, 0x00, 0x00, 0x00,
            0x00, 0x00
        ]
    );
    assert_eq!(ControlHeader::parse(&bytes), Some(header));
    assert_eq!(ControlHeader::parse(&bytes[..15]), None);
    let get = ControlHeader::request(cmd::GET_VAR, 68, false, 8);
    assert_eq!(get.flags, 0x0008_0000);
    let failed = ControlHeader {
        cmd: 262,
        len: 0,
        flags: 0x0008_0001,
        status: 0xFFFF_FFFC,
    };
    assert!(failed.is_error());
    assert_eq!(failed.id(), 8);
    assert_eq!(
        (
            flags::ERROR,
            flags::SET,
            flags::IFACE_SHIFT,
            flags::ID_SHIFT
        ),
        (1, 2, 12, 16)
    );
    assert_eq!(IFACE_STATION, 0);
    assert_eq!(CONTROL_HEADER_LEN, 16);
    assert_eq!(REQUEST_HEAD, 28);
}

/// The crate's requests of the opening, in the order of the design's table.
fn requests() -> [Request<'static>; 9] {
    [
        Request {
            stage: "",
            set: true,
            cmd: cmd::SET_VAR,
            name: var::REGULATORY,
            payload: Payload::Chunk { blob: &BLOB, at: 0 },
        },
        Request {
            stage: "",
            set: true,
            cmd: cmd::SET_VAR,
            name: var::REGULATORY,
            payload: Payload::Chunk {
                blob: &BLOB,
                at: 1400,
            },
        },
        Request {
            stage: "",
            set: true,
            cmd: cmd::SET_VAR,
            name: var::GLOM,
            payload: Payload::Word(0),
        },
        Request {
            stage: "",
            set: true,
            cmd: cmd::SET_PM,
            name: b"",
            payload: Payload::Word(0),
        },
        Request {
            stage: "",
            set: true,
            cmd: cmd::SET_GMODE,
            name: b"",
            payload: Payload::Word(1),
        },
        Request {
            stage: "",
            set: true,
            cmd: cmd::SET_VAR,
            name: var::ROAM_OFF,
            payload: Payload::Word(1),
        },
        Request {
            stage: "",
            set: true,
            cmd: cmd::SET_VAR,
            name: var::SUP_WPA2_EAPVER,
            payload: Payload::Words(0, 0xFFFF_FFFF),
        },
        Request {
            stage: "",
            set: false,
            cmd: cmd::GET_VAR,
            name: var::VERSION,
            payload: Payload::Zeros(VERSION_MAX),
        },
        Request {
            stage: "",
            set: true,
            cmd: cmd::SET_VAR,
            name: var::EVENT_MASK,
            payload: Payload::Mask(EventMask::DEFAULT),
        },
    ]
}

#[test]
fn the_request_frames_match_the_hand_derived_bytes() {
    let mut buf = [0u8; FRAME_BUF];
    let hand: Vec<Vec<u8>> = transactions(true, EventMask::DEFAULT)
        .into_iter()
        .enumerate()
        .map(|(i, (set, cmd, name, value))| {
            request_frame(i as u8, i as u16 + 1, set, cmd, name, &value)
        })
        .collect();
    for (i, request) in requests().iter().enumerate() {
        let n = request.build(&mut buf, i as u8, i as u16 + 1);
        assert_eq!(n, request.frame_len(), "request {}", i + 1);
        assert_eq!(&buf[..n], &hand[i][..], "request {}", i + 1);
    }
    assert_eq!(hand[0].len(), 1452);
    assert_eq!(
        &hand[0][..48],
        &[
            0xAC, 0x05, 0x53, 0xFA, 0x00, 0x00, 0x00, 0x0C, 0x00, 0x00, 0x00, 0x00, 0x07, 0x01,
            0x00, 0x00, 0x90, 0x05, 0x00, 0x00, 0x02, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x63, 0x6C, 0x6D, 0x6C, 0x6F, 0x61, 0x64, 0x00, 0x02, 0x10, 0x02, 0x00, 0x78, 0x05,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00
        ][..]
    );
    assert_eq!(&hand[0][48..1448], &BLOB[..1400]);
    assert_eq!(&hand[0][1448..], &[0, 0, 0, 0]);
    assert_eq!(hand[1].len(), 148);
    assert_eq!(
        &hand[1][..48],
        &[
            0x94, 0x00, 0x6B, 0xFF, 0x01, 0x00, 0x00, 0x0C, 0x00, 0x00, 0x00, 0x00, 0x07, 0x01,
            0x00, 0x00, 0x78, 0x00, 0x00, 0x00, 0x02, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x63, 0x6C, 0x6D, 0x6C, 0x6F, 0x61, 0x64, 0x00, 0x04, 0x10, 0x02, 0x00, 0x64, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00
        ][..]
    );
    assert_eq!(&hand[1][48..], &BLOB[1400..]);
    let glom: Vec<u8> = [
        &[
            0x2B, 0x00, 0xD4, 0xFF, 0x02, 0x00, 0x00, 0x0C, 0x00, 0x00, 0x00, 0x00,
        ][..],
        &[
            0x07, 0x01, 0x00, 0x00, 0x0F, 0x00, 0x00, 0x00, 0x02, 0x00, 0x03, 0x00, 0x00, 0x00,
            0x00, 0x00,
        ][..],
        b"bus:txglom\0",
        &[0, 0, 0, 0][..],
    ]
    .concat();
    assert_eq!(hand[2], glom);
    let pm: Vec<u8> = [
        &[
            0x20, 0x00, 0xDF, 0xFF, 0x03, 0x00, 0x00, 0x0C, 0x00, 0x00, 0x00, 0x00,
        ][..],
        &[
            0x56, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x02, 0x00, 0x04, 0x00, 0x00, 0x00,
            0x00, 0x00,
        ][..],
        &[0, 0, 0, 0][..],
    ]
    .concat();
    assert_eq!(hand[3], pm);
    assert_eq!(&hand[4][12..16], &[0x6E, 0, 0, 0]);
    assert_eq!(&hand[4][20..24], &[0x02, 0x00, 0x05, 0x00]);
    assert_eq!(&hand[4][28..], &[1, 0, 0, 0]);
    assert_eq!(&hand[5][..4], &[0x29, 0x00, 0xD6, 0xFF]);
    assert_eq!(
        &hand[5][12..24],
        &[
            0x07, 0x01, 0x00, 0x00, 0x0D, 0x00, 0x00, 0x00, 0x02, 0x00, 0x06, 0x00
        ]
    );
    assert_eq!(&hand[5][28..37], b"roam_off\0");
    assert_eq!(&hand[6][..4], &[0x3B, 0x00, 0xC4, 0xFF]);
    assert_eq!(
        &hand[6][12..24],
        &[
            0x07, 0x01, 0x00, 0x00, 0x1F, 0x00, 0x00, 0x00, 0x02, 0x00, 0x07, 0x00
        ]
    );
    assert_eq!(&hand[6][28..51], b"bsscfg:sup_wpa2_eapver\0");
    assert_eq!(&hand[6][51..], &[0, 0, 0, 0, 0xFF, 0xFF, 0xFF, 0xFF]);
    assert_eq!(hand[7].len(), 96);
    assert_eq!(
        &hand[7][..32],
        &[
            0x60, 0x00, 0x9F, 0xFF, 0x07, 0x00, 0x00, 0x0C, 0x00, 0x00, 0x00, 0x00, 0x06, 0x01,
            0x00, 0x00, 0x44, 0x00, 0x00, 0x00, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x76, 0x65, 0x72, 0x00
        ][..]
    );
    assert!(hand[7][32..].iter().all(|&b| b == 0));
    assert_eq!(hand[8].len(), 56);
    assert_eq!(&hand[8][..4], &[0x38, 0x00, 0xC7, 0xFF]);
    assert_eq!(&hand[8][28..39], b"event_msgs\0");
}

#[test]
fn the_download_header_and_the_rounding() {
    assert_eq!(
        download_header(true, true, 984),
        [0x06, 0x10, 0x02, 0x00, 0xD8, 0x03, 0, 0, 0, 0, 0, 0]
    );
    assert_eq!(&download_header(true, false, 1400)[..2], &[0x02, 0x10]);
    assert_eq!(&download_header(false, false, 1400)[..2], &[0x00, 0x10]);
    assert_eq!(&download_header(false, true, 100)[..2], &[0x04, 0x10]);
    assert_eq!(
        (
            download::VERSION,
            download::BEGIN,
            download::END,
            download::TYPE_REGULATORY
        ),
        (0x1000, 2, 4, 2)
    );
    assert_eq!(round8(1412), 1416);
    assert_eq!(round8(112), 112);
    assert_eq!(round8(12), 16);
    assert_eq!(round8(0), 0);
    assert_eq!(chunk_len(1500, 0), 1400);
    assert_eq!(chunk_len(1500, 1400), 100);
    assert_eq!(chunk_len(984, 0), 984);
    assert_eq!(chunk_len(2800, 1400), 1400);
    assert_eq!(chunk_len(2801, 2800), 1);
    assert_eq!(REGULATORY_CHUNK, 1400);
    assert_eq!(DOWNLOAD_HEADER_LEN, 12);
    let first = Payload::Chunk { blob: &BLOB, at: 0 };
    assert_eq!(first.len(), 1416);
    assert!(!first.is_empty());
    let mut buf = [0u8; 1416];
    first.write(&mut buf);
    assert_eq!(&buf[..], &chunk_value(&BLOB, 0)[..]);
    let second = Payload::Chunk {
        blob: &BLOB,
        at: 1400,
    };
    assert_eq!(second.len(), 112);
    let mut buf = [0u8; 112];
    second.write(&mut buf);
    assert_eq!(&buf[..], &chunk_value(&BLOB, 1400)[..]);
    assert_eq!(Payload::Word(1).len(), 4);
    assert_eq!(Payload::Words(0, 1).len(), 8);
    assert_eq!(Payload::Zeros(64).len(), 64);
    assert!(Payload::Zeros(0).is_empty());
}

#[test]
fn a_reply_is_taken_for_the_outstanding_id_alone() {
    let frame = reply_frame(2, 17, 263, 1, 0, &[]);
    assert_eq!(
        take_reply(&frame, 12, 1),
        Some(Reply {
            status: 0,
            flags: 0x0001_0000,
            at: 28,
            len: 0
        })
    );
    assert_eq!(take_reply(&frame, 12, 2), None, "another id");
    assert_eq!(take_reply(&frame[..24], 12, 1), None, "no control header");
    let mut long = frame.clone();
    long[16] = 8;
    assert_eq!(take_reply(&long, 12, 1), None, "a count past the frame");
    let version = reply_frame(9, 24, 262, 8, 0, &version_payload());
    let reply = take_reply(&version, 12, 8).expect("the version reply");
    assert_eq!((reply.at, reply.len, reply.status), (28, 64, 0));
    let refused = reply_frame(2, 17, 263, 1, 0xFFFF_FFFC, &[]);
    assert_eq!(
        take_reply(&refused, 12, 1).map(|r| r.status),
        Some(0xFFFF_FFFC)
    );
    assert_eq!(take_reply(&frame, 40, 1), None, "an offset past the frame");
}

#[test]
fn the_version_strips_and_compares() {
    let version = Version::from_reply(&version_payload());
    assert_eq!(version.as_bytes(), VERSION_STRIPPED);
    assert_eq!(version.len(), 54);
    assert!(!version.is_empty());
    assert_eq!(
        version.as_str(),
        Some("wl0: Sep 10 2026 00:00:00 version 7.95.49 (fixture CY)")
    );
    assert_eq!(
        Version::from_reply(VERSION_REPLY),
        version,
        "the padding makes no difference"
    );
    assert!(version.contains(EXPECTED));
    assert!(version.contains(VERSION_STRIPPED));
    assert!(!version.contains(b"9.99.99"));
    assert!(!version.contains(b""), "nothing to find");
    assert_eq!(Version::from_reply(b"abc\0\0\r\n").as_bytes(), b"abc");
    assert!(Version::from_reply(&[0u8; 100]).is_empty());
    assert_eq!(
        Version::from_reply(&[b'x'; 70]).len(),
        VERSION_MAX,
        "at most the capacity"
    );
    assert_eq!(Version::EMPTY.as_bytes(), b"");
    assert_eq!(Version::from_reply(b"").as_bytes(), b"");
}

#[test]
fn the_command_numbers_the_names_and_the_budgets() {
    assert_eq!(
        (cmd::GET_VAR, cmd::SET_VAR, cmd::SET_PM, cmd::SET_GMODE),
        (262, 263, 86, 110)
    );
    assert_eq!(var::VERSION, b"ver");
    assert_eq!(var::REGULATORY, b"clmload");
    assert_eq!(var::GLOM, b"bus:txglom");
    assert_eq!(var::ROAM_OFF, b"roam_off");
    assert_eq!(var::SUP_WPA2_EAPVER, b"bsscfg:sup_wpa2_eapver");
    assert_eq!(VERSION_MAX, 64);
    assert_eq!(
        (SERVICE_POLL_US, SEND_WAIT_US, REPLY_WAIT_US),
        (10_000, 3_000_000, 10_000_000)
    );
}
