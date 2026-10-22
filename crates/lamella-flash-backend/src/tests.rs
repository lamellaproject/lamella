//! Tests for the contract's guarantees, which are guarantees about ORDER.

use super::*;

/// A backend that records what it was asked to do, so a test can assert on the sequence.
struct Recorder {
    log: Vec<&'static str>,
    base: u32,
    identity: Result<u64, ()>,
    /// What a read-back returns: `None` = this mechanism cannot read back at all.
    readable: Option<Vec<u8>>,
}

impl Recorder {
    fn new(readable: Option<Vec<u8>>) -> Self {
        Recorder { log: Vec::new(), base: 0x0800_0000, identity: Ok(0x2ba0_1477), readable }
    }
}

impl FlashBackend for Recorder {
    fn mechanism(&self) -> &'static str {
        "a recording backend"
    }
    fn flash_base(&self) -> u32 {
        self.base
    }
    fn identify(&mut self) -> Result<PartIdentity, FlashError> {
        self.log.push("identify");
        match self.identity {
            Ok(value) => Ok(PartIdentity { value, what: "the part family" }),
            Err(()) => Err(FlashError::WrongPart {
                expected: PartIdentity { value: 0x2ba0_1477, what: "the part family" },
                found: 0x0bb1_1477,
            }),
        }
    }
    fn erase(&mut self, _image: &Image<'_>) -> Result<(), FlashError> {
        self.log.push("erase");
        Ok(())
    }
    fn program(&mut self, _image: &Image<'_>) -> Result<(), FlashError> {
        self.log.push("program");
        Ok(())
    }
    fn read_back(&mut self, _image: &Image<'_>) -> Option<Result<Vec<u8>, FlashError>> {
        self.log.push("read_back");
        self.readable.clone().map(Ok)
    }
    fn finish(&mut self) -> Result<(), FlashError> {
        self.log.push("finish");
        Ok(())
    }
}

const BYTES: [u8; 4] = [0xDE, 0xAD, 0xBE, 0xEF];

fn image() -> Image<'static> {
    Image { bytes: &BYTES, base: 0x0800_0000 }
}

/// The sequence the contract exists to guarantee.
#[test]
fn the_steps_run_in_the_order_the_contract_promises() {
    let mut backend = Recorder::new(Some(BYTES.to_vec()));
    let report = flash(&mut backend, &image(), VerifyPolicy::ReadBack, &Allow::Any).expect("a clean write");
    assert_eq!(backend.log, ["identify", "erase", "program", "read_back", "finish"]);
    assert_eq!(report.verification, Verification::ReadBack);
    assert_eq!(report.bytes, 4);
}

/// **A PERMISSION IS CHECKED AGAINST THE PART, AND IT IS CHECKED BEFORE THE ERASE.**
///
/// The load-bearing assertion is the call log: `erase` must not appear in it. A permission enforced
/// after the erase is not a permission, it is a report -- and the board it was protecting is gone
/// by the time it fires.
#[test]
fn a_part_outside_the_permitted_set_is_refused_with_nothing_erased() {
    let mut backend = Recorder::new(Some(BYTES.to_vec()));
    let allow = Allow::Identities(vec![0x0000_0000_DEAD_BEEF]);
    let error = flash(&mut backend, &image(), VerifyPolicy::ReadBack, &allow)
        .expect_err("this part is not on the list");

    assert!(matches!(error, FlashError::NotAllowed { .. }), "got {error:?}");
    assert_eq!(backend.log, ["identify"], "the refusal comes AFTER identify and BEFORE erase");
    assert!(!backend.log.contains(&"erase"), "THE ERASE MUST NOT HAVE HAPPENED");
    let rendered = error.to_string();
    assert!(rendered.contains("Nothing was erased"), "and it must say so: {rendered}");
}

/// The same part, on the list, goes through -- otherwise the test above would pass against a
/// driver that refused everything.
#[test]
fn a_permitted_part_is_written_normally() {
    let mut backend = Recorder::new(Some(BYTES.to_vec()));
    let allow = Allow::Identities(vec![0x2ba0_1477]);
    let report = flash(&mut backend, &image(), VerifyPolicy::ReadBack, &allow)
        .expect("this part IS on the list");
    assert_eq!(report.verification, Verification::ReadBack);
    assert!(backend.log.contains(&"erase"), "and it really wrote: {:?}", backend.log);
}

/// **THE LOAD-BEARING TEST.** A guard that runs after the erase has already destroyed the thing it
/// was protecting, so the assertion is not that the call FAILS -- it is that `erase` never appears
/// in the log. A backend that identified after erasing would fail with the same error and pass a
/// test that only read the return value.
#[test]
fn a_part_that_disagrees_is_refused_with_nothing_erased() {
    let mut backend = Recorder::new(Some(BYTES.to_vec()));
    backend.identity = Err(());
    let error = flash(&mut backend, &image(), VerifyPolicy::ReadBack, &Allow::Any).expect_err("wrong part");
    assert!(matches!(error, FlashError::WrongPart { .. }), "got {error:?}");
    assert_eq!(backend.log, ["identify"], "nothing after identify may have run");
    assert!(!backend.log.contains(&"erase"), "THE ERASE MUST NOT HAVE HAPPENED");
}

/// The base check is cheaper still and runs before the part is even asked -- so a mismatched image
/// costs no probe traffic at all, and again erases nothing.
#[test]
fn an_image_for_another_address_is_refused_before_the_part_is_touched() {
    let mut backend = Recorder::new(Some(BYTES.to_vec()));
    let elsewhere = Image { bytes: &BYTES, base: 0x1000_0000 };
    let error = flash(&mut backend, &elsewhere, VerifyPolicy::ReadBack, &Allow::Any).expect_err("wrong base");
    assert!(matches!(error, FlashError::WrongBase { .. }), "got {error:?}");
    assert!(backend.log.is_empty(), "not even identify should have run: {:?}", backend.log);
}

/// **A MECHANISM THAT CANNOT READ BACK MUST NOT PRODUCE A VERIFIED REPORT.** This is the UF2
/// volume's shape: the file is handed over, the board reboots, the volume unmounts, and there is
/// nothing left to read. Collapsing that into "verify failed" or into "verified" are both lies.
#[test]
fn a_mechanism_that_cannot_read_back_says_so_rather_than_claiming_a_verify() {
    let mut backend = Recorder::new(None);
    let report = flash(&mut backend, &image(), VerifyPolicy::ReadBack, &Allow::Any).expect("a clean write");
    assert!(
        matches!(report.verification, Verification::NotPossible(_)),
        "got {:?}",
        report.verification
    );
    assert_ne!(report.verification, Verification::ReadBack);
    assert_ne!(report.verification, Verification::Skipped, "not the same as being asked to skip");
}

/// Asking to skip is a THIRD state, distinct from a mechanism that cannot. One is a caller's
/// choice about a route that could; the other is a property of the route.
#[test]
fn skipping_is_not_the_same_answer_as_being_unable() {
    let mut backend = Recorder::new(Some(BYTES.to_vec()));
    let report = flash(&mut backend, &image(), VerifyPolicy::Skip, &Allow::Any).expect("a clean write");
    assert_eq!(report.verification, Verification::Skipped);
    assert!(!backend.log.contains(&"read_back"), "a skip must not read: {:?}", backend.log);
    assert_eq!(backend.log, ["identify", "erase", "program", "finish"]);
}

/// And the default must be the safe one. A verify that is optional by default is a verify most
/// callers never run.
#[test]
fn the_default_policy_reads_back() {
    assert_eq!(VerifyPolicy::default(), VerifyPolicy::ReadBack);
}

/// A mismatch names the ADDRESS, not an offset -- the reader is holding a memory map, not an index.
#[test]
fn a_mismatch_reports_the_first_differing_byte_by_address() {
    let mut backend = Recorder::new(Some(vec![0xDE, 0xAD, 0x00, 0xEF]));
    let error = flash(&mut backend, &image(), VerifyPolicy::ReadBack, &Allow::Any).expect_err("byte 2 differs");
    match error {
        FlashError::Verify { address, expected, got } => {
            assert_eq!(address, 0x0800_0002, "the third byte, as an address");
            assert_eq!(expected, 0xBE);
            assert_eq!(got, 0x00);
        }
        other => panic!("wanted a verify failure, got {other:?}"),
    }
}

/// **A SHORT READ IS A BROKEN INSTRUMENT AND A MISMATCH IS A BROKEN WRITE.** Reporting the first as
/// the second sends the reader to the part's flash controller when the fault is in the probe path.
#[test]
fn a_short_read_back_is_not_reported_as_a_failed_write() {
    let mut backend = Recorder::new(Some(vec![0xDE, 0xAD]));
    let error = flash(&mut backend, &image(), VerifyPolicy::ReadBack, &Allow::Any).expect_err("short read");
    match error {
        FlashError::ShortReadBack { wrote, read } => {
            assert_eq!((wrote, read), (4, 2));
        }
        other => panic!("a short read must not read as a verify failure: {other:?}"),
    }
}

/// Every error a backend can raise has to render into something a person can act on, because these
/// strings ARE the tool's output. An empty or `Debug`-shaped message would reach a user.
#[test]
fn every_error_renders_a_sentence_naming_what_to_do_about_it() {
    let cases = [
        FlashError::WrongPart {
            expected: PartIdentity { value: 0x2ba0_1477, what: "the part family" },
            found: 0x0bb1_1477,
        },
        FlashError::WrongBase { stated: 0x1000_0000, expected: 0x0800_0000 },
        FlashError::Verify { address: 0x0800_0002, expected: 0xBE, got: 0x00 },
        FlashError::ShortReadBack { wrote: 4, read: 2 },
    ];
    for case in cases {
        let rendered = case.to_string();
        assert!(rendered.len() > 30, "too terse to act on: {rendered:?}");
        assert!(
            rendered.contains("0x") || rendered.contains("bytes"),
            "an error about addresses or sizes must show them: {rendered:?}"
        );
    }
    let intact = FlashError::WrongPart {
        expected: PartIdentity { value: 0x2ba0_1477, what: "the part family" },
        found: 0x0bb1_1477,
    }
    .to_string();
    assert!(intact.contains("Nothing was erased"), "got {intact}");
}

/// An image knows the span it occupies, so a backend does not compute it and get it wrong.
#[test]
fn an_image_reports_its_own_end_address() {
    assert_eq!(image().end(), 0x0800_0004);
}

/// **THE SHAPE THAT TESTS THE DESIGN RATHER THAN THE CODE: A MECHANISM WITH NO PROBE.** A
/// bootloader volume has nothing to interrogate -- two boards of a family mount as two drives with
/// byte-identical descriptor files -- so it cannot discriminate, and a contract that DEMANDED a
/// discriminating identify would force it to invent one.
///
/// It does not. [`PartIdentity::what`] is where a reading says what it settles, so a mechanism that
/// settles only the family says exactly that, and the honesty is carried in the type rather than
/// left to a comment nobody reads.
#[test]
fn a_mechanism_that_cannot_discriminate_says_what_its_reading_settles() {
    struct Volume {
        log: Vec<&'static str>,
    }
    impl FlashBackend for Volume {
        fn mechanism(&self) -> &'static str {
            "the board's bootloader volume, by copying the image"
        }
        fn flash_base(&self) -> u32 {
            0x1000_0000
        }
        fn identify(&mut self) -> Result<PartIdentity, FlashError> {
            self.log.push("identify");
            Ok(PartIdentity {
                value: 0,
                what: "the bootloader family only -- this route cannot tell two boards apart",
            })
        }
        fn erase(&mut self, _image: &Image<'_>) -> Result<(), FlashError> {
            self.log.push("erase");
            Ok(())
        }
        fn program(&mut self, _image: &Image<'_>) -> Result<(), FlashError> {
            self.log.push("program");
            Ok(())
        }
        fn read_back(&mut self, _image: &Image<'_>) -> Option<Result<Vec<u8>, FlashError>> {
            self.log.push("read_back");
            None
        }
        fn finish(&mut self) -> Result<(), FlashError> {
            self.log.push("finish");
            Ok(())
        }
    }

    let mut volume = Volume { log: Vec::new() };
    let at_xip = Image { bytes: &BYTES, base: 0x1000_0000 };
    let report = flash(&mut volume, &at_xip, VerifyPolicy::ReadBack, &Allow::Any).expect("a clean copy");

    assert!(
        report.identity.what.contains("cannot tell two boards apart"),
        "the identity must state its own limit: {:?}",
        report.identity
    );
    assert!(
        matches!(report.verification, Verification::NotPossible(_)),
        "and the route must not claim a verify: {:?}",
        report.verification
    );
    assert_eq!(volume.log, ["identify", "erase", "program", "read_back", "finish"]);
}

/// Two regions modelled as bytes and written one unit at a time -- a word in the boot region, a
/// page in the window -- that stop taking writes after a set number of operations: the write
/// interrupted at that point.
struct SetBench {
    log: Vec<String>,
    boot: Vec<u8>,
    window: Vec<u8>,
    /// Erase and program operations left before the write is cut off. `None` never cuts it off.
    left: Option<usize>,
    window_identity: Result<u64, ()>,
    /// A window cell that keeps what it held through a program, as a worn or protected one does.
    stuck: Option<usize>,
}

const SET_BOOT: u32 = 0x0800_0000;
const SET_WINDOW: u32 = 0x9000_0000;
/// The model's erase units. Their sizes are the model's; what matters is the order they go in.
const SET_SECTOR: usize = 2048;
const SET_BLOCK: usize = 4096;
const SET_PAGE: usize = 256;
/// Where both halves keep the words a program checks them against each other by.
const STAMP_AT: usize = 0x400;
const STAMP_LEN: usize = 20;

impl SetBench {
    fn holding(old: &(Vec<u8>, Vec<u8>)) -> Self {
        SetBench {
            log: Vec::new(),
            boot: old.0.clone(),
            window: old.1.clone(),
            left: None,
            window_identity: Ok(0x00C2_201A),
            stuck: None,
        }
    }

    /// One more operation, or the cut-off.
    fn tick(&mut self) -> Result<(), FlashError> {
        match &mut self.left {
            Some(0) => Err(FlashError::Refused("the write was cut off".into())),
            Some(left) => {
                *left -= 1;
                Ok(())
            }
            None => Ok(()),
        }
    }
}

/// Erase `[from, from + length)` of `region` in units of `unit`, in address order.
fn set_erase(bench: &mut SetBench, window: bool, from: usize, length: usize, unit: usize) -> Result<(), FlashError> {
    let first = from / unit;
    let last = (from + length).div_ceil(unit);
    for number in first..last {
        bench.tick()?;
        let region = if window { &mut bench.window } else { &mut bench.boot };
        let end = ((number + 1) * unit).min(region.len());
        region[number * unit..end].fill(0xFF);
    }
    Ok(())
}

/// Program `bytes` from `from` in units of `unit`, never across a unit boundary, in address order.
fn set_program(bench: &mut SetBench, window: bool, from: usize, bytes: &[u8], unit: usize) -> Result<(), FlashError> {
    let mut at = from;
    let mut rest = bytes;
    while !rest.is_empty() {
        let take = (unit - at % unit).min(rest.len());
        bench.tick()?;
        let stuck = if window { bench.stuck } else { None };
        let region = if window { &mut bench.window } else { &mut bench.boot };
        for (offset, (cell, byte)) in region[at..at + take].iter_mut().zip(&rest[..take]).enumerate() {
            if stuck != Some(at + offset) {
                *cell &= *byte;
            }
        }
        at += take;
        rest = &rest[take..];
    }
    Ok(())
}

impl FlashBackend for SetBench {
    fn mechanism(&self) -> &'static str {
        "a two-region model"
    }
    fn flash_base(&self) -> u32 {
        SET_BOOT
    }
    fn identify(&mut self) -> Result<PartIdentity, FlashError> {
        self.log.push("identify".into());
        Ok(PartIdentity { value: 0x451, what: "the part family" })
    }
    fn erase(&mut self, image: &Image<'_>) -> Result<(), FlashError> {
        self.log.push("erase boot".into());
        set_erase(self, false, (image.base - SET_BOOT) as usize, image.bytes.len(), SET_SECTOR)
    }
    fn program(&mut self, image: &Image<'_>) -> Result<(), FlashError> {
        let from = (image.base - SET_BOOT) as usize;
        self.log.push(format!("program boot {from:#x}..{:#x}", from + image.bytes.len()));
        set_program(self, false, from, image.bytes, 4)
    }
    fn read_back(&mut self, image: &Image<'_>) -> Option<Result<Vec<u8>, FlashError>> {
        self.log.push("read boot".into());
        let from = (image.base - SET_BOOT) as usize;
        Some(Ok(self.boot[from..from + image.bytes.len()].to_vec()))
    }
    fn finish(&mut self) -> Result<(), FlashError> {
        self.log.push("finish".into());
        Ok(())
    }
}

impl WindowSteps for SetBench {
    fn window_base(&self) -> u32 {
        SET_WINDOW
    }
    fn identify_window(&mut self) -> Result<PartIdentity, FlashError> {
        self.log.push("identify window".into());
        match self.window_identity {
            Ok(value) => Ok(PartIdentity { value, what: "the window's part" }),
            Err(()) => Err(FlashError::Refused("the window's part answered no identity".into())),
        }
    }
    fn erase_window(&mut self, image: &Image<'_>) -> Result<(), FlashError> {
        self.log.push("erase window".into());
        set_erase(self, true, (image.base - SET_WINDOW) as usize, image.bytes.len(), SET_BLOCK)
    }
    fn program_window(&mut self, image: &Image<'_>) -> Result<(), FlashError> {
        let from = (image.base - SET_WINDOW) as usize;
        self.log.push(format!("program window {from:#x}..{:#x}", from + image.bytes.len()));
        set_program(self, true, from, image.bytes, SET_PAGE)
    }
    fn read_back_window(&mut self, image: &Image<'_>) -> Result<Vec<u8>, FlashError> {
        self.log.push("read window".into());
        let from = (image.base - SET_WINDOW) as usize;
        Ok(self.window[from..from + image.bytes.len()].to_vec())
    }
    fn boot_granule(&self) -> u32 {
        4
    }
}

/// A matched set: a boot half and a window half that carry the same stamp at `stamp_at`.
fn matched_set(seed: u8, stamp_at: usize) -> (Vec<u8>, Vec<u8>) {
    let boot: Vec<u8> = (0..6144usize).map(|n| ((n * 7) as u8) ^ seed).collect();
    let window: Vec<u8> = (0..10_000usize).map(|n| ((n * 13) as u8) ^ seed).collect();
    let mut set = (boot, window);
    let stamp = [seed; STAMP_LEN];
    set.0[stamp_at..stamp_at + STAMP_LEN].copy_from_slice(&stamp);
    set.1[stamp_at..stamp_at + STAMP_LEN].copy_from_slice(&stamp);
    set.0[..8].copy_from_slice(&[0x00, 0x10, 0x02, 0x20, 0x01 ^ seed, 0x02, 0x00, 0x08]);
    set
}

/// What a part does at power-up with the regions as a write left them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Boots {
    /// The reset vector is erased: the core cannot start, so nothing runs.
    Dead,
    /// The boot image starts and finds the window's stamp is not its own, and refuses.
    Refused,
    /// The boot image starts, the stamps agree, and both halves are one whole set.
    Runs(&'static str),
    /// The boot image starts, the stamps agree, and the halves are not one set: the failure the
    /// order exists to make impossible.
    Mixed,
}

fn boots(bench: &SetBench, stamp_at: usize, sets: &[(&'static str, &(Vec<u8>, Vec<u8>))]) -> Boots {
    if bench.boot[4..8] == [0xFF; 4] {
        return Boots::Dead;
    }
    if bench.boot[stamp_at..stamp_at + STAMP_LEN] != bench.window[stamp_at..stamp_at + STAMP_LEN] {
        return Boots::Refused;
    }
    for (name, set) in sets {
        if bench.boot == set.0 && bench.window == set.1 {
            return Boots::Runs(name);
        }
    }
    Boots::Mixed
}

/// Write `new` over a board holding `old`, cut off after `left` operations (or never).
fn interrupted(old: &(Vec<u8>, Vec<u8>), new: &(Vec<u8>, Vec<u8>), left: Option<usize>) -> (SetBench, Result<SetReport, FlashError>) {
    let mut bench = SetBench::holding(old);
    bench.left = left;
    let boot = Image { bytes: &new.0, base: SET_BOOT };
    let window = Image { bytes: &new.1, base: SET_WINDOW };
    let result = flash_set(&mut bench, &boot, &window, VerifyPolicy::ReadBack, &Allow::Any);
    (bench, result)
}

/// **THE ORDER'S WHOLE CLAIM, TRIED AT EVERY POINT A WRITE CAN STOP.** Whatever operation the write
/// is cut off after, the board either cannot start, starts and refuses its window, or runs one
/// whole set -- never a boot image whose stamp agrees with a window it was not written with.
#[test]
fn a_matched_set_cut_off_anywhere_never_boots_mixed() {
    let old = matched_set(0x11, STAMP_AT);
    let new = matched_set(0x22, STAMP_AT);
    let (complete, result) = interrupted(&old, &new, None);
    result.expect("the uncut write");
    let total = {
        let mut counter = SetBench::holding(&old);
        counter.left = Some(usize::MAX);
        let boot = Image { bytes: &new.0, base: SET_BOOT };
        let window = Image { bytes: &new.1, base: SET_WINDOW };
        flash_set(&mut counter, &boot, &window, VerifyPolicy::ReadBack, &Allow::Any).expect("counted");
        usize::MAX - counter.left.expect("counting")
    };
    assert_eq!(boots(&complete, STAMP_AT, &[("new", &new)]), Boots::Runs("new"));
    let mut seen = std::collections::BTreeSet::new();
    for left in 0..total {
        let (bench, result) = interrupted(&old, &new, Some(left));
        assert!(result.is_err(), "cut off after {left} of {total} operations, the write reports it");
        let state = boots(&bench, STAMP_AT, &[("old", &old), ("new", &new)]);
        assert_ne!(state, Boots::Mixed, "cut off after {left} of {total} operations: {:?}", bench.log);
        seen.insert(format!("{state:?}"));
    }
    assert_eq!(seen.into_iter().collect::<Vec<_>>(), ["Dead", "Runs(\"old\")"]);
}

/// The control: the model this walk judges by does see a mixed image -- the new boot image with
/// its stamp agreeing over a window whose other bytes are still erased, the state an order that
/// wrote each region's head last left when it was cut off inside the window's head.
#[test]
fn the_boot_model_sees_a_mixed_image() {
    let old = matched_set(0x11, STAMP_AT);
    let new = matched_set(0x22, STAMP_AT);
    let mut bench = SetBench::holding(&new);
    bench.window.fill(0xFF);
    bench.window[STAMP_AT..STAMP_AT + STAMP_LEN].copy_from_slice(&new.1[STAMP_AT..STAMP_AT + STAMP_LEN]);
    assert_eq!(boots(&bench, STAMP_AT, &[("old", &old), ("new", &new)]), Boots::Mixed);
}

/// The order itself: both identities before either erase, the boot region erased before the window
/// is touched, its vector the very last write, and both regions read back before the release.
#[test]
fn a_matched_set_runs_in_its_order() {
    let new = matched_set(0x22, STAMP_AT);
    let (bench, result) = interrupted(&matched_set(0x11, STAMP_AT), &new, None);
    let report = result.expect("a clean write");
    assert_eq!(
        bench.log,
        [
            "identify",
            "identify window",
            "erase boot",
            "erase window",
            "program window 0x0..0x2710",
            "program boot 0x8..0x1800",
            "program boot 0x0..0x8",
            "read boot",
            "read window",
            "finish",
        ]
    );
    assert_eq!((report.boot.base, report.window.base), (SET_BOOT, SET_WINDOW));
    assert_eq!((report.boot.bytes, report.window.bytes), (6144, 10_000));
    assert_eq!(report.window.verification, Verification::ReadBack);
}

/// A window part that does not answer stops the write before the boot region is touched.
#[test]
fn an_unidentified_window_erases_nothing() {
    let old = matched_set(0x11, STAMP_AT);
    let new = matched_set(0x22, STAMP_AT);
    let mut bench = SetBench::holding(&old);
    bench.window_identity = Err(());
    let boot = Image { bytes: &new.0, base: SET_BOOT };
    let window = Image { bytes: &new.1, base: SET_WINDOW };
    assert!(flash_set(&mut bench, &boot, &window, VerifyPolicy::ReadBack, &Allow::Any).is_err());
    assert_eq!(bench.log, ["identify", "identify window"]);
    assert_eq!((bench.boot.as_slice(), bench.window.as_slice()), (old.0.as_slice(), old.1.as_slice()));
}

/// A window half stated somewhere other than the window is refused before anything is asked of
/// the part.
#[test]
fn a_window_half_at_the_wrong_base_is_refused_first() {
    let new = matched_set(0x22, STAMP_AT);
    let mut bench = SetBench::holding(&matched_set(0x11, STAMP_AT));
    let boot = Image { bytes: &new.0, base: SET_BOOT };
    let window = Image { bytes: &new.1, base: SET_WINDOW + 0x1_0000 };
    let refusal = flash_set(&mut bench, &boot, &window, VerifyPolicy::ReadBack, &Allow::Any);
    assert!(matches!(refusal, Err(FlashError::WrongBase { stated, expected }) if stated == SET_WINDOW + 0x1_0000 && expected == SET_WINDOW));
    assert!(bench.log.is_empty(), "{:?}", bench.log);
}

/// A window that reads back wrong fails the set, naming the window's address, and the part is not
/// released as though it had been written.
#[test]
fn a_window_that_reads_back_wrong_fails_the_set() {
    let new = matched_set(0x22, STAMP_AT);
    let mut bench = SetBench::holding(&matched_set(0x11, STAMP_AT));
    bench.stuck = Some(0x1234);
    let boot = Image { bytes: &new.0, base: SET_BOOT };
    let window = Image { bytes: &new.1, base: SET_WINDOW };
    let result = flash_set(&mut bench, &boot, &window, VerifyPolicy::ReadBack, &Allow::Any);
    assert!(
        matches!(result, Err(FlashError::Verify { address, .. }) if address == SET_WINDOW + 0x1234),
        "{result:?}"
    );
    assert!(!bench.log.iter().any(|step| step == "finish"), "{:?}", bench.log);
}
