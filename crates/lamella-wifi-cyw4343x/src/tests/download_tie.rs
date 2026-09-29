//! The tie: the download replayed through the gSPI transport at the wire
//! and through the SDIO transport at the host. Each bus's script is
//! derived from the transport-level rows by that bus's encoding rule, the
//! derivation is checked against hand-written rows, and the recorder's log
//! of what the core asked is compared with the transport-level rows whole.
//! The derivations cover the packet-channel rows too, for the opening's
//! tie.

use super::download::{FIRMWARE, SETTINGS, full_script};
use crate::driver::{Driver, Outcome};
use crate::fixture::{
    FakeClock, FakeHost, FakeWire, HostRow, Kind, Logged, Op, Recorder, Row, run, word_of,
};
use crate::gspi::Gspi;
use crate::sdio::{OcrWindow, Response, Sdio, Shape};
use crate::transport::{Func, Part};
use std::boxed::Box;
use std::vec::Vec;

const CAP: u32 = 10_000;

pub(super) fn leak(bytes: Vec<u8>) -> &'static [u8] {
    Box::leak(bytes.into_boxed_slice())
}

/// The gSPI command word in the configured wire order: bit 31 write, bit
/// 30 incrementing, bits 29:28 the function, bits 27:11 the address, bits
/// 10:0 the length, the least significant byte first.
pub(super) fn gspi_cmd(write: bool, func: Func, addr: u32, len: usize) -> Vec<u8> {
    let word = (u32::from(write) << 31)
        | (1 << 30)
        | ((func as u32) << 28)
        | ((addr & 0x1_FFFF) << 11)
        | (len as u32 & 0x7FF);
    word.to_le_bytes().to_vec()
}

fn frame(tx: Vec<u8>, rx: Vec<u8>, times: u32) -> Row {
    Row::Frame {
        tx: leak(tx),
        rx: leak(rx),
        times,
        note: "derived from the transport-level row",
    }
}

/// A function-1 read's answer: the four-byte response delay, then the data.
fn delayed(data: &[u8]) -> Vec<u8> {
    let mut rx = std::vec![0u8; 4];
    rx.extend_from_slice(data);
    rx
}

/// The wire rows of a transport-level script: the transport's own attach
/// as the hand-written rows, then every operation by the encoding rule.
/// The packet-channel rows: the interrupt take is a 2-byte read of the
/// interrupt register, written back when not zero; the availability query
/// a 4-byte read of the status word carrying the packet-available bit and
/// the length; a frame read one function-2 read at address 0 of the exact
/// length; a frame write the status read carrying the receive-ready bit
/// when accepted, then the function-2 write; the interrupt setup and the
/// abort nothing.
pub(super) fn wire_rows(ops: &[Op]) -> Vec<Row> {
    let mut rows = Vec::new();
    for op in ops {
        match *op {
            Op::Attach { .. } => rows.extend_from_slice(&super::attach_gspi::attach_script()[..14]),
            Op::WriteDirect {
                func, addr, value, ..
            } => {
                let mut tx = gspi_cmd(true, func, addr, 1);
                tx.push(value);
                rows.push(frame(tx, Vec::new(), 1));
            }
            Op::ReadDirect {
                func,
                addr,
                value,
                times,
                ..
            } => {
                assert_eq!(func, Func::F1);
                rows.push(frame(
                    gspi_cmd(false, func, addr, 1),
                    delayed(&[value]),
                    times,
                ));
            }
            Op::WriteExtended {
                func,
                addr,
                incr,
                data,
                ..
            } => {
                assert!(incr);
                let mut tx = gspi_cmd(true, func, addr, data.len());
                tx.extend_from_slice(data);
                rows.push(frame(tx, Vec::new(), 1));
            }
            Op::ReadExtended {
                func,
                addr,
                incr,
                data,
                ..
            } => {
                assert!(incr && func == Func::F1);
                rows.push(frame(
                    gspi_cmd(false, func, addr, data.len()),
                    delayed(data),
                    1,
                ));
            }
            Op::F2Ready { ready, times } => {
                let info = if ready { 0x03 } else { 0x01 };
                rows.push(frame(
                    gspi_cmd(false, Func::F0, 0x0E, 2),
                    std::vec![info, 0x20],
                    times,
                ));
            }
            Op::TakeInterrupt { value } => {
                rows.push(frame(
                    gspi_cmd(false, Func::F0, 0x04, 2),
                    value.to_le_bytes().to_vec(),
                    1,
                ));
                if value != 0 {
                    let mut tx = gspi_cmd(true, Func::F0, 0x04, 2);
                    tx.extend_from_slice(&value.to_le_bytes());
                    rows.push(frame(tx, Vec::new(), 1));
                }
            }
            Op::F2Available { len } => {
                let word = len.map_or(0u32, |n| (1 << 8) | ((n as u32) << 9));
                rows.push(frame(
                    gspi_cmd(false, Func::F0, 0x08, 4),
                    word.to_le_bytes().to_vec(),
                    1,
                ));
            }
            Op::F2Read { data } => {
                rows.push(frame(
                    gspi_cmd(false, Func::F2, 0, data.len()),
                    data.to_vec(),
                    1,
                ));
            }
            Op::F2Write { data, accepted } => {
                let ready: u32 = if accepted { 0x20 } else { 0 };
                rows.push(frame(
                    gspi_cmd(false, Func::F0, 0x08, 4),
                    ready.to_le_bytes().to_vec(),
                    1,
                ));
                if accepted {
                    let mut tx = gspi_cmd(true, Func::F2, 0, data.len());
                    tx.extend_from_slice(data);
                    rows.push(frame(tx, Vec::new(), 1));
                }
            }
            Op::Tune { .. } | Op::WakeOnCommand | Op::F2InterruptSetup { .. } | Op::AbortF2 => {}
            _ => unreachable!("a row the download and the opening do not use"),
        }
    }
    rows
}

/// The CMD52 argument: the read/write flag in bit 31, the function in bits
/// 30:28, the address in bits 25:9, the byte in bits 7:0.
pub(super) fn cmd52(write: bool, func: Func, addr: u32, data: u8) -> u32 {
    (u32::from(write) << 31) | ((func as u32) << 28) | ((addr & 0x1_FFFF) << 9) | u32::from(data)
}

/// The CMD53 argument and the wire shape of an `n`-byte incrementing
/// transfer: under 64 bytes, byte mode rounded up to a power of two; from
/// 64, 64-byte blocks; the block or byte count in bits 8:0.
pub(super) fn cmd53(write: bool, func: Func, addr: u32, n: usize) -> (u32, Shape) {
    let shape = if n < 64 {
        Shape::bytes(n.next_power_of_two())
    } else {
        Shape::blocks(n.div_ceil(64), 64)
    };
    let count = shape.len.checked_div(shape.block).unwrap_or(shape.len);
    let arg = (u32::from(write) << 31)
        | ((func as u32) << 28)
        | (u32::from(shape.block != 0) << 27)
        | (1 << 26)
        | ((addr & 0x1_FFFF) << 9)
        | (count as u32 & 0x1FF);
    (arg, shape)
}

fn padded(data: &[u8], len: usize) -> &'static [u8] {
    let mut bytes = data.to_vec();
    bytes.resize(len, 0);
    leak(bytes)
}

fn command(index: u8, arg: u32, reply: u32, times: u32) -> HostRow {
    HostRow::Command {
        index,
        arg,
        response: Response::Short,
        reply,
        times,
        note: "derived from the transport-level row",
    }
}

/// The host rows of a transport-level script: the transport's own attach
/// as the hand-written rows, then every operation by the encoding rule.
/// The packet-channel rows: the interrupt take is the host's latch; the
/// availability query a four-byte read of the length tag at function-2
/// address 0; a frame read the read of the bytes behind the tag by the
/// sizing rule; a frame write the write of the frame by the rule, zero
/// padded; the interrupt setup the watermark's direct write on function 1;
/// the abort the I/O abort byte naming function 2.
pub(super) fn host_rows(ops: &[Op]) -> Vec<HostRow> {
    let mut rows = Vec::new();
    let mut f2_enabled = false;
    for op in ops {
        match *op {
            Op::Attach { .. } => rows.extend_from_slice(&super::attach_sdio::attach_script()[..25]),
            Op::WriteDirect {
                func, addr, value, ..
            } => rows.push(command(
                52,
                cmd52(true, func, addr, value),
                0x1000 | u32::from(value),
                1,
            )),
            Op::ReadDirect {
                func,
                addr,
                value,
                times,
                ..
            } => rows.push(command(
                52,
                cmd52(false, func, addr, 0),
                0x1000 | u32::from(value),
                times,
            )),
            Op::WriteExtended {
                func, addr, data, ..
            } => {
                if data.len() == 1 {
                    rows.push(command(
                        52,
                        cmd52(true, func, addr, data[0]),
                        0x1000 | u32::from(data[0]),
                        1,
                    ));
                } else {
                    let (arg, shape) = cmd53(true, func, addr, data.len());
                    rows.push(HostRow::Write {
                        arg,
                        shape,
                        data: padded(data, shape.len),
                        reply: 0x2000,
                        note: "derived from the transport-level row",
                    });
                }
            }
            Op::ReadExtended {
                func, addr, data, ..
            } => {
                if data.len() == 1 {
                    rows.push(command(
                        52,
                        cmd52(false, func, addr, 0),
                        0x1000 | u32::from(data[0]),
                        1,
                    ));
                } else {
                    let (arg, shape) = cmd53(false, func, addr, data.len());
                    rows.push(HostRow::Read {
                        arg,
                        shape,
                        data: padded(data, shape.len),
                        reply: 0x2000,
                        note: "derived from the transport-level row",
                    });
                }
            }
            Op::F2Ready { ready, times } => {
                if !f2_enabled {
                    rows.push(command(52, cmd52(false, Func::F0, 0x02, 0), 0x1002, 1));
                    rows.push(command(52, cmd52(true, Func::F0, 0x02, 0x06), 0x1006, 1));
                    f2_enabled = true;
                }
                let ready_byte = if ready { 0x06 } else { 0x02 };
                rows.push(command(
                    52,
                    cmd52(false, Func::F0, 0x03, 0),
                    0x1000 | ready_byte,
                    times,
                ));
            }
            Op::WakeOnCommand => {
                rows.push(command(52, cmd52(true, Func::F0, 0xF0, 0x08), 0x1008, 1))
            }
            Op::Tune { .. } => {}
            Op::TakeInterrupt { value } => rows.push(HostRow::Irq(value)),
            Op::F2Available { len } => {
                let tag = match len {
                    Some(n) => {
                        let size = n as u16;
                        let mut tag = size.to_le_bytes().to_vec();
                        tag.extend_from_slice(&(!size).to_le_bytes());
                        tag
                    }
                    None => std::vec![0, 0, 0xFF, 0xFF],
                };
                rows.push(HostRow::Read {
                    arg: 0x2400_0004,
                    shape: Shape::bytes(4),
                    data: leak(tag),
                    reply: 0x2000,
                    note: "derived from the transport-level row: the length tag",
                });
            }
            Op::F2Read { data } => {
                let (arg, shape) = cmd53(false, Func::F2, 0, data.len() - 4);
                rows.push(HostRow::Read {
                    arg,
                    shape,
                    data: padded(&data[4..], shape.len),
                    reply: 0x2000,
                    note: "derived from the transport-level row: the frame behind its tag",
                });
            }
            Op::F2Write { data, accepted } => {
                assert!(accepted, "the bus has no receive gate");
                let (arg, shape) = cmd53(true, Func::F2, 0, data.len());
                rows.push(HostRow::Write {
                    arg,
                    shape,
                    data: padded(data, shape.len),
                    reply: 0x2000,
                    note: "derived from the transport-level row: the frame",
                });
            }
            Op::F2InterruptSetup { mailbox } => {
                assert!(mailbox, "the bus routes the chip's mailbox interrupt");
                rows.push(command(52, cmd52(true, Func::F1, 0x10008, 8), 0x1008, 1));
            }
            Op::AbortF2 => rows.push(command(52, cmd52(true, Func::F0, 0x06, 0x02), 0x1002, 1)),
            _ => unreachable!("a row the download and the opening do not use"),
        }
    }
    rows
}

/// The transport-level rows as the recorder logs them.
pub(super) fn logged_of(ops: &[Op]) -> Vec<Logged> {
    ops.iter()
        .filter_map(|op| match *op {
            Op::Attach { .. } => Some(Logged::plain(Kind::Attach, 0)),
            Op::WriteDirect {
                func, addr, value, ..
            } => Some(Logged::direct(Kind::WriteDirect, func, addr, value)),
            Op::ReadDirect {
                func, addr, value, ..
            } => Some(Logged::direct(Kind::ReadDirect, func, addr, value)),
            Op::WriteExtended {
                func,
                addr,
                incr,
                data,
                ..
            } => Some(Logged::extended(
                Kind::WriteExtended,
                func,
                addr,
                incr,
                data.len(),
                word_of(data),
            )),
            Op::ReadExtended {
                func,
                addr,
                incr,
                data,
                ..
            } => Some(Logged::extended(
                Kind::ReadExtended,
                func,
                addr,
                incr,
                data.len(),
                word_of(data),
            )),
            Op::Tune { .. } => Some(Logged::plain(Kind::Tune, 0)),
            Op::F2Ready { ready, .. } => Some(Logged::plain(Kind::F2Ready, u32::from(ready))),
            Op::WakeOnCommand => Some(Logged::plain(Kind::WakeOnCommand, 0)),
            Op::TakeInterrupt { value } => {
                Some(Logged::plain(Kind::TakeInterrupt, u32::from(value)))
            }
            Op::F2Available { len } => Some(Logged::plain(
                Kind::F2Available,
                len.map_or(0, |l| l as u32),
            )),
            Op::F2Read { data } => Some(Logged::extended(
                Kind::F2Read,
                Func::F2,
                0,
                true,
                data.len(),
                word_of(data),
            )),
            Op::F2Write { data, .. } => Some(Logged::extended(
                Kind::F2Write,
                Func::F2,
                0,
                true,
                data.len(),
                word_of(data),
            )),
            Op::F2InterruptSetup { mailbox } => {
                Some(Logged::plain(Kind::F2InterruptSetup, u32::from(mailbox)))
            }
            Op::AbortF2 => Some(Logged::plain(Kind::AbortF2, 0)),
            _ => None,
        })
        .collect()
}

fn same_frame(a: &Row, b: &Row) -> bool {
    match (a, b) {
        (
            Row::Frame {
                tx: ta,
                rx: ra,
                times: na,
                ..
            },
            Row::Frame {
                tx: tb,
                rx: rb,
                times: nb,
                ..
            },
        ) => ta == tb && ra == rb && na == nb,
        _ => false,
    }
}

fn same_command(a: &HostRow, b: &HostRow) -> bool {
    match (a, b) {
        (
            HostRow::Command {
                index: ia,
                arg: aa,
                response: ra,
                reply: pa,
                times: na,
                ..
            },
            HostRow::Command {
                index: ib,
                arg: ab,
                response: rb,
                reply: pb,
                times: nb,
                ..
            },
        ) => ia == ib && aa == ab && ra == rb && pa == pb && na == nb,
        (
            HostRow::Read {
                arg: aa,
                shape: sa,
                data: da,
                reply: pa,
                ..
            },
            HostRow::Read {
                arg: ab,
                shape: sb,
                data: db,
                reply: pb,
                ..
            },
        ) => aa == ab && sa == sb && da == db && pa == pb,
        _ => false,
    }
}

#[test]
fn the_derived_wire_rows_reproduce_the_hand_written_rows() {
    let ops = full_script(64, Part::Cyw43439);
    let rows = wire_rows(&ops);
    let hand = super::attach_gspi::attach_script();
    for i in 14..21 {
        assert!(
            same_frame(&rows[i], &hand[i]),
            "the core's attach row {}",
            i + 1
        );
    }
    let first: [(&[u8], &[u8]); 5] = [
        (&[0x01, 0x58, 0x00, 0xD8, 0x10], &[]),
        (&[0x04, 0x00, 0xC0, 0x55], &[0, 0, 0, 0, 0, 0, 0, 0]),
        (&[0x04, 0x40, 0xA0, 0xD5, 0x03, 0x00, 0x00, 0x00], &[]),
        (
            &[0x04, 0x40, 0xA0, 0x55],
            &[0, 0, 0, 0, 0x03, 0x00, 0x00, 0x00],
        ),
        (&[0x04, 0x00, 0xC0, 0xD5, 0x01, 0x00, 0x00, 0x00], &[]),
    ];
    for (i, (tx, rx)) in first.iter().enumerate() {
        let Row::Frame { tx: t, rx: r, .. } = rows[21 + i] else {
            panic!("a frame");
        };
        assert_eq!((t, r), (*tx, *rx), "the download's frame {}", i + 1);
    }
    assert_eq!(
        rows.len(),
        14 + 7 + 65 - 2,
        "the two hooks put nothing on the wire"
    );
}

#[test]
fn the_download_replays_at_the_wire() {
    let ops = full_script(64, Part::Cyw43439);
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
    assert_eq!(bus.inner().wire().fault(), None);
    assert_eq!(
        uploaded.outcome,
        Some(Outcome::Uploaded { save_restore: true })
    );
    assert_eq!(bus.inner().wire().remaining(), 0);
    assert_eq!(
        uploaded.deadlines(),
        &[
            220_000, 230_011, 280_024, 290_024, 300_036, 310_036, 410_036, 510_036
        ]
    );
    assert_eq!(bus.log(), &logged_of(&ops)[..]);
    assert!(!bus.overflowed());
}

#[test]
fn the_derived_host_rows_reproduce_the_hand_written_rows() {
    let ops = full_script(2048, Part::Cyw4343w);
    let rows = host_rows(&ops);
    let hand = super::attach_sdio::attach_script();
    for i in 25..32 {
        assert!(
            same_command(&rows[i], &hand[i]),
            "the core's attach row {}",
            i + 1
        );
    }
    assert!(same_command(
        &rows[32],
        &command(52, 0x9200_1610, 0x0000_1010, 1)
    ));
    let HostRow::Read {
        arg, shape, data, ..
    } = rows[33]
    else {
        panic!("a read");
    };
    assert_eq!(
        (arg, shape, data),
        (0x1570_0004, Shape::bytes(4), &[0u8, 0, 0, 0][..])
    );
    let HostRow::Write {
        arg, shape, data, ..
    } = rows[34]
    else {
        panic!("a write");
    };
    assert_eq!(
        (arg, shape, data),
        (0x9568_1004, Shape::bytes(4), &[3u8, 0, 0, 0][..])
    );
    let HostRow::Read { arg, .. } = rows[35] else {
        panic!("a read");
    };
    assert_eq!(arg, 0x1568_1004);
    let HostRow::Write { arg, .. } = rows[36] else {
        panic!("a write");
    };
    assert_eq!(arg, 0x9570_0004);
}

#[test]
fn the_download_replays_at_the_host() {
    let ops = full_script(2048, Part::Cyw4343w);
    assert_eq!(
        ops.len(),
        8 + 61,
        "one firmware chunk and one settings chunk at 2048"
    );
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
    assert_eq!(bus.inner().host().fault(), None);
    assert_eq!(
        uploaded.outcome,
        Some(Outcome::Uploaded { save_restore: true })
    );
    assert_eq!(bus.inner().host().remaining(), 0);
    assert_eq!(
        uploaded.deadlines(),
        &[
            706_000, 716_011, 766_024, 776_024, 786_036, 796_036, 896_036, 996_036
        ]
    );
    assert_eq!(bus.log(), &logged_of(&ops)[..]);
    assert!(!bus.overflowed());
}
