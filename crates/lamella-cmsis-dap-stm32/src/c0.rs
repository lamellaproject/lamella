//! STM32C0 embedded-flash programming, per RM0490 (C011/C031/C051/C071/C091).

use crate::FlashWait;
use lamella_probe_core::{ProbeError, TargetAccess};

/// The C0 flash interface register block (RM0490 s2.2.2 memory map).
const C0_FLASH_BASE: u32 = 0x4002_2000;
const C0_KEYR: u32 = C0_FLASH_BASE + 0x008;
const C0_SR: u32 = C0_FLASH_BASE + 0x010;
const C0_CR: u32 = C0_FLASH_BASE + 0x014;

const C0_KEY1: u32 = 0x4567_0123;
const C0_KEY2: u32 = 0xCDEF_89AB;

const C0_CR_PG: u32 = 1 << 0;
const C0_CR_PER: u32 = 1 << 1;
const C0_CR_STRT: u32 = 1 << 16;
const C0_CR_LOCK: u32 = 1 << 31;
/// Page number, `PNB[6:0]` at bits 9:3 -- seven bits, so pages 0..127.
const C0_CR_PNB_SHIFT: u32 = 3;
const C0_CR_PNB_MASK: u32 = 0x7f << C0_CR_PNB_SHIFT;

const C0_SR_EOP: u32 = 1 << 0;
const C0_SR_BSY: u32 = 1 << 16;
/// Set when the first word of a double word is sent, cleared when the second completes.
const C0_SR_CFGBSY: u32 = 1 << 18;
/// Every programming error flag RM0490 requires to be CLEARED before the next operation; the same
/// bits clear them, by writing 1.
///
/// **MISSERR AND FASTERR ARE IN THIS SET EVEN THOUGH THIS DRIVER CANNOT CAUSE THEM, AND THAT IS THE
/// POINT.** Both belong to fast programming, which this driver never enables -- but step 2 of both
/// the program and the erase sequence in RM0490 s4.3.7 is *"check and clear all error programming
/// flags due to a previous programming. If not, PGSERR is set"*, and it does not exempt the flags
/// somebody else's tool left behind. A board can be attached already holding one from a session
/// that has ended.
///
/// Leaving them out was not a one-off cost. This driver clears the flags it knows about before
/// every operation, so an inherited MISSERR would have survived that clear, failed step 2, and set
/// PGSERR on the FIRST program -- and then on every program after it, because nothing in the loop
/// would ever have cleared the cause. **A flag we decline to clear is a wedge, not a warning.**
///
/// RDERR (bit 14) is deliberately NOT here: it is a PCROP *read* error, not a programming flag, so
/// step 2 does not ask for it and clearing it would erase a reading somebody else may need.
pub(crate) const C0_SR_ERRORS: u32 = (1 << 1)
    | (1 << 3)
    | (1 << 4)
    | (1 << 5)
    | (1 << 6)
    | (1 << 7)
    | (1 << 8)
    | (1 << 9)
    | (1 << 15);

/// The C0's flash page: 2 KB on every part in the series.
pub const STM32C0_PAGE: u32 = 2048;
/// Where the C0's main flash is mapped for execution.
pub const STM32C0_FLASH_BASE: u32 = 0x0800_0000;
/// The programming granule: 64 bits, written as two 32-bit stores.
///
/// RM0490 s4.3.7: *"The flash memory is programmed 64 bits at a time"* and *"It is only possible to
/// program a double word (2 x 32-bit data)"*, with a byte or half-word write setting SIZERR and a
/// misaligned double word setting PGAERR. So the granule carries an alignment requirement and is
/// not merely a preferred size.
pub const STM32C0_DOUBLE_WORD: usize = 8;

/// What a double word of this array reads as when erased -- and the part's own hardware says so.
///
/// RM0490 does not state it as a sentence about erasing. It states it as the condition PROGERR
/// tests: the flag is *"set by hardware when a double-word address to be programmed contains a
/// value different from `0xFFFF FFFF` before programming, except if the data to write is
/// `0x0000 0000`"*. That is the controller defining "erased" in the register whose whole job is to
/// decide whether a write may proceed, which is a better source than a sentence would have been.
///
/// It states the reprogram rule in the same breath: a double word is write-once between erases, and
/// the all-zero exception writes no information.
pub const STM32C0_ERASED_VALUE: u32 = 0xFFFF_FFFF;

/// See [`crate::STM32F0_FLASH_SIZE_REG`], whose doc carries the whole table and its sources.
///
/// Read from RM0490 Rev 6 s31.2, "Flash memory size data register (FSIZER)": base address
/// `0x1FFF 75A0`, offset `0x00`, a 16-bit `FLASH_SIZE` field in Kbytes.
pub const STM32C0_FLASH_SIZE_REG: u32 = 0x1FFF_75A0;

/// Where this family keeps the register that names ST's die.
///
/// RM0490 Rev 6 section 2.2.2 places the DBG block at `0x40015800`. **The STM32F0 and STM32L0 keep
/// theirs at the same address and an STM32F4 or F7 does not** -- that is a coincidence of three
/// families rather than a rule, so this is declared here with its own citation rather than borrowed
/// from a sibling's constant.
pub const STM32C0_DBGMCU_IDCODE: u32 = 0x4001_5800;

/// The `DEV_ID` values RM0490 Table 178 lists, with what each names.
///
/// **THE SAME FIVE IDS [`Stm32C0Device::from_dev_id`] DECODES, IN THE SHAPE EVERY FAMILY USES**, so
/// a route can ask any family the same question without knowing which one it holds. It is a second
/// spelling of one fact rather than a second fact, and
/// `the_c0_part_table_and_the_device_decode_are_one_list` fails if they drift.
///
/// Each string says the id names a SUB-FAMILY and not a board, because every STM32C071 answers
/// `0x493` and a caller must not read that as an identity.
pub const STM32C0_PARTS: &[(u32, &str)] = &[
    (0x443, "an STM32C011 -- the sub-family, which every part in it answers, not this board"),
    (0x453, "an STM32C031 -- the sub-family, which every part in it answers, not this board"),
    (0x44C, "an STM32C051 -- the sub-family, which every part in it answers, not this board"),
    (0x493, "an STM32C071 -- the sub-family, which every part in it answers, not this board"),
    (0x44D, "an STM32C091 or C092 -- the sub-family, which every part in it answers, not this board"),
];

/// Which STM32C0 sub-family a `DEV_ID` names.
///
/// **A SUB-FAMILY, NOT A BOARD.** Every STM32C071 answers `0x493`, so this settles which boundary
/// map and which flash extent apply and settles nothing about which board is on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stm32C0Device {
    /// `0x443`.
    C011,
    /// `0x453`.
    C031,
    /// `0x44C`.
    C051,
    /// `0x493`.
    C071,
    /// `0x44D` -- RM0490 Table 178 pairs C091xx and C092xx under one value.
    C091,
}

impl Stm32C0Device {
    /// Decodes `DEV_ID` -- the low TWELVE bits of [`STM32C0_DBGMCU_IDCODE`].
    ///
    /// Values from RM0490 Rev 6 Table 178. An id in no row returns `None` rather than a guess: this
    /// address means something else entirely on a part from another vendor, and the debug port a
    /// probe reads first is shared by every M0-class die.
    #[must_use]
    pub fn from_dev_id(dev_id: u32) -> Option<Self> {
        match dev_id {
            0x443 => Some(Stm32C0Device::C011),
            0x453 => Some(Stm32C0Device::C031),
            0x44C => Some(Stm32C0Device::C051),
            0x493 => Some(Stm32C0Device::C071),
            0x44D => Some(Stm32C0Device::C091),
            _ => None,
        }
    }
}

/// Reads [`STM32C0_DBGMCU_IDCODE`] and returns `(DEV_ID, REV_ID)`.
///
/// **TWELVE BITS, NOT SIXTEEN, AND THIS FAMILY PUNISHES THE OBVIOUS WIDTH.** RM0490 says bits
/// 15:12 are reserved and *"upon read, these reserved bits return 0b0110"* -- and they do. An
/// STM32C071 reads `0x10016493`, so a `& 0xffff` yields `0x6493` where `0x493` is expected, and the
/// part is then refused as unknown. Measured on a NUCLEO-C071RB.
pub fn stm32c0_dev_id<A: TargetAccess>(target: &mut A) -> Result<(u32, u32), ProbeError> {
    crate::stm32_dev_id(target, STM32C0_DBGMCU_IDCODE)
}

/// Names the error bits a failed operation left in `SR`.
///
/// Split out and tested because these are the whole diagnostic value of the status register, and a
/// wrong bit number turns a precise complaint into a confidently wrong one.
fn c0_error_text(sr: u32) -> Option<&'static str> {
    if sr & (1 << 4) != 0 {
        return Some("STM32C0 flash write protection error (WRPERR) -- the page is write protected");
    }
    if sr & (1 << 7) != 0 {
        return Some("STM32C0 flash programming sequence error (PGSERR) -- PG was not set, a double word was not completed, or an error flag from a previous programming was still set when this one started");
    }
    if sr & (1 << 5) != 0 {
        return Some("STM32C0 flash programming alignment error (PGAERR) -- a double word must start on an 8-byte boundary");
    }
    if sr & (1 << 6) != 0 {
        return Some("STM32C0 flash size error (SIZERR) -- a write was not 32 bits wide");
    }
    if sr & (1 << 3) != 0 {
        return Some("STM32C0 flash programming error (PROGERR) -- the target double word was not erased");
    }
    if sr & (1 << 1) != 0 {
        return Some("STM32C0 flash operation error (OPERR)");
    }
    if sr & (1 << 9) != 0 {
        return Some("STM32C0 fast-programming error (FASTERR) -- set by a fast-programming sequence this driver does not use, so it was left by another tool");
    }
    if sr & (1 << 8) != 0 {
        return Some("STM32C0 fast-programming data miss error (MISSERR) -- set by a fast-programming sequence this driver does not use, so it was left by another tool");
    }
    if sr & (1 << 15) != 0 {
        return Some("STM32C0 option-byte loading validity error (OPTVERR)");
    }
    None
}

/// The page index containing `address`.
fn c0_page_of(address: u32) -> u32 {
    (address - STM32C0_FLASH_BASE) / STM32C0_PAGE
}

/// Waits for the controller to go idle, then reports any error it left behind.
///
/// Polls `CFGBSY` as well as `BSY` -- see the module note. Clearing the flags before returning the
/// error is deliberate: RM0490 requires a clean status register before the next operation, so
/// leaving them set turns one failure into every subsequent failure.
///
/// On [`FlashWait::BeforeOperation`] the latched flags are cleared and NOT reported -- see the
/// enum, which records the board this distinction was measured on.
pub(crate) fn c0_wait_idle<A: TargetAccess>(target: &mut A, phase: FlashWait) -> Result<(), ProbeError> {
    for _ in 0..200_000 {
        let sr = target.read_word(C0_SR)?;
        if sr & (C0_SR_BSY | C0_SR_CFGBSY) != 0 {
            continue;
        }
        if phase == FlashWait::BeforeOperation {
            if sr & (C0_SR_ERRORS | C0_SR_EOP) != 0 {
                target.write_word(C0_SR, C0_SR_ERRORS | C0_SR_EOP)?;
            }
            return Ok(());
        }
        if let Some(text) = c0_error_text(sr) {
            target.write_word(C0_SR, C0_SR_ERRORS | C0_SR_EOP)?;
            return Err(ProbeError::Device(text));
        }
        if sr & C0_SR_EOP != 0 {
            target.write_word(C0_SR, C0_SR_EOP)?;
        }
        return Ok(());
    }
    Err(ProbeError::Timeout("STM32C0 flash controller busy"))
}

/// STM32C0 embedded-flash programming (RM0490), added to ANY [`TargetAccess`] probe. Halt the core
/// before erasing or writing.
///
/// This matters more on a C0 than on its siblings: a board behind an STLINK-V2EC presents **no
/// mass-storage volume**, so SWD is the only way to program it and there is no drag-drop fallback
/// to retreat to.
pub trait Stm32C0Flash {
    /// Unlocks `FLASH_CR`. Idempotent.
    ///
    /// Checked for having already been done, because RM0490 makes a wrong or repeated key
    /// sequence lock the register until the next reset -- so a "harmless" retry bricks the session.
    fn c0_unlock_flash(&mut self) -> Result<(), ProbeError>;
    /// Re-locks `FLASH_CR`.
    fn c0_lock_flash(&mut self) -> Result<(), ProbeError>;
    /// Erases the 2 KB page containing `address`.
    fn c0_erase_page(&mut self, address: u32) -> Result<(), ProbeError>;
    /// Programs `data` from `address`, which must be 8-byte aligned and lie in erased flash. A
    /// trailing partial double word is padded with `0xFF`.
    fn c0_program(&mut self, address: u32, data: &[u8]) -> Result<(), ProbeError>;
}

impl<A: TargetAccess> Stm32C0Flash for A {
    fn c0_unlock_flash(&mut self) -> Result<(), ProbeError> {
        if self.read_word(C0_CR)? & C0_CR_LOCK == 0 {
            return Ok(());
        }
        self.write_word(C0_KEYR, C0_KEY1)?;
        self.write_word(C0_KEYR, C0_KEY2)?;
        if self.read_word(C0_CR)? & C0_CR_LOCK != 0 {
            return Err(ProbeError::Device("STM32C0 flash stayed locked after the key sequence"));
        }
        Ok(())
    }

    fn c0_lock_flash(&mut self) -> Result<(), ProbeError> {
        let cr = self.read_word(C0_CR)?;
        self.write_word(C0_CR, cr | C0_CR_LOCK)
    }

    fn c0_erase_page(&mut self, address: u32) -> Result<(), ProbeError> {
        c0_wait_idle(self, FlashWait::BeforeOperation)?;
        let pnb = (c0_page_of(address) << C0_CR_PNB_SHIFT) & C0_CR_PNB_MASK;
        self.write_word(C0_CR, C0_CR_PER | pnb)?;
        self.write_word(C0_CR, C0_CR_PER | pnb | C0_CR_STRT)?;
        c0_wait_idle(self, FlashWait::AfterOperation)?;
        self.write_word(C0_CR, 0)
    }

    fn c0_program(&mut self, address: u32, data: &[u8]) -> Result<(), ProbeError> {
        if address as usize % STM32C0_DOUBLE_WORD != 0 {
            return Err(ProbeError::Device("STM32C0 programming starts on an 8-byte double-word boundary"));
        }
        c0_wait_idle(self, FlashWait::BeforeOperation)?;
        self.write_word(C0_CR, C0_CR_PG)?;

        let mut padded = data.to_vec();
        while padded.len() % STM32C0_DOUBLE_WORD != 0 {
            padded.push(0xff);
        }
        let words: Vec<u32> = padded
            .chunks(4)
            .map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
            .collect();

        let per_poll = STM32C0_PAGE as usize / 4;
        let mut at = address;
        for chunk in words.chunks(per_poll) {
            self.write_words(at, chunk)?;
            c0_wait_idle(self, FlashWait::AfterOperation)?;
            at += (chunk.len() * 4) as u32;
        }

        let cr = self.read_word(C0_CR)?;
        self.write_word(C0_CR, cr & !C0_CR_PG)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_page_number_fits_its_seven_bit_field() {
        assert_eq!(c0_page_of(0x0800_0000), 0);
        assert_eq!(c0_page_of(0x0800_0800), 1);
        assert_eq!(c0_page_of(0x0801_F800), 63);
        for page in [0u32, 1, 63, 127] {
            let field = (page << C0_CR_PNB_SHIFT) & C0_CR_PNB_MASK;
            assert_eq!(field >> C0_CR_PNB_SHIFT, page, "page {page} must survive PNB");
        }
    }

    #[test]
    fn every_program_or_erase_error_bit_is_named() {
        for bit in [1u32, 3, 4, 5, 6, 7, 8, 9, 15] {
            assert!(c0_error_text(1 << bit).is_some(), "SR bit {bit} must be named");
        }
        assert!(c0_error_text(0).is_none());
        assert!(c0_error_text(C0_SR_BSY | C0_SR_CFGBSY | C0_SR_EOP).is_none());
    }

    /// Step 2 of BOTH sequences in RM0490 s4.3.7 is "check and clear all error programming flags
    /// due to a previous programming. If not, PGSERR is set." So the mask this driver writes back
    /// has to cover the fast-programming flags as well, even though it never sets FSTPG: it can
    /// INHERIT one, and a flag it declines to clear fails step 2 on every operation afterwards
    /// rather than once.
    #[test]
    fn the_clear_mask_covers_every_programming_error_flag_rm0490_names() {
        for (bit, name) in [
            (1u32, "OPERR"),
            (3, "PROGERR"),
            (4, "WRPERR"),
            (5, "PGAERR"),
            (6, "SIZERR"),
            (7, "PGSERR"),
            (8, "MISSERR"),
            (9, "FASTERR"),
        ] {
            assert!(C0_SR_ERRORS & (1 << bit) != 0, "{name} (bit {bit}) must be cleared");
        }
        assert!(C0_SR_ERRORS & (1 << 14) == 0, "RDERR is not a programming flag");
        assert!(C0_SR_ERRORS & (C0_SR_BSY | C0_SR_CFGBSY | C0_SR_EOP) == 0);
    }

    /// The alignment refusal is not a style rule: RM0490 raises PGAERR for a double word that does
    /// not start on an 8-byte boundary, so a misaligned start would fail per-write rather than up
    /// front -- after PG was already enabled.
    #[test]
    fn programming_requires_double_word_alignment() {
        assert_eq!(0x0800_0000u32 as usize % STM32C0_DOUBLE_WORD, 0);
        assert_ne!(0x0800_0004u32 as usize % STM32C0_DOUBLE_WORD, 0);
        assert_eq!(STM32C0_DOUBLE_WORD, 8);
        assert_eq!(STM32C0_PAGE, super::super::STM32F0_PAGE, "the page size is the coincidence");
    }

    /// The flash-size register is the part's own answer to "how big am I", and it has to be read
    /// through the aligned-word path that serves the whole family -- see `stm32_flash_size_bytes`,
    /// where three of the six addresses are NOT word aligned and the halfword has to be selected.
    /// This one is, so the low half is the right half, and asserting that here is what keeps a
    /// future edit to the shared reader from silently taking the wrong sixteen bits.
    #[test]
    fn the_flash_size_register_is_word_aligned_so_the_low_half_is_the_size() {
        assert_eq!(STM32C0_FLASH_SIZE_REG, 0x1FFF_75A0);
        assert_eq!(STM32C0_FLASH_SIZE_REG & 3, 0, "RM0490 31.2 puts FSIZER on a word boundary");
        assert_eq!(STM32C0_FLASH_SIZE_REG & 2, 0, "so the size is the LOW halfword of that word");
        assert_ne!(STM32C0_FLASH_SIZE_REG, super::super::STM32F0_FLASH_SIZE_REG);
    }

    /// Every value RM0490 Table 178 lists decodes, and nothing else does.
    ///
    /// The negative half is the one that matters: this address holds something on every part with a
    /// debug port, so an id in no row has to be refused rather than carried into an erase.
    #[test]
    fn every_dev_id_rm0490_lists_decodes_and_nothing_else_does() {
        for (id, want) in [
            (0x443u32, Stm32C0Device::C011),
            (0x453, Stm32C0Device::C031),
            (0x44C, Stm32C0Device::C051),
            (0x493, Stm32C0Device::C071),
            (0x44D, Stm32C0Device::C091),
        ] {
            assert_eq!(Stm32C0Device::from_dev_id(id), Some(want), "{id:#05x}");
        }
        for foreign in [0x447u32, 0x440, 0x0bc1_1477, 0, 0xfff] {
            assert_eq!(Stm32C0Device::from_dev_id(foreign), None, "{foreign:#x}");
        }
    }

    /// The width of the mask is the whole test, because sixteen bits is the obvious choice and it
    /// is wrong on this family: the reserved bits above DEV_ID read back as 0b0110 rather than 0.
    #[test]
    fn dev_id_is_twelve_bits_because_the_reserved_nibble_reads_back_set() {
        let word = 0x1001_6493u32;
        assert_eq!(word & 0xfff, 0x493, "twelve bits is the part id");
        assert_ne!(word & 0xffff, 0x493, "sixteen bits is NOT, and decodes as nothing");
        assert_eq!(Stm32C0Device::from_dev_id(word & 0xfff), Some(Stm32C0Device::C071));
        assert_eq!(Stm32C0Device::from_dev_id(word & 0xffff), None);
        assert_eq!(word >> 16, 0x1001, "REV_ID");
    }

    /// The erased value is the condition PROGERR tests, so it is not a free-standing claim: a
    /// double word may be programmed only when it currently reads all ones.
    #[test]
    fn the_erased_value_is_all_ones() {
        assert_eq!(STM32C0_ERASED_VALUE, 0xFFFF_FFFF);
        assert_ne!(STM32C0_ERASED_VALUE, 0);
    }
}
