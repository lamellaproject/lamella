//! The core's attach at the transport level: the chip identity through the
//! window and the ALP clock request, as a recorded exchange of the trait's
//! own operations, with three controls; and the tie between the three
//! fixture levels -- the same operations logged over the gSPI wire, over
//! the SDIO host, and over the transport-level script.

use crate::driver::{Driver, Outcome, STAGE_ALP_CLOCK, STAGE_CHIP_IDENTITY};
use crate::error::Refusal;
use crate::fixture::{
    End, FakeClock, FakeHost, FakeTransport, FakeWire, Kind, Logged, Op, Recorder, Run,
    STAGE_TRANSPORT_MISMATCH, run,
};
use crate::gspi::Gspi;
use crate::sdio::{OcrWindow, Sdio};
use crate::transport::{Func, Part};

const CAP: u32 = 10_000;
const ROWS: usize = 8;

const fn write(addr: u32, value: u8, note: &'static str) -> Op {
    Op::WriteDirect {
        func: Func::F1,
        addr,
        value,
        note,
    }
}

/// The recorded exchange: the transport's attach, then the seven
/// operations of the core.
fn core_script() -> [Op; ROWS] {
    [
        Op::Attach { pending: 0 },
        write(0x1000A, 0x00, "the window's low byte"),
        write(0x1000B, 0x00, "the window's middle byte"),
        write(0x1000C, 0x18, "the window's high byte"),
        Op::ReadExtended {
            func: Func::F1,
            addr: 0x8000,
            incr: true,
            data: &[0xAF, 0xA9, 0x45, 0x15],
            note: "the identity word at 0x18000000",
        },
        write(0x1000E, 0x29, "the ALP request"),
        Op::ReadDirect {
            func: Func::F1,
            addr: 0x1000E,
            value: 0x69,
            times: 1,
            note: "the request bits and ALP_AVAIL",
        },
        write(0x1000E, 0x00, "the request released"),
    ]
}

fn attach_over(script: &[Op], part: Part) -> (Run, usize, Option<Refusal>) {
    let mut bus = FakeTransport::new(script, part);
    let mut clock = FakeClock::new(0);
    let mut driver = Driver::new();
    assert!(driver.attach());
    let record = run(&mut driver, &mut bus, &mut clock, CAP);
    (record, bus.remaining(), bus.fault())
}

#[test]
fn the_core_replays_the_recorded_exchange_to_an_attached_chip() {
    let script = core_script();
    let (record, remaining, fault) = attach_over(&script, Part::Cyw43439);
    assert_eq!(fault, None);
    assert_eq!(record.end, End::Done);
    assert_eq!(
        record.outcome,
        Some(Outcome::Attached {
            chip_id: 0x1545_A9AF
        })
    );
    assert_eq!(remaining, 0);
    assert_eq!(record.deadlines(), &[10_000, 110_000]);
    assert_eq!(record.polls, 6);
}

#[test]
fn a_wrong_identity_word_is_refused_by_name() {
    let mut script = core_script();
    script[4] = Op::ReadExtended {
        func: Func::F1,
        addr: 0x8000,
        incr: true,
        data: &[0xA6, 0xA9, 0x31, 0x15],
        note: "the CYW4343W's word",
    };
    let (record, remaining, fault) = attach_over(&script, Part::Cyw43439);
    assert_eq!(fault, None);
    assert_eq!(
        record.outcome,
        Some(Outcome::Refused(Refusal::new(
            STAGE_CHIP_IDENTITY,
            0x1531_A9A6
        )))
    );
    assert_eq!(remaining, 3, "the ALP rows never reached");
}

#[test]
fn an_alp_clock_that_never_comes_is_refused_at_the_deadline() {
    let mut script = core_script();
    script[6] = Op::ReadDirect {
        func: Func::F1,
        addr: 0x1000E,
        value: 0x29,
        times: u32::MAX,
        note: "the request bits alone, forever",
    };
    let (record, remaining, fault) = attach_over(&script, Part::Cyw43439);
    assert_eq!(fault, None);
    assert_eq!(
        record.outcome,
        Some(Outcome::Refused(Refusal::new(STAGE_ALP_CLOCK, 0x29)))
    );
    assert_eq!(
        remaining, 2,
        "the read row still current, the release never written"
    );
    assert_eq!(
        record.deadlines(),
        &[
            10_000, 20_000, 30_000, 40_000, 50_000, 60_000, 70_000, 80_000, 90_000, 100_000
        ],
        "ten tries, ten milliseconds apart"
    );
}

#[test]
fn a_driver_that_deviates_from_the_script_fails_by_row_number() {
    let mut script = core_script();
    script[2] = write(0x1000B, 0x01, "one bit doctored in row 3");
    let (record, _, fault) = attach_over(&script, Part::Cyw43439);
    assert_eq!(fault, Some(Refusal::new(STAGE_TRANSPORT_MISMATCH, 3)));
    assert_eq!(
        record.outcome,
        Some(Outcome::Refused(Refusal::new(STAGE_TRANSPORT_MISMATCH, 3)))
    );
}

/// The seven operations of the core, as the recorder logs them, for a
/// part whose identity word is `word`.
fn expected_core(word: u32) -> [Logged; 7] {
    [
        Logged::direct(Kind::WriteDirect, Func::F1, 0x1000A, 0x00),
        Logged::direct(Kind::WriteDirect, Func::F1, 0x1000B, 0x00),
        Logged::direct(Kind::WriteDirect, Func::F1, 0x1000C, 0x18),
        Logged::extended(Kind::ReadExtended, Func::F1, 0x8000, true, 4, word),
        Logged::direct(Kind::WriteDirect, Func::F1, 0x1000E, 0x29),
        Logged::direct(Kind::ReadDirect, Func::F1, 0x1000E, 0x69),
        Logged::direct(Kind::WriteDirect, Func::F1, 0x1000E, 0x00),
    ]
}

/// The log after the transport's own attach.
fn after_attach(log: &[Logged]) -> &[Logged] {
    let start = log
        .iter()
        .rposition(|e| e.kind == Kind::Attach)
        .map_or(0, |i| i + 1);
    &log[start..]
}

#[test]
fn the_three_fixture_levels_carry_the_same_core_operations() {
    let mut clock = FakeClock::new(0);

    let wire_script = super::attach_gspi::attach_script();
    let mut over_wire = Recorder::new(Gspi::new(FakeWire::new(&wire_script)));
    let mut driver = Driver::new();
    assert!(driver.attach());
    let record = run(&mut driver, &mut over_wire, &mut clock, CAP);
    assert_eq!(
        record.outcome,
        Some(Outcome::Attached {
            chip_id: 0x1545_A9AF
        })
    );
    assert_eq!(after_attach(over_wire.log()), &expected_core(0x1545_A9AF));

    let host_script = super::attach_sdio::attach_script();
    let mut over_host = Recorder::new(Sdio::new(
        FakeHost::new(&host_script),
        Part::Cyw4343w,
        OcrWindow::V3_3,
    ));
    let mut driver = Driver::new();
    assert!(driver.attach());
    let record = run(&mut driver, &mut over_host, &mut clock, CAP);
    assert_eq!(
        record.outcome,
        Some(Outcome::Attached {
            chip_id: 0x1531_A9A6
        })
    );
    assert_eq!(after_attach(over_host.log()), &expected_core(0x1531_A9A6));

    let script = core_script();
    let mut over_script = Recorder::new(FakeTransport::new(&script, Part::Cyw43439));
    let mut driver = Driver::new();
    assert!(driver.attach());
    let record = run(&mut driver, &mut over_script, &mut clock, CAP);
    assert_eq!(
        record.outcome,
        Some(Outcome::Attached {
            chip_id: 0x1545_A9AF
        })
    );
    assert_eq!(after_attach(over_script.log()), &expected_core(0x1545_A9AF));
    assert!(!over_script.overflowed());
}
