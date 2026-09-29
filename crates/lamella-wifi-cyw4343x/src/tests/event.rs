//! The event path's pure parts: the mask's bytes and its bounds, the walk
//! through a hand-built event frame and its faults, the big-endian fields,
//! the reading of one event for the link on the observed sequences, the
//! offset forms of a reason, the numbers, the statuses and the reasons,
//! and the mask request frame against hand-derived bytes.

use super::control::{ADDRESS, LINK_PAYLOAD, event_of, header, request_frame};
use crate::control::{Payload, Request, cmd, var};
use crate::event::{
    ETHERTYPE, EVENT_COUNT, Event, EventMask, Fault, Link, MASK_LEN, OUI, Parsed, number, parse,
    reason, status,
};
use crate::frame::FRAME_BUF;

#[test]
fn the_default_mask_is_the_thirteen_numbers() {
    assert_eq!(
        EventMask::DEFAULT.as_bytes(),
        &[
            0xEB, 0x18, 0x01, 0, 0, 0x41, 0, 0, 0x20, 0, 0x20, 0, 0, 0, 0, 0, 0
        ]
    );
    for n in [0, 1, 3, 5, 6, 7, 11, 12, 16, 40, 46, 69, 85] {
        assert!(EventMask::DEFAULT.contains(n), "event {n}");
    }
    assert!(!EventMask::DEFAULT.contains(2));
    assert!(!EventMask::DEFAULT.contains(number::SCAN_COMPLETE));
    assert_eq!(
        (0..EVENT_COUNT)
            .filter(|&n| EventMask::DEFAULT.contains(n))
            .count(),
        13
    );
    assert_eq!(EventMask::default(), EventMask::DEFAULT);
    assert_eq!(EventMask::EMPTY.as_bytes(), &[0; MASK_LEN]);
    assert_eq!((MASK_LEN, EVENT_COUNT, number::COUNT), (17, 129, 129));
}

#[test]
fn a_number_is_added_inside_the_count_and_refused_past_it() {
    let mut mask = EventMask::EMPTY;
    assert!(mask.add(number::SCAN_COMPLETE));
    assert!(mask.contains(26));
    assert_eq!(mask.as_bytes()[3], 0x04, "event 26 is bit 2 of byte 3");
    assert!(mask.add(128));
    assert_eq!(
        mask.as_bytes()[16],
        0x01,
        "event 128 is bit 0 of the last byte"
    );
    let bytes = *mask.as_bytes();
    assert!(!mask.add(129));
    assert!(!mask.add(200));
    assert!(!mask.add(u32::MAX));
    assert!(!mask.contains(129));
    assert!(!mask.contains(u32::MAX));
    assert!(mask.add(26), "adding twice is the same mask");
    assert_eq!(mask.as_bytes(), &bytes);
}

#[test]
fn the_walk_reads_the_header_big_endian() {
    let frame = event_of(
        11,
        26,
        0,
        number::PSK_SUP,
        status::UNSOLICITED,
        0,
        2,
        ADDRESS,
        &[],
    );
    assert_eq!(frame.len(), 88);
    assert_eq!(
        &frame[..12],
        &[
            0x58, 0x00, 0xA7, 0xFF, 0x0B, 0x01, 0x00, 0x0C, 0x00, 0x1A, 0x00, 0x00
        ]
    );
    assert_eq!(
        &frame[12..16],
        &[0x20, 0, 0, 0],
        "the BDC header, no padding"
    );
    assert_eq!(&frame[28..30], &ETHERTYPE);
    assert_eq!(
        &frame[30..40],
        &[0x00, 0x01, 0x00, 0x30, 0x01, 0x00, 0x10, 0x18, 0x00, 0x01]
    );
    assert_eq!(
        &frame[44..48],
        &[0, 0, 0, 46],
        "the number, most significant byte first"
    );
    assert_eq!(&frame[48..52], &[0, 0, 0, 6], "the status");
    assert_eq!(&frame[56..60], &[0, 0, 0, 2], "the authentication type");
    assert_eq!(&frame[64..70], &ADDRESS);
    let Parsed::Event { event, at } = parse(&frame, 12) else {
        panic!("an event");
    };
    assert_eq!(
        event,
        Event {
            number: 46,
            status: 6,
            reason: 0,
            auth_type: 2,
            address: ADDRESS,
            len: 0
        }
    );
    assert_eq!(at, 88);
    assert_eq!(
        event.link(),
        Link::SupplicantUp,
        "keyed on the reason, not the status"
    );
}

#[test]
fn the_bdc_offset_is_in_words_and_the_payload_lies_after_the_header() {
    let frame = event_of(12, 27, 1, number::LINK, 0, 0, 0, ADDRESS, &LINK_PAYLOAD);
    assert_eq!(frame.len(), 100);
    assert_eq!(
        &frame[..12],
        &[
            0x64, 0x00, 0x9B, 0xFF, 0x0C, 0x01, 0x00, 0x0C, 0x00, 0x1B, 0x00, 0x00
        ]
    );
    assert_eq!(
        &frame[12..20],
        &[0x20, 0, 0, 1, 0, 0, 0, 0],
        "one word of padding"
    );
    assert_eq!(&frame[32..34], &ETHERTYPE);
    assert_eq!(&frame[64..68], &[0, 0, 0, 8], "the payload's length");
    let Parsed::Event { event, at } = parse(&frame, 12) else {
        panic!("an event");
    };
    assert_eq!((event.number, event.len, at), (16, 8, 92));
    assert_eq!(&frame[at..at + event.len], &LINK_PAYLOAD);
    assert_eq!(
        event.link(),
        Link::Unchanged,
        "a link indication with reason 0 says nothing"
    );
}

#[test]
fn a_payload_is_delivered_as_the_bytes_present() {
    let mut frame = event_of(12, 27, 0, number::LINK, 0, 0, 0, ADDRESS, &LINK_PAYLOAD);
    frame[60..64].copy_from_slice(&[0, 0, 0, 20]);
    let Parsed::Event { event, at } = parse(&frame, 12) else {
        panic!("an event");
    };
    assert_eq!(
        (event.len, at),
        (8, 88),
        "a count past the frame is clipped"
    );
    frame[60..64].copy_from_slice(&[0, 0, 0, 3]);
    let Parsed::Event { event, .. } = parse(&frame, 12) else {
        panic!("an event");
    };
    assert_eq!(event.len, 3, "a count inside the frame is the count");
}

#[test]
fn the_empty_event_and_the_faults_are_named() {
    let mut empty = header(16, 5, 1, 16, 26);
    empty.resize(16, 0);
    assert_eq!(
        parse(&empty, 16),
        Parsed::Empty,
        "the payload offset equals the size"
    );
    let frame = event_of(11, 26, 0, number::RADIO, 0, 0, 0, ADDRESS, &[]);
    assert_eq!(parse(&frame, 88), Parsed::Empty);
    assert_eq!(
        parse(&frame, 89),
        Parsed::Dropped(Fault::Short),
        "an offset past the frame"
    );
    for cut in [13, 14, 15, 20, 30, 34, 40, 80, 87] {
        assert_eq!(
            parse(&frame[..cut], 12),
            Parsed::Dropped(Fault::Short),
            "cut at {cut}"
        );
    }
    let mut wrong = frame.clone();
    wrong[29] = 0x6D;
    assert_eq!(parse(&wrong, 12), Parsed::Dropped(Fault::Ethertype));
    let mut wrong = frame.clone();
    wrong[37] = 0x19;
    assert_eq!(parse(&wrong, 12), Parsed::Dropped(Fault::Oui));
    let mut wrong = frame.clone();
    wrong[44..48].copy_from_slice(&[0, 0, 0, 129]);
    assert_eq!(parse(&wrong, 12), Parsed::Dropped(Fault::Number));
    wrong[44..48].copy_from_slice(&[0, 0, 0, 200]);
    assert_eq!(parse(&wrong, 12), Parsed::Dropped(Fault::Number));
    wrong[44..48].copy_from_slice(&[0, 0, 0, 128]);
    assert!(matches!(parse(&wrong, 12), Parsed::Event { event, .. } if event.number == 128));
    assert_eq!(
        parse(&frame, 12),
        parse(&frame, 12),
        "the walk is a function of the bytes"
    );
}

const fn ev(number: u32, status: u32, reason: u32) -> Event {
    Event {
        number,
        status,
        reason,
        auth_type: 0,
        address: [0; 6],
        len: 0,
    }
}

#[test]
fn the_reading_on_the_observed_sequences() {
    // A successful secured join: the authentication, the association, the
    // handshake reported unsolicited with reason 0.
    assert_eq!(ev(number::AUTH, 0, 0).link(), Link::Unchanged);
    assert_eq!(ev(number::ASSOC, 0, 0).link(), Link::Unchanged);
    assert_eq!(ev(number::JOIN, 0, 0).link(), Link::Unchanged);
    assert_eq!(
        ev(number::PSK_SUP, status::UNSOLICITED, 0).link(),
        Link::SupplicantUp
    );
    assert_eq!(
        ev(number::PSK_SUP, status::UNSOLICITED, 512).link(),
        Link::SupplicantUp,
        "the offset form"
    );
    assert_eq!(
        ev(number::SET_SSID, status::SUCCESS, 0).link(),
        Link::Associated,
        "associated, not up"
    );
    // The failures.
    assert_eq!(
        ev(number::ASSOC, status::NO_ACK, 0).link(),
        Link::Unchanged,
        "the association's status is the ladder's"
    );
    assert_eq!(
        ev(number::SET_SSID, status::FAIL, 0).link(),
        Link::JoinFailed
    );
    assert_eq!(
        ev(number::SET_SSID, status::NO_NETWORKS, 0).link(),
        Link::JoinFailed
    );
    assert_eq!(
        ev(
            number::PSK_SUP,
            status::UNSOLICITED,
            reason::supplicant::WPA_PSK_TIMEOUT
        )
        .link(),
        Link::SupplicantDown
    );
    assert_eq!(
        ev(number::PSK_SUP, 0, 527).link(),
        Link::SupplicantDown,
        "the offset form of the timeout"
    );
    // The observed loss burst.
    assert_eq!(
        ev(
            number::PSK_SUP,
            status::UNSOLICITED,
            reason::supplicant::DEAUTH
        )
        .link(),
        Link::SupplicantDown
    );
    assert_eq!(
        ev(
            number::DISASSOC_IND,
            0,
            reason::dot11::PREVIOUS_AUTH_INVALID
        )
        .link(),
        Link::Down
    );
    assert_eq!(
        ev(number::DEAUTH, 0, reason::dot11::CLASS2_FROM_NONAUTH).link(),
        Link::Down
    );
    assert_eq!(
        ev(number::DEAUTH, 0, reason::dot11::CLASS3_FROM_NONASSOC).link(),
        Link::Down
    );
    assert_eq!(ev(number::LINK, 0, 1).link(), Link::Down);
    assert_eq!(ev(number::DEAUTH_IND, 0, 8).link(), Link::Down);
    assert_eq!(ev(number::DISASSOC, 0, 3).link(), Link::Down);
    assert_eq!(
        ev(number::DEAUTH, 0, 774).link(),
        Link::Down,
        "the offset form changes nothing here"
    );
    // What says nothing.
    assert_eq!(ev(number::LINK, 0, 0).link(), Link::Unchanged);
    assert_eq!(ev(number::RADIO, 0, 0).link(), Link::Unchanged);
    assert_eq!(
        ev(number::ESCAN_RESULT, status::PARTIAL, 0).link(),
        Link::Unchanged
    );
    assert_eq!(ev(number::ASSOC_IND_NDIS, 0, 0).link(), Link::Unchanged);
}

#[test]
fn the_offset_forms_and_the_codes() {
    assert_eq!(reason::bare(526, reason::SUPPLICANT_OFFSET), 14);
    assert_eq!(reason::bare(14, reason::SUPPLICANT_OFFSET), 14);
    assert_eq!(reason::bare(512, reason::SUPPLICANT_OFFSET), 0);
    assert_eq!(reason::bare(767, reason::SUPPLICANT_OFFSET), 255);
    assert_eq!(
        reason::bare(768, reason::SUPPLICANT_OFFSET),
        768,
        "another space's value is left alone"
    );
    assert_eq!(reason::bare(774, reason::DOT11_OFFSET), 6);
    assert_eq!(reason::bare(2, reason::DOT11_OFFSET), 2);
    assert_eq!(reason::bare(1024, reason::DOT11_OFFSET), 1024);
    assert_eq!(reason::bare(300, reason::PRUNE_OFFSET), 44);
    assert_eq!(
        (
            reason::PRUNE_OFFSET,
            reason::SUPPLICANT_OFFSET,
            reason::DOT11_OFFSET
        ),
        (256, 512, 768)
    );
    assert_eq!(
        (
            reason::supplicant::OTHER,
            reason::supplicant::DEAUTH,
            reason::supplicant::WPA_PSK_TIMEOUT
        ),
        (0, 14, 15)
    );
    assert_eq!(
        (
            reason::dot11::UNSPECIFIED,
            reason::dot11::PREVIOUS_AUTH_INVALID,
            reason::dot11::CLASS2_FROM_NONAUTH,
            reason::dot11::CLASS3_FROM_NONASSOC,
            reason::dot11::DISASSOC_LEAVING,
            reason::dot11::NOT_AUTHENTICATED
        ),
        (1, 2, 6, 7, 8, 9)
    );
    assert_eq!(
        [
            number::SET_SSID,
            number::JOIN,
            number::AUTH,
            number::DEAUTH,
            number::DEAUTH_IND,
            number::ASSOC,
            number::DISASSOC,
            number::DISASSOC_IND,
            number::LINK,
            number::SCAN_COMPLETE,
            number::RADIO,
            number::PSK_SUP,
            number::ESCAN_RESULT,
            number::ASSOC_IND_NDIS
        ],
        [0, 1, 3, 5, 6, 7, 11, 12, 16, 26, 40, 46, 69, 85]
    );
    assert_eq!(
        [
            status::SUCCESS,
            status::FAIL,
            status::TIMEOUT,
            status::NO_NETWORKS,
            status::ABORT,
            status::NO_ACK,
            status::UNSOLICITED,
            status::ATTEMPT,
            status::PARTIAL,
            status::NEWSCAN,
            status::NEWASSOC,
            status::QUIET_11H,
            status::SUPPRESS,
            status::NO_CHANNELS,
            status::CCX_FAST_ROAM,
            status::CHANNEL_SELECT_ABORT
        ],
        [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]
    );
    assert_eq!(ETHERTYPE, [0x88, 0x6C]);
    assert_eq!(OUI, [0x00, 0x10, 0x18]);
}

#[test]
fn the_mask_request_frame_matches_the_hand_derived_bytes() {
    let request = Request {
        stage: "",
        set: true,
        cmd: cmd::SET_VAR,
        name: var::EVENT_MASK,
        payload: Payload::Mask(EventMask::DEFAULT),
    };
    let mut buf = [0u8; FRAME_BUF];
    let n = request.build(&mut buf, 8, 9);
    assert_eq!(n, 56);
    assert_eq!(request.frame_len(), 56);
    let hand = request_frame(
        8,
        9,
        true,
        263,
        b"event_msgs",
        EventMask::DEFAULT.as_bytes(),
    );
    assert_eq!(&buf[..n], &hand[..]);
    assert_eq!(
        &buf[..28],
        &[
            0x38, 0x00, 0xC7, 0xFF, 0x08, 0x00, 0x00, 0x0C, 0x00, 0x00, 0x00, 0x00, 0x07, 0x01,
            0x00, 0x00, 0x1C, 0x00, 0x00, 0x00, 0x02, 0x00, 0x09, 0x00, 0x00, 0x00, 0x00, 0x00
        ][..]
    );
    assert_eq!(&buf[28..39], b"event_msgs\0");
    assert_eq!(&buf[39..56], EventMask::DEFAULT.as_bytes());
    assert_eq!(Payload::Mask(EventMask::EMPTY).len(), 17);
    assert!(!Payload::Mask(EventMask::EMPTY).is_empty());
    assert_eq!(var::EVENT_MASK, b"event_msgs");
}
