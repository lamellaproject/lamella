//! The tuning: the rule on the report-back's three rates, and the sweep
//! over a model of the chip's bus with the eye tables the battery names,
//! with its controls.

use crate::clock::Clock;
use crate::error::Refusal;
use crate::fixture::{EyeWire, FakeClock, FakeWire, REFUSED, STAGE_EYE_REFUSED};
use crate::gspi::{
    Gspi, GspiWire, STAGE_TUNING, Setting, TUNE_PHASES_MAX, Verdict, judge, tune_pattern,
};
use crate::transport::{Attach, Transport, Tune};

const S625: Setting = Setting {
    step_ps: 20_000,
    steps_per_bit: 8,
    first: -3,
    phases: 8,
};
const S9375: Setting = Setting {
    step_ps: 13_333,
    steps_per_bit: 8,
    first: -3,
    phases: 8,
};
const S1875: Setting = Setting {
    step_ps: 6_667,
    steps_per_bit: 8,
    first: -3,
    phases: 8,
};
static LADDER: [Setting; 3] = [S625, S9375, S1875];

const fn row(counts: [u16; 8]) -> [u16; TUNE_PHASES_MAX] {
    let mut r = [0u16; TUNE_PHASES_MAX];
    let mut i = 0;
    while i < 8 {
        r[i] = counts[i];
        i += 1;
    }
    r
}

/// Clean from -3 to +1, the next bit from +2: the report-back at 6.25 and
/// 9.375 MHz.
const CLEAN_TO_1: [u16; TUNE_PHASES_MAX] = row([0, 0, 0, 0, 0, 9, 9, 9]);
/// Clean from -3 to +3, the next bit at +4: the report-back at 18.75 MHz.
const CLEAN_TO_3: [u16; TUNE_PHASES_MAX] = row([0, 0, 0, 0, 0, 0, 0, 9]);
/// Clean everywhere: the window's end unseen.
const CLEAN_ALL: [u16; TUNE_PHASES_MAX] = row([0; 8]);
/// Dirty everywhere.
const DIRTY: [u16; TUNE_PHASES_MAX] = row([9; 8]);
/// Clean at -1 and 0 only.
const NARROW: [u16; TUNE_PHASES_MAX] = row([9, 9, 0, 0, 9, 9, 9, 9]);
/// One read refused at +3.
const ONE_REFUSED: [u16; TUNE_PHASES_MAX] = row([0, 0, 0, 0, 0, 0, REFUSED, 9]);

static REPORT_BACK: [[u16; TUNE_PHASES_MAX]; 3] = [CLEAN_TO_1, CLEAN_TO_1, CLEAN_TO_3];
static UNSEEN_END: [[u16; TUNE_PHASES_MAX]; 3] = [CLEAN_TO_1, CLEAN_TO_1, CLEAN_ALL];
static ALL_UNSEEN: [[u16; TUNE_PHASES_MAX]; 3] = [CLEAN_ALL, CLEAN_ALL, CLEAN_ALL];
static ALL_DIRTY: [[u16; TUNE_PHASES_MAX]; 3] = [DIRTY, DIRTY, DIRTY];
static NARROW_TOP: [[u16; TUNE_PHASES_MAX]; 3] = [CLEAN_TO_1, CLEAN_TO_1, NARROW];
static REFUSED_ONCE: [[u16; TUNE_PHASES_MAX]; 3] = [CLEAN_TO_1, CLEAN_TO_1, ONE_REFUSED];

#[test]
fn the_rule_on_the_report_backs_three_rates() {
    assert_eq!(
        judge(S625, &CLEAN_TO_1[..8]),
        Some(Verdict {
            phase: -3,
            before_ps: 80_000,
            after_ps: 80_000
        })
    );
    assert_eq!(
        judge(S9375, &CLEAN_TO_1[..8]),
        Some(Verdict {
            phase: -3,
            before_ps: 53_332,
            after_ps: 53_332
        })
    );
    assert_eq!(
        judge(S1875, &CLEAN_TO_3[..8]),
        Some(Verdict {
            phase: -1,
            before_ps: 26_668,
            after_ps: 26_668
        })
    );
}

#[test]
fn the_rule_rejects_what_it_cannot_measure_or_cannot_afford() {
    assert_eq!(
        judge(S1875, &CLEAN_ALL[..8]),
        None,
        "the window's end unseen"
    );
    assert_eq!(judge(S1875, &DIRTY[..8]), None, "no clean phase");
    assert_eq!(
        judge(S1875, &NARROW[..8]),
        None,
        "a run too narrow for the margin"
    );
    assert_eq!(
        judge(S1875, &ONE_REFUSED[..8]),
        Some(Verdict {
            phase: -2,
            before_ps: 26_668,
            after_ps: 26_668
        }),
        "a refused read ends the run like a dirty phase"
    );
    assert_eq!(judge(Setting::NONE, &[]), None);
}

#[test]
fn the_pattern_toggles_every_bit_and_repeats_no_neighbour() {
    let p = tune_pattern(0x40);
    let mut ones = [0u32; 8];
    for (i, b) in p.iter().enumerate() {
        for (bit, count) in ones.iter_mut().enumerate() {
            *count += u32::from((b >> bit) & 1);
        }
        if i > 0 {
            assert_ne!(p[i], p[i - 1], "byte {i}");
        }
    }
    assert!(ones.iter().all(|&c| c > 8 && c < 56), "{ones:?}");
    assert_ne!(tune_pattern(0x40), tune_pattern(0x3C));
}

/// The transport's own attach over the model, to `Ready`.
fn attached(wire: EyeWire<'static>) -> (Gspi<EyeWire<'static>>, FakeClock) {
    let mut bus = Gspi::new(wire);
    let mut clock = FakeClock::new(0);
    let mut steps = 0;
    loop {
        let now = clock.now_us();
        match bus.attach(now).expect("the attach over the model") {
            Attach::Pending { until } => clock.set(until),
            Attach::Ready => break,
        }
        steps += 1;
        assert!(steps < 100);
    }
    assert!(bus.is_configured());
    (bus, clock)
}

/// The tuning pumped to its end: the outcome and the number of calls.
fn tuned(bus: &mut Gspi<EyeWire<'static>>, clock: &mut FakeClock) -> (Result<(), Refusal>, u32) {
    let mut polls = 0u32;
    loop {
        polls += 1;
        assert!(polls < 10_000);
        let now = clock.now_us();
        match bus.tune(0, now) {
            Ok(Tune::Pending { until }) => {
                assert_eq!(until, now, "again at once");
            }
            Ok(Tune::Done) => return (Ok(()), polls),
            Err(refusal) => return (Err(refusal), polls),
        }
    }
}

#[test]
fn the_sweep_chooses_the_fastest_setting_with_the_margin() {
    let (mut bus, mut clock) = attached(EyeWire::new(&LADDER, &REPORT_BACK, 0));
    let frames_at_ready = bus.wire().frames();
    assert_eq!(frames_at_ready, 10, "the attach's frames");
    let (end, polls) = tuned(&mut bus, &mut clock);
    assert_eq!(end, Ok(()));
    assert_eq!(polls, 201, "two frameless calls and 199 frames");
    assert_eq!(bus.wire().frames() - frames_at_ready, 199);
    assert_eq!(bus.tuned(), Some((2, -1)));
    assert_eq!(bus.wire().selected(), (2, -1));
    assert_eq!(bus.wire().fault(), None);
    let eye = bus.eye();
    assert_eq!(
        &eye[0][..8],
        &[0, 0, 0, 0, 0, 72, 72, 72],
        "eight reads of nine bad bytes"
    );
    assert_eq!(&eye[1][..8], &[0, 0, 0, 0, 0, 72, 72, 72]);
    assert_eq!(&eye[2][..8], &[0, 0, 0, 0, 0, 0, 0, 72]);
    assert_eq!(eye[3], [0; TUNE_PHASES_MAX]);
    let selects = bus.wire().selects();
    assert_eq!(
        selects.len(),
        25,
        "eight phases per setting, then the choice"
    );
    for s in 0..3u8 {
        for p in 0..8u8 {
            assert_eq!(selects[usize::from(s * 8 + p)], (s, -3 + p as i8));
        }
    }
    assert_eq!(selects[24], (2, -1));
    assert_eq!(
        bus.wire().ram(),
        &tune_pattern(0x3C),
        "the proof's pattern stands in the RAM"
    );
}

#[test]
fn a_setting_whose_window_end_is_unseen_is_passed_over() {
    let (mut bus, mut clock) = attached(EyeWire::new(&LADDER, &UNSEEN_END, 0));
    let (end, _) = tuned(&mut bus, &mut clock);
    assert_eq!(end, Ok(()));
    assert_eq!(bus.tuned(), Some((1, -3)));
    assert_eq!(bus.wire().selected(), (1, -3));
}

#[test]
fn a_narrow_run_is_rejected_for_its_margins() {
    let (mut bus, mut clock) = attached(EyeWire::new(&LADDER, &NARROW_TOP, 0));
    let (end, _) = tuned(&mut bus, &mut clock);
    assert_eq!(end, Ok(()));
    assert_eq!(bus.tuned(), Some((1, -3)));
}

#[test]
fn a_refused_read_marks_its_phase_and_the_sweep_goes_on() {
    let (mut bus, mut clock) = attached(EyeWire::new(&LADDER, &REFUSED_ONCE, 0));
    let (end, polls) = tuned(&mut bus, &mut clock);
    assert_eq!(end, Ok(()));
    assert_eq!(polls, 201 - 7, "the refused phase ends after one read");
    assert_eq!(bus.eye()[2][6], REFUSED);
    assert_eq!(bus.tuned(), Some((2, -2)));
    assert_eq!(
        bus.wire().fault(),
        Some(Refusal::new(STAGE_EYE_REFUSED, 0x5000_0040))
    );
}

#[test]
fn no_setting_accepted_restores_the_wires_own_and_proves_it() {
    let (mut bus, mut clock) = attached(EyeWire::new(&LADDER, &ALL_UNSEEN, 0));
    let (end, _) = tuned(&mut bus, &mut clock);
    assert_eq!(end, Ok(()));
    assert_eq!(bus.tuned(), None);
    assert_eq!(
        bus.wire().selected(),
        (0, 0),
        "the wire's own setting stands"
    );
    assert_eq!(bus.wire().selects().last(), Some(&(0, 0)));
}

#[test]
fn a_proof_that_reads_back_wrong_is_refused_at_the_address() {
    let (mut bus, mut clock) = attached(EyeWire::new(&LADDER, &ALL_DIRTY, 0));
    let (end, polls) = tuned(&mut bus, &mut clock);
    assert_eq!(
        end,
        Err(Refusal::new(STAGE_TUNING, 0)),
        "the first byte of the proof differs"
    );
    assert_eq!(polls, 199, "refused at the proof's first read-back");
    assert_eq!(bus.tuned(), None);
}

#[test]
fn a_test_register_wrong_at_the_chosen_setting_is_refused_with_the_word() {
    let (mut bus, mut clock) = attached(EyeWire::new(&LADDER, &REPORT_BACK, 0));
    bus.wire_mut().corrupt_test_register();
    let (end, polls) = tuned(&mut bus, &mut clock);
    assert_eq!(end, Err(Refusal::new(STAGE_TUNING, 0xDEAD_BEEF)));
    assert_eq!(polls, 201);
    assert_eq!(
        bus.tuned(),
        Some((2, -1)),
        "the choice was made before the proof failed"
    );
}

#[test]
fn a_wire_with_nothing_to_tune_is_done_in_one_call_with_nothing_on_the_wire() {
    let script = super::attach_gspi::attach_script();
    let mut bus = Gspi::new(FakeWire::new(&script));
    assert_eq!(bus.tune(0, 5), Ok(Tune::Done));
    assert_eq!(bus.wire().remaining(), script.len());
    assert_eq!(bus.tuned(), None);
}

#[test]
fn a_wire_offering_more_settings_than_the_rows_is_refused_by_name() {
    static FIVE: [Setting; 5] = [S625, S625, S625, S625, S625];
    static TABLE: [[u16; TUNE_PHASES_MAX]; 5] = [CLEAN_TO_1; 5];
    let (mut bus, mut clock) = attached(EyeWire::new(&FIVE, &TABLE, 0));
    let (end, polls) = tuned(&mut bus, &mut clock);
    assert_eq!(end, Err(Refusal::new(STAGE_TUNING, 5)));
    assert_eq!(polls, 1);
}
