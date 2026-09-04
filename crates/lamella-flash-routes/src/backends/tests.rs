//! The backend driven against a fake target, so the sequence is checked without a board.

use super::*;
use lamella_flash_backend::{Allow, Verification, VerifyPolicy, flash};
use lamella_probe_core::ProbeError;

/// A target with a byte-addressable flash array and a log of what was asked of it.
struct FakeTarget {
    log: Vec<&'static str>,
    idcode: u32,
    flash: Vec<u8>,
    /// Set to corrupt the read-back, so a verify failure can be exercised without a broken board.
    corrupt_at: Option<usize>,
}

impl FakeTarget {
    fn new(idcode: u32) -> Self {
        FakeTarget {
            log: Vec::new(),
            idcode,
            flash: vec![0xFF; 256],
            corrupt_at: None,
        }
    }
}

impl TargetAccess for FakeTarget {
    fn connect(&mut self) -> Result<(), ProbeError> {
        self.log.push("connect");
        Ok(())
    }
    fn read_idcode(&mut self) -> Result<u32, ProbeError> {
        self.log.push("read_idcode");
        Ok(self.idcode)
    }
    fn init_mem(&mut self) -> Result<(), ProbeError> {
        self.log.push("init_mem");
        Ok(())
    }
    fn read_word(&mut self, address: u32) -> Result<u32, ProbeError> {
        if address >= NVMC_BASE {
            return Ok(1);
        }
        let at = address as usize;
        let mut word = [0u8; 4];
        word.copy_from_slice(
            self.flash
                .get(at..at + 4)
                .ok_or(ProbeError::Device("oob"))?,
        );
        Ok(u32::from_le_bytes(word))
    }
    fn write_word(&mut self, address: u32, value: u32) -> Result<(), ProbeError> {
        if address == NVMC_ERASEALL && value == 1 {
            self.log.push("erase_all");
            self.flash.iter_mut().for_each(|byte| *byte = 0xFF);
            return Ok(());
        }
        if address >= NVMC_BASE {
            return Ok(());
        }
        let at = address as usize;
        self.flash
            .get_mut(at..at + 4)
            .ok_or(ProbeError::Device("write past the modelled flash"))?
            .copy_from_slice(&value.to_le_bytes());
        Ok(())
    }
    fn read_words_into(&mut self, address: u32, out: &mut [u32]) -> Result<(), ProbeError> {
        self.log.push("read_words_into");
        for (index, slot) in out.iter_mut().enumerate() {
            *slot = self.read_word(address + (index * 4) as u32)?;
        }
        if let Some(at) = self.corrupt_at {
            if let Some(slot) = out.get_mut(at) {
                *slot ^= 0x0000_FF00;
            }
        }
        Ok(())
    }
    fn halt(&mut self) -> Result<(), ProbeError> {
        self.log.push("halt");
        Ok(())
    }
    fn reset_and_run(&mut self) -> Result<(), ProbeError> {
        self.log.push("reset_and_run");
        Ok(())
    }

    /// **PROGRAMMING AN nRF *IS* A RAW MEMORY WRITE**, with the controller left in write-enable
    /// mode: the NVMC gates the writes rather than receiving the data, so this seam is part of the
    /// flashing path and the ones below it are not.
    fn write_words(&mut self, address: u32, words: &[u32]) -> Result<(), ProbeError> {
        self.log.push("write_words");
        for (index, word) in words.iter().enumerate() {
            self.write_word(address + (index * 4) as u32, *word)?;
        }
        Ok(())
    }
    fn read_byte(&mut self, _address: u32) -> Result<u8, ProbeError> {
        unreachable!("byte access is not part of the flashing path")
    }
    fn write_byte(&mut self, _address: u32, _value: u8) -> Result<(), ProbeError> {
        unreachable!("byte access is not part of the flashing path")
    }
    fn read_halfword(&mut self, _address: u32) -> Result<u16, ProbeError> {
        unreachable!("halfword access is not part of the flashing path")
    }
    fn write_halfword(&mut self, _address: u32, _value: u16) -> Result<(), ProbeError> {
        unreachable!("halfword access is not part of the flashing path")
    }
    fn resume(&mut self) -> Result<(), ProbeError> {
        unreachable!("a flashing backend leaves the part through reset_and_run, not resume")
    }
    fn step(&mut self) -> Result<(), ProbeError> {
        unreachable!("run control is not the flashing path")
    }
    fn is_halted(&mut self) -> Result<bool, ProbeError> {
        unreachable!("run control is not the flashing path")
    }
    fn wait_halted(&mut self) -> Result<(), ProbeError> {
        unreachable!("run control is not the flashing path")
    }
    fn reset_and_halt(&mut self) -> Result<(), ProbeError> {
        unreachable!("run control is not the flashing path")
    }
    fn set_reset(&mut self, _assert: bool) -> Result<u8, ProbeError> {
        unreachable!("a flashing backend does not drive the reset line directly")
    }
    fn read_core_reg(&mut self, _selector: u8) -> Result<u32, ProbeError> {
        unreachable!("core registers are not the flashing path")
    }
    fn write_core_reg(&mut self, _selector: u8, _value: u32) -> Result<(), ProbeError> {
        unreachable!("core registers are not the flashing path")
    }
    fn arm_reset_catch(&mut self) -> Result<(), ProbeError> {
        unreachable!("reset catch is not the flashing path")
    }
    fn disarm_reset_catch(&mut self) -> Result<(), ProbeError> {
        unreachable!("reset catch is not the flashing path")
    }
    fn set_breakpoint(&mut self, _address: u32) -> Result<(), ProbeError> {
        unreachable!("breakpoints are not the flashing path")
    }
    fn clear_breakpoint(&mut self) -> Result<(), ProbeError> {
        unreachable!("breakpoints are not the flashing path")
    }
    fn set_breakpoints(&mut self, _addresses: &[u32]) -> Result<(), ProbeError> {
        unreachable!("breakpoints are not the flashing path")
    }
    fn call_target(
        &mut self,
        _address: u32,
        _args: &[u32],
        _frame: &lamella_probe_core::CallFrame,
    ) -> Result<u32, ProbeError> {
        unreachable!("a flashing backend does not run code on the target")
    }
}

/// The nRF Non-Volatile Memory Controller's register block. Everything at or above it is the
/// CONTROLLER; everything below is the flash array.
const NVMC_BASE: u32 = 0x4001_e400;
/// Writing 1 here erases the whole main flash block.
const NVMC_ERASEALL: u32 = 0x4001_e50c;

const NRF51: u32 = 0x0bb1_1477;
const NRF52: u32 = 0x2ba0_1477;
const PROGRAM: [u8; 6] = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06];

fn image() -> Image<'static> {
    Image {
        bytes: &PROGRAM,
        base: 0,
    }
}

/// **THE VALIDATION THE CONTRACT WAS BUILT FOR: A REAL PART'S STEPS COMPOSE INTO THE ORDER.** A
/// contract proved only against a mock backend is a contract proved against itself.
#[test]
fn a_real_parts_primitives_compose_into_the_contracts_order() {
    let mut backend = MicrobitDaplink::new(FakeTarget::new(NRF51), NRF51, "the part family");
    let report =
        flash(&mut backend, &image(), VerifyPolicy::ReadBack, &Allow::Any).expect("a clean write");
    assert_eq!(report.verification, Verification::ReadBack);
    assert_eq!(report.bytes, 6);
    assert_eq!(
        backend.target.log,
        [
            "connect",
            "read_idcode",
            "init_mem",
            "halt",
            "erase_all",
            "write_words",
            "read_words_into",
            "reset_and_run",
        ],
        "identify reads before init_mem/halt, and the read-back precedes the reset"
    );
}

/// **THE ONE THAT MATTERS ON REAL HARDWARE.** Pointing a v2 image at a v1 board must stop at a
/// message, and the assertion is on the LOG: `halt` and the erase must never have run.
#[test]
fn a_v2_image_aimed_at_a_v1_board_erases_nothing() {
    let mut backend = MicrobitDaplink::new(FakeTarget::new(NRF51), NRF52, "the part family");
    let error =
        flash(&mut backend, &image(), VerifyPolicy::ReadBack, &Allow::Any).expect_err("wrong part");
    assert!(
        matches!(error, FlashError::WrongPart { .. }),
        "got {error:?}"
    );
    assert_eq!(backend.target.log, ["connect", "read_idcode"]);
    assert!(
        !backend.target.log.contains(&"halt"),
        "the core was halted before the refusal"
    );
    assert!(
        backend.target.flash.iter().all(|byte| *byte == 0xFF),
        "THE FLASH WAS TOUCHED DESPITE THE REFUSAL"
    );
}

/// A write that does not stick is caught by the read-back and named by address.
#[test]
fn a_byte_that_does_not_stick_is_caught_and_located() {
    let mut target = FakeTarget::new(NRF51);
    target.corrupt_at = Some(1);
    let mut backend = MicrobitDaplink::new(target, NRF51, "the part family");
    let error =
        flash(&mut backend, &image(), VerifyPolicy::ReadBack, &Allow::Any).expect_err("a bad byte");
    match error {
        FlashError::Verify { address, .. } => {
            assert_eq!(
                address, 5,
                "the corrupted bit is in the second word's high byte"
            );
        }
        other => panic!("wanted a verify failure, got {other:?}"),
    }
}

/// **A SIX-BYTE IMAGE IS NOT A SIX-BYTE READ UNLESS SOMETHING TRIMS IT.** The memory interface
/// deals in words, so a span that is not a multiple of four reads back long -- and the contract
/// refuses a length mismatch, correctly, as an uncomparable result. The trim is what makes an
/// odd-length program flashable at all.
#[test]
fn an_image_whose_length_is_not_a_multiple_of_four_still_verifies() {
    for length in 1..=8usize {
        let bytes: Vec<u8> = (0..length).map(|n| n as u8 + 1).collect();
        let mut backend = MicrobitDaplink::new(FakeTarget::new(NRF51), NRF51, "the part family");
        let image = Image {
            bytes: &bytes,
            base: 0,
        };
        let report = flash(&mut backend, &image, VerifyPolicy::ReadBack, &Allow::Any)
            .unwrap_or_else(|why| panic!("{length} bytes did not verify: {why}"));
        assert_eq!(report.bytes, length);
        assert_eq!(report.verification, Verification::ReadBack);
    }
}

/// The padding a partial trailing word gets must match what is written, or the last word of every
/// odd-length image would fail its own verify.
#[test]
fn a_partial_trailing_word_is_zero_padded_the_way_the_part_crate_pads_it() {
    assert_eq!(to_words(&[0x01]), vec![0x0000_0001]);
    assert_eq!(
        to_words(&[0x01, 0x02, 0x03, 0x04, 0x05]),
        vec![0x0403_0201, 0x0000_0005]
    );
}


/// The L0 flash controller's register block (RM0377 and RM0367, base `0x40022000`).
const L0_BASE: u32 = 0x4002_2000;
const L0_PECR: u32 = L0_BASE + 0x04;
const L0_PEKEYR: u32 = L0_BASE + 0x0c;
const L0_PRGKEYR: u32 = L0_BASE + 0x10;
const L0_SR: u32 = L0_BASE + 0x18;
const L0_DBGMCU: u32 = 0x4001_5800;
const L0_FSIZE: u32 = 0x1FF8_007C;
const L0_FLASH: u32 = 0x0800_0000;

/// `DEV_ID` 0x447: the category 5 parts, the L073 and L083.
const L0_CAT5: u32 = 0x447;
/// An STM32F0's `DEV_ID`, which answers at the SAME address and is not an L0 at all.
const F0_DEV_ID: u32 = 0x440;

/// An STM32L0 modelled as its CONTROLLER and its ARRAY, because the two together are what the
/// backend actually drives.
///
/// **THE ARRAY STARTS AT ZERO, NOT 0xFF.** That is what erased means on this family, and a fake
/// initialised the other way would make the erase look like it did nothing and the not-zero rule
/// fire on every word.
struct FakeL0 {
    log: Vec<&'static str>,
    dev_id: u32,
    flash_kb: u32,
    flash: Vec<u8>,
    pelock: bool,
    prglock: bool,
    pecr: u32,
    sr: u32,
    keys: Vec<u32>,
    corrupt_at: Option<usize>,
}

impl FakeL0 {
    fn new() -> Self {
        FakeL0 {
            log: Vec::new(),
            dev_id: L0_CAT5,
            flash_kb: 192,
            flash: vec![0x00; 1024],
            pelock: true,
            prglock: true,
            pecr: 0x0000_0007,
            sr: 0x0000_000C,
            keys: Vec::new(),
            corrupt_at: None,
        }
    }

    /// Whether any byte of the modelled array has been written or erased away from its initial
    /// state -- the check that a refusal really happened BEFORE anything was destroyed.
    fn array_untouched(&self, original: &[u8]) -> bool {
        self.flash == original
    }
}

impl TargetAccess for FakeL0 {
    fn connect(&mut self) -> Result<(), ProbeError> {
        Ok(())
    }
    fn read_idcode(&mut self) -> Result<u32, ProbeError> {
        Ok(0x0bc1_1477)
    }
    fn init_mem(&mut self) -> Result<(), ProbeError> {
        Ok(())
    }

    fn read_word(&mut self, address: u32) -> Result<u32, ProbeError> {
        match address {
            L0_DBGMCU => Ok((0x2000 << 16) | 0x6000 | self.dev_id),
            L0_FSIZE => Ok(self.flash_kb),
            L0_PECR => {
                let mut pecr = self.pecr & !0b11;
                if self.pelock {
                    pecr |= 1;
                }
                if self.prglock {
                    pecr |= 2;
                }
                Ok(pecr)
            }
            L0_SR => Ok(self.sr),
            _ if address >= L0_FLASH => {
                let at = (address - L0_FLASH) as usize;
                let mut word = [0u8; 4];
                word.copy_from_slice(
                    self.flash
                        .get(at..at + 4)
                        .ok_or(ProbeError::Device("read past the array"))?,
                );
                Ok(u32::from_le_bytes(word))
            }
            _ => Err(ProbeError::Device(
                "read of an address this fake does not model",
            )),
        }
    }

    fn write_word(&mut self, address: u32, value: u32) -> Result<(), ProbeError> {
        match address {
            L0_PEKEYR | L0_PRGKEYR => {
                self.keys.push(value);
                if self.keys.len() == 2 {
                    let pair = (self.keys[0], self.keys[1]);
                    if address == L0_PEKEYR && pair == (0x89ab_cdef, 0x0203_0405) {
                        self.pelock = false;
                        self.log.push("unlock_pecr");
                    }
                    if address == L0_PRGKEYR && pair == (0x8c9d_aebf, 0x1314_1516) {
                        self.prglock = false;
                        self.log.push("unlock_program");
                    }
                    self.keys.clear();
                }
                Ok(())
            }
            L0_PECR => {
                self.pecr = value;
                if value & 1 != 0 {
                    self.pelock = true;
                    self.prglock = true;
                    self.log.push("lock");
                }
                Ok(())
            }
            L0_SR => {
                self.sr &= !value;
                Ok(())
            }
            _ if address >= L0_FLASH => {
                if self.pelock || self.prglock {
                    return Err(ProbeError::Device("a write to a locked controller"));
                }
                let at = (address - L0_FLASH) as usize;
                if self.pecr & (1 << 9) != 0 && self.pecr & (1 << 3) != 0 {
                    let page = at & !(128 - 1);
                    let end = (page + 128).min(self.flash.len());
                    self.flash
                        .get_mut(page..end)
                        .ok_or(ProbeError::Device("erase past the array"))?
                        .iter_mut()
                        .for_each(|byte| *byte = 0x00);
                    self.log.push("erase_page");
                    return Ok(());
                }
                let mut current = [0u8; 4];
                current.copy_from_slice(
                    self.flash
                        .get(at..at + 4)
                        .ok_or(ProbeError::Device("write past the array"))?,
                );
                if u32::from_le_bytes(current) != 0 {
                    self.sr |= 1 << 16;
                    self.log.push("notzeroerr");
                    return Ok(());
                }
                self.flash[at..at + 4].copy_from_slice(&value.to_le_bytes());
                self.log.push("program_word");
                Ok(())
            }
            _ => Err(ProbeError::Device(
                "write to an address this fake does not model",
            )),
        }
    }

    fn read_words_into(&mut self, address: u32, out: &mut [u32]) -> Result<(), ProbeError> {
        for (index, slot) in out.iter_mut().enumerate() {
            *slot = self.read_word(address + (index * 4) as u32)?;
        }
        if let Some(at) = self.corrupt_at {
            if let Some(slot) = out.get_mut(at) {
                *slot ^= 0x0000_FF00;
            }
        }
        Ok(())
    }

    fn write_words(&mut self, address: u32, words: &[u32]) -> Result<(), ProbeError> {
        for (index, word) in words.iter().enumerate() {
            self.write_word(address + (index * 4) as u32, *word)?;
        }
        Ok(())
    }

    fn halt(&mut self) -> Result<(), ProbeError> {
        self.log.push("halt");
        Ok(())
    }
    fn reset_and_run(&mut self) -> Result<(), ProbeError> {
        self.log.push("reset_and_run");
        Ok(())
    }

    fn read_byte(&mut self, _address: u32) -> Result<u8, ProbeError> {
        unreachable!("byte access is not part of the flashing path")
    }
    fn write_byte(&mut self, _address: u32, _value: u8) -> Result<(), ProbeError> {
        unreachable!("byte access is not part of the flashing path")
    }
    fn read_halfword(&mut self, _address: u32) -> Result<u16, ProbeError> {
        unreachable!("halfword access is not part of the flashing path")
    }
    fn write_halfword(&mut self, _address: u32, _value: u16) -> Result<(), ProbeError> {
        unreachable!("halfword access is not part of the flashing path")
    }
    fn resume(&mut self) -> Result<(), ProbeError> {
        unreachable!("a flashing backend leaves the part through reset_and_run, not resume")
    }
    fn step(&mut self) -> Result<(), ProbeError> {
        unreachable!("run control is not the flashing path")
    }
    fn is_halted(&mut self) -> Result<bool, ProbeError> {
        unreachable!("run control is not the flashing path")
    }
    fn wait_halted(&mut self) -> Result<(), ProbeError> {
        unreachable!("run control is not the flashing path")
    }
    fn reset_and_halt(&mut self) -> Result<(), ProbeError> {
        unreachable!("run control is not the flashing path")
    }
    fn set_reset(&mut self, _assert: bool) -> Result<u8, ProbeError> {
        unreachable!("a flashing backend does not drive the reset line directly")
    }
    fn read_core_reg(&mut self, _selector: u8) -> Result<u32, ProbeError> {
        unreachable!("core registers are not the flashing path")
    }
    fn write_core_reg(&mut self, _selector: u8, _value: u32) -> Result<(), ProbeError> {
        unreachable!("core registers are not the flashing path")
    }
    fn arm_reset_catch(&mut self) -> Result<(), ProbeError> {
        unreachable!("reset catch is not the flashing path")
    }
    fn disarm_reset_catch(&mut self) -> Result<(), ProbeError> {
        unreachable!("reset catch is not the flashing path")
    }
    fn set_breakpoint(&mut self, _address: u32) -> Result<(), ProbeError> {
        unreachable!("breakpoints are not the flashing path")
    }
    fn clear_breakpoint(&mut self) -> Result<(), ProbeError> {
        unreachable!("breakpoints are not the flashing path")
    }
    fn set_breakpoints(&mut self, _addresses: &[u32]) -> Result<(), ProbeError> {
        unreachable!("breakpoints are not the flashing path")
    }
    fn call_target(
        &mut self,
        _address: u32,
        _args: &[u32],
        _frame: &lamella_probe_core::CallFrame,
    ) -> Result<u32, ProbeError> {
        unreachable!("a flashing backend does not run code on the target")
    }
}

/// Six bytes that are all non-zero, so nothing in them can be mistaken for a skipped word.
const L0_PROGRAM: [u8; 6] = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66];

fn l0_image() -> Image<'static> {
    Image {
        bytes: &L0_PROGRAM,
        base: L0_FLASH,
    }
}

/// The same validation the nRF backend gets: a REAL part's primitives, composed by the contract.
#[test]
fn the_l0_primitives_compose_into_the_contracts_order() {
    let mut backend = StProbe::new(FakeL0::new(), crate::StFamily::L0.plan());
    let report = flash(
        &mut backend,
        &l0_image(),
        VerifyPolicy::ReadBack,
        &Allow::Any,
    )
    .expect("the L0 sequence");

    assert_eq!(report.verification, Verification::ReadBack);
    assert_eq!(report.base, L0_FLASH);
    assert_eq!(report.bytes, L0_PROGRAM.len());

    let log = &backend.target.log;
    let halt = log
        .iter()
        .position(|step| *step == "halt")
        .expect("the core is halted");
    let erase = log
        .iter()
        .position(|step| *step == "erase_page")
        .expect("a page is erased");
    let program = log
        .iter()
        .position(|step| *step == "program_word")
        .expect("a word is written");
    let run = log
        .iter()
        .position(|step| *step == "reset_and_run")
        .expect("the part is released");
    assert!(
        halt < erase,
        "the erase must not run on a core that is still fetching: {log:?}"
    );
    assert!(
        erase < program,
        "a program before its erase is the defect this part punishes"
    );
    assert!(
        program < run,
        "the part is released only after it has been written"
    );
    assert!(
        !log.contains(&"notzeroerr"),
        "no word was written over unerased flash: {log:?}"
    );
}

/// **THE FAMILY'S WHOLE CHARACTER, ASSERTED RATHER THAN ASSUMED.** An erase here leaves ZERO. A
/// backend carrying a ones-erasing assumption over from any other part in this module would leave
/// the tail of the page holding `0xFF`, and this is the test that would notice.
#[test]
fn an_erase_on_this_family_leaves_zero_and_the_untouched_tail_stays_erased() {
    let mut backend = StProbe::new(FakeL0::new(), crate::StFamily::L0.plan());
    backend
        .target
        .flash
        .iter_mut()
        .for_each(|byte| *byte = 0x5a);
    flash(
        &mut backend,
        &l0_image(),
        VerifyPolicy::ReadBack,
        &Allow::Any,
    )
    .expect("the L0 sequence");

    let page = &backend.target.flash[..128];
    assert_eq!(&page[..L0_PROGRAM.len()], &L0_PROGRAM, "the image landed");
    assert!(
        page[L0_PROGRAM.len()..].iter().all(|byte| *byte == 0x00),
        "the rest of the erased page must read ZERO on this family, not 0xff: {:02x?}",
        &page[L0_PROGRAM.len()..16]
    );
}

/// A short tail is padded with zero, and zero is what an erased cell already holds -- so the
/// padding is never programmed. On a ones-erasing part the same line would write filler.
#[test]
fn a_short_tail_is_left_erased_rather_than_programmed_with_padding() {
    for length in 1..=8usize {
        let bytes: Vec<u8> = (0..length).map(|n| n as u8 + 1).collect();
        let mut backend = StProbe::new(FakeL0::new(), crate::StFamily::L0.plan());
        let image = Image {
            bytes: &bytes,
            base: L0_FLASH,
        };
        let report = flash(&mut backend, &image, VerifyPolicy::ReadBack, &Allow::Any)
            .unwrap_or_else(|why| panic!("{length} bytes did not verify: {why}"));
        assert_eq!(report.bytes, length);
        assert_eq!(report.verification, Verification::ReadBack);
        assert_eq!(&backend.target.flash[..length], bytes.as_slice());
    }
}

/// **THE REFUSAL HAS TO LAND BEFORE THE ERASE, AND THE ARRAY IS THE WITNESS.** A guard that fires
/// after the erase has already destroyed what it was protecting, so this checks the flash rather
/// than the error.
#[test]
fn a_foreign_dev_id_is_refused_before_anything_is_erased() {
    let mut backend = StProbe::new(FakeL0::new(), crate::StFamily::L0.plan());
    backend.target.dev_id = F0_DEV_ID;
    let original = backend.target.flash.clone();

    let error = flash(
        &mut backend,
        &l0_image(),
        VerifyPolicy::ReadBack,
        &Allow::Any,
    )
    .expect_err("an F0 is not an L0");
    assert!(
        format!("{error}").contains("0x440"),
        "the error names what it read: {error}"
    );
    assert!(
        backend.target.array_untouched(&original),
        "the array was touched before the refusal"
    );
    assert!(
        !backend.target.log.contains(&"erase_page"),
        "{:?}",
        backend.target.log
    );
}

/// Likewise for an image the part has no room for: the part is asked how big it is, and the
/// refusal precedes the first page erase rather than arriving partway through the array.
#[test]
fn an_image_larger_than_the_fitted_flash_is_refused_before_anything_is_erased() {
    let big = vec![0x11u8; 4096];
    let mut backend = StProbe::new(FakeL0::new(), crate::StFamily::L0.plan());
    backend.target.flash_kb = 2;
    let original = backend.target.flash.clone();

    let error = flash(
        &mut backend,
        &Image {
            bytes: &big,
            base: L0_FLASH,
        },
        VerifyPolicy::ReadBack,
        &Allow::Any,
    )
    .expect_err("4096 bytes do not fit in 2 KB");
    assert!(
        format!("{error}").contains("2 KB"),
        "the error names the part's own answer: {error}"
    );
    assert!(backend.target.array_untouched(&original));
    assert!(
        !backend.target.log.contains(&"erase_page"),
        "{:?}",
        backend.target.log
    );
}

/// And an image whose LENGTH fits while its PAGES do not: the bound is on where the walk reaches,
/// not on how many bytes the image holds, so a based image that would erase past the end of the
/// array is refused before the halt.
///
#[test]
fn an_image_whose_pages_leave_the_array_is_refused_even_though_its_length_fits() {
    let exactly_the_array = vec![0x22u8; 2048];
    let mut backend = StProbe::new(FakeL0::new(), crate::StFamily::L0.plan());
    backend.target.flash_kb = 2;
    let original = backend.target.flash.clone();

    let error = backend
        .erase(&Image {
            bytes: &exactly_the_array,
            base: L0_FLASH + STM32L0_PAGE,
        })
        .expect_err("the walk runs one page past the end of a 2 KB array");
    let text = format!("{error}");
    assert!(
        text.contains("2 KB"),
        "the error names the part's own answer: {text}"
    );
    assert!(
        text.contains("0x08000800"),
        "the error names where the array ends: {text}"
    );
    assert!(backend.target.array_untouched(&original));
    assert!(
        !backend.target.log.contains(&"halt"),
        "halted before the refusal: {:?}",
        backend.target.log
    );
    assert!(
        !backend.target.log.contains(&"erase_page"),
        "{:?}",
        backend.target.log
    );
}

/// The identity names a CATEGORY and says so, which is the contract's sixth prohibition met by
/// disclosure rather than by pretending to more.
#[test]
fn the_identity_names_a_category_and_says_it_is_not_a_board() {
    let mut backend = StProbe::new(FakeL0::new(), crate::StFamily::L0.plan());
    let identity = backend.identify().expect("the part answers");
    assert_eq!(identity.value, u64::from(L0_CAT5));
    assert!(identity.what.contains("category 5"), "{}", identity.what);
    assert!(
        identity.what.contains("not this board"),
        "{}",
        identity.what
    );
    assert_ne!(identity.value, 0x0bc1_1477);
}

/// `Allow` can pin the category, and a part outside it is refused between identify and erase.
#[test]
fn a_permission_naming_another_category_refuses_before_the_erase() {
    let mut backend = StProbe::new(FakeL0::new(), crate::StFamily::L0.plan());
    let original = backend.target.flash.clone();
    let only_category_one = Allow::Identities(vec![0x457]);

    flash(
        &mut backend,
        &l0_image(),
        VerifyPolicy::ReadBack,
        &only_category_one,
    )
    .expect_err("a category 5 part is not permitted here");
    assert!(backend.target.array_untouched(&original));
    assert!(
        !backend.target.log.contains(&"erase_page"),
        "{:?}",
        backend.target.log
    );
}

/// The read-back is real: corrupt what comes off the wire and the contract reports a verify
/// failure rather than success.
#[test]
fn the_read_back_is_used_and_a_bad_one_fails_the_flash() {
    let mut backend = StProbe::new(FakeL0::new(), crate::StFamily::L0.plan());
    backend.target.corrupt_at = Some(0);
    let error = flash(
        &mut backend,
        &l0_image(),
        VerifyPolicy::ReadBack,
        &Allow::Any,
    )
    .expect_err("a corrupted read-back must not report success");
    assert!(
        matches!(error, FlashError::Verify { address, .. } if address == L0_FLASH + 1),
        "a corrupted word must be reported as a verify failure at its own address: {error}"
    );
}


use lamella_cmsis_dap_stm32::{
    STM32H7_BANK2_BASE, STM32H7_DBGMCU_IDC, STM32H7_FLASH_BASE, STM32H7_FLASH_SIZE_REG,
    STM32H7_FLASH_WORD, STM32H7_SECTOR, STM32L0_DBGMCU_IDCODE, STM32L0_ERASED_WORD,
    STM32L0_FLASH_BASE, STM32L0_FLASH_SIZE_REG, STM32L0_PAGE, STM32U5_DBGMCU_IDCODE,
    STM32U5_FLASH_BASE, STM32U5_FLASH_SIZE_REG, STM32U5_PAGE, STM32U5_QUAD_WORD,
};

/// Every number in every plan is the part crate's, checked field by field.
///
/// **THE POINT IS THE DIRECTION OF THE CHECK.** A plan is the one place in this crate where a pile
/// of register addresses sits next to each other, which is exactly where a value gets typed in
/// rather than referenced -- and a hard-coded address that happens to be right today is a second
/// source for a fact the chip crate owns, diverging the first time that crate corrects one.
#[test]
fn every_plan_takes_its_numbers_from_the_part_crate() {
    let l0 = crate::StFamily::L0.plan();
    assert_eq!(l0.flash_base, STM32L0_FLASH_BASE);
    assert_eq!(l0.size_register, STM32L0_FLASH_SIZE_REG);
    assert_eq!(l0.id_register, STM32L0_DBGMCU_IDCODE);
    assert_eq!(l0.erase_granule, STM32L0_PAGE);
    assert_eq!(l0.erased_word, STM32L0_ERASED_WORD);

    let h7 = crate::StFamily::H7.plan();
    assert_eq!(h7.flash_base, STM32H7_FLASH_BASE);
    assert_eq!(h7.size_register, STM32H7_FLASH_SIZE_REG);
    assert_eq!(h7.id_register, STM32H7_DBGMCU_IDC);
    assert_eq!(h7.erase_granule, STM32H7_SECTOR);
    assert_eq!(h7.program_align, STM32H7_FLASH_WORD as u32);
    assert_eq!(h7.banks, &[STM32H7_FLASH_BASE, STM32H7_BANK2_BASE]);

    let u5 = crate::StFamily::U5.plan();
    assert_eq!(u5.flash_base, STM32U5_FLASH_BASE);
    assert_eq!(u5.size_register, STM32U5_FLASH_SIZE_REG);
    assert_eq!(u5.id_register, STM32U5_DBGMCU_IDCODE);
    assert_eq!(u5.erase_granule, STM32U5_PAGE);
    assert_eq!(u5.program_align, STM32U5_QUAD_WORD as u32);

    for family in [
        crate::StFamily::L0,
        crate::StFamily::C0,
        crate::StFamily::H7,
        crate::StFamily::U5,
    ] {
        let plan = family.plan();
        assert!(
            !plan.parts.is_empty(),
            "{} lists no device id",
            family.name()
        );
        assert!(!plan.manual.is_empty(), "{} names no manual", family.name());
        assert!(
            plan.program_align > 0,
            "{} would divide by zero",
            family.name()
        );
        assert!(
            plan.erase_granule > 0,
            "{} would erase forever",
            family.name()
        );
        assert!(
            !plan.banks.is_empty(),
            "{} would unlock nothing",
            family.name()
        );
    }

    assert!(
        crate::StFamily::H7.plan().attach_under_reset,
        "a running H755 refuses memory access"
    );
    for family in [
        crate::StFamily::L0,
        crate::StFamily::C0,
        crate::StFamily::U5,
    ] {
        assert!(
            !family.plan().attach_under_reset,
            "{} was driven through the plain attach and must keep it",
            family.name()
        );
    }
}

/// The three identity registers are three DIFFERENT addresses, and that is the whole reason
/// [`lamella_cmsis_dap_stm32::stm32_dev_id`] takes one rather than knowing one.
///
/// A plan that inherited a sibling's `id_register` would read a peripheral that is something else
/// on this part, decode whatever it found as a `DEV_ID`, and refuse a perfectly good board -- or,
/// worse, match one. This is the cheapest possible guard against the copy that produces that.
#[test]
fn no_two_families_read_the_same_identity_register() {
    let registers = [
        crate::StFamily::L0.plan().id_register,
        crate::StFamily::H7.plan().id_register,
        crate::StFamily::U5.plan().id_register,
    ];
    for (index, one) in registers.iter().enumerate() {
        for other in &registers[index + 1..] {
            assert_ne!(one, other, "two families read the same DBGMCU address");
        }
    }
    assert!(!registers.contains(&0xE004_2000));
}

/// A single-bank family answers its one lock for ANY address, including one outside its own map.
///
/// **THIS IS THE CASE A RANGE TEST WOULD GET WRONG.** The L0's and the U5's unlock primitives ignore
/// the address entirely, so the entry exists to hang one lock on rather than to be matched against.
/// Were this filtered by range, an image based anywhere unexpected would come back with NO banks,
/// nothing would be unlocked, and the failure would arrive after the erase rather than before it.
#[test]
fn a_single_bank_family_answers_its_one_lock_whatever_the_address() {
    let l0 = StProbe::new(FakeL0::new(), crate::StFamily::L0.plan());
    assert_eq!(l0.banks_covering(L0_FLASH, 64), vec![STM32L0_FLASH_BASE]);
    assert_eq!(l0.banks_covering(0x2000_0000, 4), vec![STM32L0_FLASH_BASE]);
    assert_eq!(l0.banks_covering(L0_FLASH, 0), vec![STM32L0_FLASH_BASE]);

    let u5 = StProbe::new(FakeL0::new(), crate::StFamily::U5.plan());
    assert_eq!(
        u5.banks_covering(STM32U5_FLASH_BASE, 4 * 1024 * 1024),
        vec![STM32U5_FLASH_BASE]
    );
}

/// The H7 has two locks and an image takes exactly the ones it reaches.
///
/// **UNLOCKING BANK 1 LEAVES BANK 2 LOCKED**, and a write to a locked bank does nothing while
/// reporting success -- so both an image that wrongly takes two locks and one that wrongly takes
/// one are defects, and only the second has a symptom. The three cases are entirely below the join,
/// spanning it, and entirely above it, plus the boundary itself in both directions.
#[test]
fn an_h7_image_takes_exactly_the_banks_it_reaches() {
    let h7 = StProbe::new(FakeL0::new(), crate::StFamily::H7.plan());
    let one_bank = STM32H7_BANK2_BASE - STM32H7_FLASH_BASE;

    assert_eq!(
        h7.banks_covering(STM32H7_FLASH_BASE, 64 * 1024),
        vec![STM32H7_FLASH_BASE],
        "an image inside bank 1 must not unlock bank 2"
    );
    assert_eq!(
        h7.banks_covering(STM32H7_BANK2_BASE, 64 * 1024),
        vec![STM32H7_BANK2_BASE],
        "an image inside bank 2 must not unlock bank 1"
    );
    assert_eq!(
        h7.banks_covering(STM32H7_FLASH_BASE, one_bank + 16),
        vec![STM32H7_FLASH_BASE, STM32H7_BANK2_BASE],
        "an image spanning the join must unlock BOTH"
    );
    assert_eq!(
        h7.banks_covering(STM32H7_FLASH_BASE, one_bank),
        vec![STM32H7_FLASH_BASE],
        "ending exactly at the join is bank 1 alone"
    );
    assert_eq!(
        h7.banks_covering(STM32H7_FLASH_BASE, one_bank + 1),
        vec![STM32H7_FLASH_BASE, STM32H7_BANK2_BASE],
        "one byte past the join reaches bank 2"
    );
}

/// Rounding cannot reach a bank the lock walk did not take, in either direction.
///
/// **BOTH WALKS ROUND UP AND THE LOCK WALK DOES NOT**, which is the shape that would leave a write
/// landing in a locked bank -- silently, because a write to a locked H7 bank does nothing and
/// reports success. Two roundings have to be shown safe:
///
/// - the ERASE covers `ceil(len / granule)` granules, so its last address is
///   `base + (granules - 1) * granule`, which is strictly below `base + len` and therefore inside a
///   bank `banks_covering` already took;
/// - the PROGRAM pads up to `program_align`, and the bank span is a whole multiple of that
///   alignment, so the padded end cannot pass a boundary the unpadded length had not reached.
///
/// The second holds only while the alignment divides the bank span, so that is what is asserted --
/// a future part with a bank size that is not a multiple of its write granule breaks it here rather
/// than on a board.
#[test]
fn neither_rounding_can_reach_a_bank_the_lock_walk_did_not_take() {
    let h7 = StProbe::new(FakeL0::new(), crate::StFamily::H7.plan());
    let plan = crate::StFamily::H7.plan();
    let one_bank = STM32H7_BANK2_BASE - STM32H7_FLASH_BASE;

    assert_eq!(
        one_bank % plan.program_align,
        0,
        "padding could cross a bank boundary"
    );
    assert_eq!(
        one_bank % plan.erase_granule,
        0,
        "a granule could straddle a bank boundary"
    );

    let over = one_bank + 1;
    let granules = over.div_ceil(plan.erase_granule);
    let last_erase = STM32H7_FLASH_BASE + (granules - 1) * plan.erase_granule;
    assert!(
        last_erase >= STM32H7_BANK2_BASE,
        "this case is meant to reach bank 2"
    );
    assert!(
        h7.banks_covering(STM32H7_FLASH_BASE, over)
            .contains(&STM32H7_BANK2_BASE),
        "the erase reaches bank 2 and the lock walk did not take it"
    );

    let granules = one_bank.div_ceil(plan.erase_granule);
    let last_erase = STM32H7_FLASH_BASE + (granules - 1) * plan.erase_granule;
    assert!(last_erase < STM32H7_BANK2_BASE);
    assert_eq!(
        h7.banks_covering(STM32H7_FLASH_BASE, one_bank),
        vec![STM32H7_FLASH_BASE]
    );
}

/// The padding byte is the family's own erased value, and the L0 is the one that differs.
///
/// The L0 pads with zero so its programmer SKIPS the padding and leaves those cells erased; a
/// ones-erasing part pads with `0xFF` and writes the granule once. Either constant written in
/// directly would be a defect on the other family, so this asserts that they really do differ --
/// a test that only checked each value against itself would pass with one hard-coded byte.
#[test]
fn the_padding_byte_is_the_erased_one_and_the_l0_is_the_odd_family() {
    let l0 = StProbe::new(FakeL0::new(), crate::StFamily::L0.plan());
    let h7 = StProbe::new(FakeL0::new(), crate::StFamily::H7.plan());
    let u5 = StProbe::new(FakeL0::new(), crate::StFamily::U5.plan());

    assert_eq!(
        l0.erased_byte(),
        0x00,
        "the L0 erases to zero -- RM0377 3.3.4"
    );
    assert_eq!(h7.erased_byte(), 0xff);
    assert_eq!(u5.erased_byte(), 0xff);
    assert_ne!(
        l0.erased_byte(),
        h7.erased_byte(),
        "the difference between the families is the whole reason the field exists"
    );
}

/// An EDBG route resolves within the EDBG's vendor and the KIT's product id, never by vendor alone.
///
/// **THE FILTER IS WHAT MAKES THE SERIAL LADDER MEAN ANYTHING HERE.** Every Xplained board on a
/// bench is `03eb:something`, and three kits share `0x2111`, so a route that narrowed only to the
/// vendor would treat every attached Microchip kit as one candidate pool. The pair narrows to a kit
/// family and the serial settles the board -- which is the micro:bit DAPLink's rule and the
/// ST-LINK's, not the Pico's.
#[test]
fn an_edbg_route_narrows_to_the_kit_and_not_to_the_vendor() {
    use crate::{Programmer, SamFamily};
    let d21 = Programmer::EdbgOnboard {
        family: SamFamily::Samd21,
        probe_id: 0x2169,
    };
    let xpro = Programmer::EdbgOnboard {
        family: SamFamily::Samd21,
        probe_id: 0x2111,
    };
    assert_eq!(d21.usb_identity(), Some((lamella_cmsis_dap_sam::EDBG_VENDOR_ID, 0x2169)));
    assert_eq!(xpro.usb_identity(), Some((lamella_cmsis_dap_sam::EDBG_VENDOR_ID, 0x2111)));
    assert_ne!(d21.usb_identity(), xpro.usb_identity());
    assert!(d21.usb_identity().is_some(), "an on-board debugger has an identity to filter on");
}

/// Every routed Microchip board names a kit product id that its own board file states.
///
/// **THE ROW AND THE BOARD FILE ARE TWO SPELLINGS OF ONE FACT** -- `usb_pid` in
/// `bsp/<board>/board.toml` is where board truth lives, and the row is what the router reads. A row
/// carrying an id the board file does not state would open a debugger that is not on that board.
#[test]
fn the_routed_microchip_kits_carry_their_own_product_ids() {
    use crate::{PROGRAMMING, Programmer};
    let expected = [
        ("samd21-xpro", 0x2169u16),
        ("atsamd11-xpro", 0x2111),
        ("atsamd10-xmini", 0x2145),
        ("samw25-xpro", 0x2111),
        ("same54-xpro", 0x2111),
        ("sam4e-xpro", 0x2111),
        ("sam4n-xpro", 0x2111),
        ("sam4l8-xpro", 0x2111),
        ("sam4s-xpro", 0x2111),
    ];
    for (board, id) in expected {
        let row = PROGRAMMING
            .iter()
            .find(|row| row.board == board)
            .unwrap_or_else(|| panic!("{board} has no routing row"));
        match row.programmer {
            Programmer::EdbgOnboard { probe_id, .. } => {
                assert_eq!(probe_id, id, "{board} names the wrong kit product id")
            }
            other => panic!("{board} is routed by {other:?}, not an EDBG"),
        }
    }
    let edbg = PROGRAMMING
        .iter()
        .filter(|row| matches!(row.programmer, Programmer::EdbgOnboard { .. }))
        .count();
    assert_eq!(edbg, expected.len(), "an EDBG row was added without a line in this test");
}

/// The EEFC family is single-plane BY NAME, and every routed board carrying it is single-plane.
///
/// **THE VARIANT'S WHOLE CLAIM IS THAT AN ADDRESS SETTLES WHICH CONTROLLER TO DRIVE**, which is only
/// true of a part with one plane behind one EEFC. On a dual-plane part -- an ATSAM4SD32C, say --
/// which controller fronts which window is decided by a `GPNVM` swap bit that no address reveals,
/// and filling one plane's write latch while commanding the other reports success.
///
/// So `sam4s-xpro` must NOT carry this family: it is the dual-plane part, and it is routed on
/// [`crate::SamFamily::Sam4sDual`] instead. The erase arm refuses at run time on the descriptor's
/// own plane count, and this refuses at the table.
#[test]
fn the_eefc_family_routes_no_dual_plane_board() {
    use crate::{PROGRAMMING, Programmer, SamFamily};
    let eefc: Vec<&str> = PROGRAMMING
        .iter()
        .filter(|row| {
            matches!(
                row.programmer,
                Programmer::EdbgOnboard { family: SamFamily::Sam4Eefc, .. }
            )
        })
        .map(|row| row.board)
        .collect();

    assert!(!eefc.is_empty(), "no board carries this family, so the check proves nothing");
    assert!(
        !eefc.contains(&"sam4s-xpro"),
        "the SAM4S Xplained Pro carries an ATSAM4SD32C -- two planes, two controllers, chosen by a \
         GPNVM bit rather than by the address: {eefc:?}"
    );
    assert!(eefc.contains(&"sam4e-xpro") && eefc.contains(&"sam4n-xpro"), "{eefc:?}");

    let dual: Vec<&str> = PROGRAMMING
        .iter()
        .filter(|row| {
            matches!(
                row.programmer,
                Programmer::EdbgOnboard { family: SamFamily::Sam4sDual, .. }
            )
        })
        .map(|row| row.board)
        .collect();
    assert_eq!(dual, vec!["sam4s-xpro"], "{dual:?}");
}

/// A route's declared units count the family's own granule, so the same image does not read as
/// eight times longer on one part than another.
#[test]
fn each_family_counts_the_granule_it_actually_programs() {
    use crate::{Programmer, StFamily};
    let probe_id = lamella_stlink::product_id::V2_1;
    let units = |family| Programmer::StlinkOnboard { family, probe_id }.units(1024);

    assert_eq!(units(StFamily::L0), "256 words");
    assert_eq!(units(StFamily::U5), "64 quad-words");
    assert_eq!(units(StFamily::H7), "32 flash words");
}


const C0_FLASH: u32 = 0x0800_0000;
const C0_DBGMCU: u32 = 0x4001_5800;
const C0_FSIZE: u32 = 0x1FFF_75A0;
const C0_REGS: u32 = 0x4002_2000;
/// `DEV_ID` 0x493: the STM32C071, which is the part this row was measured against.
const C0_C071: u32 = 0x493;

/// An STM32C0 modelled as its CONTROLLER and its ARRAY.
///
/// **THE ARRAY STARTS AT 0xFF**, which is what erased means here -- the opposite of the FakeL0
/// above, and the reason these two fakes cannot share an initialiser.
///
/// **AND IT ENFORCES THE WRITE-ONCE RULE, because the real controller does.** RM0490 raises PROGERR
/// when a double word to be programmed does not currently read all ones. A fake that silently
/// accepted a second write would let a backend that programs without erasing pass here and fail on
/// silicon, which is the shape of hole a fake is supposed to close rather than open.
struct FakeC0 {
    log: Vec<&'static str>,
    dev_id: u32,
    /// The value FSIZER's LOW halfword reports, in Kbytes.
    flash_kb: u32,
    flash: Vec<u8>,
    locked: bool,
    cr: u32,
    sr: u32,
}

impl FakeC0 {
    fn new() -> Self {
        FakeC0 {
            log: Vec::new(),
            dev_id: C0_C071,
            flash_kb: 128,
            flash: vec![0xff; 128 * 1024],
            locked: true,
            cr: 1 << 31,
            sr: 0,
        }
    }

    fn array_untouched(&self, original: &[u8]) -> bool {
        self.flash == original
    }

    fn offset(&self, address: u32) -> Option<usize> {
        let end = C0_FLASH + self.flash.len() as u32;
        (C0_FLASH..end)
            .contains(&address)
            .then(|| (address - C0_FLASH) as usize)
    }
}

impl TargetAccess for FakeC0 {
    fn connect(&mut self) -> Result<(), ProbeError> {
        Ok(())
    }

    fn read_idcode(&mut self) -> Result<u32, ProbeError> {
        Ok(0x0bc1_1477)
    }

    fn read_word(&mut self, address: u32) -> Result<u32, ProbeError> {
        match address {
            C0_DBGMCU => Ok((0x1001 << 16) | 0x6000 | self.dev_id),
            C0_FSIZE => Ok(0xffff_0000 | self.flash_kb),
            a if a == C0_REGS + 0x10 => Ok(self.sr),
            a if a == C0_REGS + 0x14 => Ok(self.cr),
            a => match self.offset(a) {
                Some(at) => Ok(u32::from_le_bytes([
                    self.flash[at],
                    self.flash[at + 1],
                    self.flash[at + 2],
                    self.flash[at + 3],
                ])),
                None => Err(ProbeError::Device("FakeC0: read outside the model")),
            },
        }
    }

    fn write_word(&mut self, address: u32, value: u32) -> Result<(), ProbeError> {
        match address {
            a if a == C0_REGS + 0x008 => {
                if value == 0xCDEF_89AB {
                    self.locked = false;
                    self.cr &= !(1 << 31);
                }
                Ok(())
            }
            a if a == C0_REGS + 0x010 => {
                self.sr &= !value;
                Ok(())
            }
            a if a == C0_REGS + 0x014 => {
                self.cr = value;
                if value & (1 << 31) != 0 {
                    self.locked = true;
                }
                if value & (1 << 1) != 0 && value & (1 << 16) != 0 {
                    if self.locked {
                        self.sr |= 1 << 4;
                        return Ok(());
                    }
                    let page = (value >> 3) & 0x7f;
                    let at = (page * 2048) as usize;
                    if at + 2048 <= self.flash.len() {
                        self.flash[at..at + 2048].fill(0xff);
                        self.log.push("erase_page");
                        self.sr |= 1;
                    } else {
                        self.sr |= 1 << 4;
                    }
                }
                Ok(())
            }
            a => {
                let Some(at) = self.offset(a) else {
                    return Err(ProbeError::Device("FakeC0: write outside the model"));
                };
                if self.cr & 1 == 0 {
                    self.sr |= 1 << 7;
                    return Ok(());
                }
                if self.locked {
                    self.sr |= 1 << 4;
                    return Ok(());
                }
                let current = u32::from_le_bytes([
                    self.flash[at],
                    self.flash[at + 1],
                    self.flash[at + 2],
                    self.flash[at + 3],
                ]);
                if current != 0xffff_ffff && value != 0 {
                    self.sr |= 1 << 3;
                    return Ok(());
                }
                self.flash[at..at + 4].copy_from_slice(&value.to_le_bytes());
                self.log.push("program_word");
                self.sr |= 1;
                Ok(())
            }
        }
    }

    fn read_words_into(&mut self, address: u32, out: &mut [u32]) -> Result<(), ProbeError> {
        for (i, slot) in out.iter_mut().enumerate() {
            *slot = self.read_word(address + (i as u32) * 4)?;
        }
        Ok(())
    }

    fn halt(&mut self) -> Result<(), ProbeError> {
        self.log.push("halt");
        Ok(())
    }

    fn reset_and_run(&mut self) -> Result<(), ProbeError> {
        self.log.push("reset_and_run");
        Ok(())
    }

    fn init_mem(&mut self) -> Result<(), ProbeError> {
        Ok(())
    }
    fn write_words(&mut self, address: u32, words: &[u32]) -> Result<(), ProbeError> {
        for (index, word) in words.iter().enumerate() {
            self.write_word(address + (index * 4) as u32, *word)?;
        }
        Ok(())
    }
    fn read_byte(&mut self, _address: u32) -> Result<u8, ProbeError> {
        unreachable!("byte access is not part of the C0 flashing path")
    }
    fn write_byte(&mut self, _address: u32, _value: u8) -> Result<(), ProbeError> {
        unreachable!("byte access is not part of the C0 flashing path")
    }
    fn read_halfword(&mut self, _address: u32) -> Result<u16, ProbeError> {
        unreachable!("halfword access is not part of the C0 flashing path")
    }
    fn write_halfword(&mut self, _address: u32, _value: u16) -> Result<(), ProbeError> {
        unreachable!("a C0 programs a DOUBLE WORD; a halfword write would be SIZERR on the part")
    }
    fn resume(&mut self) -> Result<(), ProbeError> {
        unreachable!("a flashing backend leaves the part through reset_and_run, not resume")
    }
    fn step(&mut self) -> Result<(), ProbeError> {
        unreachable!("run control is not the flashing path")
    }
    fn is_halted(&mut self) -> Result<bool, ProbeError> {
        unreachable!("run control is not the flashing path")
    }
    fn wait_halted(&mut self) -> Result<(), ProbeError> {
        unreachable!("run control is not the flashing path")
    }
    fn reset_and_halt(&mut self) -> Result<(), ProbeError> {
        unreachable!("run control is not the flashing path")
    }
    fn set_reset(&mut self, _assert: bool) -> Result<u8, ProbeError> {
        unreachable!("a flashing backend does not drive the reset line directly")
    }
    fn read_core_reg(&mut self, _selector: u8) -> Result<u32, ProbeError> {
        unreachable!("core registers are not the flashing path")
    }
    fn write_core_reg(&mut self, _selector: u8, _value: u32) -> Result<(), ProbeError> {
        unreachable!("core registers are not the flashing path")
    }
    fn arm_reset_catch(&mut self) -> Result<(), ProbeError> {
        unreachable!("reset catch is not the flashing path")
    }
    fn disarm_reset_catch(&mut self) -> Result<(), ProbeError> {
        unreachable!("reset catch is not the flashing path")
    }
    fn set_breakpoint(&mut self, _address: u32) -> Result<(), ProbeError> {
        unreachable!("breakpoints are not the flashing path")
    }
    fn clear_breakpoint(&mut self) -> Result<(), ProbeError> {
        unreachable!("breakpoints are not the flashing path")
    }
    fn set_breakpoints(&mut self, _addresses: &[u32]) -> Result<(), ProbeError> {
        unreachable!("breakpoints are not the flashing path")
    }
    fn call_target(
        &mut self,
        _address: u32,
        _args: &[u32],
        _frame: &lamella_probe_core::CallFrame,
    ) -> Result<u32, ProbeError> {
        unreachable!("a C0 is programmed straight over SWD; no loader runs on the target")
    }
}

/// The whole contract, end to end, against a part that enforces its own write-once rule.
#[test]
fn a_c0_image_is_identified_erased_programmed_and_read_back() {
    let image: Vec<u8> = (0..20u8).map(|i| i.wrapping_mul(7)).collect();
    let mut backend = StProbe::new(FakeC0::new(), crate::StFamily::C0.plan());

    let report = flash(
        &mut backend,
        &Image {
            bytes: &image,
            base: C0_FLASH,
        },
        VerifyPolicy::ReadBack,
        &Allow::Any,
    )
    .expect("the sequence completes");

    assert_eq!(report.verification, Verification::ReadBack);
    assert_eq!(report.identity.value, u64::from(C0_C071));
    assert!(
        report.identity.what.contains("not this board"),
        "{}",
        report.identity.what
    );
    assert_eq!(&backend.target.flash[..20], &image[..]);
    assert_eq!(&backend.target.flash[20..24], &[0xff; 4]);
}

/// Identify precedes the erase, and a part that is not a C0 never reaches it.
#[test]
fn a_foreign_dev_id_is_refused_before_a_c0_is_erased() {
    let mut backend = StProbe::new(FakeC0::new(), crate::StFamily::C0.plan());
    backend.target.dev_id = 0x447;
    let original = backend.target.flash.clone();

    let error = flash(
        &mut backend,
        &Image {
            bytes: &[1, 2, 3, 4],
            base: C0_FLASH,
        },
        VerifyPolicy::ReadBack,
        &Allow::Any,
    )
    .expect_err("an L0's DEV_ID is not a C0's");
    assert!(format!("{error}").contains("0x447"), "{error}");
    assert!(backend.target.array_untouched(&original));
    assert!(
        !backend.target.log.contains(&"halt"),
        "halted before the refusal"
    );
    assert!(
        !backend.target.log.contains(&"erase_page"),
        "{:?}",
        backend.target.log
    );
}

/// The reserved nibble is modelled, so a backend masking sixteen bits fails HERE.
///
#[test]
fn the_identity_reads_twelve_bits_and_not_sixteen() {
    let mut backend = StProbe::new(FakeC0::new(), crate::StFamily::C0.plan());
    let raw = backend
        .target
        .read_word(C0_DBGMCU)
        .expect("the part answers");
    assert_eq!(raw, 0x1001_6493, "the word a NUCLEO-C071RB returns");
    assert_ne!(
        raw & 0xffff,
        C0_C071,
        "sixteen bits is the wrong width on this family"
    );

    let identity = backend.identify().expect("twelve bits decode");
    assert_eq!(identity.value, u64::from(C0_C071));
}

/// The bound is on where the page walk REACHES, not on how many bytes the image holds.
///
#[test]
fn a_c0_image_whose_pages_leave_the_array_is_refused_even_though_its_length_fits() {
    let mut backend = StProbe::new(FakeC0::new(), crate::StFamily::C0.plan());
    backend.target.flash_kb = 4;
    backend.target.flash = vec![0xff; 4 * 1024];
    let original = backend.target.flash.clone();

    let image = vec![0x5au8; 4096];
    let error = backend
        .erase(&Image {
            bytes: &image,
            base: C0_FLASH + STM32C0_PAGE,
        })
        .expect_err("the walk runs a page past the end of a 4 KB array");
    let text = format!("{error}");
    assert!(
        text.contains("4 KB"),
        "the error names the part's own answer: {text}"
    );
    assert!(backend.target.array_untouched(&original));
    assert!(
        !backend.target.log.contains(&"halt"),
        "halted before the refusal"
    );
    assert!(
        !backend.target.log.contains(&"erase_page"),
        "{:?}",
        backend.target.log
    );
}

/// An image larger than the fitted flash is refused before the first page erase.
#[test]
fn a_c0_image_larger_than_its_fitted_flash_is_refused_before_the_erase() {
    let mut backend = StProbe::new(FakeC0::new(), crate::StFamily::C0.plan());
    backend.target.flash_kb = 2;
    backend.target.flash = vec![0xff; 2 * 1024];
    let original = backend.target.flash.clone();

    let error = flash(
        &mut backend,
        &Image {
            bytes: &vec![0x11u8; 4096],
            base: C0_FLASH,
        },
        VerifyPolicy::ReadBack,
        &Allow::Any,
    )
    .expect_err("4096 bytes do not fit in 2 KB");
    assert!(format!("{error}").contains("2 KB"), "{error}");
    assert!(backend.target.array_untouched(&original));
    assert!(
        !backend.target.log.contains(&"erase_page"),
        "{:?}",
        backend.target.log
    );
}

/// `Allow` can pin the sub-family, and a part outside it is refused between identify and erase.
#[test]
fn a_permission_naming_another_c0_sub_family_refuses_before_the_erase() {
    let mut backend = StProbe::new(FakeC0::new(), crate::StFamily::C0.plan());
    let original = backend.target.flash.clone();
    let only_c031 = Allow::Identities(vec![0x453]);

    let error = flash(
        &mut backend,
        &Image {
            bytes: &[1, 2, 3, 4],
            base: C0_FLASH,
        },
        VerifyPolicy::ReadBack,
        &only_c031,
    )
    .expect_err("a C071 is not a C031");
    assert!(backend.target.array_untouched(&original), "{error}");
    assert!(
        !backend.target.log.contains(&"erase_page"),
        "{:?}",
        backend.target.log
    );
}

/// The part enforces write-once, so programming without erasing FAILS rather than quietly
/// corrupting -- and this asserts the fake is strict enough to prove that.
#[test]
fn programming_a_double_word_that_is_not_erased_is_refused_by_the_part() {
    let mut backend = StProbe::new(FakeC0::new(), crate::StFamily::C0.plan());
    backend.target.flash[..8].copy_from_slice(&[0xaa; 8]);

    let error = backend
        .program(&Image {
            bytes: &[1, 2, 3, 4, 5, 6, 7, 8],
            base: C0_FLASH,
        })
        .expect_err("the target double word is not erased");
    assert!(format!("{error}").contains("PROGERR"), "{error}");
}


/// The FLASHCALW user interface -- Atmel-42023H section 14, and the offsets its register map gives.
///
/// **THESE ARE WRITTEN OUT HERE RATHER THAN IMPORTED, and that is deliberate**: the driver keeps
/// them private, and a fake that reached for the driver's own constants would agree with it by
/// construction. Reading them out of the datasheet a second time is what makes this a check.
const L4W_BASE: u32 = lamella_cmsis_dap_sam::SAM4L_FLASHCALW;
const L4W_FCMD: u32 = L4W_BASE + 0x04;
const L4W_FSR: u32 = L4W_BASE + 0x08;
const L4W_FPR: u32 = L4W_BASE + 0x0c;
const L4W_MAINT0: u32 = L4W_BASE + 0x420;
/// WARNING: `0xA5`. An EEFC's key is `0x5A` -- the same two nibbles, swapped -- so the fake
/// REFUSES a wrong key rather than ignoring the field. A permissive fake would pass a driver that
/// had copied its neighbour's constant, which is the single most likely mistake in this file.
const L4W_KEY: u32 = 0xa5 << 24;
const L4W_CMD_WP: u32 = 1;
const L4W_CMD_EP: u32 = 2;
const L4W_CMD_CPB: u32 = 3;
const L4W_CMD_QPR: u32 = 12;
const L4W_FRDY: u32 = 1 << 0;
const L4W_LOCKE: u32 = 1 << 2;
const L4W_PROGE: u32 = 1 << 3;
const L4W_SECURITY: u32 = 1 << 4;
const L4W_QPRR: u32 = 1 << 5;
const L4W_LOCK0: u32 = 16;
/// `PSZ` = 4 is a 512-byte page (32 << 4), which is what an ATSAM4LC8C reports.
const L4W_PSZ: u32 = 4;
/// `FSZ` = 3 is 32 KB -- 64 pages of 512, so the array is small enough to hold in a test and still
/// divides the sixteen lock regions evenly, at four pages each.
const L4W_FSZ: u32 = 3;
const L4W_PAGE: usize = 512;
const L4W_PAGES: usize = 64;
/// The CHIPID an ATSAM4LC8C actually answers, read off the board rather than off a table.
///
/// **THE EXID IS THE POINT.** `sam4_identify`'s SAM4L rows are the 48-pin LS parts and this pair is
/// not among them, so the exact-member lookup returns `None` for the one part this route was
/// measured on. The family guard is what accepts it, and this constant is why that matters.
/// What the backend calls itself in these tests. The real string comes from the ROUTE, and which
/// route reached a given controller is not what any test below is about.
const SAM_TEST_MECHANISM: &str = "a fake probe, by the part's own flash controller";
const L4W_CIDR: u32 = 0xab0b_0ae0;
const L4W_EXID: u32 = 0x1400_000f;
/// The ATSAM4LS8's pair, which IS on the member table -- the control for the family guard.
const L4W_LS8_EXID: u32 = 0x1200_0002;

/// A SAM4L modelled as its controller, its array, its page buffer AND its cache.
struct FakeSam4l {
    log: Vec<&'static str>,
    /// The array as the part holds it.
    flash: Vec<u8>,
    /// What a read over the wire sees, which is NOT the array until `MAINT0.INVALL` is written.
    ///
    /// **A REAL PicoCache ONLY HOLDS LINES SOMETHING HAS READ, so this is stricter than the part**
    /// -- every read is stale here, where on silicon only some are. Stricter is the right direction
    /// for a test: it makes the invalidate mandatory instead of usually-unnecessary, and "usually"
    /// is how a missing invalidate survives a test suite and fails on a bench.
    cache: Vec<u8>,
    /// The page buffer. **It can only clear bits and a write does not reset it**, which is the
    /// whole reason `Clear Page Buffer` is mandatory before every fill.
    buffer: Vec<u8>,
    /// `FSR` bits 31:16, one per lock region.
    locks: u16,
    secure: bool,
    /// Latched `PROGE` / `LOCKE`, cleared by a read of `FSR` as the real register is.
    errors: u32,
    /// The last `QPR` result, reported in `FSR.QPRR`.
    qpr: bool,
    /// What `CHIPID_EXID` answers. Settable because the two SAM4L pairs that matter here differ in
    /// this field alone: the fitted LC8C is not on the member table and an LS8 is.
    exid: u32,
}

impl FakeSam4l {
    fn new() -> Self {
        FakeSam4l {
            log: Vec::new(),
            flash: vec![0xff; L4W_PAGE * L4W_PAGES],
            cache: vec![0xff; L4W_PAGE * L4W_PAGES],
            buffer: vec![0xff; L4W_PAGE],
            locks: 0,
            secure: false,
            errors: 0,
            qpr: false,
            exid: L4W_EXID,
        }
    }

    /// Commit the page buffer, the way flash commits: bits go from one to zero and never back.
    fn write_page(&mut self, page: usize) {
        let at = page * L4W_PAGE;
        let Some(target) = self.flash.get_mut(at..at + L4W_PAGE) else {
            self.errors |= L4W_PROGE;
            return;
        };
        for (cell, fill) in target.iter_mut().zip(self.buffer.iter()) {
            *cell &= *fill;
        }
        self.log.push("write_page");
    }
}

impl TargetAccess for FakeSam4l {
    fn connect(&mut self) -> Result<(), ProbeError> {
        Ok(())
    }
    fn read_idcode(&mut self) -> Result<u32, ProbeError> {
        Ok(0x2ba0_1477)
    }
    fn init_mem(&mut self) -> Result<(), ProbeError> {
        Ok(())
    }

    fn read_word(&mut self, address: u32) -> Result<u32, ProbeError> {
        match address {
            lamella_cmsis_dap_sam::SAM4_CHIPID_CIDR => Ok(L4W_CIDR),
            lamella_cmsis_dap_sam::SAM4_CHIPID_EXID => Ok(self.exid),
            L4W_FPR => Ok((L4W_PSZ << 8) | L4W_FSZ),
            L4W_FSR => {
                let mut fsr = L4W_FRDY | self.errors | (u32::from(self.locks) << L4W_LOCK0);
                if self.secure {
                    fsr |= L4W_SECURITY;
                }
                if self.qpr {
                    fsr |= L4W_QPRR;
                }
                self.errors = 0;
                Ok(fsr)
            }
            _ if (address as usize) < self.cache.len() => {
                let at = address as usize;
                let mut word = [0u8; 4];
                word.copy_from_slice(
                    self.cache
                        .get(at..at + 4)
                        .ok_or(ProbeError::Device("read past the array"))?,
                );
                Ok(u32::from_le_bytes(word))
            }
            _ => Err(ProbeError::Device(
                "read of an address this fake does not model",
            )),
        }
    }

    fn write_word(&mut self, address: u32, value: u32) -> Result<(), ProbeError> {
        match address {
            L4W_MAINT0 => {
                if value & 1 != 0 {
                    self.cache.copy_from_slice(&self.flash);
                    self.log.push("invalidate_cache");
                }
                Ok(())
            }
            L4W_FCMD => {
                if value & 0xff00_0000 != L4W_KEY {
                    self.errors |= L4W_PROGE;
                    self.log.push("bad_key");
                    return Ok(());
                }
                let page = ((value >> 8) & 0xffff) as usize;
                let region = (page * L4W_LOCK0 as usize) / L4W_PAGES;
                let locked = self.locks & (1 << region) != 0;
                match value & 0x3f {
                    L4W_CMD_CPB => {
                        self.buffer.iter_mut().for_each(|byte| *byte = 0xff);
                        self.log.push("clear_page_buffer");
                    }
                    L4W_CMD_EP if locked => {
                        self.errors |= L4W_LOCKE;
                        self.log.push("erase_locked");
                    }
                    L4W_CMD_EP => {
                        let at = page * L4W_PAGE;
                        match self.flash.get_mut(at..at + L4W_PAGE) {
                            Some(span) => {
                                span.iter_mut().for_each(|byte| *byte = 0xff);
                                self.log.push("erase_page");
                            }
                            None => self.errors |= L4W_PROGE,
                        }
                    }
                    L4W_CMD_WP if locked => {
                        self.errors |= L4W_LOCKE;
                        self.log.push("write_locked");
                    }
                    L4W_CMD_WP => self.write_page(page),
                    L4W_CMD_QPR => {
                        let at = page * L4W_PAGE;
                        self.qpr = self
                            .flash
                            .get(at..at + L4W_PAGE)
                            .is_some_and(|span| span.iter().all(|byte| *byte == 0xff));
                        self.log.push("quick_page_read");
                    }
                    _ => self.errors |= L4W_PROGE,
                }
                Ok(())
            }
            _ if (address as usize) < self.flash.len() => {
                let at = (address as usize) % L4W_PAGE;
                let slot = self
                    .buffer
                    .get_mut(at..at + 4)
                    .ok_or(ProbeError::Device("fill past the page buffer"))?;
                for (cell, byte) in slot.iter_mut().zip(value.to_le_bytes()) {
                    *cell &= byte;
                }
                Ok(())
            }
            _ => Err(ProbeError::Device(
                "write to an address this fake does not model",
            )),
        }
    }

    fn read_words_into(&mut self, address: u32, out: &mut [u32]) -> Result<(), ProbeError> {
        for (index, slot) in out.iter_mut().enumerate() {
            *slot = self.read_word(address + (index * 4) as u32)?;
        }
        Ok(())
    }

    fn write_words(&mut self, address: u32, words: &[u32]) -> Result<(), ProbeError> {
        for (index, word) in words.iter().enumerate() {
            self.write_word(address + (index * 4) as u32, *word)?;
        }
        Ok(())
    }

    fn halt(&mut self) -> Result<(), ProbeError> {
        self.log.push("halt");
        Ok(())
    }
    fn reset_and_run(&mut self) -> Result<(), ProbeError> {
        self.log.push("reset_and_run");
        Ok(())
    }

    fn read_byte(&mut self, _address: u32) -> Result<u8, ProbeError> {
        unreachable!("byte access is not part of the flashing path")
    }
    fn write_byte(&mut self, _address: u32, _value: u8) -> Result<(), ProbeError> {
        unreachable!("byte access is not part of the flashing path")
    }
    fn read_halfword(&mut self, _address: u32) -> Result<u16, ProbeError> {
        unreachable!("halfword access is not part of the flashing path")
    }
    fn write_halfword(&mut self, _address: u32, _value: u16) -> Result<(), ProbeError> {
        unreachable!("halfword access is not part of the flashing path")
    }
    fn resume(&mut self) -> Result<(), ProbeError> {
        unreachable!("a flashing backend leaves the part through reset_and_run, not resume")
    }
    fn step(&mut self) -> Result<(), ProbeError> {
        unreachable!("run control is not the flashing path")
    }
    fn is_halted(&mut self) -> Result<bool, ProbeError> {
        unreachable!("run control is not the flashing path")
    }
    fn wait_halted(&mut self) -> Result<(), ProbeError> {
        unreachable!("run control is not the flashing path")
    }
    fn reset_and_halt(&mut self) -> Result<(), ProbeError> {
        unreachable!("run control is not the flashing path")
    }
    fn set_reset(&mut self, _assert: bool) -> Result<u8, ProbeError> {
        unreachable!("a flashing backend does not drive the reset line directly")
    }
    fn read_core_reg(&mut self, _selector: u8) -> Result<u32, ProbeError> {
        unreachable!("core registers are not the flashing path")
    }
    fn write_core_reg(&mut self, _selector: u8, _value: u32) -> Result<(), ProbeError> {
        unreachable!("core registers are not the flashing path")
    }
    fn arm_reset_catch(&mut self) -> Result<(), ProbeError> {
        unreachable!("reset catch is not the flashing path")
    }
    fn disarm_reset_catch(&mut self) -> Result<(), ProbeError> {
        unreachable!("reset catch is not the flashing path")
    }
    fn set_breakpoint(&mut self, _address: u32) -> Result<(), ProbeError> {
        unreachable!("breakpoints are not the flashing path")
    }
    fn clear_breakpoint(&mut self) -> Result<(), ProbeError> {
        unreachable!("breakpoints are not the flashing path")
    }
    fn set_breakpoints(&mut self, _addresses: &[u32]) -> Result<(), ProbeError> {
        unreachable!("breakpoints are not the flashing path")
    }
    fn call_target(
        &mut self,
        _address: u32,
        _args: &[u32],
        _frame: &lamella_probe_core::CallFrame,
    ) -> Result<u32, ProbeError> {
        unreachable!("a flashing backend does not run code on the target")
    }
}

/// Two pages whose second would be destroyed by a missing Clear Page Buffer.
///
/// **THE VALUES ARE THE TEST.** The page buffer only clears bits, so a fill that skipped the clear
/// would commit `0xF0 & 0x0F` -- zero -- for every byte of the second page. Two pages of the same
/// pattern would come out right either way and prove nothing.
fn sam4l_image() -> Vec<u8> {
    let mut bytes = vec![0xf0u8; L4W_PAGE];
    bytes.extend(std::iter::repeat_n(0x0fu8, L4W_PAGE));
    bytes
}

/// The same validation the nRF and L0 backends get: a REAL part's primitives, composed by the
/// contract, in the order the contract guarantees.
#[test]
fn the_sam4l_primitives_compose_into_the_contracts_order() {
    let bytes = sam4l_image();
    let mut backend = SamProbe::new(FakeSam4l::new(), crate::SamFamily::Sam4l, SAM_TEST_MECHANISM);
    let report = flash(
        &mut backend,
        &Image { bytes: &bytes, base: 0 },
        VerifyPolicy::ReadBack,
        &Allow::Any,
    )
    .expect("the sequence runs");

    assert!(matches!(report.verification, Verification::ReadBack));
    assert_eq!(
        backend.target.log,
        vec![
            "halt",
            "erase_page",
            "invalidate_cache",
            "erase_page",
            "invalidate_cache",
            "clear_page_buffer",
            "write_page",
            "invalidate_cache",
            "clear_page_buffer",
            "write_page",
            "invalidate_cache",
            "reset_and_run",
        ],
        "{:?}",
        backend.target.log
    );
}

/// The second page is the image, not the image ANDed with the page before it.
///
/// **THIS IS THE DEFECT THE DRIVER'S OWN HEADER WARNS ABOUT, MADE INTO A CHECK.** The page buffer
/// is not reset by a write, so an implementation that issued `Clear Page Buffer` once -- or never,
/// since the buffer starts erased -- programs its FIRST page correctly and silently corrupts every
/// one after it. A single-page test passes against exactly that bug.
#[test]
fn every_page_is_cleared_before_its_fill_so_the_second_is_not_anded_with_the_first() {
    let bytes = sam4l_image();
    let mut backend = SamProbe::new(FakeSam4l::new(), crate::SamFamily::Sam4l, SAM_TEST_MECHANISM);
    flash(
        &mut backend,
        &Image { bytes: &bytes, base: 0 },
        VerifyPolicy::ReadBack,
        &Allow::Any,
    )
    .expect("the sequence runs");

    assert_eq!(&backend.target.flash[..L4W_PAGE], &[0xf0u8; L4W_PAGE], "the first page");
    assert_eq!(
        &backend.target.flash[L4W_PAGE..L4W_PAGE * 2],
        &[0x0fu8; L4W_PAGE],
        "the second page, which is the one a missing clear destroys"
    );
    assert_eq!(
        backend.target.log.iter().filter(|step| **step == "clear_page_buffer").count(),
        2
    );
}

/// The verify reads the ARRAY, which on this family means the cache has to be invalidated first.
///
/// **A READ AFTER A PROGRAM IS NOT A VERIFICATION ON THIS FAMILY UNTIL THE CACHE IS INVALIDATED.**
/// The array can be correct while every read over the MEM-AP returns `0xFFFFFFFF`, whose first
/// reading is "the write silently failed". No other SAM in this file behaves this way, so every
/// habit built on the others says a read-back is the verification.
#[test]
fn a_verify_on_this_family_is_only_a_verify_once_the_cache_is_invalidated() {
    let bytes = sam4l_image();
    let mut backend = SamProbe::new(FakeSam4l::new(), crate::SamFamily::Sam4l, SAM_TEST_MECHANISM);
    let report = flash(
        &mut backend,
        &Image { bytes: &bytes, base: 0 },
        VerifyPolicy::ReadBack,
        &Allow::Any,
    )
    .expect("the sequence runs");

    assert!(matches!(report.verification, Verification::ReadBack));
    assert_eq!(backend.target.cache, backend.target.flash);
}

/// A locked region is refused BEFORE the erase, and the array is the witness.
///
/// **THERE IS NO UNLOCK ON THIS ROUTE**, so a lock is a refusal rather than something to work
/// around -- and a lock violation discovered on page 40 has already destroyed pages 0 to 39. The
/// check is a read of a register the part already answers, placed before the halt.
#[test]
fn a_locked_region_is_refused_before_anything_is_erased() {
    let bytes = sam4l_image();
    let mut backend = SamProbe::new(FakeSam4l::new(), crate::SamFamily::Sam4l, SAM_TEST_MECHANISM);
    backend.target.locks = 0b1;
    let error = flash(
        &mut backend,
        &Image { bytes: &bytes, base: 0 },
        VerifyPolicy::ReadBack,
        &Allow::Any,
    )
    .expect_err("region 0 is locked");

    assert!(format!("{error}").contains("lock regions [0]"), "{error}");
    assert!(backend.target.flash.iter().all(|byte| *byte == 0xff), "nothing was erased");
    assert!(!backend.target.log.contains(&"halt"), "{:?}", backend.target.log);
}

/// A locked region the image does not reach is not this write's problem.
///
/// **THE OPPOSITE ERROR IS AS REAL AS THE ONE ABOVE**: refusing on any lock bit anywhere would make
/// a part with its bootloader region locked unflashable everywhere else, which is the normal state
/// of a shipped board rather than an exceptional one.
#[test]
fn a_locked_region_the_image_never_reaches_does_not_refuse_the_write() {
    let bytes = sam4l_image();
    let mut backend = SamProbe::new(FakeSam4l::new(), crate::SamFamily::Sam4l, SAM_TEST_MECHANISM);
    backend.target.locks = 1 << 15;
    flash(
        &mut backend,
        &Image { bytes: &bytes, base: 0 },
        VerifyPolicy::ReadBack,
        &Allow::Any,
    )
    .expect("the image is nowhere near region 15");
}

/// A protected part says so rather than failing at its first command.
#[test]
fn a_protected_part_is_refused_before_anything_is_erased() {
    let bytes = sam4l_image();
    let mut backend = SamProbe::new(FakeSam4l::new(), crate::SamFamily::Sam4l, SAM_TEST_MECHANISM);
    backend.target.secure = true;
    let error = flash(
        &mut backend,
        &Image { bytes: &bytes, base: 0 },
        VerifyPolicy::ReadBack,
        &Allow::Any,
    )
    .expect_err("the part is protected");

    assert!(format!("{error}").contains("FSR.SECURITY"), "{error}");
    assert!(format!("{error}").contains("chip-erase"), "{error}");
    assert!(!backend.target.log.contains(&"halt"), "{:?}", backend.target.log);
}

/// An image whose pages leave the array is refused before the halt, on the geometry the PART
/// reports rather than on a size written down here.
///
#[test]
fn a_sam4l_image_whose_pages_leave_the_array_is_refused_before_the_halt() {
    let bytes = vec![0xa5u8; L4W_PAGE * 4];
    let mut backend = SamProbe::new(FakeSam4l::new(), crate::SamFamily::Sam4l, SAM_TEST_MECHANISM);
    let base = ((L4W_PAGES - 3) * L4W_PAGE) as u32;
    let error = backend
        .erase(&Image { bytes: &bytes, base })
        .expect_err("the walk leaves the array");

    let text = format!("{error}");
    assert!(text.contains("32 KB"), "the part's own figure: {text}");
    assert!(text.contains("64 pages"), "and the figure it derived: {text}");
    assert!(backend.target.flash.iter().all(|byte| *byte == 0xff), "nothing was erased");
    assert!(!backend.target.log.contains(&"halt"), "{:?}", backend.target.log);
}

/// And the same bound reached the way a real deploy would reach it: an image longer than the
/// fitted array, starting where every image starts.
#[test]
fn a_sam4l_image_longer_than_the_fitted_array_is_refused_through_the_contract() {
    let bytes = vec![0xa5u8; L4W_PAGE * (L4W_PAGES + 1)];
    let mut backend = SamProbe::new(FakeSam4l::new(), crate::SamFamily::Sam4l, SAM_TEST_MECHANISM);
    let error = flash(
        &mut backend,
        &Image { bytes: &bytes, base: 0 },
        VerifyPolicy::ReadBack,
        &Allow::Any,
    )
    .expect_err("the image is a page longer than the part");

    assert!(format!("{error}").contains("32 KB"), "{error}");
    assert!(backend.target.flash.iter().all(|byte| *byte == 0xff), "nothing was erased");
}

/// An image that starts inside a page is refused rather than rounded.
///
/// **NEITHER ROUNDING IS SAFE.** Erasing the page the image starts inside destroys what is in front
/// of it; not erasing it leaves bits that cannot be set. A page is both the erase granule and the
/// write unit here, so there is no third option to fall back on.
#[test]
fn a_sam4l_image_that_starts_inside_a_page_is_refused_rather_than_rounded() {
    let bytes = vec![0xa5u8; 16];
    let mut backend = SamProbe::new(FakeSam4l::new(), crate::SamFamily::Sam4l, SAM_TEST_MECHANISM);
    let error = backend
        .erase(&Image { bytes: &bytes, base: 4 })
        .expect_err("the base is not a page boundary");

    assert!(format!("{error}").contains("page is 512"), "{error}");
    assert!(backend.target.flash.iter().all(|byte| *byte == 0xff), "nothing was erased");
    assert!(!backend.target.log.contains(&"halt"), "{:?}", backend.target.log);
}

/// The part this route was measured on is accepted by FAMILY, and the member table does not name
/// it.
///
/// **THE EXACT-MEMBER LOOKUP MISSES THE ONE BOARD THAT CARRIES THIS FAMILY**, which is not a gap in
/// the table: `sam4_identify`'s SAM4L rows are the 48-pin ATSAM4LS parts that Atmel-42023H table
/// 9-1 tabulates, and a 100-pin LC part shares their CIDR while reporting a different EXID. So the
/// guard has to be the CIDR, which is what that crate documents, and the member is only the report.
#[test]
fn the_fitted_part_is_accepted_by_family_where_the_member_table_cannot_name_it() {
    assert_eq!(
        lamella_cmsis_dap_sam::sam4_identify(L4W_CIDR, L4W_EXID),
        None,
        "the ATSAM4LC8C is not on the member table, which is why the family guard exists"
    );
    let mut backend = SamProbe::new(FakeSam4l::new(), crate::SamFamily::Sam4l, SAM_TEST_MECHANISM);
    let identity = backend.identify().expect("the CIDR names the family");
    assert_eq!(identity.value, u64::from(L4W_CIDR));
    assert!(identity.what.contains("SAM4L"), "{}", identity.what);
    assert!(identity.what.contains("not this board"), "{}", identity.what);
}

/// An EEFC route pointed at a SAM4L refuses rather than driving it.
///
/// **`sam4_identify` WILL NAME A SAM4L TO A CALLER DRIVING AN EEFC**, and its own table carries a
/// warning saying so in words. While the identify here consulted that lookup alone, a SAM4L
/// satisfied the EEFC route -- and an EEFC route drives `0x400E0A00`, where a SAM4L's flash
/// controller is not. Naming the families a route drives is what turns that warning into a refusal.
#[test]
fn an_eefc_route_refuses_a_sam4l_rather_than_driving_it() {
    let mut target = FakeSam4l::new();
    target.exid = L4W_LS8_EXID;
    assert!(
        lamella_cmsis_dap_sam::sam4_identify(L4W_CIDR, L4W_LS8_EXID).is_some(),
        "an ATSAM4LS8 is on the member table, so a member-only guard would have driven it"
    );

    let mut backend = SamProbe::new(target, crate::SamFamily::Sam4Eefc, SAM_TEST_MECHANISM);
    let error = backend.identify().expect_err("a SAM4L is not an EEFC part");
    assert!(format!("{error}").contains("single-plane SAM4 EEFC"), "{error}");

    let mut ls8 = FakeSam4l::new();
    ls8.exid = L4W_LS8_EXID;
    let identity = SamProbe::new(ls8, crate::SamFamily::Sam4l, SAM_TEST_MECHANISM)
        .identify()
        .expect("the SAM4L route drives a SAM4L");
    assert!(identity.what.contains("ATSAM4LS8"), "{}", identity.what);
}

/// The fake really would catch a missing Clear Page Buffer, which is what makes the test above a
/// check rather than a restatement.
///
/// **A FAKE THAT RESET THE BUFFER ON A WRITE WOULD PASS THE BROKEN DRIVER TOO**, so the model has
/// to be exercised directly: fill, commit, fill again with a different pattern, commit again --
/// with no clear anywhere -- and the second page has to come out as the AND of the two.
#[test]
fn the_page_buffer_ands_and_a_write_does_not_reset_it() {
    let mut target = FakeSam4l::new();
    target.write_words(0, &[0xf0f0_f0f0; L4W_PAGE / 4]).unwrap();
    target.write_word(L4W_FCMD, L4W_KEY | (0 << 8) | L4W_CMD_WP).unwrap();
    target
        .write_words(L4W_PAGE as u32, &[0x0f0f_0f0f; L4W_PAGE / 4])
        .unwrap();
    target.write_word(L4W_FCMD, L4W_KEY | (1 << 8) | L4W_CMD_WP).unwrap();

    assert_eq!(target.flash[0], 0xf0, "the first page is correct even so");
    assert_eq!(
        target.flash[L4W_PAGE], 0x00,
        "and the second is the AND of both -- the corruption a missing clear produces"
    );
}

/// And the key is checked, so a driver carrying an EEFC's `0x5A` reaches nothing.
#[test]
fn the_fake_refuses_the_eefc_key_the_way_the_part_does() {
    let mut target = FakeSam4l::new();
    target.write_word(L4W_FCMD, (0x5a << 24) | L4W_CMD_EP).unwrap();
    assert_eq!(target.log, vec!["bad_key"], "{:?}", target.log);
}

/// The address a route writes an image from and the address its backend expects are ONE fact.
///
/// **TWO ANSWERS TO IT DISAGREE SILENTLY, AND BOTH LOOK RIGHT IN ISOLATION.**
/// `Programmer::flash_base` answers for the route, the backend answers for itself, and
/// `lamella_flash_backend::flash` compares them before it erases anything -- so a mismatch refuses
/// every write as `WrongBase` while nothing on either side is visibly wrong. An EEFC part's base is
/// `0x00400000` and an NVMCTRL part's is `SAM_NVMCTRL_FLASH_BASE`, so a rule stated per VENDOR
/// rather than per FAMILY gets one of them wrong and reads as though it covers both.
///
/// NOTE: this check cannot be made by reading either side, because each is self-consistent on its
/// own. It has to compare them.
#[test]
fn a_routes_flash_base_is_the_one_its_backend_expects() {
    use crate::{PROGRAMMING, Programmer};
    let mut checked = 0;
    for row in PROGRAMMING {
        let Programmer::EdbgOnboard { family, .. } = row.programmer else {
            continue;
        };
        let backend = SamProbe::new(FakeSam4l::new(), family, SAM_TEST_MECHANISM);
        assert_eq!(
            row.programmer.flash_base(),
            backend.flash_base(),
            "{} builds its image at {:#010x} and its backend writes from {:#010x}",
            row.board,
            row.programmer.flash_base(),
            backend.flash_base()
        );
        checked += 1;
    }
    assert!(checked > 0, "no EDBG row was checked, so this proves nothing");
    assert_ne!(
        crate::SamFamily::Sam4Eefc.flash_base(),
        crate::SamFamily::Samd21.flash_base(),
        "if these agreed, the loop above would pass with the defect still in"
    );
    assert_eq!(crate::SamFamily::Sam4l.flash_base(), 0, "the SAM4L maps its array at zero");
}


const L3X_FCR: u32 = 0x04;
const L3X_FSR: u32 = 0x08;
const L3X_FRR: u32 = 0x0c;
const L3X_KEY: u32 = 0x5a << 24;
const L3X_CMD_GETD: u32 = 0x00;
const L3X_CMD_EWP: u32 = 0x03;
const L3X_CMD_EA: u32 = 0x05;
const L3X_CMD_GLB: u32 = 0x0a;
const L3X_CMD_GGPB: u32 = 0x0d;
const L3X_FRDY: u32 = 1 << 0;
const L3X_FCMDE: u32 = 1 << 1;
const L3X_FLOCKE: u32 = 1 << 2;
/// The value an Arduino Due answers, and the part Atmel-11057 table 29-1 names for it.
const L3X_CIDR: u32 = 0x285e_0a60;
const L3X_PAGES_PER_PLANE: usize = SAM3X_PLANE_SIZE as usize / SAM3X_PAGE;

/// A SAM3X8E modelled as TWO controllers over TWO planes, with a latch buffer each.
struct FakeSam3x {
    log: Vec<String>,
    /// Both planes end to end, indexed from `SAM3X_FLASH0_BASE`, exactly as they are mapped.
    flash: Vec<u8>,
    /// One write latch per controller. **A fill goes here and NOT to the array**, which is what
    /// makes filling one plane's latch while commanding the other a silent success on the part.
    latch: [Vec<u8>; 2],
    /// Words a `GETD` / `GLB` / `GGPB` left for successive `FRR` reads to collect.
    results: Vec<u32>,
    /// Lock bits per controller.
    locks: [u32; 2],
    gpnvm: u32,
    /// Latched `FCMDE` / `FLOCKE`, reported on the next `FSR` read.
    errors: u32,
    cidr: u32,
}

impl FakeSam3x {
    fn new() -> Self {
        FakeSam3x {
            log: Vec::new(),
            flash: vec![0xff; SAM3X_PLANE_SIZE as usize * 2],
            latch: [vec![0xff; SAM3X_PAGE], vec![0xff; SAM3X_PAGE]],
            results: Vec::new(),
            locks: [0, 0],
            gpnvm: 0,
            errors: 0,
            cidr: L3X_CIDR,
        }
    }

    /// Which controller a user-interface address belongs to, or `None` for anything else.
    fn controller(address: u32) -> Option<usize> {
        match address & !0xff {
            SAM3X_EEFC0 => Some(0),
            SAM3X_EEFC1 => Some(1),
            _ => None,
        }
    }

    /// The byte the plane holds, so a test can read the array without going through the cache-free
    /// read path and its offsets.
    fn at(&self, address: u32) -> u8 {
        self.flash[(address - SAM3X_FLASH0_BASE) as usize]
    }
}

impl TargetAccess for FakeSam3x {
    fn connect(&mut self) -> Result<(), ProbeError> {
        Ok(())
    }
    fn read_idcode(&mut self) -> Result<u32, ProbeError> {
        Ok(0x2ba0_1477)
    }
    fn init_mem(&mut self) -> Result<(), ProbeError> {
        Ok(())
    }

    fn read_word(&mut self, address: u32) -> Result<u32, ProbeError> {
        if address == SAM3X_CHIPID_CIDR {
            return Ok(self.cidr);
        }
        if address == SAM4_CHIPID_CIDR || address == SAM4_CHIPID_EXID {
            return Err(ProbeError::Device("a SAM4 chip-id read on a SAM3X"));
        }
        if let Some(unit) = FakeSam3x::controller(address & !0xf) {
            let _ = unit;
            return match address & 0xf {
                offset if offset == L3X_FSR => {
                    let fsr = L3X_FRDY | self.errors;
                    self.errors = 0;
                    Ok(fsr)
                }
                offset if offset == L3X_FRR => Ok(if self.results.is_empty() {
                    0
                } else {
                    self.results.remove(0)
                }),
                _ => Err(ProbeError::Device("an EEFC register this fake does not model")),
            };
        }
        let at = address.wrapping_sub(SAM3X_FLASH0_BASE) as usize;
        let mut word = [0u8; 4];
        word.copy_from_slice(
            self.flash
                .get(at..at + 4)
                .ok_or(ProbeError::Device("read of an address this fake does not model"))?,
        );
        Ok(u32::from_le_bytes(word))
    }

    fn write_word(&mut self, address: u32, value: u32) -> Result<(), ProbeError> {
        if let Some(unit) = FakeSam3x::controller(address & !0xf) {
            if address & 0xf != L3X_FCR {
                return Err(ProbeError::Device("a write to an EEFC register this fake refuses"));
            }
            if value & 0xff00_0000 != L3X_KEY {
                self.errors |= L3X_FCMDE;
                self.log.push(String::from("bad_key"));
                return Ok(());
            }
            let arg = (value >> 8) & 0xffff;
            match value & 0xff {
                L3X_CMD_GETD => {
                    self.results = vec![
                        1,
                        SAM3X_PLANE_SIZE,
                        SAM3X_PAGE as u32,
                        1,
                        SAM3X_PLANE_SIZE,
                    ];
                    self.log.push(format!("getd{unit}"));
                }
                L3X_CMD_GLB => {
                    self.results = vec![self.locks[unit]];
                    self.log.push(format!("glb{unit}"));
                }
                L3X_CMD_GGPB => {
                    self.results = vec![self.gpnvm];
                    self.log.push(String::from("ggpb"));
                }
                L3X_CMD_EA => {
                    let base = unit * SAM3X_PLANE_SIZE as usize;
                    self.flash[base..base + SAM3X_PLANE_SIZE as usize]
                        .iter_mut()
                        .for_each(|byte| *byte = 0xff);
                    self.log.push(format!("erase_all{unit}"));
                }
                L3X_CMD_EWP => {
                    let page = arg as usize;
                    if page >= L3X_PAGES_PER_PLANE {
                        self.errors |= L3X_FCMDE;
                        self.log.push(format!("ewp{unit}:{page}:out-of-plane"));
                        return Ok(());
                    }
                    if self.locks[unit] & (1 << (page as u32 / SAM3X_LOCK_PAGES)) != 0 {
                        self.errors |= L3X_FLOCKE;
                        self.log.push(format!("ewp{unit}:{page}:locked"));
                        return Ok(());
                    }
                    let at = unit * SAM3X_PLANE_SIZE as usize + page * SAM3X_PAGE;
                    self.flash[at..at + SAM3X_PAGE].copy_from_slice(&self.latch[unit]);
                    self.log.push(format!("ewp{unit}:{page}"));
                }
                _ => self.errors |= L3X_FCMDE,
            }
            return Ok(());
        }
        let at = address.wrapping_sub(SAM3X_FLASH0_BASE) as usize;
        if at >= self.flash.len() {
            return Err(ProbeError::Device("write to an address this fake does not model"));
        }
        let unit = at / SAM3X_PLANE_SIZE as usize;
        let within = at % SAM3X_PAGE;
        self.latch[unit][within..within + 4].copy_from_slice(&value.to_le_bytes());
        Ok(())
    }

    fn read_words_into(&mut self, address: u32, out: &mut [u32]) -> Result<(), ProbeError> {
        for (index, slot) in out.iter_mut().enumerate() {
            *slot = self.read_word(address + (index * 4) as u32)?;
        }
        Ok(())
    }

    fn write_words(&mut self, address: u32, words: &[u32]) -> Result<(), ProbeError> {
        for (index, word) in words.iter().enumerate() {
            self.write_word(address + (index * 4) as u32, *word)?;
        }
        Ok(())
    }

    fn halt(&mut self) -> Result<(), ProbeError> {
        self.log.push(String::from("halt"));
        Ok(())
    }
    fn reset_and_run(&mut self) -> Result<(), ProbeError> {
        self.log.push(String::from("reset_and_run"));
        Ok(())
    }

    fn read_byte(&mut self, _address: u32) -> Result<u8, ProbeError> {
        unreachable!("byte access is not part of the flashing path")
    }
    fn write_byte(&mut self, _address: u32, _value: u8) -> Result<(), ProbeError> {
        unreachable!("byte access is not part of the flashing path")
    }
    fn read_halfword(&mut self, _address: u32) -> Result<u16, ProbeError> {
        unreachable!("halfword access is not part of the flashing path")
    }
    fn write_halfword(&mut self, _address: u32, _value: u16) -> Result<(), ProbeError> {
        unreachable!("halfword access is not part of the flashing path")
    }
    fn resume(&mut self) -> Result<(), ProbeError> {
        unreachable!("a flashing backend leaves the part through reset_and_run, not resume")
    }
    fn step(&mut self) -> Result<(), ProbeError> {
        unreachable!("run control is not the flashing path")
    }
    fn is_halted(&mut self) -> Result<bool, ProbeError> {
        unreachable!("run control is not the flashing path")
    }
    fn wait_halted(&mut self) -> Result<(), ProbeError> {
        unreachable!("run control is not the flashing path")
    }
    fn reset_and_halt(&mut self) -> Result<(), ProbeError> {
        unreachable!("run control is not the flashing path")
    }
    fn set_reset(&mut self, _assert: bool) -> Result<u8, ProbeError> {
        unreachable!("a flashing backend does not drive the reset line directly")
    }
    fn read_core_reg(&mut self, _selector: u8) -> Result<u32, ProbeError> {
        unreachable!("core registers are not the flashing path")
    }
    fn write_core_reg(&mut self, _selector: u8, _value: u32) -> Result<(), ProbeError> {
        unreachable!("core registers are not the flashing path")
    }
    fn arm_reset_catch(&mut self) -> Result<(), ProbeError> {
        unreachable!("reset catch is not the flashing path")
    }
    fn disarm_reset_catch(&mut self) -> Result<(), ProbeError> {
        unreachable!("reset catch is not the flashing path")
    }
    fn set_breakpoint(&mut self, _address: u32) -> Result<(), ProbeError> {
        unreachable!("breakpoints are not the flashing path")
    }
    fn clear_breakpoint(&mut self) -> Result<(), ProbeError> {
        unreachable!("breakpoints are not the flashing path")
    }
    fn set_breakpoints(&mut self, _addresses: &[u32]) -> Result<(), ProbeError> {
        unreachable!("breakpoints are not the flashing path")
    }
    fn call_target(
        &mut self,
        _address: u32,
        _args: &[u32],
        _frame: &lamella_probe_core::CallFrame,
    ) -> Result<u32, ProbeError> {
        unreachable!("a flashing backend does not run code on the target")
    }
}

fn sam3x_backend() -> SamProbe<FakeSam3x> {
    SamProbe::new(FakeSam3x::new(), crate::SamFamily::Sam3x, SAM_TEST_MECHANISM)
}

/// The contract's order on a part with no erase command, and the erase step really is empty.
///
/// **THE ABSENCE IS THE ASSERTION.** Every other backend in this module erases in `erase`; here the
/// only thing that step may leave behind is the halt, because a pre-erase pass would erase each
/// page twice and the one bulk erase this part offers takes a whole 256 KB plane.
#[test]
fn the_sam3x_erase_step_erases_nothing_and_the_write_does_it_per_page() {
    let bytes = vec![0x5au8; SAM3X_PAGE * 2];
    let mut backend = sam3x_backend();
    let report = flash(
        &mut backend,
        &Image { bytes: &bytes, base: SAM3X_FLASH0_BASE },
        VerifyPolicy::ReadBack,
        &Allow::Any,
    )
    .expect("the sequence runs");

    assert!(matches!(report.verification, Verification::ReadBack));
    assert_eq!(
        backend.target.log,
        vec![
            "ggpb", "getd0", "glb0", "halt",
            "ggpb", "getd0", "glb0",
            "ewp0:0",
            "ewp0:1",
            "reset_and_run",
        ],
        "{:?}",
        backend.target.log
    );
    assert!(
        !backend.target.log.iter().any(|step| step.starts_with("erase_all")),
        "{:?}",
        backend.target.log
    );
    assert_eq!(
        backend.target.log.iter().filter(|step| step.starts_with("ewp")).count(),
        2,
        "two pages, written once each: {:?}",
        backend.target.log
    );
    assert_eq!(backend.target.at(SAM3X_FLASH0_BASE), 0x5a);
}

/// An image spanning the plane join is two command sequences, and the page number restarts.
///
/// **THIS IS THE ONE A SINGLE-PLANE TEST CANNOT REACH.** A walk that kept counting pages across the
/// join would command page 1024 of a plane that has 1024 -- and the SAM4S twin of this driver takes
/// `first_page` relative to the PLANE, so the mistake is one argument away in code that reads
/// correctly.
#[test]
fn an_image_across_the_plane_join_switches_controller_and_restarts_the_page_number() {
    let base = SAM3X_FLASH0_BASE + SAM3X_PLANE_SIZE - SAM3X_PAGE as u32;
    let bytes = vec![0xc3u8; SAM3X_PAGE * 2];
    let image = Image { bytes: &bytes, base };
    let mut backend = sam3x_backend();
    backend.erase(&image).expect("the guards pass");
    backend.program(&image).expect("both planes take their share");

    let last_of_plane0 = L3X_PAGES_PER_PLANE - 1;
    assert!(
        backend.target.log.contains(&format!("ewp0:{last_of_plane0}")),
        "{:?}",
        backend.target.log
    );
    assert!(backend.target.log.contains(&String::from("ewp1:0")), "{:?}", backend.target.log);
    assert!(
        !backend.target.log.iter().any(|step| step.contains("out-of-plane")),
        "{:?}",
        backend.target.log
    );
    assert_eq!(backend.target.at(base), 0xc3);
    assert_eq!(backend.target.at(SAM3X_FLASH1_BASE), 0xc3);
    assert_eq!(backend.target.at(base - 1), 0xff, "the page before it is untouched");
}

/// The plane-swap fuse is READ, and a part carrying it set is refused rather than driven.
///
/// **NO ADDRESS REVEALS THIS.** With `GPNVM2` set the two windows exchange controllers, so a route
/// that assumed the reset mapping fills one plane's latch and commits the other -- and every
/// command completes, so the failure is a successful write to the wrong place.
#[test]
fn a_swapped_plane_fuse_is_refused_before_anything_is_written() {
    let bytes = vec![0x5au8; SAM3X_PAGE];
    let mut backend = sam3x_backend();
    backend.target.gpnvm = 1 << SAM3X_GPNVM_PLANE_SWAP;
    let error = flash(
        &mut backend,
        &Image { bytes: &bytes, base: SAM3X_FLASH0_BASE },
        VerifyPolicy::ReadBack,
        &Allow::Any,
    )
    .expect_err("the planes are swapped");

    assert!(format!("{error}").contains("plane swap"), "{error}");
    assert!(backend.target.flash.iter().all(|byte| *byte == 0xff), "nothing was written");
    assert!(
        !backend.target.log.contains(&String::from("halt")),
        "{:?}",
        backend.target.log
    );
}

/// A locked region is refused before the write, and this route does not clear the lock.
///
/// **THERE IS A CLEAR-LOCK COMMAND AND NOT SENDING IT IS THE DECISION.** A lock bit is
/// non-volatile, so clearing one changes somebody's board permanently -- which is not what typing
/// `flash` asked for.
#[test]
fn a_locked_sam3x_region_is_refused_rather_than_unlocked() {
    let bytes = vec![0x5au8; SAM3X_PAGE];
    let mut backend = sam3x_backend();
    backend.target.locks[0] = 0b1;
    let error = flash(
        &mut backend,
        &Image { bytes: &bytes, base: SAM3X_FLASH0_BASE },
        VerifyPolicy::ReadBack,
        &Allow::Any,
    )
    .expect_err("region 0 of plane 0 is locked");

    assert!(format!("{error}").contains("lock regions [0]"), "{error}");
    assert!(backend.target.flash.iter().all(|byte| *byte == 0xff), "nothing was written");
    assert!(!backend.target.log.contains(&String::from("clb0")), "no lock was cleared");
}

/// A locked region in the OTHER plane does not refuse a write that never reaches it.
#[test]
fn a_lock_in_the_far_plane_does_not_refuse_an_image_in_the_near_one() {
    let bytes = vec![0x5au8; SAM3X_PAGE];
    let mut backend = sam3x_backend();
    backend.target.locks[1] = 0xffff;
    flash(
        &mut backend,
        &Image { bytes: &bytes, base: SAM3X_FLASH0_BASE },
        VerifyPolicy::ReadBack,
        &Allow::Any,
    )
    .expect("the image is entirely in plane 0, whose controller reports no locks");
    assert!(!backend.target.log.contains(&String::from("glb1")), "{:?}", backend.target.log);
}

/// An image longer than the two planes together is refused before the write.
#[test]
fn a_sam3x_image_past_the_second_plane_is_refused() {
    let bytes = vec![0x5au8; SAM3X_PLANE_SIZE as usize * 2 + SAM3X_PAGE];
    let mut backend = sam3x_backend();
    let error = flash(
        &mut backend,
        &Image { bytes: &bytes, base: SAM3X_FLASH0_BASE },
        VerifyPolicy::ReadBack,
        &Allow::Any,
    )
    .expect_err("a page more than the part holds");

    assert!(format!("{error}").contains("512 KB"), "{error}");
    assert!(backend.target.flash.iter().all(|byte| *byte == 0xff), "nothing was written");
}

/// The SAM3X reads its OWN chip id, at its own address.
///
/// **A SAM4's `CHIPID` IS 0x200 LOWER AND THE FAKE REFUSES IT**, so a reader that carried the SAM4
/// constant across fails here rather than decoding a TWI controller as a chip id.
#[test]
fn the_sam3x_identify_reads_its_own_chipid_address() {
    let identity = sam3x_backend().identify().expect("the part answers");
    assert_eq!(identity.value, u64::from(L3X_CIDR));
    assert!(identity.what.contains("ATSAM3X8E"), "{}", identity.what);

    let mut other = FakeSam3x::new();
    other.cidr = 0x1234_5678;
    let error = SamProbe::new(other, crate::SamFamily::Sam3x, SAM_TEST_MECHANISM)
        .identify()
        .expect_err("not a SAM3X this tree knows");
    assert!(format!("{error}").contains("0x12345678"), "{error}");
}

/// The Due's row is the external-probe variant, and it declares the ambiguity rather than hiding
/// it.
///
/// **AN `EdbgOnboard` ROW WOULD HAVE COMPILED AND BEEN WRONG.** Its `usb_identity` narrows to a
/// debugger soldered to the board, and this board has none -- so a route that answered `Some` here
/// would filter a bench down to whichever kit happened to match and write the Due's image into it.
#[test]
fn the_due_is_routed_by_an_external_probe_and_says_so() {
    use crate::{PROGRAMMING, Programmer};
    let row = PROGRAMMING
        .iter()
        .find(|row| row.board == "arduino-due")
        .expect("arduino-due has a routing row");
    assert!(
        matches!(
            row.programmer,
            Programmer::SamExternalProbe { family: crate::SamFamily::Sam3x }
        ),
        "{:?}",
        row.programmer
    );
    assert_eq!(row.programmer.usb_identity(), None, "this board has no debugger to filter on");
    assert_eq!(row.alternate, None, "and no second route to offer");
    assert_eq!(row.programmer.flash_base(), SAM3X_FLASH0_BASE);
    assert_ne!(row.programmer.flash_base(), crate::SamFamily::Sam4Eefc.flash_base());
    assert_ne!(row.programmer.flash_base(), crate::SamFamily::Samd21.flash_base());
}

/// The fake really would catch a page number that kept counting across the join.
///
/// **THE CONTROL FOR THE CROSSING TEST**, and it is not optional: a fake that wrapped a page number
/// into the next plane -- or into the start of its own -- would pass a route that never switched
/// controller, writing plane 0 twice and reporting success both times.
#[test]
fn the_fake_sam3x_refuses_a_page_number_outside_its_plane() {
    let mut target = FakeSam3x::new();
    let page = L3X_PAGES_PER_PLANE as u32;
    target
        .write_word(SAM3X_EEFC0 + L3X_FCR, L3X_KEY | (page << 8) | L3X_CMD_EWP)
        .unwrap();
    assert!(
        target.log.iter().any(|step| step.ends_with("out-of-plane")),
        "{:?}",
        target.log
    );
    let fsr = target.read_word(SAM3X_EEFC0 + L3X_FSR).unwrap();
    assert_eq!(fsr & L3X_FCMDE, L3X_FCMDE, "{fsr:#x}");
    assert!(target.flash.iter().all(|byte| *byte == 0xff), "and nothing was written");
}

/// And the control for the missing erase pass: `EA` really does take flash the image never covers.
///
/// **THIS IS WHY "ADAPT THE ERASE STEP" IS NOT AVAILABLE ON THIS PART.** The only erase command
/// below a whole plane is the one inside `EWP`, so an erase pass written for any other family here
/// would have to reach for `EA` -- and `EA` is 256 KB.
#[test]
fn erase_all_on_this_part_takes_the_whole_plane_and_not_the_image() {
    let mut target = FakeSam3x::new();
    let far = SAM3X_FLASH0_BASE + SAM3X_PLANE_SIZE - 4;
    target.flash[(far - SAM3X_FLASH0_BASE) as usize] = 0x11;

    target
        .write_word(SAM3X_EEFC0 + L3X_FCR, L3X_KEY | L3X_CMD_EA)
        .unwrap();

    assert_eq!(target.at(far), 0xff, "an erase-all pass would have taken this with it");
}


const L4S_FCR: u32 = 0x04;
const L4S_FSR: u32 = 0x08;
const L4S_FRR: u32 = 0x0c;
const L4S_KEY: u32 = 0x5a << 24;
const L4S_CMD_GETD: u32 = 0x00;
const L4S_CMD_WP: u32 = 0x01;
const L4S_CMD_EPA: u32 = 0x07;
const L4S_CMD_GLB: u32 = 0x0a;
const L4S_CMD_GGPB: u32 = 0x0d;
const L4S_FRDY: u32 = 1 << 0;
const L4S_FCMDE: u32 = 1 << 1;
const L4S_FLOCKE: u32 = 1 << 2;
/// The ATSAM4SD32C's `CHIPID_CIDR`, from the table this tree already carries.
const L4S_CIDR: u32 = 0x29a7_0ee0;
/// The ATSAM4SD32C's plane, which is the only geometry this route accepts.
///
/// **A SMALLER ONE WOULD BE REFUSED AND SHOULD BE**: the plan derives the second plane's window
/// from the first plane's own report and checks it against the published `SAM4S_FLASH1_BASE`, so a
/// made-up plane size lands somewhere the part does not have. The tests carry the 2 MB array that
/// costs, rather than testing a geometry no part has.
const L4S_PLANE: u32 = SAM4S_FLASH1_BASE - SAM4S_FLASH0_BASE;

/// A dual-plane SAM4S modelled as TWO controllers over TWO planes, with a latch buffer each.
struct FakeSam4sDual {
    log: Vec<String>,
    /// Both planes end to end, indexed from `SAM4S_FLASH0_BASE`, as they are mapped.
    flash: Vec<u8>,
    latch: [Vec<u8>; 2],
    results: Vec<u32>,
    /// 128 lock bits per controller.
    locks: [[u32; 4]; 2],
    gpnvm: u32,
    errors: u32,
    plane_size: u32,
    /// What each controller reports for `FL_NB_PLANE`.
    planes: u32,
    page_size: u32,
}

impl FakeSam4sDual {
    fn new(plane_size: u32) -> Self {
        FakeSam4sDual {
            log: Vec::new(),
            flash: vec![0xff; plane_size as usize * 2],
            latch: [vec![0xff; SAM4S_PAGE], vec![0xff; SAM4S_PAGE]],
            results: Vec::new(),
            locks: [[0; 4]; 2],
            gpnvm: 0,
            errors: 0,
            plane_size,
            planes: 2,
            page_size: SAM4S_PAGE as u32,
        }
    }

    fn controller(address: u32) -> Option<usize> {
        match address & !0xff {
            SAM4S_EEFC0 => Some(0),
            SAM4S_EEFC1 => Some(1),
            _ => None,
        }
    }

    fn at(&self, address: u32) -> u8 {
        self.flash[(address - SAM4S_FLASH0_BASE) as usize]
    }
}

impl TargetAccess for FakeSam4sDual {
    fn connect(&mut self) -> Result<(), ProbeError> {
        Ok(())
    }
    fn read_idcode(&mut self) -> Result<u32, ProbeError> {
        Ok(0x2ba0_1477)
    }
    fn init_mem(&mut self) -> Result<(), ProbeError> {
        Ok(())
    }

    fn read_word(&mut self, address: u32) -> Result<u32, ProbeError> {
        if address == SAM4_CHIPID_CIDR {
            return Ok(L4S_CIDR);
        }
        if address == SAM4_CHIPID_EXID {
            return Ok(0);
        }
        if FakeSam4sDual::controller(address & !0xf).is_some() {
            return match address & 0xf {
                offset if offset == L4S_FSR => {
                    let fsr = L4S_FRDY | self.errors;
                    self.errors = 0;
                    Ok(fsr)
                }
                offset if offset == L4S_FRR => Ok(if self.results.is_empty() {
                    0
                } else {
                    self.results.remove(0)
                }),
                _ => Err(ProbeError::Device("an EEFC register this fake does not model")),
            };
        }
        let at = address.wrapping_sub(SAM4S_FLASH0_BASE) as usize;
        let mut word = [0u8; 4];
        word.copy_from_slice(
            self.flash
                .get(at..at + 4)
                .ok_or(ProbeError::Device("read of an address this fake does not model"))?,
        );
        Ok(u32::from_le_bytes(word))
    }

    fn write_word(&mut self, address: u32, value: u32) -> Result<(), ProbeError> {
        if let Some(unit) = FakeSam4sDual::controller(address & !0xf) {
            if address & 0xf != L4S_FCR {
                return Err(ProbeError::Device("a write to an EEFC register this fake refuses"));
            }
            if value & 0xff00_0000 != L4S_KEY {
                self.errors |= L4S_FCMDE;
                self.log.push(String::from("bad_key"));
                return Ok(());
            }
            let arg = (value >> 8) & 0xffff;
            let pages_in_plane = self.plane_size / self.page_size;
            let page_bytes = self.page_size as usize;
            match value & 0xff {
                L4S_CMD_GETD => {
                    self.results = vec![
                        1,
                        self.plane_size,
                        self.page_size,
                        self.planes,
                        self.plane_size,
                    ];
                    self.log.push(format!("getd{unit}"));
                }
                L4S_CMD_GLB => {
                    self.results = self.locks[unit].to_vec();
                    self.log.push(format!("glb{unit}"));
                }
                L4S_CMD_GGPB => {
                    self.results = vec![self.gpnvm];
                    self.log.push(String::from("ggpb"));
                }
                L4S_CMD_EPA => {
                    let page = (arg & !0x7) as usize;
                    let code = arg & 0x3;
                    if code != 1 {
                        self.errors |= L4S_FCMDE;
                        self.log.push(format!("epa{unit}:{page}:code{code}"));
                        return Ok(());
                    }
                    if page + SAM4S_ERASE_PAGES as usize > pages_in_plane as usize {
                        self.errors |= L4S_FCMDE;
                        self.log.push(format!("epa{unit}:{page}:out-of-plane"));
                        return Ok(());
                    }
                    if self.locks[unit][(page as u32 / SAM4S_LOCK_PAGES / 32) as usize]
                        & (1 << (page as u32 / SAM4S_LOCK_PAGES % 32))
                        != 0
                    {
                        self.errors |= L4S_FLOCKE;
                        self.log.push(format!("epa{unit}:{page}:locked"));
                        return Ok(());
                    }
                    let at = unit * self.plane_size as usize + page * page_bytes;
                    let span = SAM4S_ERASE_PAGES as usize * page_bytes;
                    self.flash[at..at + span].iter_mut().for_each(|byte| *byte = 0xff);
                    self.log.push(format!("epa{unit}:{page}"));
                }
                L4S_CMD_WP => {
                    let page = arg as usize;
                    if page >= pages_in_plane as usize {
                        self.errors |= L4S_FCMDE;
                        self.log.push(format!("wp{unit}:{page}:out-of-plane"));
                        return Ok(());
                    }
                    let at = unit * self.plane_size as usize + page * page_bytes;
                    for (cell, fill) in self.flash[at..at + page_bytes]
                        .iter_mut()
                        .zip(self.latch[unit].iter())
                    {
                        *cell &= *fill;
                    }
                    self.log.push(format!("wp{unit}:{page}"));
                }
                _ => self.errors |= L4S_FCMDE,
            }
            return Ok(());
        }
        let at = address.wrapping_sub(SAM4S_FLASH0_BASE) as usize;
        if at >= self.flash.len() {
            return Err(ProbeError::Device("write to an address this fake does not model"));
        }
        let unit = at / self.plane_size as usize;
        let within = at % SAM4S_PAGE;
        self.latch[unit][within..within + 4].copy_from_slice(&value.to_le_bytes());
        Ok(())
    }

    fn read_words_into(&mut self, address: u32, out: &mut [u32]) -> Result<(), ProbeError> {
        for (index, slot) in out.iter_mut().enumerate() {
            *slot = self.read_word(address + (index * 4) as u32)?;
        }
        Ok(())
    }

    fn write_words(&mut self, address: u32, words: &[u32]) -> Result<(), ProbeError> {
        for (index, word) in words.iter().enumerate() {
            self.write_word(address + (index * 4) as u32, *word)?;
        }
        Ok(())
    }

    fn halt(&mut self) -> Result<(), ProbeError> {
        self.log.push(String::from("halt"));
        Ok(())
    }
    fn reset_and_run(&mut self) -> Result<(), ProbeError> {
        self.log.push(String::from("reset_and_run"));
        Ok(())
    }

    fn read_byte(&mut self, _address: u32) -> Result<u8, ProbeError> {
        unreachable!("byte access is not part of the flashing path")
    }
    fn write_byte(&mut self, _address: u32, _value: u8) -> Result<(), ProbeError> {
        unreachable!("byte access is not part of the flashing path")
    }
    fn read_halfword(&mut self, _address: u32) -> Result<u16, ProbeError> {
        unreachable!("halfword access is not part of the flashing path")
    }
    fn write_halfword(&mut self, _address: u32, _value: u16) -> Result<(), ProbeError> {
        unreachable!("halfword access is not part of the flashing path")
    }
    fn resume(&mut self) -> Result<(), ProbeError> {
        unreachable!("a flashing backend leaves the part through reset_and_run, not resume")
    }
    fn step(&mut self) -> Result<(), ProbeError> {
        unreachable!("run control is not the flashing path")
    }
    fn is_halted(&mut self) -> Result<bool, ProbeError> {
        unreachable!("run control is not the flashing path")
    }
    fn wait_halted(&mut self) -> Result<(), ProbeError> {
        unreachable!("run control is not the flashing path")
    }
    fn reset_and_halt(&mut self) -> Result<(), ProbeError> {
        unreachable!("run control is not the flashing path")
    }
    fn set_reset(&mut self, _assert: bool) -> Result<u8, ProbeError> {
        unreachable!("a flashing backend does not drive the reset line directly")
    }
    fn read_core_reg(&mut self, _selector: u8) -> Result<u32, ProbeError> {
        unreachable!("core registers are not the flashing path")
    }
    fn write_core_reg(&mut self, _selector: u8, _value: u32) -> Result<(), ProbeError> {
        unreachable!("core registers are not the flashing path")
    }
    fn arm_reset_catch(&mut self) -> Result<(), ProbeError> {
        unreachable!("reset catch is not the flashing path")
    }
    fn disarm_reset_catch(&mut self) -> Result<(), ProbeError> {
        unreachable!("reset catch is not the flashing path")
    }
    fn set_breakpoint(&mut self, _address: u32) -> Result<(), ProbeError> {
        unreachable!("breakpoints are not the flashing path")
    }
    fn clear_breakpoint(&mut self) -> Result<(), ProbeError> {
        unreachable!("breakpoints are not the flashing path")
    }
    fn set_breakpoints(&mut self, _addresses: &[u32]) -> Result<(), ProbeError> {
        unreachable!("breakpoints are not the flashing path")
    }
    fn call_target(
        &mut self,
        _address: u32,
        _args: &[u32],
        _frame: &lamella_probe_core::CallFrame,
    ) -> Result<u32, ProbeError> {
        unreachable!("a flashing backend does not run code on the target")
    }
}

fn sam4s_dual_backend(plane_size: u32) -> SamProbe<FakeSam4sDual> {
    SamProbe::new(
        FakeSam4sDual::new(plane_size),
        crate::SamFamily::Sam4sDual,
        SAM_TEST_MECHANISM,
    )
}

/// The contract's order on the dual-plane part, with the erase and the write both per controller.
#[test]
fn the_dual_plane_sam4s_erases_in_blocks_then_writes_in_pages() {
    let bytes = vec![0x5au8; SAM4S_PAGE * 2];
    let mut backend = sam4s_dual_backend(L4S_PLANE);
    let report = flash(
        &mut backend,
        &Image { bytes: &bytes, base: SAM4S_FLASH0_BASE },
        VerifyPolicy::ReadBack,
        &Allow::Any,
    )
    .expect("the sequence runs");

    assert!(matches!(report.verification, Verification::ReadBack));
    assert_eq!(
        backend.target.log,
        vec![
            "ggpb", "getd0", "glb0",
            "halt", "epa0:0",
            "ggpb", "getd0", "glb0",
            "wp0:0", "wp0:1", "reset_and_run",
        ],
        "{:?}",
        backend.target.log
    );
    assert_eq!(backend.target.at(SAM4S_FLASH0_BASE), 0x5a);
}

/// An image across the plane join switches controller and restarts BOTH the page number and the
/// erase block number.
///
/// **THE ERASE HAS ITS OWN COUNTER AND IT IS THE ONE THAT WOULD GO UNNOTICED.** A write to a page
/// past the end of a plane is a command error; an erase past it takes eight pages of somewhere
/// else, and the image that follows lands on top of a region that reads correct afterwards.
#[test]
fn a_dual_plane_image_across_the_join_restarts_the_block_and_the_page() {
    let plane = L4S_PLANE;
    let block = SAM4S_PAGE as u32 * SAM4S_ERASE_PAGES;
    let base = SAM4S_FLASH0_BASE + plane - block;
    let bytes = vec![0xc3u8; block as usize * 2];
    let image = Image { bytes: &bytes, base };
    let mut backend = sam4s_dual_backend(plane);
    backend.erase(&image).expect("the guards pass");
    backend.program(&image).expect("both planes take their share");

    let last_block = (plane - block) / SAM4S_PAGE as u32;
    assert!(backend.target.log.contains(&format!("epa0:{last_block}")), "{:?}", backend.target.log);
    assert!(backend.target.log.contains(&String::from("epa1:0")), "{:?}", backend.target.log);
    assert!(backend.target.log.contains(&String::from("wp1:0")), "{:?}", backend.target.log);
    assert!(
        !backend.target.log.iter().any(|step| step.contains("out-of-plane")),
        "{:?}",
        backend.target.log
    );
    assert_eq!(backend.target.at(base), 0xc3);
    assert_eq!(backend.target.at(SAM4S_FLASH0_BASE + plane), 0xc3);
    assert_eq!(backend.target.at(base - 1), 0xff, "the byte before it is untouched");
}

/// The second plane's window is DERIVED from the first plane's own report, and on the real
/// geometry the derivation lands exactly on the published constant.
///
/// **AND THE FIELD IT IS DERIVED FROM IS THE ONE THIS ROUTE GOT WRONG FIRST.** An ATSAM4SD32's
/// second plane is at `0x00500000`, exactly one plane above the first -- so asking the part how big
/// its FIRST PLANE is reproduces the published window. Deriving it from `FL_SIZE` instead lands one
/// whole plane too high, because that word is the DEVICE's flash and this part has two of them.
#[test]
fn the_second_plane_window_derived_from_the_part_is_the_published_one() {
    let real = SAM4S_FLASH1_BASE - SAM4S_FLASH0_BASE;
    let base = SAM4S_FLASH1_BASE;
    let bytes = vec![0x77u8; SAM4S_PAGE];
    let image = Image { bytes: &bytes, base };
    let mut backend = sam4s_dual_backend(real);
    backend.erase(&image).expect("the guards pass");
    backend.program(&image).expect("the second plane takes it");

    assert!(backend.target.log.contains(&String::from("wp1:0")), "{:?}", backend.target.log);
    assert_eq!(backend.target.at(SAM4S_FLASH1_BASE), 0x77);
    assert_eq!(backend.target.at(SAM4S_FLASH0_BASE), 0xff, "plane 0 is untouched");
}

/// A descriptor describes THE CONTROLLER THAT ANSWERED, so its two size words agree.
///
/// **THE TEMPTING WRONG MODEL IS "word 1 is the device and word 4 is the plane"**, which table 20-3
/// of Atmel-11100 permits on its own and s20.4.1 rules out in words: *"The embedded Flash is
/// composed of: One memory plane ... The EEFC returns a descriptor of the Flash controller."* An
/// ATSAM3X8E's EEFC0 and EEFC1 EACH report one 256 KiB plane, `FL_NB_PLANE` = 1 -- so a fake built
/// on the other model encodes a part that does not exist.
///
/// **WHAT THIS PINS IS THE AGREEMENT**, because that is the property a reader is tempted to break:
/// two words that always match are two chances to bound a walk by the one that only happens to be
/// right.
#[test]
fn a_descriptor_describes_the_controller_that_answered_so_its_size_words_agree() {
    let mut sam3x = FakeSam3x::new();
    let d0 = sam3x.sam3x_flash_descriptor(SAM3X_EEFC0).expect("GETD answers");
    assert_eq!(d0.size, SAM3X_PLANE_SIZE, "the controller's own plane, not the device");
    assert_eq!(d0.plane_bytes, d0.size, "and word 4 says the same thing");
    assert_eq!(d0.planes, 1, "one plane per controller");

    let d1 = sam3x.sam3x_flash_descriptor(SAM3X_EEFC1).expect("GETD answers");
    assert_eq!(d1, d0);

    let mut sam4s = FakeSam4sDual::new(L4S_PLANE);
    let d = sam4s.sam4s_flash_descriptor(SAM4S_EEFC0).expect("GETD answers");
    assert_eq!(d.plane_bytes, d.size, "the same agreement one family over");
    assert_eq!(d.planes, 2);
}

/// The plane-swap fuse is READ, and a part carrying it set is refused rather than driven.
///
/// **WHICH CONTROLLER FRONTS WHICH WINDOW IS NOT DERIVABLE FROM AN ADDRESS** -- but it IS readable
/// from the part, over the same wire the write goes down. So the fuse is read rather than assumed,
/// and a part carrying it set is refused before anything is erased.
#[test]
fn a_swapped_sam4s_plane_fuse_is_refused_before_anything_is_erased() {
    let bytes = vec![0x5au8; SAM4S_PAGE];
    let mut backend = sam4s_dual_backend(L4S_PLANE);
    backend.target.gpnvm = 1 << SAM4S_GPNVM_PLANE_SWAP;
    let error = flash(
        &mut backend,
        &Image { bytes: &bytes, base: SAM4S_FLASH0_BASE },
        VerifyPolicy::ReadBack,
        &Allow::Any,
    )
    .expect_err("the planes are swapped");

    assert!(format!("{error}").contains("plane swap"), "{error}");
    assert!(backend.target.flash.iter().all(|byte| *byte == 0xff), "nothing was erased");
    assert!(!backend.target.log.contains(&String::from("halt")), "{:?}", backend.target.log);
}

/// A single-plane part on this route is refused, and it names the route that drives it.
///
/// **THE TWO SAM4S ROUTES REFUSE EACH OTHER'S PARTS**, on the same reading, from opposite sides:
/// the single-plane arm refuses a part reporting two planes and this one refuses a part reporting
/// one. Neither refusal is redundant, because both parts answer the same CHIPID family.
#[test]
fn a_single_plane_part_on_the_dual_route_is_refused_on_the_parts_own_report() {
    let bytes = vec![0x5au8; SAM4S_PAGE];
    let mut backend = sam4s_dual_backend(L4S_PLANE);
    backend.target.planes = 1;
    let error = flash(
        &mut backend,
        &Image { bytes: &bytes, base: SAM4S_FLASH0_BASE },
        VerifyPolicy::ReadBack,
        &Allow::Any,
    )
    .expect_err("one plane is not this route");

    assert!(format!("{error}").contains("1 flash plane"), "{error}");
    assert!(backend.target.flash.iter().all(|byte| *byte == 0xff), "nothing was erased");
}

/// An image that does not start on an erase block is refused rather than reaching an assertion.
///
/// **`sam4s_erase_pages8` ASSERTS AN EIGHT-PAGE START**, so without this check an unaligned image
/// PANICS inside the part crate instead of being refused by the route -- a crash where a sentence
/// belongs, on the one path where the caller is holding somebody's board.
#[test]
fn an_unaligned_dual_plane_image_is_refused_rather_than_reaching_the_assertion() {
    let bytes = vec![0x5au8; SAM4S_PAGE];
    let mut backend = sam4s_dual_backend(L4S_PLANE);
    let error = backend
        .erase(&Image { bytes: &bytes, base: SAM4S_FLASH0_BASE + SAM4S_PAGE as u32 })
        .expect_err("one page in is not an eight-page boundary");

    assert!(format!("{error}").contains("EPA erase covers 8 pages"), "{error}");
    assert!(!backend.target.log.contains(&String::from("halt")), "{:?}", backend.target.log);
}

/// A locked region the image REACHES is refused, including one only the erase rounding reaches.
#[test]
fn a_locked_dual_plane_region_is_refused_before_anything_is_erased() {
    let bytes = vec![0x5au8; SAM4S_PAGE];
    let mut backend = sam4s_dual_backend(L4S_PLANE);
    backend.target.locks[0][0] = 0b1;
    let error = flash(
        &mut backend,
        &Image { bytes: &bytes, base: SAM4S_FLASH0_BASE },
        VerifyPolicy::ReadBack,
        &Allow::Any,
    )
    .expect_err("region 0 of plane 0 is locked");

    assert!(format!("{error}").contains("lock regions [0]"), "{error}");
    assert!(backend.target.flash.iter().all(|byte| *byte == 0xff), "nothing was erased");
}

/// And the fake would catch a block number that kept counting past a plane.
#[test]
fn the_fake_sam4s_refuses_an_erase_block_outside_its_plane() {
    let mut target = FakeSam4sDual::new(L4S_PLANE);
    let past = L4S_PLANE / SAM4S_PAGE as u32;
    target
        .write_word(SAM4S_EEFC0 + L4S_FCR, L4S_KEY | ((past | 1) << 8) | L4S_CMD_EPA)
        .unwrap();
    assert!(
        target.log.iter().any(|step| step.ends_with("out-of-plane")),
        "{:?}",
        target.log
    );
    let fsr = target.read_word(SAM4S_EEFC0 + L4S_FSR).unwrap();
    assert_eq!(fsr & L4S_FCMDE, L4S_FCMDE, "{fsr:#x}");
}

/// `--via probe` on a board whose only route IS a probe says so, rather than denying it has one.
///
/// **`alternate: None` MEANS THERE IS NO SECOND ROUTE, NOT THAT THERE IS NO PROBE ROUTE.** An
/// Arduino Due's only route is an external probe, and every micro:bit and NUCLEO reaches the part
/// through the debugger soldered to it. A reader holding that probe must not be told the board has
/// none: the field is about a table, and the sentence would read as a claim about the hardware.
#[test]
fn asking_for_a_probe_on_a_board_that_is_already_one_says_which() {
    use crate::{PROGRAMMING, route_for};
    let due = PROGRAMMING.iter().find(|row| row.board == "arduino-due").expect("listed");
    let error = route_for(due, Some("probe")).expect_err("there is no SECOND route");
    assert!(error.contains("ALREADY a probe"), "{error}");
    assert!(error.contains("external SWD probe"), "the route is named: {error}");
    assert!(
        !error.contains("has no probe route"),
        "the old sentence, and it is the one thing this must not say: {error}"
    );

    assert!(due.programmer.writes_over_a_probe());
    assert!(
        !crate::Programmer::Uf2Volume { family: crate::RP2350_UF2_FAMILY, base: crate::RP2_XIP_BASE }
            .writes_over_a_probe(),
        "a bootloader volume is the one mechanism here that needs no probe"
    );
}
