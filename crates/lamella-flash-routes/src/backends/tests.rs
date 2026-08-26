//! The backend driven against a fake target, so the sequence is checked without a board.

use super::*;
use lamella_flash_backend::{Allow, VerifyPolicy, Verification, flash};
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
        FakeTarget { log: Vec::new(), idcode, flash: vec![0xFF; 256], corrupt_at: None }
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
        word.copy_from_slice(self.flash.get(at..at + 4).ok_or(ProbeError::Device("oob"))?);
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
    Image { bytes: &PROGRAM, base: 0 }
}

/// **THE VALIDATION THE CONTRACT WAS BUILT FOR: A REAL PART'S STEPS COMPOSE INTO THE ORDER.** A
/// contract proved only against a mock backend is a contract proved against itself.
#[test]
fn a_real_parts_primitives_compose_into_the_contracts_order() {
    let mut backend = MicrobitDaplink::new(FakeTarget::new(NRF51), NRF51, "the part family");
    let report = flash(&mut backend, &image(), VerifyPolicy::ReadBack, &Allow::Any).expect("a clean write");
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
    let error = flash(&mut backend, &image(), VerifyPolicy::ReadBack, &Allow::Any).expect_err("wrong part");
    assert!(matches!(error, FlashError::WrongPart { .. }), "got {error:?}");
    assert_eq!(backend.target.log, ["connect", "read_idcode"]);
    assert!(!backend.target.log.contains(&"halt"), "the core was halted before the refusal");
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
    let error = flash(&mut backend, &image(), VerifyPolicy::ReadBack, &Allow::Any).expect_err("a bad byte");
    match error {
        FlashError::Verify { address, .. } => {
            assert_eq!(address, 5, "the corrupted bit is in the second word's high byte");
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
        let image = Image { bytes: &bytes, base: 0 };
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
    assert_eq!(to_words(&[0x01, 0x02, 0x03, 0x04, 0x05]), vec![0x0403_0201, 0x0000_0005]);
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
                    self.flash.get(at..at + 4).ok_or(ProbeError::Device("read past the array"))?,
                );
                Ok(u32::from_le_bytes(word))
            }
            _ => Err(ProbeError::Device("read of an address this fake does not model")),
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
                    self.flash.get(at..at + 4).ok_or(ProbeError::Device("write past the array"))?,
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
            _ => Err(ProbeError::Device("write to an address this fake does not model")),
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
    Image { bytes: &L0_PROGRAM, base: L0_FLASH }
}

/// The same validation the nRF backend gets: a REAL part's primitives, composed by the contract.
#[test]
fn the_l0_primitives_compose_into_the_contracts_order() {
    let mut backend = Stm32L0Probe::new(FakeL0::new());
    let report = flash(&mut backend, &l0_image(), VerifyPolicy::ReadBack, &Allow::Any)
        .expect("the L0 sequence");

    assert_eq!(report.verification, Verification::ReadBack);
    assert_eq!(report.base, L0_FLASH);
    assert_eq!(report.bytes, L0_PROGRAM.len());

    let log = &backend.target.log;
    let halt = log.iter().position(|step| *step == "halt").expect("the core is halted");
    let erase = log.iter().position(|step| *step == "erase_page").expect("a page is erased");
    let program = log.iter().position(|step| *step == "program_word").expect("a word is written");
    let run = log.iter().position(|step| *step == "reset_and_run").expect("the part is released");
    assert!(halt < erase, "the erase must not run on a core that is still fetching: {log:?}");
    assert!(erase < program, "a program before its erase is the defect this part punishes");
    assert!(program < run, "the part is released only after it has been written");
    assert!(!log.contains(&"notzeroerr"), "no word was written over unerased flash: {log:?}");
}

/// **THE FAMILY'S WHOLE CHARACTER, ASSERTED RATHER THAN ASSUMED.** An erase here leaves ZERO. A
/// backend carrying a ones-erasing assumption over from any other part in this module would leave
/// the tail of the page holding `0xFF`, and this is the test that would notice.
#[test]
fn an_erase_on_this_family_leaves_zero_and_the_untouched_tail_stays_erased() {
    let mut backend = Stm32L0Probe::new(FakeL0::new());
    backend.target.flash.iter_mut().for_each(|byte| *byte = 0x5a);
    flash(&mut backend, &l0_image(), VerifyPolicy::ReadBack, &Allow::Any).expect("the L0 sequence");

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
        let mut backend = Stm32L0Probe::new(FakeL0::new());
        let image = Image { bytes: &bytes, base: L0_FLASH };
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
    let mut backend = Stm32L0Probe::new(FakeL0::new());
    backend.target.dev_id = F0_DEV_ID;
    let original = backend.target.flash.clone();

    let error = flash(&mut backend, &l0_image(), VerifyPolicy::ReadBack, &Allow::Any)
        .expect_err("an F0 is not an L0");
    assert!(format!("{error}").contains("0x440"), "the error names what it read: {error}");
    assert!(backend.target.array_untouched(&original), "the array was touched before the refusal");
    assert!(!backend.target.log.contains(&"erase_page"), "{:?}", backend.target.log);
}

/// Likewise for an image the part has no room for: the part is asked how big it is, and the
/// refusal precedes the first page erase rather than arriving partway through the array.
#[test]
fn an_image_larger_than_the_fitted_flash_is_refused_before_anything_is_erased() {
    let big = vec![0x11u8; 4096];
    let mut backend = Stm32L0Probe::new(FakeL0::new());
    backend.target.flash_kb = 2;
    let original = backend.target.flash.clone();

    let error = flash(
        &mut backend,
        &Image { bytes: &big, base: L0_FLASH },
        VerifyPolicy::ReadBack,
        &Allow::Any,
    )
    .expect_err("4096 bytes do not fit in 2 KB");
    assert!(format!("{error}").contains("2 KB"), "the error names the part's own answer: {error}");
    assert!(backend.target.array_untouched(&original));
    assert!(!backend.target.log.contains(&"erase_page"), "{:?}", backend.target.log);
}

/// The identity names a CATEGORY and says so, which is the contract's sixth prohibition met by
/// disclosure rather than by pretending to more.
#[test]
fn the_identity_names_a_category_and_says_it_is_not_a_board() {
    let mut backend = Stm32L0Probe::new(FakeL0::new());
    let identity = backend.identify().expect("the part answers");
    assert_eq!(identity.value, u64::from(L0_CAT5));
    assert!(identity.what.contains("category 5"), "{}", identity.what);
    assert!(identity.what.contains("not this board"), "{}", identity.what);
    assert_ne!(identity.value, 0x0bc1_1477);
}

/// `Allow` can pin the category, and a part outside it is refused between identify and erase.
#[test]
fn a_permission_naming_another_category_refuses_before_the_erase() {
    let mut backend = Stm32L0Probe::new(FakeL0::new());
    let original = backend.target.flash.clone();
    let only_category_one = Allow::Identities(vec![0x457]);

    flash(&mut backend, &l0_image(), VerifyPolicy::ReadBack, &only_category_one)
        .expect_err("a category 5 part is not permitted here");
    assert!(backend.target.array_untouched(&original));
    assert!(!backend.target.log.contains(&"erase_page"), "{:?}", backend.target.log);
}

/// The read-back is real: corrupt what comes off the wire and the contract reports a verify
/// failure rather than success.
#[test]
fn the_read_back_is_used_and_a_bad_one_fails_the_flash() {
    let mut backend = Stm32L0Probe::new(FakeL0::new());
    backend.target.corrupt_at = Some(0);
    let error = flash(&mut backend, &l0_image(), VerifyPolicy::ReadBack, &Allow::Any)
        .expect_err("a corrupted read-back must not report success");
    assert!(
        matches!(error, FlashError::Verify { address, .. } if address == L0_FLASH + 1),
        "a corrupted word must be reported as a verify failure at its own address: {error}"
    );
}
