//! The tie: the data plane replayed through the gSPI transport at the wire
//! and through the SDIO transport at the host, each bus's script derived
//! from the transport-level rows by that bus's encoding rule, the new
//! frames checked against hand-derived bytes, and the recorder's log
//! compared with the transport-level rows whole after the opening.

use super::control::{BLOB, EXPECTED, VERSION_REPLY, open_ops};
use super::data::{
    arp_reply, arp_request, data_ops, datagram, drive_data_plane, join_over, rx_frame, tx_frame,
};
use super::download::{FIRMWARE, SETTINGS, full_script};
use super::download_tie::{host_rows, logged_of, wire_rows};
use super::station::{Cursor, join_ops, up_ops};
use crate::control::Version;
use crate::driver::{Driver, Outcome};
use crate::fixture::{FakeClock, FakeHost, FakeWire, HostRow, Op, Recorder, Row, run};
use crate::gspi::Gspi;
use crate::sdio::{OcrWindow, Sdio, Shape};
use crate::transport::Part;
use std::vec::Vec;

const CAP: u32 = 10_000;

/// The radio up, the join and the data plane after the opening.
fn plane(mailbox: bool, explicit: bool) -> Vec<Op> {
    let mut c = Cursor::AFTER_OPENING;
    let mut v = up_ops(&mut c, mailbox);
    v.extend(join_ops(&mut c, mailbox));
    v.extend(data_ops(&mut c, mailbox, explicit));
    v
}

fn wire_frame(row: &Row) -> (&'static [u8], &'static [u8]) {
    let Row::Frame { tx, rx, .. } = *row else {
        panic!("a frame");
    };
    (tx, rx)
}

#[test]
fn the_derived_rows_of_the_data_plane_reproduce_the_hand_derived_ones() {
    let want_tx = tx_frame(19, &arp_request());
    let ops = plane(false, true);
    let rows = wire_rows(&ops);
    let write = rows
        .iter()
        .find(|r| wire_frame(r).0.starts_with(&[0x3C, 0x00, 0x00, 0xE0]))
        .expect("the ARP request's function-2 write");
    assert_eq!(&wire_frame(write).0[4..], &want_tx[..]);
    let avail = rows
        .iter()
        .position(|r| {
            wire_frame(r) == (&[0x04, 0x40, 0x00, 0x40][..], &[0x00, 0x79, 0x00, 0x00][..])
        })
        .expect("the status word: bit 8 and the length 60");
    let (tx, rx) = wire_frame(&rows[avail + 1]);
    assert_eq!(
        tx,
        &[0x3C, 0x00, 0x00, 0x60][..],
        "the function-2 read of 60 bytes"
    );
    assert_eq!(rx, &rx_frame(27, 41, 14, 0, &arp_reply())[..]);

    let ops = plane(true, true);
    let rows = host_rows(&ops);
    let write = rows
        .iter()
        .find_map(|r| match *r {
            HostRow::Write {
                arg, shape, data, ..
            } if data.len() == 64 && data[..4] == [0x3C, 0x00, 0xC3, 0xFF] && data[5] == 2 => {
                Some((arg, shape, data))
            }
            _ => None,
        })
        .expect("the ARP request's write");
    assert_eq!((write.0, write.1), (0xA400_0040, Shape::bytes(64)));
    assert_eq!(&write.2[..60], &want_tx[..]);
    assert!(
        write.2[60..].iter().all(|&b| b == 0),
        "four zero bytes of padding"
    );
    let tag = rows
        .iter()
        .position(|r| matches!(*r, HostRow::Read { data, .. } if data == [0x3C, 0x00, 0xC3, 0xFF]))
        .expect("the reply's tag");
    let HostRow::Read {
        arg, shape, data, ..
    } = rows[tag + 1]
    else {
        panic!("the read behind the tag");
    };
    assert_eq!(
        (arg, shape, data.len()),
        (0x2400_0040, Shape::bytes(64), 64)
    );
    assert_eq!(&data[..56], &rx_frame(27, 41, 14, 0, &arp_reply())[4..]);
    // The datagram's tag: the same size as an association event's frame,
    // so the search starts after the reply's.
    let tag = tag
        + rows[tag..]
            .iter()
            .position(
                |r| matches!(*r, HostRow::Read { data, .. } if data == [0x60, 0x00, 0x9F, 0xFF]),
            )
            .expect("the datagram's tag");
    let HostRow::Read {
        arg, shape, data, ..
    } = rows[tag + 1]
    else {
        panic!("the read behind the tag");
    };
    assert_eq!(
        (arg, shape, data.len()),
        (0x2C00_0002, Shape::blocks(2, 64), 128)
    );
    assert_eq!(&data[..92], &rx_frame(28, 42, 24, 1, &datagram())[4..]);
}

#[test]
fn the_data_plane_replays_at_the_wire() {
    let mut ops = full_script(64, Part::Cyw43439);
    ops.extend(open_ops(false, true));
    let plane = plane(false, true);
    ops.extend_from_slice(&plane);
    let rows = wire_rows(&ops);
    let mut bus = Recorder::new(Gspi::new(FakeWire::new(&rows)));
    let mut clock = FakeClock::new(0);
    let mut driver: Driver<'static> = Driver::new();
    assert!(driver.attach());
    assert!(matches!(
        run(&mut driver, &mut bus, &mut clock, CAP).outcome,
        Some(Outcome::Attached { .. })
    ));
    assert!(driver.upload(&FIRMWARE, &SETTINGS));
    assert_eq!(
        run(&mut driver, &mut bus, &mut clock, CAP).outcome,
        Some(Outcome::Uploaded { save_restore: true })
    );
    assert!(driver.open(&BLOB, Some(EXPECTED)));
    let opened = run(&mut driver, &mut bus, &mut clock, CAP);
    assert_eq!(
        opened.outcome,
        Some(Outcome::Ready {
            version: Version::from_reply(VERSION_REPLY)
        })
    );
    assert_eq!(clock.now(), 520_036);
    bus.clear();
    join_over(&mut driver, &mut bus, &mut clock);
    assert_eq!(clock.now(), 580_036);
    drive_data_plane(&mut driver, &mut bus, &mut clock);
    assert_eq!(bus.inner().wire().fault(), None);
    assert_eq!(bus.inner().wire().remaining(), 0);
    assert_eq!(bus.log(), &logged_of(&plane)[..]);
    assert!(!bus.overflowed());
}

#[test]
fn the_data_plane_replays_at_the_host() {
    let mut ops = full_script(2048, Part::Cyw4343w);
    ops.extend(open_ops(true, true));
    let plane = plane(true, true);
    ops.extend_from_slice(&plane);
    let rows = host_rows(&ops);
    let mut bus = Recorder::new(Sdio::new(
        FakeHost::new(&rows),
        Part::Cyw4343w,
        OcrWindow::V3_3,
    ));
    let mut clock = FakeClock::new(0);
    let mut driver: Driver<'static> = Driver::new();
    assert!(driver.attach());
    assert!(matches!(
        run(&mut driver, &mut bus, &mut clock, CAP).outcome,
        Some(Outcome::Attached { .. })
    ));
    assert!(driver.upload(&FIRMWARE, &SETTINGS));
    assert_eq!(
        run(&mut driver, &mut bus, &mut clock, CAP).outcome,
        Some(Outcome::Uploaded { save_restore: true })
    );
    assert!(driver.open(&BLOB, Some(EXPECTED)));
    let opened = run(&mut driver, &mut bus, &mut clock, CAP);
    assert_eq!(
        opened.outcome,
        Some(Outcome::Ready {
            version: Version::from_reply(VERSION_REPLY)
        })
    );
    assert_eq!(clock.now(), 1_006_036);
    bus.clear();
    join_over(&mut driver, &mut bus, &mut clock);
    assert_eq!(clock.now(), 1_066_036);
    drive_data_plane(&mut driver, &mut bus, &mut clock);
    assert_eq!(bus.inner().host().fault(), None);
    assert_eq!(bus.inner().host().remaining(), 0);
    assert_eq!(bus.log(), &logged_of(&plane)[..]);
    assert!(!bus.overflowed());
}
