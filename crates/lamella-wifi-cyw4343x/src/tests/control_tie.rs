//! The tie: the opening and the events at `Ready` replayed through the gSPI
//! transport at the wire and through the SDIO transport at the host, each
//! bus's script derived from the transport-level rows by that bus's
//! encoding rule, the derivation checked against hand-derived rows for the
//! packet-channel operations, and the recorder's log compared with the
//! transport-level rows whole.

use super::control::{
    BLOB, EXPECTED, LINK_EVENT, LINK_PAYLOAD, RADIO_EVENT, VERSION_REPLY, chunk_value, event_frame,
    open_ops, radio_frame, ready_ops, request_frame,
};
use super::download::{FIRMWARE, SETTINGS, full_script};
use super::download_tie::{host_rows, logged_of, wire_rows};
use crate::control::Version;
use crate::driver::{Driver, Outcome};
use crate::event::EventMask;
use crate::fixture::{End, FakeClock, FakeHost, FakeWire, HostRow, Recorder, Row, run};
use crate::gspi::Gspi;
use crate::sdio::{OcrWindow, Sdio, Shape};
use crate::transport::Part;

const CAP: u32 = 10_000;

fn wire_frame(row: &Row) -> (&'static [u8], &'static [u8]) {
    let Row::Frame { tx, rx, .. } = *row else {
        panic!("a frame");
    };
    (tx, rx)
}

#[test]
fn the_derived_packet_channel_rows_reproduce_the_hand_derived_ones() {
    let ops = open_ops(false, true);
    let rows = wire_rows(&ops);
    assert_eq!(
        wire_frame(&rows[0]),
        (&[0x02, 0x20, 0x00, 0x40][..], &[0x20, 0x00][..]),
        "the interrupt register read"
    );
    assert_eq!(
        wire_frame(&rows[1]),
        (&[0x02, 0x20, 0x00, 0xC0, 0x20, 0x00][..], &[][..]),
        "written back"
    );
    assert_eq!(
        wire_frame(&rows[2]),
        (&[0x04, 0x40, 0x00, 0x40][..], &[0x00, 0x01, 0x02, 0x00][..]),
        "the status word: bit 8 and the length 256"
    );
    let (tx, rx) = wire_frame(&rows[3]);
    assert_eq!(
        tx,
        &[0x00, 0x01, 0x00, 0x60][..],
        "the function-2 read of 256 bytes"
    );
    assert_eq!(rx, &event_frame(0, 0)[..]);
    assert_eq!(
        wire_frame(&rows[8]),
        (&[0x04, 0x40, 0x00, 0x40][..], &[0x20, 0x00, 0x00, 0x00][..]),
        "the receive-ready bit"
    );
    let (tx, rx) = wire_frame(&rows[9]);
    let chunk = request_frame(0, 1, true, 263, b"clmload", &chunk_value(&BLOB, 0));
    assert_eq!(
        &tx[..4],
        &[0xAC, 0x05, 0x00, 0xE0],
        "the function-2 write of 1,452 bytes"
    );
    assert_eq!(&tx[4..], &chunk[..]);
    assert_eq!(rx, &[][..]);
    assert_eq!(
        rows.len(),
        4 + 4 + 2 + 2 + 4 + 8 * 6,
        "the two passes, the send, the empty pass (a zero latch is not written back), the reply pass, eight more transactions"
    );
    let (tx, _) = wire_frame(&rows[59]);
    let mask = request_frame(
        8,
        9,
        true,
        263,
        b"event_msgs",
        EventMask::DEFAULT.as_bytes(),
    );
    assert_eq!(
        &tx[..4],
        &[0x38, 0x00, 0x00, 0xE0],
        "the function-2 write of the 56-byte mask request"
    );
    assert_eq!(&tx[4..], &mask[..]);

    let rows = wire_rows(&ready_ops(false));
    assert_eq!(rows.len(), 4 + 4 + 2 + 2 + 2);
    assert_eq!(
        wire_frame(&rows[2]),
        (&[0x04, 0x40, 0x00, 0x40][..], &[0x00, 0xB1, 0x00, 0x00][..]),
        "the status word: bit 8 and the length 88"
    );
    let (tx, rx) = wire_frame(&rows[3]);
    assert_eq!(
        tx,
        &[0x58, 0x00, 0x00, 0x60][..],
        "the function-2 read of 88 bytes"
    );
    assert_eq!(rx, &radio_frame()[..]);
    assert_eq!(
        wire_frame(&rows[8]),
        (&[0x02, 0x20, 0x00, 0x40][..], &[0x00, 0x00][..]),
        "an empty pass: the latch read"
    );
    assert_eq!(
        wire_frame(&rows[9]),
        (&[0x04, 0x40, 0x00, 0x40][..], &[0x00, 0x00, 0x00, 0x00][..]),
        "and the status word"
    );

    let ops = open_ops(true, true);
    let rows = host_rows(&ops);
    let HostRow::Write {
        arg, shape, data, ..
    } = rows[63]
    else {
        panic!("the mask's send");
    };
    assert_eq!(
        (arg, shape, data.len()),
        (0xA400_0040, Shape::bytes(64), 64)
    );
    assert_eq!(&data[..56], &mask[..]);
    assert!(data[56..].iter().all(|&b| b == 0));
    let rows = host_rows(&ready_ops(true));
    assert_eq!(rows.len(), 5 + 5 + 2 + 2 + 2);
    let HostRow::Read {
        arg, shape, data, ..
    } = rows[3]
    else {
        panic!("the radio event's tag");
    };
    assert_eq!(
        (arg, shape, data),
        (0x2400_0004, Shape::bytes(4), &[0x58, 0x00, 0xA7, 0xFF][..])
    );
    let HostRow::Read {
        arg, shape, data, ..
    } = rows[4]
    else {
        panic!("the radio event behind its tag");
    };
    assert_eq!(
        (arg, shape, data.len()),
        (0x2C00_0002, Shape::blocks(2, 64), 128)
    );
    assert_eq!(&data[..84], &radio_frame()[4..]);

    let ops = open_ops(true, true);
    let rows = host_rows(&ops);
    let HostRow::Command {
        index, arg, reply, ..
    } = rows[0]
    else {
        panic!("the watermark");
    };
    assert_eq!((index, arg, reply), (52, 0x9200_1008, 0x1008));
    let HostRow::Irq(latch) = rows[3] else {
        panic!("the latch");
    };
    assert_eq!(latch, 0x20);
    let HostRow::Read {
        arg, shape, data, ..
    } = rows[6]
    else {
        panic!("the tag");
    };
    assert_eq!(
        (arg, shape, data),
        (0x2400_0004, Shape::bytes(4), &[0x00, 0x01, 0xFF, 0xFE][..])
    );
    let HostRow::Read {
        arg, shape, data, ..
    } = rows[7]
    else {
        panic!("the frame behind its tag");
    };
    assert_eq!(
        (arg, shape, data.len()),
        (0x2C00_0004, Shape::blocks(4, 64), 256)
    );
    assert_eq!(&data[..252], &event_frame(0, 0)[4..]);
    let HostRow::Write {
        arg, shape, data, ..
    } = rows[13]
    else {
        panic!("the first send");
    };
    assert_eq!(
        (arg, shape, data.len()),
        (0xAC00_0017, Shape::blocks(23, 64), 1472)
    );
    assert_eq!(&data[..1452], &chunk[..]);
    assert!(data[1452..].iter().all(|&b| b == 0));
    let HostRow::Read { data, .. } = rows[15] else {
        panic!("the empty tag");
    };
    assert_eq!(data, &[0x00, 0x00, 0xFF, 0xFF][..]);
    let HostRow::Read {
        arg, shape, data, ..
    } = rows[19]
    else {
        panic!("a reply's tag");
    };
    assert_eq!(
        (arg, shape, data),
        (0x2400_0004, Shape::bytes(4), &[0x1C, 0x00, 0xE3, 0xFF][..])
    );
    let HostRow::Read { arg, shape, .. } = rows[20] else {
        panic!("a reply behind its tag");
    };
    assert_eq!((arg, shape), (0x2400_0020, Shape::bytes(32)));
}

/// The two events and the three empty passes at `Ready`, on a driver
/// opened over `bus` with the clock at `now`: the first run ends in the
/// radio event, the second in the link event with its payload, the third
/// at the cap with three deadlines ten milliseconds apart.
fn events_at_ready<T: crate::transport::Transport>(
    driver: &mut Driver,
    bus: &mut T,
    clock: &mut FakeClock,
    now: u64,
) {
    let first = run(driver, bus, clock, CAP);
    assert_eq!(first.outcome, Some(Outcome::Event(RADIO_EVENT)));
    assert_eq!(first.polls, 1);
    let second = run(driver, bus, clock, CAP);
    assert_eq!(second.outcome, Some(Outcome::Event(LINK_EVENT)));
    assert_eq!(second.polls, 1);
    assert_eq!(driver.event_payload(), &LINK_PAYLOAD);
    let third = run(driver, bus, clock, 3);
    assert_eq!(third.end, End::Cap);
    assert_eq!(
        third.deadlines(),
        &[now + 10_000, now + 20_000, now + 30_000]
    );
    assert_eq!(driver.credit(), (9, 27));
    assert_eq!(
        (
            driver.frames().events,
            driver.frames().malformed,
            driver.frames().received
        ),
        (4, 2, 13)
    );
}

#[test]
fn the_opening_and_the_events_replay_at_the_wire() {
    let mut ops = full_script(64, Part::Cyw43439);
    let mut open = open_ops(false, true);
    open.extend(ready_ops(false));
    ops.extend_from_slice(&open);
    let rows = wire_rows(&ops);
    let mut bus = Recorder::new(Gspi::new(FakeWire::new(&rows)));
    let mut clock = FakeClock::new(0);
    let mut driver = Driver::new();
    assert!(driver.attach());
    let attached = run(&mut driver, &mut bus, &mut clock, CAP);
    assert_eq!(
        attached.outcome,
        Some(Outcome::Attached {
            chip_id: 0x1545_A9AF
        })
    );
    assert!(driver.upload(&FIRMWARE, &SETTINGS));
    let uploaded = run(&mut driver, &mut bus, &mut clock, CAP);
    assert_eq!(
        uploaded.outcome,
        Some(Outcome::Uploaded { save_restore: true })
    );
    assert_eq!(clock.now(), 510_036);
    bus.clear();
    assert!(driver.open(&BLOB, Some(EXPECTED)));
    let opened = run(&mut driver, &mut bus, &mut clock, CAP);
    assert_eq!(bus.inner().wire().fault(), None);
    assert_eq!(
        opened.outcome,
        Some(Outcome::Ready {
            version: Version::from_reply(VERSION_REPLY)
        })
    );
    assert_eq!(opened.deadlines(), &[520_036]);
    assert_eq!(opened.polls, 22);
    assert_eq!(driver.credit(), (9, 25));
    assert!(driver.is_ready());
    events_at_ready(&mut driver, &mut bus, &mut clock, 520_036);
    assert_eq!(bus.inner().wire().fault(), None);
    assert_eq!(bus.inner().wire().remaining(), 0);
    assert_eq!(bus.log(), &logged_of(&open)[..]);
    assert!(!bus.overflowed());
}

#[test]
fn the_opening_and_the_events_replay_at_the_host() {
    let mut ops = full_script(2048, Part::Cyw4343w);
    let mut open = open_ops(true, true);
    open.extend(ready_ops(true));
    ops.extend_from_slice(&open);
    let rows = host_rows(&ops);
    let mut bus = Recorder::new(Sdio::new(
        FakeHost::new(&rows),
        Part::Cyw4343w,
        OcrWindow::V3_3,
    ));
    let mut clock = FakeClock::new(0);
    let mut driver = Driver::new();
    assert!(driver.attach());
    let attached = run(&mut driver, &mut bus, &mut clock, CAP);
    assert_eq!(
        attached.outcome,
        Some(Outcome::Attached {
            chip_id: 0x1531_A9A6
        })
    );
    assert!(driver.upload(&FIRMWARE, &SETTINGS));
    let uploaded = run(&mut driver, &mut bus, &mut clock, CAP);
    assert_eq!(
        uploaded.outcome,
        Some(Outcome::Uploaded { save_restore: true })
    );
    assert_eq!(clock.now(), 996_036);
    bus.clear();
    assert!(driver.open(&BLOB, Some(EXPECTED)));
    let opened = run(&mut driver, &mut bus, &mut clock, CAP);
    assert_eq!(bus.inner().host().fault(), None);
    assert_eq!(
        opened.outcome,
        Some(Outcome::Ready {
            version: Version::from_reply(VERSION_REPLY)
        })
    );
    assert_eq!(opened.deadlines(), &[1_006_036]);
    assert_eq!(opened.polls, 22);
    assert_eq!(driver.credit(), (9, 25));
    assert!(driver.is_ready());
    events_at_ready(&mut driver, &mut bus, &mut clock, 1_006_036);
    assert_eq!(bus.inner().host().fault(), None);
    assert_eq!(bus.inner().host().remaining(), 0);
    assert_eq!(bus.log(), &logged_of(&open)[..]);
    assert!(!bus.overflowed());
}
