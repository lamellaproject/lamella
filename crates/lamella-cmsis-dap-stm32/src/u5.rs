//! STM32U5 embedded-flash programming, per RM0456 (U535/U545/U575/U585/U59x/U5Ax/U5Fx/U5Gx).


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
const U5_SR_ERRORS: u32 = (1 << 1) | (1 << 3) | (1 << 4) | (1 << 5) | (1 << 6) | (1 << 7) | (1 << 13);

/// The U5's flash page: 8 KB.
pub const STM32U5_PAGE: u32 = 8 * 1024;
/// Where the U5's main flash is mapped for execution (non-secure alias).
pub const STM32U5_FLASH_BASE: u32 = 0x0800_0000;
/// The programming granule: 128 bits, written as four 32-bit stores.
pub const STM32U5_QUAD_WORD: usize = 16;
/// The part's own flash-size register: bits 15:0 hold the size in KB (RM0456).
pub const STM32U5_FLASH_SIZE_REG: u32 = 0x0BFA_07A0;

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

/// Reads the part's flash size and derives how many pages one of its two banks holds.
///
/// One extra register read per erase, which is nothing against a page erase, and it removes a whole
/// class of "right for one board, wrong for its siblings" from this module.
fn u5_pages_per_bank<A: TargetAccess>(target: &mut A) -> Result<u32, ProbeError> {
    let kb = target.read_word(STM32U5_FLASH_SIZE_REG)? & 0xffff;
    if kb == 0 || kb > 2 * 1024 * 2 {
        return Err(ProbeError::Device("STM32U5 flash-size register reads implausibly -- is this a U5?"));
    }
    Ok((kb * 1024 / 2) / STM32U5_PAGE)
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
fn u5_wait_idle<A: TargetAccess>(target: &mut A) -> Result<(), ProbeError> {
    for _ in 0..200_000 {
        let sr = target.read_word(U5_NSSR)?;
        if sr & (U5_SR_BSY | U5_SR_WDW) != 0 {
            continue;
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
        u5_wait_idle(self)?;
        let pages_per_bank = u5_pages_per_bank(self)?;
        let (page, bank2) = u5_page_of(address, pages_per_bank);
        let mut cr = U5_CR_PER | ((page << U5_CR_PNB_SHIFT) & U5_CR_PNB_MASK);
        if bank2 {
            cr |= U5_CR_BKER;
        }
        self.write_word(U5_NSCR, cr)?;
        self.write_word(U5_NSCR, cr | U5_CR_STRT)?;
        u5_wait_idle(self)?;
        self.write_word(U5_NSCR, 0)
    }

    fn u5_program(&mut self, address: u32, data: &[u8]) -> Result<(), ProbeError> {
        if address as usize % STM32U5_QUAD_WORD != 0 {
            return Err(ProbeError::Device("STM32U5 programming starts on a 16-byte quad-word boundary"));
        }
        u5_wait_idle(self)?;
        self.write_word(U5_NSCR, U5_CR_PG)?;

        let mut at = address;
        for chunk in data.chunks(STM32U5_QUAD_WORD) {
            let mut quad = [0xffu8; STM32U5_QUAD_WORD];
            quad[..chunk.len()].copy_from_slice(chunk);
            for word in 0..4u32 {
                let i = (word * 4) as usize;
                let value = u32::from_le_bytes([quad[i], quad[i + 1], quad[i + 2], quad[i + 3]]);
                self.write_word(at + word * 4, value)?;
            }
            u5_wait_idle(self)?;
            at += STM32U5_QUAD_WORD as u32;
        }

        let cr = self.read_word(U5_NSCR)?;
        self.write_word(U5_NSCR, cr & !U5_CR_PG)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
