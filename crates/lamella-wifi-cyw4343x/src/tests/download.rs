//! The download at the transport level: upload mode, the firmware and its
//! read-back, the settings image and its trailer, the ARM release, the
//! packet channel's readiness and the keep-bus-on request, as a recorded
//! exchange of the trait's own operations, with its controls.

use crate::backplane::Window;
use crate::clock::Micros;
use crate::download::{
    STAGE_ARM_START, STAGE_F2_READY, STAGE_FIRMWARE_SIZE, STAGE_KSO, STAGE_READ_BACK,
    STAGE_SETTINGS_SIZE, STAGE_SOCSRAM, pad_piece,
};
use crate::driver::{Driver, Outcome};
use crate::error::Refusal;
use crate::fixture::{End, FakeClock, FakeTransport, Op, Run, STAGE_TRANSPORT_MISMATCH, run};
use crate::transport::{Func, Part};
use std::vec::Vec;

const CAP: u32 = 100_000;

/// The download's first row in the full script, after the attach's eight.
pub(super) const FIRST: usize = 8;

const fn firmware() -> [u8; 200] {
    let mut image = [0u8; 200];
    let mut i = 0;
    while i < 200 {
        image[i] = (i as u8).wrapping_mul(0x2B) ^ 0xA5;
        i += 1;
    }
    image
}

const fn doctored() -> [u8; 64] {
    let image = firmware();
    let mut chunk = [0u8; 64];
    let mut i = 0;
    while i < 64 {
        chunk[i] = image[64 + i];
        i += 1;
    }
    chunk[5] ^= 0xFF;
    chunk
}

/// A synthetic firmware image of 200 bytes.
pub(super) static FIRMWARE: [u8; 200] = firmware();
/// A synthetic settings image of 100 bytes: seven NUL-terminated strings.
pub(super) static SETTINGS: [u8; 100] =
    *b"xtalfreq=37400\0boardflags=0x00404201\0macaddr=00:A0:50:00:00:01\0nvramrev=1\0aa2g=1\0ccode=0\0sromrev=11\0";

static DOCTORED: [u8; 64] = doctored();
static W0: [u8; 4] = [0, 0, 0, 0];
static W1: [u8; 4] = [1, 0, 0, 0];
static W3: [u8; 4] = [3, 0, 0, 0];
static WFF: [u8; 4] = [0xFF; 4];
static SR: [u8; 4] = [0xF8, 0x01, 0, 0];
static TRAILER: [u8; 4] = [0x20, 0x00, 0xDF, 0xFF];
static TRAILER_UNPADDED: [u8; 4] = [0x19, 0x00, 0xE6, 0xFF];
static TRAILER_BYTES: [u8; 4] = [0x80, 0x00, 0x7F, 0xFF];
static TRAILER_WHOLE: [u8; 4] = [0xDF, 0xFF, 0xFF, 0xFF];
static ZEROS: [u8; 32] = [0; 32];
static ID_43439: [u8; 4] = [0xAF, 0xA9, 0x45, 0x15];
static ID_4343W: [u8; 4] = [0xA6, 0xA9, 0x31, 0x15];

pub(super) const fn wd(addr: u32, value: u8) -> Op {
    Op::WriteDirect {
        func: Func::F1,
        addr,
        value,
        note: "a direct write on function 1",
    }
}

pub(super) const fn rd(addr: u32, value: u8) -> Op {
    Op::ReadDirect {
        func: Func::F1,
        addr,
        value,
        times: 1,
        note: "a direct read on function 1",
    }
}

pub(super) const fn we(addr: u32, data: &'static [u8]) -> Op {
    Op::WriteExtended {
        func: Func::F1,
        addr,
        incr: true,
        data,
        note: "an extended write on function 1",
    }
}

pub(super) const fn re(addr: u32, data: &'static [u8]) -> Op {
    Op::ReadExtended {
        func: Func::F1,
        addr,
        incr: true,
        data,
        note: "an extended read on function 1",
    }
}

/// The attach at the transport level for `part`: the transport's own
/// attach, then the seven operations of the core.
pub(super) fn attach_ops(part: Part) -> Vec<Op> {
    let identity: &'static [u8] = match part {
        Part::Cyw43439 => &ID_43439,
        Part::Cyw4343w => &ID_4343W,
    };
    std::vec![
        Op::Attach { pending: 0 },
        wd(0x1000A, 0x00),
        wd(0x1000B, 0x00),
        wd(0x1000C, 0x18),
        re(0x8000, identity),
        wd(0x1000E, 0x29),
        rd(0x1000E, 0x69),
        wd(0x1000E, 0x00),
    ]
}

/// The download's rows for a transport whose function-1 bulk transfer is
/// `chunk` bytes: the design's table, with the firmware and the settings
/// image chunked at that size.
pub(super) fn download_ops(chunk: usize) -> Vec<Op> {
    let mut v = std::vec![
        // Upload mode: the ARM core disabled.
        wd(0x1000B, 0x10),
        re(0xB800, &W0),
        we(0xB408, &W3),
        re(0xB408, &W3),
        we(0xB800, &W1),
        we(0xB408, &W3),
        re(0xB408, &W3),
        // The RAM core reset.
        re(0xC800, &W0),
        we(0xC408, &W3),
        re(0xC408, &W3),
        we(0xC800, &W1),
        we(0xC408, &W3),
        re(0xC408, &W3),
        we(0xC800, &W0),
        re(0xC800, &W0),
        we(0xC408, &W1),
        re(0xC408, &W1),
        // The bank remap disabled.
        wd(0x1000B, 0x00),
        we(0xC010, &W3),
        we(0xC044, &W0),
        // The tuning hook with the window at chip RAM 0.
        wd(0x1000C, 0x00),
        Op::Tune { pending: 1 },
    ];
    // The firmware, then its read-back in 64-byte pieces.
    let mut at = 0;
    while at < FIRMWARE.len() {
        let n = (FIRMWARE.len() - at).min(chunk);
        v.push(we(at as u32, &FIRMWARE[at..at + n]));
        at += n;
    }
    let mut at = 0;
    while at < FIRMWARE.len() {
        let n = (FIRMWARE.len() - at).min(64);
        v.push(re(at as u32, &FIRMWARE[at..at + n]));
        at += n;
    }
    // The settings image under the trailer, its padding, the trailer.
    v.push(wd(0x1000A, 0x80));
    v.push(wd(0x1000B, 0x07));
    let mut at = 0;
    while at < SETTINGS.len() {
        let n = (SETTINGS.len() - at).min(chunk);
        v.push(we(0x7F7C + at as u32, &SETTINGS[at..at + n]));
        at += n;
    }
    while at < 128 {
        let piece = pad_piece(128 - at);
        v.push(we(0x7F7C + at as u32, &ZEROS[..piece]));
        at += piece;
    }
    v.push(we(0xFFFC, &TRAILER));
    // The RAM core checked, the interrupt status cleared, the ARM released
    // and checked.
    v.push(wd(0x1000A, 0x00));
    v.push(wd(0x1000B, 0x10));
    v.push(wd(0x1000C, 0x18));
    v.push(re(0xC408, &W1));
    v.push(re(0xC800, &W0));
    v.push(wd(0x1000B, 0x00));
    v.push(we(0xA020, &WFF));
    v.push(wd(0x1000B, 0x10));
    v.push(re(0xB800, &W1));
    v.push(we(0xB408, &W3));
    v.push(re(0xB408, &W3));
    v.push(we(0xB800, &W0));
    v.push(re(0xB800, &W0));
    v.push(we(0xB408, &W1));
    v.push(re(0xB408, &W1));
    v.push(re(0xB408, &W1));
    v.push(re(0xB800, &W0));
    // The packet channel ready on the second read.
    v.push(Op::F2Ready {
        ready: false,
        times: 1,
    });
    v.push(Op::F2Ready {
        ready: true,
        times: 1,
    });
    // The save/restore probe and the setup it commands.
    v.push(wd(0x1000B, 0x00));
    v.push(re(0x8508, &SR));
    v.push(rd(0x1001E, 0x00));
    v.push(wd(0x1001E, 0x02));
    v.push(Op::WakeOnCommand);
    v.push(wd(0x1000E, 0x02));
    v.push(wd(0x1001F, 0x03));
    v.push(rd(0x1001F, 0x03));
    v
}

/// The attach then the download, for `part` over a transport chunking at
/// `chunk`.
pub(super) fn full_script(chunk: usize, part: Part) -> Vec<Op> {
    let mut v = attach_ops(part);
    v.extend(download_ops(chunk));
    v
}

/// A fresh driver attached over `script`, the clock at 110,000.
fn attach_over<'s>(script: &'s [Op]) -> (Driver<'static>, FakeTransport<'s>, FakeClock) {
    let mut bus = FakeTransport::new(script, Part::Cyw43439);
    let mut clock = FakeClock::new(0);
    let mut driver = Driver::new();
    assert!(driver.attach());
    let record = run(&mut driver, &mut bus, &mut clock, CAP);
    assert_eq!(
        record.outcome,
        Some(Outcome::Attached {
            chip_id: 0x1545_A9AF
        })
    );
    assert_eq!(clock.now(), 110_000);
    (driver, bus, clock)
}

/// The upload over `script` from an attached driver: the record, the rows
/// left, the fake's fault, whether the firmware runs, and the clock.
fn upload_over(script: &[Op]) -> (Run, usize, Option<Refusal>, bool, Micros) {
    let (mut driver, mut bus, mut clock) = attach_over(script);
    assert!(driver.upload(&FIRMWARE, &SETTINGS));
    let record = run(&mut driver, &mut bus, &mut clock, CAP);
    (
        record,
        bus.remaining(),
        bus.fault(),
        driver.is_running(),
        clock.now(),
    )
}

#[test]
fn the_download_replays_the_recorded_exchange_to_a_running_chip() {
    let script = full_script(64, Part::Cyw43439);
    assert_eq!(script.len(), FIRST + 65);
    let (record, remaining, fault, running, now) = upload_over(&script);
    assert_eq!(fault, None);
    assert_eq!(record.end, End::Done);
    assert_eq!(
        record.outcome,
        Some(Outcome::Uploaded { save_restore: true })
    );
    assert_eq!(remaining, 0, "every row consumed");
    assert!(running);
    assert_eq!(now, 411_036);
    assert_eq!(
        record.deadlines(),
        &[
            120_000, 130_011, 180_024, 181_024, 191_024, 201_036, 211_036, 311_036, 411_036
        ]
    );
    assert_eq!(record.polls, 36);
}

#[test]
fn upload_is_refused_where_the_state_forbids_it() {
    let script = full_script(64, Part::Cyw43439);
    let mut fresh = Driver::new();
    assert!(!fresh.upload(&FIRMWARE, &SETTINGS), "not before the attach");
    let (mut driver, mut bus, mut clock) = attach_over(&script);
    assert!(driver.upload(&FIRMWARE, &SETTINGS));
    assert!(
        !driver.upload(&FIRMWARE, &SETTINGS),
        "not while downloading"
    );
    let record = run(&mut driver, &mut bus, &mut clock, CAP);
    assert_eq!(
        record.outcome,
        Some(Outcome::Uploaded { save_restore: true })
    );
    assert!(!driver.upload(&FIRMWARE, &SETTINGS), "not while running");
    assert!(!driver.attach(), "attach is refused while running");
}

#[test]
fn images_that_do_not_fit_are_refused_before_the_bus() {
    let script = attach_ops(Part::Cyw43439);
    let big = std::vec![0u8; 0x8_0000];
    let cases: [(&[u8], &[u8], &str, u32); 4] = [
        (&FIRMWARE[..0], &SETTINGS, STAGE_FIRMWARE_SIZE, 0),
        (&FIRMWARE, &SETTINGS[..0], STAGE_SETTINGS_SIZE, 0),
        (&big, &SETTINGS, STAGE_FIRMWARE_SIZE, 0x8_0000),
        (
            &big[..0x7FF7C - 63],
            &SETTINGS,
            STAGE_FIRMWARE_SIZE,
            524_093,
        ),
    ];
    let mut driver = Driver::new();
    for (firmware, settings, stage, status) in cases {
        let mut bus = FakeTransport::new(&script, Part::Cyw43439);
        let mut clock = FakeClock::new(0);
        assert!(driver.attach());
        let attached = run(&mut driver, &mut bus, &mut clock, CAP);
        assert!(matches!(attached.outcome, Some(Outcome::Attached { .. })));
        assert!(driver.upload(firmware, settings));
        let record = run(&mut driver, &mut bus, &mut clock, CAP);
        assert_eq!(
            record.outcome,
            Some(Outcome::Refused(Refusal::new(stage, status)))
        );
        assert_eq!(record.polls, 1, "refused at the first poll");
        assert_eq!(bus.fault(), None, "nothing reached the bus");
        assert!(driver.is_parked());
    }
}

#[test]
fn the_three_ways_to_get_the_trailer_wrong_fail_at_the_trailer_row() {
    let wrong: [(&'static [u8], &str); 3] = [
        (&TRAILER_UNPADDED, "the unpadded size"),
        (&TRAILER_BYTES, "the size in bytes"),
        (&TRAILER_WHOLE, "the whole word complemented"),
    ];
    for (bytes, name) in wrong {
        let mut script = full_script(64, Part::Cyw43439);
        script[FIRST + 37] = we(0xFFFC, bytes);
        let (record, _, fault, running, _) = upload_over(&script);
        assert_eq!(
            fault,
            Some(Refusal::new(STAGE_TRANSPORT_MISMATCH, 46)),
            "{name}"
        );
        assert_eq!(
            record.outcome,
            Some(Outcome::Refused(Refusal::new(STAGE_TRANSPORT_MISMATCH, 46))),
            "{name}"
        );
        assert!(!running, "{name}");
    }
}

#[test]
fn a_firmware_byte_that_reads_back_differently_is_refused_with_its_address() {
    let mut script = full_script(64, Part::Cyw43439);
    script[FIRST + 27] = re(0x0040, &DOCTORED);
    let (record, remaining, fault, running, _) = upload_over(&script);
    assert_eq!(fault, None);
    assert_eq!(
        record.outcome,
        Some(Outcome::Refused(Refusal::new(STAGE_READ_BACK, 69)))
    );
    assert_eq!(remaining, 37, "the settings never written");
    assert!(!running);
}

#[test]
fn a_ram_core_that_reads_down_before_the_release_is_refused() {
    let mut script = full_script(64, Part::Cyw43439);
    script[FIRST + 41] = re(0xC408, &W3);
    let (record, remaining, fault, _, _) = upload_over(&script);
    assert_eq!(fault, None);
    assert_eq!(
        record.outcome,
        Some(Outcome::Refused(Refusal::new(STAGE_SOCSRAM, 3)))
    );
    assert_eq!(remaining, 23, "the ARM never released");
}

#[test]
fn an_arm_core_that_does_not_run_after_the_release_is_refused() {
    let mut script = full_script(64, Part::Cyw43439);
    script[FIRST + 54] = re(0xB800, &W1);
    let (record, remaining, fault, _, _) = upload_over(&script);
    assert_eq!(fault, None);
    assert_eq!(
        record.outcome,
        Some(Outcome::Refused(Refusal::new(STAGE_ARM_START, 1)))
    );
    assert_eq!(remaining, 10, "the packet channel never asked");
}

#[test]
fn a_packet_channel_that_never_reports_ready_is_refused_at_the_deadline() {
    let mut script = full_script(64, Part::Cyw43439);
    script[FIRST + 55] = Op::F2Ready {
        ready: false,
        times: u32::MAX,
    };
    let (record, remaining, fault, _, _) = upload_over(&script);
    assert_eq!(fault, None);
    assert_eq!(
        record.outcome,
        Some(Outcome::Refused(Refusal::new(STAGE_F2_READY, 100)))
    );
    assert_eq!(remaining, 10, "the forever row still current");
    let deadlines = record.deadlines();
    assert_eq!(
        deadlines.len(),
        105,
        "six to the first read, then 99 retries"
    );
    assert_eq!(deadlines[5], 201_036);
    assert_eq!(
        deadlines[104], 1_191_036,
        "one hundred reads, 990 ms past the first"
    );
}

static KSO_TRY: [Op; 2] = [wd(0x1001F, 0x03), rd(0x1001F, 0x00)];

#[test]
fn a_keep_bus_on_request_never_granted_is_refused_at_the_deadline() {
    let mut script = full_script(64, Part::Cyw43439);
    script.truncate(FIRST + 63);
    script.push(Op::Repeat {
        ops: &KSO_TRY,
        times: 200,
    });
    let (record, remaining, fault, _, now) = upload_over(&script);
    assert_eq!(fault, None);
    assert_eq!(
        record.outcome,
        Some(Outcome::Refused(Refusal::new(STAGE_KSO, 0x00)))
    );
    assert_eq!(
        remaining, 0,
        "two hundred tries, the request rewritten each time"
    );
    let deadlines = record.deadlines();
    assert_eq!(
        deadlines.len(),
        208,
        "eight to the first request, then 200 polls"
    );
    assert_eq!(deadlines[8], 411_036);
    assert_eq!(
        deadlines[207], 20_311_036,
        "the two hundredth poll, 20 s past the first"
    );
    assert_eq!(now, 20_311_036);
}

#[test]
fn a_zero_probe_ends_the_download_without_the_keep_bus_on_request() {
    let mut script = full_script(64, Part::Cyw43439);
    script[FIRST + 58] = re(0x8508, &W0);
    let (record, remaining, fault, running, _) = upload_over(&script);
    assert_eq!(fault, None);
    assert_eq!(
        record.outcome,
        Some(Outcome::Uploaded {
            save_restore: false
        })
    );
    assert_eq!(remaining, 6, "the setup never commanded");
    assert!(running);
    assert_eq!(record.deadlines().len(), 8);
}

#[test]
fn a_driver_that_deviates_from_the_script_fails_by_row_number() {
    let mut script = full_script(64, Part::Cyw43439);
    script[FIRST + 19] = we(0xC044, &W1);
    let (record, _, fault, _, _) = upload_over(&script);
    assert_eq!(fault, Some(Refusal::new(STAGE_TRANSPORT_MISMATCH, 28)));
    assert_eq!(
        record.outcome,
        Some(Outcome::Refused(Refusal::new(STAGE_TRANSPORT_MISMATCH, 28)))
    );
}

#[test]
fn the_window_writes_only_the_bytes_that_change_across_its_boundaries() {
    let script = [
        wd(0x1000A, 0x00),
        wd(0x1000B, 0x00),
        wd(0x1000C, 0x00),
        wd(0x1000A, 0x80),
        wd(0x1000A, 0x00),
        wd(0x1000B, 0x01),
        wd(0x1000B, 0x00),
        wd(0x1000C, 0x18),
    ];
    let mut bus = FakeTransport::new(&script, Part::Cyw43439);
    let mut window = Window::new();
    assert_eq!(window.select(&mut bus, 0x7FC0), Ok(()));
    assert_eq!(bus.remaining(), 5, "the first selection writes all three");
    assert_eq!(window.select(&mut bus, 0x7FFF), Ok(()));
    assert_eq!(bus.remaining(), 5, "the same window writes nothing");
    assert_eq!(window.select(&mut bus, 0x8000), Ok(()));
    assert_eq!(bus.remaining(), 4, "the 32 KB line changes the low byte");
    assert_eq!(window.select(&mut bus, 0x1_0000), Ok(()));
    assert_eq!(bus.remaining(), 2, "the 64 KB line changes two bytes");
    assert_eq!(window.select(&mut bus, 0x1800_0508), Ok(()));
    assert_eq!(bus.remaining(), 0);
    assert_eq!(bus.fault(), None);
}
