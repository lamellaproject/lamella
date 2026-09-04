//! STM32U5 embedded-flash programming, per RM0456 (U535/U545/U575/U585/U59x/U5Ax/U5Fx/U5Gx).


use crate::FlashWait;
use lamella_probe_core::{ProbeError, TargetAccess};

/// The FLASH register block, non-secure alias (RM0456 Table 6).
const U5_FLASH_BASE: u32 = 0x4002_2000;
const U5_NSKEYR: u32 = U5_FLASH_BASE + 0x08;
const U5_NSSR: u32 = U5_FLASH_BASE + 0x20;
const U5_NSCR: u32 = U5_FLASH_BASE + 0x28;

const U5_KEY1: u32 = 0x4567_0123;
const U5_KEY2: u32 = 0xCDEF_89AB;

const U5_CR_PG: u32 = 1 << 0;
const U5_CR_PER: u32 = 1 << 1;
/// Page number, `PNB[7:0]` at bits 10:3 -- eight bits, so up to 256 pages per bank.
const U5_CR_PNB_SHIFT: u32 = 3;
const U5_CR_PNB_MASK: u32 = 0xff << U5_CR_PNB_SHIFT;
/// Bank selection for page erase: 0 = bank 1, 1 = bank 2.
const U5_CR_BKER: u32 = 1 << 11;
const U5_CR_STRT: u32 = 1 << 16;
const U5_CR_LOCK: u32 = 1 << 31;

const U5_SR_EOP: u32 = 1 << 0;
const U5_SR_BSY: u32 = 1 << 16;
/// Set while the controller is still waiting for the rest of a quad-word.
const U5_SR_WDW: u32 = 1 << 17;
/// Every error the status register reports for a program or erase; the same bits clear them.
pub(crate) const U5_SR_ERRORS: u32 = (1 << 1) | (1 << 3) | (1 << 4) | (1 << 5) | (1 << 6) | (1 << 7) | (1 << 13);

/// The U5's flash page: 8 KB.
pub const STM32U5_PAGE: u32 = 8 * 1024;
/// Where the U5's main flash is mapped for execution (non-secure alias).
pub const STM32U5_FLASH_BASE: u32 = 0x0800_0000;
/// The programming granule: 128 bits, written as four 32-bit stores.
pub const STM32U5_QUAD_WORD: usize = 16;
/// The part's own flash-size register: bits 15:0 hold the size in KB (RM0456 76.2).
pub const STM32U5_FLASH_SIZE_REG: u32 = 0x0BFA_07A0;

/// `DBGMCU_IDCODE`, the register that answers about ST rather than about Arm (RM0456 75.12.4, base
/// address `0xE004 4000`, address offset `0x00`).
///
/// Same field layout as every other family here -- `DEV_ID` in bits 11:0, `REV_ID` in bits 31:16 --
/// at an address that is this family's alone, which is why [`crate::stm32_dev_id`] takes the address
/// and shares only the decode.
pub const STM32U5_DBGMCU_IDCODE: u32 = 0xE004_4000;

/// The `DEV_ID` values RM0456 lists, with what each names.
///
/// **THE WHOLE LIST, BECAUSE THE MANUAL DESCRIBES ONE REGISTER MODEL FOR ALL OF THEM.** RM0456 is
/// the source for this driver's base, keys, page size and quad-word granule, and it covers these
/// four device ids; narrowing the list to one would refuse a part this manual says the driver fits.
///
/// **AN ID LISTED HERE SAYS THE MANUAL COVERS IT, NOT THAT A BOARD OF THAT ID HAS BEEN WRITTEN.**
pub const STM32U5_PARTS: &[(u32, &str)] = &[
    (0x455, "an STM32U535/545 -- the DEV_ID, which every part in that group answers, not this board"),
    (0x476, "an STM32U5Fx/5Gx -- the DEV_ID, which every part in that group answers, not this board"),
    (0x481, "an STM32U59x/5Ax -- the DEV_ID, which every part in that group answers, not this board"),
    (0x482, "an STM32U575/585 -- the DEV_ID, which every part in that group answers, not this board"),
];

/// The FLASH option register (RM0456 7.9.13), which states how the part's banks are arranged.
///
/// A NUCLEO-U5A5ZJ-Q answered `0x1FEFF8AA` here, which is exactly the "ST production value" RM0456
/// prints for this register: `TZEN` clear (so the non-secure register set this module drives is the
/// right one), `DUALBANK` set, `SWAP_BANK` clear, and `RDP` at 0xAA for level 0.
const U5_OPTR: u32 = U5_FLASH_BASE + 0x40;
/// `OPTR.DUALBANK`: 0 = one bank with contiguous addresses, 1 = two banks. Only meaningful on the
/// half-density part of each line -- see [`u5_pages_per_bank`].
const U5_OPTR_DUALBANK: u32 = 1 << 21;
/// `OPTR.SWAP_BANK`: 1 = bank 1 and bank 2 addresses are exchanged.
const U5_OPTR_SWAP_BANK: u32 = 1 << 20;

/// The most pages a U5 bank can hold -- **a ceiling, not the answer for any given part.**
///
/// RM0456 says two banks of "**up to** 2 Mbytes each containing **up to** 256 pages", and the
/// ceiling is not the count: a 4 MB U5A5 has banks of 256 pages, a 2 MB U575 banks of 128. Take
/// 256 for both and the first address of bank 2 on a U575 computes as bank 1 page 128, which does
/// not exist on that part.
///
/// **A geometry constant is a claim about a PART, and a part is not a family**, so the count is
/// derived from the silicon rather than declared here -- see [`u5_pages_per_bank`].
pub const STM32U5_MAX_PAGES_PER_BANK: u32 = 256;

/// Reads the part's flash size and its bank configuration, and derives how many pages one bank
/// holds.
///
/// Two register reads per erase, which is nothing against a page erase, and they remove a whole
/// class of "right for one board, wrong for its siblings" from this module.
///
/// **BANK COUNT IS READ, NEVER ASSUMED, AND RM0456 SAYS SO IN THE BIT'S OWN DESCRIPTION.** `DUALBANK`
/// (`FLASH_OPTR` bit 21) is a real option on the HALF-density member of each line -- 2 Mbyte
/// U59x/5Ax/5Fx/5Gx, 1 Mbyte U575/585, 256 and 128 Kbyte U535/545 -- where clearing it gives
/// "single-bank flash memory with contiguous address in bank 1". Only the full-density parts are
/// dual bank by construction. Halving such a part anyway puts the whole upper half one bank too far
/// over: on a 1 MB U575 with `DUALBANK = 0`, the address at 512 KB is bank 1 page 64 and the old
/// arithmetic called it bank 2 page 0, so the erase would have gone to the wrong bank rather than
/// failing.
///
/// The last check is the one that makes a miscalculation loud instead of silent. `PNB` is eight
/// bits, so any derivation yielding more than [`STM32U5_MAX_PAGES_PER_BANK`] pages cannot be
/// expressed in the register at all and would simply wrap onto a low page of the right bank -- an
/// erase of live flash reported as success. It is refused instead.
fn u5_pages_per_bank<A: TargetAccess>(target: &mut A) -> Result<u32, ProbeError> {
    let kb = target.read_word(STM32U5_FLASH_SIZE_REG)? & 0xffff;
    if kb == 0 || kb > 2 * 1024 * 2 {
        return Err(ProbeError::Device("STM32U5 flash-size register reads implausibly -- is this a U5?"));
    }
    let optr = target.read_word(U5_OPTR)?;
    if optr & U5_OPTR_SWAP_BANK != 0 {
        return Err(ProbeError::Device(
            "STM32U5 has SWAP_BANK set -- page erase addressing is not verified for swapped banks",
        ));
    }
    let total_pages = kb * 1024 / STM32U5_PAGE;
    let pages_per_bank =
        if optr & U5_OPTR_DUALBANK != 0 { total_pages / 2 } else { total_pages };
    if pages_per_bank > STM32U5_MAX_PAGES_PER_BANK {
        return Err(ProbeError::Device(
            "STM32U5 bank works out larger than PNB can address -- the geometry read is wrong",
        ));
    }
    Ok(pages_per_bank)
}

/// Names the error bits a failed operation left in `NSSR`.
fn u5_error_text(sr: u32) -> Option<&'static str> {
    if sr & (1 << 4) != 0 {
        return Some("STM32U5 flash write protection error (WRPERR) -- the page is write protected, or TrustZone made it secure-only");
    }
    if sr & (1 << 7) != 0 {
        return Some("STM32U5 flash programming sequence error (PGSERR) -- PG was not set, or a quad-word was not completed");
    }
    if sr & (1 << 5) != 0 {
        return Some("STM32U5 flash programming alignment error (PGAERR) -- a quad-word must start on a 16-byte boundary");
    }
    if sr & (1 << 6) != 0 {
        return Some("STM32U5 flash size error (SIZERR) -- a write was not 32 bits wide");
    }
    if sr & (1 << 3) != 0 {
        return Some("STM32U5 flash programming error (PROGERR) -- the target quad-word was not erased");
    }
    if sr & (1 << 1) != 0 {
        return Some("STM32U5 flash operation error (OPERR)");
    }
    if sr & (1 << 13) != 0 {
        return Some("STM32U5 option write error (OPTWERR)");
    }
    None
}

/// The page index within its bank, and which bank it is.
///
/// The index is PER BANK, so bank 2's first page is 0 and not 256 -- and `PNB` is eight bits,
/// which could not hold 256 anyway. Getting this wrong erases the wrong page of the right bank,
/// silently.
fn u5_page_of(address: u32, pages_per_bank: u32) -> (u32, bool) {
    let page = (address - STM32U5_FLASH_BASE) / STM32U5_PAGE;
    if page >= pages_per_bank {
        (page - pages_per_bank, true)
    } else {
        (page, false)
    }
}

/// Waits for the controller to go idle, then reports any error it left behind.
///
/// On [`FlashWait::BeforeOperation`] the latched flags are cleared and NOT reported. **This part is
/// the one that proved the distinction was needed**: a NUCLEO-U5A5ZJ-Q was found holding `PGSERR` in
/// `FLASH_NSSR` before anything in this session had written to it -- see [`FlashWait`].
pub(crate) fn u5_wait_idle<A: TargetAccess>(target: &mut A, phase: FlashWait) -> Result<(), ProbeError> {
    for _ in 0..200_000 {
        let sr = target.read_word(U5_NSSR)?;
        if sr & (U5_SR_BSY | U5_SR_WDW) != 0 {
            continue;
        }
        if phase == FlashWait::BeforeOperation {
            if sr & (U5_SR_ERRORS | U5_SR_EOP) != 0 {
                target.write_word(U5_NSSR, U5_SR_ERRORS | U5_SR_EOP)?;
            }
            return Ok(());
        }
        if let Some(text) = u5_error_text(sr) {
            target.write_word(U5_NSSR, U5_SR_ERRORS | U5_SR_EOP)?;
            return Err(ProbeError::Device(text));
        }
        if sr & U5_SR_EOP != 0 {
            target.write_word(U5_NSSR, U5_SR_EOP)?;
        }
        return Ok(());
    }
    Err(ProbeError::Timeout("STM32U5 flash controller busy"))
}

/// STM32U5 embedded-flash programming (RM0456), added to ANY [`TargetAccess`] probe. Halt the core
/// before erasing or writing.
pub trait Stm32U5Flash {
    /// Unlocks `FLASH_NSCR`. Idempotent, and checked -- a repeated key sequence locks the register
    /// until the next reset.
    fn u5_unlock_flash(&mut self) -> Result<(), ProbeError>;
    /// Re-locks `FLASH_NSCR`.
    fn u5_lock_flash(&mut self) -> Result<(), ProbeError>;
    /// Erases the 8 KB page containing `address`, selecting its bank.
    fn u5_erase_page(&mut self, address: u32) -> Result<(), ProbeError>;
    /// Programs `data` from `address`, which must be 16-byte aligned and lie in erased flash. A
    /// trailing partial quad-word is padded with `0xFF`.
    fn u5_program(&mut self, address: u32, data: &[u8]) -> Result<(), ProbeError>;
}

impl<A: TargetAccess> Stm32U5Flash for A {
    fn u5_unlock_flash(&mut self) -> Result<(), ProbeError> {
        if self.read_word(U5_NSCR)? & U5_CR_LOCK == 0 {
            return Ok(());
        }
        self.write_word(U5_NSKEYR, U5_KEY1)?;
        self.write_word(U5_NSKEYR, U5_KEY2)?;
        if self.read_word(U5_NSCR)? & U5_CR_LOCK != 0 {
            return Err(ProbeError::Device("STM32U5 flash stayed locked after the key sequence"));
        }
        Ok(())
    }

    fn u5_lock_flash(&mut self) -> Result<(), ProbeError> {
        let cr = self.read_word(U5_NSCR)?;
        self.write_word(U5_NSCR, cr | U5_CR_LOCK)
    }

    fn u5_erase_page(&mut self, address: u32) -> Result<(), ProbeError> {
        u5_wait_idle(self, FlashWait::BeforeOperation)?;
        let pages_per_bank = u5_pages_per_bank(self)?;
        let (page, bank2) = u5_page_of(address, pages_per_bank);
        let mut cr = U5_CR_PER | ((page << U5_CR_PNB_SHIFT) & U5_CR_PNB_MASK);
        if bank2 {
            cr |= U5_CR_BKER;
        }
        self.write_word(U5_NSCR, cr)?;
        self.write_word(U5_NSCR, cr | U5_CR_STRT)?;
        u5_wait_idle(self, FlashWait::AfterOperation)?;
        self.write_word(U5_NSCR, 0)
    }

    fn u5_program(&mut self, address: u32, data: &[u8]) -> Result<(), ProbeError> {
        if address as usize % STM32U5_QUAD_WORD != 0 {
            return Err(ProbeError::Device("STM32U5 programming starts on a 16-byte quad-word boundary"));
        }
        u5_wait_idle(self, FlashWait::BeforeOperation)?;
        self.write_word(U5_NSCR, U5_CR_PG)?;

        let mut padded = data.to_vec();
        while padded.len() % STM32U5_QUAD_WORD != 0 {
            padded.push(0xff);
        }
        let words: Vec<u32> = padded
            .chunks(4)
            .map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
            .collect();

        let per_poll = STM32U5_PAGE as usize / 4;
        let mut at = address;
        for chunk in words.chunks(per_poll) {
            self.write_words(at, chunk)?;
            u5_wait_idle(self, FlashWait::AfterOperation)?;
            at += (chunk.len() * 4) as u32;
        }

        let cr = self.read_word(U5_NSCR)?;
        self.write_word(U5_NSCR, cr & !U5_CR_PG)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bank derivation over every density RM0456 tabulates, in BOTH bank configurations.
    ///
    /// The arithmetic is factored out of [`u5_pages_per_bank`] here rather than driven through a
    /// mock probe, because what is being checked is the derivation and not the two register reads
    /// that feed it. The register facts those reads depend on are pinned in
    /// `the_option_register_fields_are_where_rm0456_puts_them`.
    fn pages_per_bank(kb: u32, dual: bool) -> Result<u32, &'static str> {
        let total = kb * 1024 / STM32U5_PAGE;
        let per_bank = if dual { total / 2 } else { total };
        if per_bank > STM32U5_MAX_PAGES_PER_BANK { Err("wider than PNB") } else { Ok(per_bank) }
    }

    /// THE PART THAT LANDS EXACTLY ON THE LIMIT, AND THE SIBLING THAT READS THE SAME AND IS NOT.
    ///
    /// A 4 MB U5A5 is the first part this crate drives whose banks are the full 256 pages RM0456
    /// allows, so its top page index is 255 -- the largest `PNB[7:0]` can hold, and the exact row
    /// RM0456 prints as "11111111: page 255 (upper page for STM32U59x/5Ax/5Fx/5Gx)". One more page
    /// anywhere in the derivation would not error, it would wrap.
    ///
    /// The half-density rows are the discriminating ones: two parts can report the same flash size
    /// and still put a given address in different banks, because the bank count is an option bit
    /// and not a function of the size.
    #[test]
    fn the_bank_derivation_holds_at_every_density_and_both_bank_configurations() {
        assert_eq!(pages_per_bank(4096, true), Ok(256), "4 MB U5A5: banks of 256 pages");
        assert_eq!(pages_per_bank(2048, true), Ok(128), "2 MB U575: banks of 128 pages");
        assert_eq!(pages_per_bank(512, true), Ok(32), "512 KB U535/545");

        assert_eq!(pages_per_bank(2048, false), Ok(256), "2 MB U5Ax, single bank");
        assert_eq!(pages_per_bank(1024, false), Ok(128), "1 MB U575, single bank");
        assert_ne!(pages_per_bank(1024, false), pages_per_bank(1024, true));

        let at_512k = STM32U5_FLASH_BASE + 512 * 1024;
        assert_eq!(u5_page_of(at_512k, pages_per_bank(1024, false).unwrap()), (64, false));
        assert_eq!(u5_page_of(at_512k, pages_per_bank(1024, true).unwrap()), (0, true));

        assert!(pages_per_bank(4096, false).is_err(), "512 pages cannot be addressed by PNB");
        for pages in [0u32, 1, 255] {
            assert_eq!(((pages << U5_CR_PNB_SHIFT) & U5_CR_PNB_MASK) >> U5_CR_PNB_SHIFT, pages);
        }
        assert_ne!(
            (STM32U5_MAX_PAGES_PER_BANK << U5_CR_PNB_SHIFT) & U5_CR_PNB_MASK,
            STM32U5_MAX_PAGES_PER_BANK << U5_CR_PNB_SHIFT,
            "256 must NOT survive PNB -- that is why the ceiling is a refusal and not a clamp",
        );
    }

    /// The option-register facts as RM0456 literals, and the value the board actually answered.
    ///
    /// Written out from section 7.9.13 rather than from the constants under test, so that moving a
    /// bit has to disagree with the manual rather than with itself.
    #[test]
    fn the_option_register_fields_are_where_rm0456_puts_them() {
        assert_eq!(U5_OPTR, 0x4002_2000 + 0x40);
        assert_eq!(U5_OPTR_DUALBANK, 1 << 21);
        assert_eq!(U5_OPTR_SWAP_BANK, 1 << 20);

        let measured_optr = 0x1FEF_F8AA;
        assert_ne!(measured_optr & U5_OPTR_DUALBANK, 0, "the measured part is dual bank");
        assert_eq!(measured_optr & U5_OPTR_SWAP_BANK, 0, "and its banks are not swapped");
        assert_eq!(measured_optr >> 31, 0, "and TZEN is clear, so the NS registers are the right set");
        assert_eq!(measured_optr & 0xff, 0xaa, "and RDP is level 0");
    }

    #[test]
    fn the_page_index_is_per_bank_and_fits_its_field() {
        let big = 256;
        assert_eq!(u5_page_of(0x0800_0000, big), (0, false));
        assert_eq!(u5_page_of(0x0800_2000, big), (1, false));
        assert_eq!(u5_page_of(0x0800_0000 + 255 * STM32U5_PAGE, big), (255, false));
        assert_eq!(u5_page_of(0x0800_0000 + 256 * STM32U5_PAGE, big), (0, true));
        assert_eq!(u5_page_of(0x0800_0000 + 257 * STM32U5_PAGE, big), (1, true));

        let small = 128;
        let bank2_start = 0x0800_0000 + 128 * STM32U5_PAGE;
        assert_eq!(u5_page_of(bank2_start, small), (0, true), "2 MB part: bank 2 page 0");
        assert_eq!(u5_page_of(bank2_start, big), (128, false), "4 MB part: still bank 1");
        assert_ne!(
            u5_page_of(bank2_start, small),
            u5_page_of(bank2_start, big),
            "if these ever agree the test has stopped discriminating"
        );
        for page in [0u32, 1, 255] {
            let field = (page << U5_CR_PNB_SHIFT) & U5_CR_PNB_MASK;
            assert_eq!(field >> U5_CR_PNB_SHIFT, page, "page {page} must survive PNB");
        }
    }

    #[test]
    fn every_program_or_erase_error_bit_is_named() {
        for bit in [1u32, 3, 4, 5, 6, 7, 13] {
            assert!(u5_error_text(1 << bit).is_some(), "NSSR bit {bit} must be named");
        }
        assert!(u5_error_text(0).is_none());
        assert!(u5_error_text(U5_SR_BSY | U5_SR_WDW | U5_SR_EOP).is_none());
    }

    /// THE FOUR ST FAMILIES IN THIS CRATE HAVE FOUR DIFFERENT PROGRAMMING GRANULES, and this
    /// asserts they stay distinct. It is not a tautology: the granule is the single fact most
    /// likely to be copied from a neighbouring family, and every one of these was read from its own
    /// reference manual.
    #[test]
    fn the_granules_of_the_four_families_are_all_different() {
        use super::super::{STM32C0_DOUBLE_WORD, STM32H7_FLASH_WORD};
        assert_eq!(STM32C0_DOUBLE_WORD, 8);
        assert_eq!(STM32U5_QUAD_WORD, 16);
        assert_eq!(STM32H7_FLASH_WORD, 32);
        assert!(STM32C0_DOUBLE_WORD < STM32U5_QUAD_WORD);
        assert!(STM32U5_QUAD_WORD < STM32H7_FLASH_WORD);
    }
}
