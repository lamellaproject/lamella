//! The function-2 path over SDIO at the host level: the four-byte length
//! tag, the read behind it, a write's padding to whole blocks, the abort,
//! and the sizing rule as it reaches the wire.

use crate::error::Refusal;
use crate::fixture::{FakeHost, HostRow, STAGE_TRANSFER_MISMATCH};
use crate::sdio::{OcrWindow, Response, STAGE_FRAME_TAG, STAGE_TRANSFER, Sdio, Shape};
use crate::transport::{Func, Part, Transport};

fn bus(script: &[HostRow]) -> Sdio<FakeHost<'_>> {
    Sdio::new(FakeHost::new(script), Part::Cyw4343w, OcrWindow::V3_3)
}

const fn read(arg: u32, shape: Shape, data: &'static [u8], note: &'static str) -> HostRow {
    HostRow::Read {
        arg,
        shape,
        data,
        reply: 0x0000_2000,
        note,
    }
}

const BODY: [u8; 8] = [1, 2, 3, 4, 5, 6, 7, 8];

#[test]
fn a_tag_announces_the_frame_and_the_read_puts_it_back_at_the_head() {
    let script = [
        read(
            0x2400_0004,
            Shape::bytes(4),
            &[0x0C, 0x00, 0xF3, 0xFF],
            "the tag: size 12",
        ),
        read(0x2400_0008, Shape::bytes(8), &BODY, "the rest of the frame"),
    ];
    let mut bus = bus(&script);
    assert_eq!(bus.f2_frame_available(), Ok(Some(12)));
    let mut frame = [0u8; 16];
    assert_eq!(bus.f2_read(12, &mut frame), Ok(12));
    assert_eq!(&frame[..4], &[0x0C, 0x00, 0xF3, 0xFF]);
    assert_eq!(&frame[4..12], &BODY);
    assert_eq!(bus.host().remaining(), 0);
    assert_eq!(bus.host().fault(), None);
}

#[test]
fn a_zero_tag_means_no_frame() {
    let script = [read(
        0x2400_0004,
        Shape::bytes(4),
        &[0, 0, 0xFF, 0xFF],
        "no frame",
    )];
    let mut bus = bus(&script);
    assert_eq!(bus.f2_frame_available(), Ok(None));
}

#[test]
fn a_tag_whose_complement_fails_is_refused() {
    let script = [read(
        0x2400_0004,
        Shape::bytes(4),
        &[0x10, 0, 0, 0],
        "the complement zero",
    )];
    let mut bus = bus(&script);
    assert_eq!(
        bus.f2_frame_available(),
        Err(Refusal::new(STAGE_FRAME_TAG, 0x0000_0010))
    );
}

#[test]
fn a_tag_under_the_header_length_is_refused() {
    let script = [read(
        0x2400_0004,
        Shape::bytes(4),
        &[0x08, 0x00, 0xF7, 0xFF],
        "size 8",
    )];
    let mut bus = bus(&script);
    assert_eq!(
        bus.f2_frame_available(),
        Err(Refusal::new(STAGE_FRAME_TAG, 0xFFF7_0008))
    );
}

#[test]
fn a_read_without_a_tag_is_refused_before_the_host() {
    let script: [HostRow; 0] = [];
    let mut bus = bus(&script);
    let mut frame = [0u8; 16];
    assert_eq!(
        bus.f2_read(12, &mut frame),
        Err(Refusal::new(STAGE_TRANSFER, 12))
    );
    assert_eq!(bus.host().fault(), None);
}

const fn padded() -> [u8; 128] {
    let mut wire = [0u8; 128];
    let mut i = 0;
    while i < 100 {
        wire[i] = (i as u8).wrapping_mul(7).wrapping_add(3);
        i += 1;
    }
    wire
}

static WIRE: [u8; 128] = padded();

#[test]
fn a_write_pads_to_whole_blocks() {
    let script = [HostRow::Write {
        arg: 0xAC00_0002,
        shape: Shape::blocks(2, 64),
        data: &WIRE,
        reply: 0x0000_2000,
        note: "two blocks of 64, the last 28 bytes zero",
    }];
    let mut bus = bus(&script);
    assert_eq!(bus.f2_write(&WIRE[..100]), Ok(true));
    assert_eq!(bus.host().remaining(), 0);
    assert_eq!(bus.host().fault(), None);
}

#[test]
fn the_abort_names_function_2() {
    let script = [HostRow::Command {
        index: 52,
        arg: 0x8000_0C02,
        response: Response::Short,
        reply: 0x0000_1002,
        times: 1,
        note: "write CCCR 0x06, the abort of function 2",
    }];
    let mut bus = bus(&script);
    assert_eq!(bus.abort_f2(), Ok(()));
    assert_eq!(bus.host().remaining(), 0);
}

#[test]
fn a_short_read_moves_the_rounded_count_and_hands_back_the_asked_bytes() {
    let script = [read(
        0x1424_6808,
        Shape::bytes(8),
        &[10, 11, 12, 13, 14, 15, 16, 17],
        "five bytes asked, eight on the wire",
    )];
    let mut bus = bus(&script);
    let mut buf = [0u8; 5];
    assert_eq!(bus.read_extended(Func::F1, 0x1234, true, &mut buf), Ok(()));
    assert_eq!(buf, [10, 11, 12, 13, 14]);
}

#[test]
fn a_function_0_run_past_its_block_is_refused_before_the_host() {
    let script: [HostRow; 0] = [];
    let mut bus = bus(&script);
    let mut buf = [0u8; 40];
    assert_eq!(
        bus.read_extended(Func::F0, 0, true, &mut buf),
        Err(Refusal::new(STAGE_TRANSFER, 40))
    );
    assert_eq!(bus.host().fault(), None);
}

#[test]
fn a_transfer_the_script_did_not_expect_fails_by_row_number() {
    let script = [HostRow::Write {
        arg: 0xAC00_0002,
        shape: Shape::blocks(2, 64),
        data: &WIRE,
        reply: 0x0000_2000,
        note: "the padded frame",
    }];
    let mut bus = bus(&script);
    assert_eq!(
        bus.f2_write(&WIRE[..99]),
        Err(Refusal::new(STAGE_TRANSFER_MISMATCH, 1))
    );
    assert_eq!(
        bus.host().fault(),
        Some(Refusal::new(STAGE_TRANSFER_MISMATCH, 1))
    );
}
