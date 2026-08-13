//! STM32C0 embedded-flash programming, per RM0490 (C011/C031/C051/C071/C091).

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
/// Every error the status register reports for a program or erase; the same bits clear them.
const C0_SR_ERRORS: u32 =
    (1 << 1) | (1 << 3) | (1 << 4) | (1 << 5) | (1 << 6) | (1 << 7) | (1 << 15);

/// The C0's flash page: 2 KB on every part in the series.
pub const STM32C0_PAGE: u32 = 2048;
/// Where the C0's main flash is mapped for execution.
pub const STM32C0_FLASH_BASE: u32 = 0x0800_0000;
/// The programming granule: 64 bits, written as two 32-bit stores.
pub const STM32C0_DOUBLE_WORD: usize = 8;

/// Names the error bits a failed operation left in `SR`.
///
/// Split out and tested because these are the whole diagnostic value of the status register, and a
/// wrong bit number turns a precise complaint into a confidently wrong one.
fn c0_error_text(sr: u32) -> Option<&'static str> {
    if sr & (1 << 4) != 0 {
        return Some("STM32C0 flash write protection error (WRPERR) -- the page is write protected");
    }
    if sr & (1 << 7) != 0 {
        return Some("STM32C0 flash programming sequence error (PGSERR) -- PG was not set, or a double word was not completed");
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
fn c0_wait_idle<A: TargetAccess>(target: &mut A) -> Result<(), ProbeError> {
    for _ in 0..200_000 {
        let sr = target.read_word(C0_SR)?;
        if sr & (C0_SR_BSY | C0_SR_CFGBSY) != 0 {
            continue;
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
        c0_wait_idle(self)?;
        let pnb = (c0_page_of(address) << C0_CR_PNB_SHIFT) & C0_CR_PNB_MASK;
        self.write_word(C0_CR, C0_CR_PER | pnb)?;
        self.write_word(C0_CR, C0_CR_PER | pnb | C0_CR_STRT)?;
        c0_wait_idle(self)?;
        self.write_word(C0_CR, 0)
    }

    fn c0_program(&mut self, address: u32, data: &[u8]) -> Result<(), ProbeError> {
        if address as usize % STM32C0_DOUBLE_WORD != 0 {
            return Err(ProbeError::Device("STM32C0 programming starts on an 8-byte double-word boundary"));
        }
        c0_wait_idle(self)?;
        self.write_word(C0_CR, C0_CR_PG)?;

        let mut at = address;
        for chunk in data.chunks(STM32C0_DOUBLE_WORD) {
            let mut dword = [0xffu8; STM32C0_DOUBLE_WORD];
            dword[..chunk.len()].copy_from_slice(chunk);
            let low = u32::from_le_bytes([dword[0], dword[1], dword[2], dword[3]]);
            let high = u32::from_le_bytes([dword[4], dword[5], dword[6], dword[7]]);
            self.write_word(at, low)?;
            self.write_word(at + 4, high)?;
            c0_wait_idle(self)?;
            at += STM32C0_DOUBLE_WORD as u32;
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
        for bit in [1u32, 3, 4, 5, 6, 7, 15] {
            assert!(c0_error_text(1 << bit).is_some(), "SR bit {bit} must be named");
        }
        assert!(c0_error_text(0).is_none());
        assert!(c0_error_text(C0_SR_BSY | C0_SR_CFGBSY | C0_SR_EOP).is_none());
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
}
