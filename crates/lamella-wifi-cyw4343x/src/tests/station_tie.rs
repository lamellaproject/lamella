//! The tie: the whole control plane replayed through the gSPI transport at
//! the wire and through the SDIO transport at the host, each bus's script
//! derived from the transport-level rows by that bus's encoding rule, two
//! new frames checked against hand-derived bytes, and the recorder's log
//! compared with the transport-level rows whole after the opening.

use super::control::{BLOB, EXPECTED, VERSION_REPLY, open_ops};
use super::download::{FIRMWARE, SETTINGS, full_script};
use super::download_tie::{host_rows, logged_of, wire_rows};
use super::station::{drive_plane, drive_sae, plane_ops, sae_plane_ops};
use crate::control::Version;
use crate::driver::{Driver, Outcome};
use crate::fixture::{FakeClock, FakeHost, FakeWire, HostRow, Recorder, Row, run};
use crate::gspi::Gspi;
use crate::sdio::{OcrWindow, Sdio, Shape};
use crate::transport::Part;

const CAP: u32 = 10_000;

fn wire_tx(row: &Row) -> &'static [u8] {
    let Row::Frame { tx, .. } = *row else {
        panic!("a frame");
    };
    tx
}

#[test]
fn the_derived_rows_of_the_plane_reproduce_the_hand_derived_ones() {
    let ops = plane_ops(false, true);
    assert_eq!(ops.len(), 13 + 19 + 57 + 15 + 248 + 7);
    let rows = wire_rows(&ops);
    let scan_start = rows
        .iter()
        .find(|r| wire_tx(r).starts_with(&[0x6C, 0x00, 0x00, 0xE0]))
        .expect("the scan start's function-2 write");
    assert_eq!(wire_tx(scan_start).len(), 4 + 108);
    assert_eq!(&wire_tx(scan_start)[4..8], &[0x6C, 0x00, 0x93, 0xFF]);
    let result = rows
        .iter()
        .find(|r| wire_tx(r) == [0x9A, 0x01, 0x00, 0x60])
        .expect("the first result's function-2 read");
    let Row::Frame { rx, .. } = *result else {
        panic!("a frame");
    };
    assert_eq!(rx.len(), 410);
    assert_eq!(&rx[..4], &[0x9A, 0x01, 0x65, 0xFE]);

    let ops = plane_ops(true, true);
    assert_eq!(ops.len(), 19 + 29 + 83 + 25 + 270 + 11);
    let rows = host_rows(&ops);
    let scan_start = rows
        .iter()
        .find_map(|r| match *r {
            HostRow::Write {
                arg, shape, data, ..
            } if data.len() == 128 && data[..4] == [0x6C, 0x00, 0x93, 0xFF] => {
                Some((arg, shape, data))
            }
            _ => None,
        })
        .expect("the scan start's write");
    assert_eq!(
        (scan_start.0, scan_start.1),
        (0xAC00_0002, Shape::blocks(2, 64))
    );
    assert!(
        scan_start.2[108..].iter().all(|&b| b == 0),
        "twenty zero bytes of padding"
    );
    let tag = rows
        .iter()
        .position(|r| matches!(*r, HostRow::Read { data, .. } if data == [0x9A, 0x01, 0x65, 0xFE]))
        .expect("the first result's tag");
    let HostRow::Read {
        arg, shape, data, ..
    } = rows[tag + 1]
    else {
        panic!("the read behind the tag");
    };
    assert_eq!(
        (arg, shape, data.len()),
        (0x2C00_0007, Shape::blocks(7, 64), 448)
    );
}

#[test]
fn the_plane_replays_at_the_wire() {
    let mut ops = full_script(64, Part::Cyw43439);
    ops.extend(open_ops(false, true));
    let plane = plane_ops(false, true);
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
    drive_plane(&mut driver, &mut bus, &mut clock, 520_036);
    assert_eq!(bus.inner().wire().fault(), None);
    assert_eq!(bus.inner().wire().remaining(), 0);
    assert_eq!(bus.log(), &logged_of(&plane)[..]);
    assert!(!bus.overflowed());
}

#[test]
fn the_plane_replays_at_the_host() {
    let mut ops = full_script(2048, Part::Cyw4343w);
    ops.extend(open_ops(true, true));
    let plane = plane_ops(true, true);
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
    drive_plane(&mut driver, &mut bus, &mut clock, 1_006_036);
    assert_eq!(bus.inner().host().fault(), None);
    assert_eq!(bus.inner().host().remaining(), 0);
    assert_eq!(bus.log(), &logged_of(&plane)[..]);
    assert!(!bus.overflowed());
}

#[test]
fn the_wpa3_join_and_the_capability_query_replay_at_both_levels() {
    let plane = sae_plane_ops(false);
    assert_eq!(plane.len(), 4 + 13 + 19 + 64);
    let rows = wire_rows(&plane);
    assert!(
        rows.iter()
            .any(|r| wire_tx(r).starts_with(&[0x20, 0x03, 0x00, 0xE0])
                && wire_tx(r).len() == 4 + 800),
        "the capability query's function-2 write"
    );
    assert!(
        rows.iter()
            .any(|r| wire_tx(r).starts_with(&[0xAB, 0x00, 0x00, 0xE0])
                && wire_tx(r).len() == 4 + 171),
        "the password's function-2 write"
    );
    let mut ops = full_script(64, Part::Cyw43439);
    ops.extend(open_ops(false, true));
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
    assert!(matches!(
        run(&mut driver, &mut bus, &mut clock, CAP).outcome,
        Some(Outcome::Ready { .. })
    ));
    assert_eq!(clock.now(), 520_036);
    bus.clear();
    drive_sae(&mut driver, &mut bus, &mut clock);
    assert_eq!(bus.inner().wire().fault(), None);
    assert_eq!(bus.inner().wire().remaining(), 0);
    assert_eq!(bus.log(), &logged_of(&plane)[..]);
    assert!(!bus.overflowed());

    let plane = sae_plane_ops(true);
    assert_eq!(plane.len(), 6 + 19 + 29 + 94);
    let rows = host_rows(&plane);
    let password = rows
        .iter()
        .find_map(|r| match *r {
            HostRow::Write {
                arg, shape, data, ..
            } if data.len() == 192 && data[..4] == [0xAB, 0x00, 0x54, 0xFF] => {
                Some((arg, shape, data))
            }
            _ => None,
        })
        .expect("the password's write");
    assert_eq!(
        (password.0, password.1),
        (0xAC00_0003, Shape::blocks(3, 64))
    );
    assert!(
        password.2[171..].iter().all(|&b| b == 0),
        "twenty-one zero bytes of padding"
    );
    let query = rows
        .iter()
        .find_map(|r| match *r {
            HostRow::Write {
                arg, shape, data, ..
            } if data.len() == 832 && data[..4] == [0x20, 0x03, 0xDF, 0xFC] => Some((arg, shape)),
            _ => None,
        })
        .expect("the capability query's write");
    assert_eq!(query, (0xAC00_000D, Shape::blocks(13, 64)));
    let mut ops = full_script(2048, Part::Cyw4343w);
    ops.extend(open_ops(true, true));
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
    assert!(matches!(
        run(&mut driver, &mut bus, &mut clock, CAP).outcome,
        Some(Outcome::Ready { .. })
    ));
    assert_eq!(clock.now(), 1_006_036);
    bus.clear();
    drive_sae(&mut driver, &mut bus, &mut clock);
    assert_eq!(bus.inner().host().fault(), None);
    assert_eq!(bus.inner().host().remaining(), 0);
    assert_eq!(bus.log(), &logged_of(&plane)[..]);
    assert!(!bus.overflowed());
}
