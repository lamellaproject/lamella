//! The SDIO attach: the chip's bring-up from power-on to a running ALP
//! clock, as a recorded exchange at the host, with three doctored controls
//! and further cases.
//!
//! The rows are the commands the SDIO Simplified Specification's
//! initialization flow prescribes (CMD0, CMD5, CMD3, CMD7), the card's
//! common control registers and block sizes written with CMD52, function 1
//! enabled and polled ready, the chip's pull-ups turned off, then the
//! identity word through the function-1 window and the ALP clock request.
//! The replies are those a CYW4343W gives on the Arduino GIGA R1 WiFi.

use crate::clock::Micros;
use crate::driver::{Driver, Outcome, STAGE_CHIP_IDENTITY};
use crate::error::Refusal;
use crate::fixture::{End, FakeClock, FakeHost, HostRow, Run, STAGE_COMMAND_MISMATCH, run};
use crate::sdio::{
    OcrWindow, Rate, Response, STAGE_BUS_WIDTH, STAGE_CARD_READY, STAGE_FUNCTION_READY,
    STAGE_RESPONSE_FLAGS, STAGE_VOLTAGE_WINDOW, Sdio, Shape, Width,
};
use crate::transport::Part;

const CAP: u32 = 10_000;
pub(super) const ROWS: usize = 32;

const fn cmd(index: u8, arg: u32, response: Response, reply: u32, note: &'static str) -> HostRow {
    HostRow::Command {
        index,
        arg,
        response,
        reply,
        times: 1,
        note,
    }
}

const fn cmd52(arg: u32, reply: u32, note: &'static str) -> HostRow {
    cmd(52, arg, Response::Short, reply, note)
}

/// The recorded exchange: rows 1 to 32 of the design's table.
pub(super) fn attach_script() -> [HostRow; ROWS] {
    [
        HostRow::Attach { pending: 3 },
        HostRow::Power(false),
        HostRow::Power(true),
        cmd(0, 0, Response::None, 0, "CMD0, the card to idle"),
        cmd(
            5,
            0,
            Response::ShortWithoutCrc,
            0x20FF_FF00,
            "CMD5 with no window: not ready, two functions, the operating conditions 2.0 to 3.6 V",
        ),
        cmd(
            5,
            0x0030_0000,
            Response::ShortWithoutCrc,
            0xA0FF_FF00,
            "CMD5 with the 3.3 V windows: ready",
        ),
        cmd(
            3,
            0,
            Response::Short,
            0x0001_0000,
            "CMD3: the card publishes relative address 1",
        ),
        cmd(
            7,
            0x0001_0000,
            Response::Short,
            0x0000_1E00,
            "CMD7: the card selected; an I/O-only card reports state 15",
        ),
        cmd52(
            0x0000_0E00,
            0x0000_1000,
            "read CCCR 0x07, the bus interface control",
        ),
        cmd52(0x8000_0E02, 0x0000_1002, "write CCCR 0x07, the 4-bit width"),
        HostRow::Bus {
            rate: Rate::Identification,
            width: Width::Four,
        },
        cmd52(0x0000_0E00, 0x0000_1002, "read CCCR 0x07 back"),
        cmd52(
            0x8000_2020,
            0x0000_1020,
            "write CCCR 0x10, function 0's block size low byte, 32",
        ),
        cmd52(0x8000_2200, 0x0000_1000, "write CCCR 0x11, its high byte"),
        cmd52(
            0x8002_2040,
            0x0000_1040,
            "write FBR 0x110, function 1's block size low byte, 64",
        ),
        cmd52(0x8002_2200, 0x0000_1000, "write FBR 0x111, its high byte"),
        cmd52(
            0x8004_2040,
            0x0000_1040,
            "write FBR 0x210, function 2's block size low byte, 64",
        ),
        cmd52(0x8004_2200, 0x0000_1000, "write FBR 0x211, its high byte"),
        cmd52(
            0x8000_0807,
            0x0000_1007,
            "write CCCR 0x04, the master enable with functions 1 and 2",
        ),
        cmd52(0x0000_0800, 0x0000_1007, "read CCCR 0x04 back"),
        HostRow::Bus {
            rate: Rate::Transfer,
            width: Width::Four,
        },
        cmd52(0x0000_0400, 0x0000_1000, "read CCCR 0x02, the I/O enables"),
        cmd52(
            0x8000_0402,
            0x0000_1002,
            "write CCCR 0x02, function 1 enabled",
        ),
        cmd52(0x0000_0600, 0x0000_1002, "read CCCR 0x03: function 1 ready"),
        cmd52(
            0x9200_1E00,
            0x0000_1000,
            "write F1 0x1000F, the chip's pull-ups off",
        ),
        cmd52(
            0x9200_1400,
            0x0000_1000,
            "write F1 0x1000A, the window's low byte",
        ),
        cmd52(
            0x9200_1600,
            0x0000_1000,
            "write F1 0x1000B, the window's middle byte",
        ),
        cmd52(
            0x9200_1818,
            0x0000_1018,
            "write F1 0x1000C, the window's high byte",
        ),
        HostRow::Read {
            arg: 0x1500_0004,
            shape: Shape::bytes(4),
            data: &[0xA6, 0xA9, 0x31, 0x15],
            reply: 0x0000_2000,
            note: "CMD53 of four bytes at F1 0x8000: the identity word through the window",
        },
        cmd52(
            0x9200_1C29,
            0x0000_1029,
            "write F1 0x1000E, the ALP request",
        ),
        cmd52(
            0x1200_1C00,
            0x0000_1069,
            "read F1 0x1000E: the request bits and ALP_AVAIL",
        ),
        cmd52(
            0x9200_1C00,
            0x0000_1000,
            "write F1 0x1000E, the request released",
        ),
    ]
}

/// The deadlines through the first CMD5: the host's three steps, the two
/// reset holds and the wait after CMD0.
const TO_IDLE: [Micros; 6] = [2_000, 4_000, 6_000, 16_000, 26_000, 76_000];

/// A run over `script` from a fresh driver on a transport to `part`: the
/// record, the rows left, the fake's fault, and the relative card address.
fn attach_over(script: &[HostRow], part: Part) -> (Run, usize, Option<Refusal>, u16) {
    let mut bus = Sdio::new(FakeHost::new(script), part, OcrWindow::V3_3);
    let mut clock = FakeClock::new(0);
    let mut driver = Driver::new();
    assert!(driver.attach());
    let record = run(&mut driver, &mut bus, &mut clock, CAP);
    (
        record,
        bus.host().remaining(),
        bus.host().fault(),
        bus.rca(),
    )
}

#[test]
fn the_attach_replays_the_recorded_exchange_to_an_attached_chip() {
    let script = attach_script();
    let (record, remaining, fault, rca) = attach_over(&script, Part::Cyw4343w);
    assert_eq!(fault, None);
    assert_eq!(record.end, End::Done);
    assert_eq!(
        record.outcome,
        Some(Outcome::Attached {
            chip_id: 0x1531_A9A6
        })
    );
    assert_eq!(remaining, 0, "every row consumed");
    assert_eq!(rca, 1);
    assert_eq!(
        record.deadlines(),
        &[
            2_000, 4_000, 6_000, 16_000, 26_000, 76_000, 576_000, 586_000, 596_000, 696_000
        ]
    );
    assert_eq!(record.polls, 21);
}

#[test]
fn operating_conditions_without_the_window_are_refused_by_name() {
    let mut script = attach_script();
    script[4] = cmd(
        5,
        0,
        Response::ShortWithoutCrc,
        0x2000_0300,
        "only the 2.0 to 2.2 V windows",
    );
    let (record, remaining, fault, _) = attach_over(&script, Part::Cyw4343w);
    assert_eq!(fault, None);
    assert_eq!(
        record.outcome,
        Some(Outcome::Refused(Refusal::new(
            STAGE_VOLTAGE_WINDOW,
            0x2000_0300
        )))
    );
    assert_eq!(remaining, ROWS - 5, "nothing written after the first CMD5");
    assert_eq!(record.deadlines(), &TO_IDLE);
}

#[test]
fn a_function_that_never_reports_ready_is_refused_at_the_deadline() {
    let mut script = attach_script();
    script[23] = HostRow::Command {
        index: 52,
        arg: 0x0000_0600,
        response: Response::Short,
        reply: 0x0000_1000,
        times: u32::MAX,
        note: "CCCR 0x03 never sets function 1's bit",
    };
    let (record, remaining, fault, _) = attach_over(&script, Part::Cyw4343w);
    assert_eq!(fault, None);
    assert_eq!(
        record.outcome,
        Some(Outcome::Refused(Refusal::new(STAGE_FUNCTION_READY, 0x00)))
    );
    assert_eq!(remaining, ROWS - 23, "the ready row still current");
    let deadlines = record.deadlines();
    assert_eq!(
        deadlines.len(),
        107,
        "eight to the first read, then 99 retries"
    );
    assert_eq!(deadlines[7], 586_000);
    assert_eq!(
        deadlines[106], 1_576_000,
        "one hundred reads, 990 ms past the first"
    );
}

#[test]
fn a_width_write_the_card_ignored_is_refused_at_the_read_back() {
    let mut script = attach_script();
    script[11] = cmd52(0x0000_0E00, 0x0000_1000, "the width still 1-bit");
    let (record, remaining, fault, _) = attach_over(&script, Part::Cyw4343w);
    assert_eq!(fault, None);
    assert_eq!(
        record.outcome,
        Some(Outcome::Refused(Refusal::new(STAGE_BUS_WIDTH, 0x00)))
    );
    assert_eq!(remaining, ROWS - 12, "refused before the block sizes");
    assert_eq!(record.deadlines(), &TO_IDLE);
}

#[test]
fn a_card_that_never_reports_ready_is_refused_at_the_deadline() {
    let mut script = attach_script();
    script[5] = HostRow::Command {
        index: 5,
        arg: 0x0030_0000,
        response: Response::ShortWithoutCrc,
        reply: 0x20FF_FF00,
        times: u32::MAX,
        note: "the C bit never sets",
    };
    let (record, remaining, fault, _) = attach_over(&script, Part::Cyw4343w);
    assert_eq!(fault, None);
    assert_eq!(
        record.outcome,
        Some(Outcome::Refused(Refusal::new(
            STAGE_CARD_READY,
            0x20FF_FF00
        )))
    );
    assert_eq!(remaining, ROWS - 5, "the second CMD5 row still current");
    let deadlines = record.deadlines();
    assert_eq!(
        deadlines.len(),
        105,
        "six to the first try, then 99 retries"
    );
    assert_eq!(deadlines[104], 1_066_000);
}

#[test]
fn response_flags_that_report_an_error_are_refused_by_name() {
    let mut script = attach_script();
    script[12] = cmd52(0x8000_2020, 0x0000_1220, "FUNCTION_NUMBER flagged");
    let (record, remaining, fault, _) = attach_over(&script, Part::Cyw4343w);
    assert_eq!(fault, None);
    assert_eq!(
        record.outcome,
        Some(Outcome::Refused(Refusal::new(
            STAGE_RESPONSE_FLAGS,
            0x0000_1220
        )))
    );
    assert_eq!(
        remaining,
        ROWS - 13,
        "refused at the first block-size write"
    );
}

#[test]
fn a_transport_that_deviates_from_the_script_fails_by_row_number() {
    let mut script = attach_script();
    script[9] = cmd52(0x8000_0E83, 0x0000_1002, "one bit doctored in row 10");
    let (record, _, fault, _) = attach_over(&script, Part::Cyw4343w);
    assert_eq!(fault, Some(Refusal::new(STAGE_COMMAND_MISMATCH, 10)));
    assert_eq!(
        record.outcome,
        Some(Outcome::Refused(Refusal::new(STAGE_COMMAND_MISMATCH, 10)))
    );
}

#[test]
fn the_identity_is_checked_against_the_part_the_transport_was_built_for() {
    let mut script = attach_script();
    script[28] = HostRow::Read {
        arg: 0x1500_0004,
        shape: Shape::bytes(4),
        data: &[0xAF, 0xA9, 0x45, 0x15],
        reply: 0x0000_2000,
        note: "the CYW43439's identity word",
    };
    let (record, remaining, fault, rca) = attach_over(&script, Part::Cyw4343w);
    assert_eq!(fault, None);
    assert_eq!(
        record.outcome,
        Some(Outcome::Refused(Refusal::new(
            STAGE_CHIP_IDENTITY,
            0x1545_A9AF
        )))
    );
    assert_eq!(remaining, 3, "the ALP rows never reached");
    assert_eq!(rca, 1, "the bus attached; the refusal is the core's");
}

#[test]
fn a_parked_driver_attaches_again_from_power_on_over_the_same_host() {
    let mut script = attach_script();
    script[28] = HostRow::Read {
        arg: 0x1500_0004,
        shape: Shape::bytes(4),
        data: &[0xAF, 0xA9, 0x45, 0x15],
        reply: 0x0000_2000,
        note: "the wrong part",
    };
    let mut bus = Sdio::new(FakeHost::new(&script), Part::Cyw4343w, OcrWindow::V3_3);
    let mut clock = FakeClock::new(0);
    let mut driver = Driver::new();
    assert!(driver.attach());
    let first = run(&mut driver, &mut bus, &mut clock, CAP);
    assert_eq!(
        first.outcome,
        Some(Outcome::Refused(Refusal::new(
            STAGE_CHIP_IDENTITY,
            0x1545_A9AF
        )))
    );
    assert!(driver.is_parked());
    assert_eq!(clock.now(), 586_000);

    bus.host_mut().restart();
    assert!(driver.attach(), "attach is accepted again after a refusal");
    let second = run(&mut driver, &mut bus, &mut clock, CAP);
    assert_eq!(
        second.outcome,
        Some(Outcome::Refused(Refusal::new(
            STAGE_CHIP_IDENTITY,
            0x1545_A9AF
        )))
    );
    assert_eq!(
        bus.host().remaining(),
        3,
        "the exchange replayed from its first row"
    );
    assert_eq!(
        &second.deadlines()[..3],
        &[588_000, 590_000, 592_000],
        "the host's power cycle again, on the clock as it stood"
    );

    let good = attach_script();
    let mut bus = Sdio::new(FakeHost::new(&good), Part::Cyw4343w, OcrWindow::V3_3);
    assert!(driver.attach());
    let third = run(&mut driver, &mut bus, &mut clock, CAP);
    assert_eq!(
        third.outcome,
        Some(Outcome::Attached {
            chip_id: 0x1531_A9A6
        })
    );
    assert!(driver.is_attached());
    assert!(!driver.attach(), "attach is refused while attached");
}
