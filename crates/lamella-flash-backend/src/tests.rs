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
