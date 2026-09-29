//! The gSPI attach: the chip's bring-up from power-on to a running ALP
//! clock, as a recorded exchange at the wire, with three doctored controls.
//!
//! The rows are the bytes the chip is documented to send and receive
//! (Infineon CYW43439 datasheet, sections 4.2.1 and 4.2.3): the test
//! pattern halfword-swapped in the power-up order, the configuration dword,
//! the response delay, the interrupt registers, the identity word through
//! the function-1 window, and the ALP clock request and its grant.

use crate::driver::{Driver, Outcome, STAGE_CHIP_IDENTITY};
use crate::error::Refusal;
use crate::fixture::{End, FakeClock, FakeWire, Row, STAGE_FRAME_MISMATCH, run};
use crate::gspi::{Gspi, STAGE_BUS_VERIFICATION, STAGE_TEST_PATTERN};

const CAP: u32 = 10_000;
pub(super) const ROWS: usize = 21;

const fn frame(tx: &'static [u8], rx: &'static [u8], note: &'static str) -> Row {
    Row::Frame {
        tx,
        rx,
        times: 1,
        note,
    }
}

/// The recorded exchange: rows 1 to 21 of the design's table.
pub(super) fn attach_script() -> [Row; ROWS] {
    [
        Row::Strap(true),
        Row::Power(false),
        Row::Power(true),
        Row::Strap(false),
        frame(
            &[0xA0, 0x04, 0x40, 0x00],
            &[0xBE, 0xAD, 0xFE, 0xED],
            "read F0 0x14, the test pattern in the power-up order",
        ),
        frame(
            &[0x00, 0x04, 0xC0, 0x00, 0x00, 0xB3, 0x00, 0x02],
            &[],
            "write F0 0x00, the configuration dword",
        ),
        frame(
            &[0x01, 0xE8, 0x00, 0xC0, 0x04],
            &[],
            "write F0 0x1D, the function-1 response delay",
        ),
        frame(
            &[0x04, 0xA0, 0x00, 0x40],
            &[0xAD, 0xBE, 0xED, 0xFE],
            "read F0 0x14 in the configured order",
        ),
        frame(
            &[0x04, 0xC0, 0x00, 0xC0, 0x4C, 0x3D, 0x2E, 0x1F],
            &[],
            "write F0 0x18, the scratch pattern",
        ),
        frame(
            &[0x04, 0xC0, 0x00, 0x40],
            &[0x4C, 0x3D, 0x2E, 0x1F],
            "read F0 0x18",
        ),
        frame(
            &[0x04, 0xC0, 0x00, 0xC0, 0x00, 0x00, 0x00, 0x00],
            &[],
            "write F0 0x18, cleared",
        ),
        frame(
            &[0x02, 0x20, 0x00, 0xC0, 0x99, 0x00],
            &[],
            "write F0 0x04, the interrupt latches cleared",
        ),
        frame(
            &[0x02, 0x30, 0x00, 0xC0, 0x20, 0x00],
            &[],
            "write F0 0x06, the function-2 interrupt enabled",
        ),
        frame(&[0x02, 0x30, 0x00, 0x40], &[0x20, 0x00], "read F0 0x06"),
        frame(
            &[0x01, 0x50, 0x00, 0xD8, 0x00],
            &[],
            "write F1 0x1000A, the window's low byte",
        ),
        frame(
            &[0x01, 0x58, 0x00, 0xD8, 0x00],
            &[],
            "write F1 0x1000B, the window's middle byte",
        ),
        frame(
            &[0x01, 0x60, 0x00, 0xD8, 0x18],
            &[],
            "write F1 0x1000C, the window's high byte",
        ),
        frame(
            &[0x04, 0x00, 0x00, 0x54],
            &[0x00, 0x00, 0x00, 0x00, 0xAF, 0xA9, 0x45, 0x15],
            "read F1 0x8000, the delay word then the identity word",
        ),
        frame(
            &[0x01, 0x70, 0x00, 0xD8, 0x29],
            &[],
            "write F1 0x1000E, the ALP request",
        ),
        frame(
            &[0x01, 0x70, 0x00, 0x58],
            &[0x00, 0x00, 0x00, 0x00, 0x69],
            "read F1 0x1000E, the request bits and ALP_AVAIL",
        ),
        frame(
            &[0x01, 0x70, 0x00, 0xD8, 0x00],
            &[],
            "write F1 0x1000E, the request released",
        ),
    ]
}

/// A run over `script` from a fresh driver: the record, the rows left, the
/// fake's fault, and whether the bus ended in the configured mode.
fn attach_over(script: &[Row], stale: bool) -> (crate::fixture::Run, usize, Option<Refusal>, bool) {
    let wire = FakeWire::new(script);
    let wire = if stale { wire.stale_answers() } else { wire };
    let mut bus = Gspi::new(wire);
    let mut clock = FakeClock::new(0);
    let mut driver = Driver::new();
    assert!(driver.attach());
    let record = run(&mut driver, &mut bus, &mut clock, CAP);
    (
        record,
        bus.wire().remaining(),
        bus.wire().fault(),
        bus.is_configured(),
    )
}

#[test]
fn the_attach_replays_the_recorded_exchange_to_an_attached_chip() {
    let script = attach_script();
    let (record, remaining, fault, configured) = attach_over(&script, false);
    assert_eq!(fault, None);
    assert!(configured);
    assert_eq!(record.end, End::Done);
    assert_eq!(
        record.outcome,
        Some(Outcome::Attached {
            chip_id: 0x1545_A9AF
        })
    );
    assert_eq!(remaining, 0, "every row consumed");
    assert_eq!(record.deadlines(), &[50_000, 100_000, 110_000, 210_000]);
    assert_eq!(record.polls, 12);
}

#[test]
fn a_test_pattern_answered_once_with_the_fill_then_the_pattern_attaches() {
    let script = attach_script();
    let mut with_fill = std::vec::Vec::with_capacity(ROWS + 1);
    with_fill.extend_from_slice(&script[..4]);
    with_fill.push(frame(
        &[0xA0, 0x04, 0x40, 0x00],
        &[0x03, 0x03, 0x03, 0x03],
        "the first read after power-on: the chip not yet answering, as the silicon did",
    ));
    with_fill.extend_from_slice(&script[4..]);
    let (record, remaining, fault, configured) = attach_over(&with_fill, false);
    assert_eq!(fault, None);
    assert!(configured);
    assert_eq!(
        record.outcome,
        Some(Outcome::Attached {
            chip_id: 0x1545_A9AF
        })
    );
    assert_eq!(
        remaining, 0,
        "every row consumed: one more than the exchange of record"
    );
    assert_eq!(
        record.deadlines(),
        &[50_000, 100_000, 110_000, 120_000, 220_000],
        "one retry of the pattern, ten milliseconds later"
    );
    assert_eq!(record.polls, 13);
}

#[test]
fn a_wrong_identity_word_is_refused_by_name() {
    let mut script = attach_script();
    script[17] = frame(
        &[0x04, 0x00, 0x00, 0x54],
        &[0x00, 0x00, 0x00, 0x00, 0xA6, 0xA9, 0x31, 0x15],
        "the CYW4343W's identity word",
    );
    let (record, remaining, fault, configured) = attach_over(&script, false);
    assert_eq!(fault, None);
    assert!(configured, "the bus attached; the refusal is the core's");
    assert_eq!(
        record.outcome,
        Some(Outcome::Refused(Refusal::new(
            STAGE_CHIP_IDENTITY,
            0x1531_A9A6
        )))
    );
    assert_eq!(remaining, 3, "the ALP rows never reached");
    assert_eq!(record.deadlines(), &[50_000, 100_000]);
}

#[test]
fn a_test_pattern_that_never_matches_is_refused_at_the_deadline() {
    let mut script = attach_script();
    script[4] = Row::Frame {
        tx: &[0xA0, 0x04, 0x40, 0x00],
        rx: &[0x7D, 0x5B, 0xFD, 0xDA],
        times: u32::MAX,
        note: "the pattern shifted by one bit, forever",
    };
    let (record, remaining, fault, configured) = attach_over(&script, false);
    assert_eq!(fault, None);
    assert!(!configured, "the configuration write was never reached");
    assert_eq!(
        record.outcome,
        Some(Outcome::Refused(Refusal::new(
            STAGE_TEST_PATTERN,
            0xFDDA_7D5B
        )))
    );
    assert_eq!(remaining, ROWS - 4, "the pattern row still current");
    let deadlines = record.deadlines();
    assert_eq!(deadlines.len(), 11, "the two holds and nine retries");
    assert_eq!(&deadlines[..2], &[50_000, 100_000]);
    assert_eq!(deadlines[10], 190_000, "ten tries, 90 ms past the first");
}

#[test]
fn a_transport_that_answers_late_is_refused_at_the_bus_verification() {
    let mut script = attach_script();
    script[4] = Row::Frame {
        tx: &[0xA0, 0x04, 0x40, 0x00],
        rx: &[0xBE, 0xAD, 0xFE, 0xED],
        times: 2,
        note: "the test pattern read twice: the stale zeros, then the late answer",
    };
    let (record, remaining, fault, configured) = attach_over(&script, true);
    assert_eq!(fault, None);
    assert!(
        !configured,
        "the transport's own refusal returns it to the power-up state"
    );
    assert_eq!(
        record.outcome,
        Some(Outcome::Refused(Refusal::new(
            STAGE_BUS_VERIFICATION,
            0xEDFE_ADBE
        )))
    );
    assert_eq!(
        remaining,
        ROWS - 8,
        "refused at the first checkable answer after the configuration"
    );
    assert_eq!(record.deadlines(), &[50_000, 100_000, 110_000]);
}

#[test]
fn a_driver_that_deviates_from_the_script_fails_by_row_number() {
    let mut script = attach_script();
    script[11] = frame(
        &[0x02, 0x20, 0x00, 0xC0, 0x98, 0x00],
        &[],
        "one byte doctored in row 12",
    );
    let (record, _, fault, _) = attach_over(&script, false);
    assert_eq!(fault, Some(Refusal::new(STAGE_FRAME_MISMATCH, 12)));
    assert_eq!(
        record.outcome,
        Some(Outcome::Refused(Refusal::new(STAGE_FRAME_MISMATCH, 12)))
    );
}

#[test]
fn a_parked_driver_attaches_again_from_power_on_over_the_same_bus() {
    let mut script = attach_script();
    script[17] = frame(
        &[0x04, 0x00, 0x00, 0x54],
        &[0x00, 0x00, 0x00, 0x00, 0xA6, 0xA9, 0x31, 0x15],
        "the wrong part",
    );
    let mut bus = Gspi::new(FakeWire::new(&script));
    let mut clock = FakeClock::new(0);
    let mut driver = Driver::new();
    assert!(driver.attach());
    let first = run(&mut driver, &mut bus, &mut clock, CAP);
    assert_eq!(
        first.outcome,
        Some(Outcome::Refused(Refusal::new(
            STAGE_CHIP_IDENTITY,
            0x1531_A9A6
        )))
    );
    assert!(driver.is_parked());
    assert!(
        bus.is_configured(),
        "the bus attached; the refusal was the core's"
    );
    assert_eq!(clock.now(), 100_000);

    bus.wire_mut().restart();
    assert!(driver.attach(), "attach is accepted again after a refusal");
    let second = run(&mut driver, &mut bus, &mut clock, CAP);
    assert_eq!(
        second.outcome,
        Some(Outcome::Refused(Refusal::new(
            STAGE_CHIP_IDENTITY,
            0x1531_A9A6
        )))
    );
    assert_eq!(
        bus.wire().remaining(),
        3,
        "the exchange replayed from its first row"
    );
    assert_eq!(
        second.deadlines(),
        &[150_000, 200_000],
        "the two power holds again, on the clock as it stood"
    );

    let good = attach_script();
    let mut bus = Gspi::new(FakeWire::new(&good));
    assert!(driver.attach());
    let third = run(&mut driver, &mut bus, &mut clock, CAP);
    assert_eq!(
        third.outcome,
        Some(Outcome::Attached {
            chip_id: 0x1545_A9AF
        })
    );
    assert!(driver.is_attached());
    assert!(!driver.attach(), "attach is refused while attached");
}
