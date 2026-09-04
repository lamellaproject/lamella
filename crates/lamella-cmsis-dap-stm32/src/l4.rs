//! STM32L4 embedded-flash programming, per RM0351 (L47x/L48x/L49x/L4Ax).

use crate::FlashWait;
use lamella_probe_core::{ProbeError, TargetAccess};

const L4_FLASH_BASE: u32 = 0x4002_2000;
const L4_KEYR: u32 = L4_FLASH_BASE + 0x08;
const L4_SR: u32 = L4_FLASH_BASE + 0x10;
const L4_CR: u32 = L4_FLASH_BASE + 0x14;

const L4_KEY1: u32 = 0x4567_0123;
const L4_KEY2: u32 = 0xCDEF_89AB;

const L4_CR_PG: u32 = 1 << 0;
const L4_CR_PER: u32 = 1 << 1;
const L4_CR_PNB_SHIFT: u32 = 3;
/// EIGHT bits, not the C0's seven: a bank here holds 256 pages.
const L4_CR_PNB_MASK: u32 = 0xff << L4_CR_PNB_SHIFT;
/// `BKER`: which bank the page number refers to.
const L4_CR_BKER: u32 = 1 << 11;
const L4_CR_STRT: u32 = 1 << 16;
const L4_CR_LOCK: u32 = 1 << 31;

const L4_SR_EOP: u32 = 1 << 0;
const L4_SR_BSY: u32 = 1 << 16;
/// Every error flag `SR` reports for a program or an erase (RM0351 3.7.5).
///
/// `RDERR` (14) is a PCROP READ error and is included deliberately: unlike the H7's ECC read flags,
/// reading erased flash here is not an error condition, so a set `RDERR` always means something a
/// caller should hear about rather than a false alarm on a correct verify.
const L4_SR_ERRORS: u32 = L4_SR_OPERR
    | L4_SR_PROGERR
    | L4_SR_WRPERR
    | L4_SR_PGAERR
    | L4_SR_SIZERR
    | L4_SR_PGSERR
    | L4_SR_MISERR
    | L4_SR_FASTERR
    | L4_SR_RDERR
    | L4_SR_OPTVERR;
const L4_SR_OPERR: u32 = 1 << 1;
const L4_SR_PROGERR: u32 = 1 << 3;
const L4_SR_WRPERR: u32 = 1 << 4;
const L4_SR_PGAERR: u32 = 1 << 5;
const L4_SR_SIZERR: u32 = 1 << 6;
const L4_SR_PGSERR: u32 = 1 << 7;
const L4_SR_MISERR: u32 = 1 << 8;
const L4_SR_FASTERR: u32 = 1 << 9;
const L4_SR_RDERR: u32 = 1 << 14;
const L4_SR_OPTVERR: u32 = 1 << 15;

/// One erase page.
pub const STM32L4_PAGE: u32 = 2048;
/// The programming granule: 64 bits, written as two 32-bit accesses.
pub const STM32L4_DOUBLE_WORD: usize = 8;
/// Where main flash is mapped.
pub const STM32L4_FLASH_BASE: u32 = 0x0800_0000;
/// Bytes in one bank on a dual-bank part (RM0351 3.3.1: 256 pages of 2 KB).
pub const STM32L4_BANK: u32 = 512 * 1024;
/// What an erased cell reads as -- ones, unlike the STM32L0's zero.
pub const STM32L4_ERASED_WORD: u32 = 0xFFFF_FFFF;
/// Where the part reports its own flash size, in Kbytes, in the LOW halfword.
///
/// WARNING: the HIGH halfword is not part of the size. An L476RG answers `0xFFFF0400` here: `0x0400` is
/// 1024 Kbytes and the `0xFFFF` above it is not a number about this part at all.
pub const STM32L4_FLASH_SIZE_REG: u32 = 0x1FFF_75E0;

/// `DBGMCU_IDCODE`, the register that answers about ST rather than about Arm (RM0351, "Debug
/// support": the DBG block is *"located in the external PPB memory map at address 0xE0042000"* and
/// the register's own address line reads `0xE004 2000`).
///
/// **THIS IS THE F4/F7 DEBUG-REGION ADDRESS AND NOT THE PERIPHERAL ONE ITS L0 SIBLING USES.** The L0
/// keeps its at `0x40015800`, as do the F0 and the C0; the L4 does not. A constant borrowed from the
/// nearest-named family would read a peripheral that is something else here -- which is exactly why
/// [`crate::stm32_dev_id`] takes the address instead of knowing one.
pub const STM32L4_DBGMCU_IDCODE: u32 = 0xE004_2000;

/// The `DEV_ID` values RM0351 lists, with what each names.
///
/// The manual gives two for the parts it covers, and they are the whole list: *"0x461 for
/// STM32L49x/L4Ax devices, 0x415 for STM32L47x/L48x devices"*.
pub const STM32L4_PARTS: &[(u32, &str)] = &[
    (0x415, "an STM32L47x or L48x -- the DEV_ID, which every part in that group answers, not this board"),
    (0x461, "an STM32L49x or L4Ax -- the DEV_ID, which every part in that group answers, not this board"),
];

/// Names the error a `FLASH_SR` value reports, so a failure says which rule was broken.
fn l4_error_text(sr: u32) -> Option<&'static str> {
    if sr & L4_SR_WRPERR != 0 {
        return Some("STM32L4 flash write protection error (WRPERR) -- the page is protected by WRP or PCROP");
    }
    if sr & L4_SR_PROGERR != 0 {
        return Some("STM32L4 programming error (PROGERR) -- the target double word was not erased");
    }
    if sr & L4_SR_PGSERR != 0 {
        return Some("STM32L4 programming sequence error (PGSERR) -- an error flag was left set by a previous operation, or PG was not set");
    }
    if sr & L4_SR_PGAERR != 0 {
        return Some("STM32L4 programming alignment error (PGAERR) -- a double word must start on an 8-byte boundary");
    }
    if sr & L4_SR_SIZERR != 0 {
        return Some("STM32L4 size error (SIZERR) -- only 32-bit accesses may program this flash");
    }
    if sr & L4_SR_MISERR != 0 {
        return Some("STM32L4 fast programming data miss error (MISERR)");
    }
    if sr & L4_SR_FASTERR != 0 {
        return Some("STM32L4 fast programming error (FASTERR)");
    }
    if sr & L4_SR_RDERR != 0 {
        return Some("STM32L4 read of a PcROP-protected area (RDERR)");
    }
    if sr & L4_SR_OPTVERR != 0 {
        return Some("STM32L4 option bytes failed to load (OPTVERR) -- the part reset mid-operation");
    }
    if sr & L4_SR_OPERR != 0 {
        return Some("STM32L4 flash operation error (OPERR)");
    }
    None
}

/// The bank an execution address lives in, as the `BKER` bit, and its page number WITHIN that bank.
///
/// THE PAGE NUMBER RESTARTS AT ZERO IN BANK 2, which is the whole reason this is a function.
/// Bank 2's first page is 0 with `BKER` set, not 256 -- and `PNB` is eight bits, so 256 would not
/// fit the field anyway. Getting it wrong erases the same-numbered page of the OTHER bank: a real
/// erase, of the wrong 2 KB, that reports success.
fn l4_page_of(address: u32) -> (u32, u32) {
    let offset = address - STM32L4_FLASH_BASE;
    if offset >= STM32L4_BANK {
        (L4_CR_BKER, (offset - STM32L4_BANK) / STM32L4_PAGE)
    } else {
        (0, offset / STM32L4_PAGE)
    }
}

/// Waits for the controller to go idle, then reports any error it left behind.
///
/// On [`FlashWait::BeforeOperation`] the latched flags are cleared and NOT reported, which is
/// RM0351's own step 2 for both sequences: leaving one set makes the NEXT operation fail with
/// `PGSERR` and every one after it.
pub(crate) fn l4_wait_idle<A: TargetAccess>(
    target: &mut A,
    phase: FlashWait,
) -> Result<(), ProbeError> {
    for _ in 0..200_000 {
        let sr = target.read_word(L4_SR)?;
        if sr & L4_SR_BSY != 0 {
            continue;
        }
        if sr & (L4_SR_ERRORS | L4_SR_EOP) != 0 {
            target.write_word(L4_SR, L4_SR_ERRORS | L4_SR_EOP)?;
        }
        if phase == FlashWait::BeforeOperation {
            return Ok(());
        }
        return match l4_error_text(sr) {
            Some(text) => Err(ProbeError::Device(text)),
            None => Ok(()),
        };
    }
    Err(ProbeError::Timeout("STM32L4 flash controller busy"))
}

/// STM32L4 embedded-flash programming, added to ANY [`TargetAccess`] probe. Halt the core before
/// erasing or writing.
pub trait Stm32L4Flash {
    /// Unlocks `FLASH_CR` for erase and program. Idempotent.
    fn l4_unlock_flash(&mut self) -> Result<(), ProbeError>;
    /// Re-locks `FLASH_CR`.
    fn l4_lock_flash(&mut self) -> Result<(), ProbeError>;
    /// Erases the 2 KB page containing `address`, in whichever bank that is.
    fn l4_erase_page(&mut self, address: u32) -> Result<(), ProbeError>;
    /// Programs `data` from `address`, which must be 8-byte aligned and lie in erased flash. A
    /// trailing partial double word is padded with the erased value.
    fn l4_program(&mut self, address: u32, data: &[u8]) -> Result<(), ProbeError>;
}

impl<A: TargetAccess> Stm32L4Flash for A {
    fn l4_unlock_flash(&mut self) -> Result<(), ProbeError> {
        if self.read_word(L4_CR)? & L4_CR_LOCK == 0 {
            return Ok(());
        }
        self.write_word(L4_KEYR, L4_KEY1)?;
        self.write_word(L4_KEYR, L4_KEY2)?;
        if self.read_word(L4_CR)? & L4_CR_LOCK != 0 {
            return Err(ProbeError::Device("STM32L4 FLASH_CR stayed locked after the key sequence"));
        }
        Ok(())
    }

    fn l4_lock_flash(&mut self) -> Result<(), ProbeError> {
        let cr = self.read_word(L4_CR)?;
        self.write_word(L4_CR, cr | L4_CR_LOCK)
    }

    fn l4_erase_page(&mut self, address: u32) -> Result<(), ProbeError> {
        l4_wait_idle(self, FlashWait::BeforeOperation)?;
        let (bank, page) = l4_page_of(address);
        let select = L4_CR_PER | bank | ((page << L4_CR_PNB_SHIFT) & L4_CR_PNB_MASK);
        self.write_word(L4_CR, select)?;
        self.write_word(L4_CR, select | L4_CR_STRT)?;
        let result = l4_wait_idle(self, FlashWait::AfterOperation);
        self.write_word(L4_CR, 0)?;
        result
    }

    fn l4_program(&mut self, address: u32, data: &[u8]) -> Result<(), ProbeError> {
        if address as usize % STM32L4_DOUBLE_WORD != 0 {
            return Err(ProbeError::Device(
                "STM32L4 programming starts on an 8-byte double-word boundary",
            ));
        }
        l4_wait_idle(self, FlashWait::BeforeOperation)?;
        self.write_word(L4_CR, L4_CR_PG)?;

        let mut padded = data.to_vec();
        while padded.len() % STM32L4_DOUBLE_WORD != 0 {
            padded.push(0xff);
        }
        let words: Vec<u32> = padded
            .chunks(4)
            .map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
            .collect();

        let per_poll = STM32L4_PAGE as usize / 4;
        let mut at = address;
        for chunk in words.chunks(per_poll) {
            self.write_words(at, chunk)?;
            l4_wait_idle(self, FlashWait::AfterOperation)?;
            at += (chunk.len() * 4) as u32;
        }

        let cr = self.read_word(L4_CR)?;
        self.write_word(L4_CR, cr & !L4_CR_PG)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_page_number_restarts_in_bank_two() {
        assert_eq!(l4_page_of(0x0800_0000), (0, 0));
        assert_eq!(l4_page_of(0x0800_0800), (0, 1));
        assert_eq!(l4_page_of(0x0807_F800), (0, 255));
        assert_eq!(l4_page_of(0x0808_0000), (L4_CR_BKER, 0));
        assert_eq!(l4_page_of(0x0808_0800), (L4_CR_BKER, 1));
        assert_eq!(l4_page_of(0x080F_F800), (L4_CR_BKER, 255));
    }

    #[test]
    fn every_page_number_survives_its_eight_bit_field() {
        for page in [0u32, 1, 127, 128, 255] {
            let field = (page << L4_CR_PNB_SHIFT) & L4_CR_PNB_MASK;
            assert_eq!(field >> L4_CR_PNB_SHIFT, page, "page {page} must survive PNB");
        }
        assert_eq!(L4_CR_PNB_MASK >> L4_CR_PNB_SHIFT, 0xff);
        assert_eq!(L4_CR_PNB_MASK & L4_CR_BKER, 0);
    }

    #[test]
    fn every_error_bit_is_named_and_the_mask_matches() {
        for bit in [1u32, 3, 4, 5, 6, 7, 8, 9, 14, 15] {
            assert!(l4_error_text(1 << bit).is_some(), "SR bit {bit} must be named");
            assert_ne!(L4_SR_ERRORS & (1 << bit), 0, "SR bit {bit} must be in the mask");
        }
        assert!(l4_error_text(0).is_none());
        assert!(l4_error_text(L4_SR_BSY | L4_SR_EOP).is_none());
        assert_eq!(L4_SR_ERRORS & (L4_SR_BSY | L4_SR_EOP), 0);
    }

    #[test]
    fn the_geometry_is_this_family_and_not_the_c0_it_resembles() {
        assert_eq!(STM32L4_PAGE, 2048);
        assert_eq!(STM32L4_DOUBLE_WORD, 8);
        assert_eq!(STM32L4_BANK % STM32L4_PAGE, 0);
        assert_eq!(STM32L4_BANK / STM32L4_PAGE, 256, "256 pages of 2 KB per bank, RM0351 3.3.1");
        assert_eq!(STM32L4_ERASED_WORD, 0xFFFF_FFFF);
    }
}
