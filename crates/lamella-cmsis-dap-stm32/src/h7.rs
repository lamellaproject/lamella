//! STM32H7 embedded-flash programming, per RM0399 (H745/H747/H755/H757).

use crate::FlashWait;
use lamella_probe_core::{ProbeError, TargetAccess};

/// Bank 1's register block. Bank 2's is the same layout [`H7_BANK_STRIDE`] higher.
const H7_FLASH_BASE: u32 = 0x5200_2000;
/// Distance from bank 1's register block to bank 2's (RM0399 Table 19).
const H7_BANK_STRIDE: u32 = 0x100;
const H7_KEYR: u32 = 0x04;
const H7_CR: u32 = 0x0c;
const H7_SR: u32 = 0x10;
const H7_CCR: u32 = 0x14;

const H7_KEY1: u32 = 0x4567_0123;
const H7_KEY2: u32 = 0xCDEF_89AB;

const H7_CR_LOCK: u32 = 1 << 0;
const H7_CR_PG: u32 = 1 << 1;
const H7_CR_SER: u32 = 1 << 2;
/// Program parallelism, `10` = word -- chosen to MATCH the 32-bit accesses a debug probe performs.
/// The reset value is `11` (double word), which describes a wider access than we make.
const H7_CR_PSIZE_WORD: u32 = 0b10 << 4;
const H7_CR_START: u32 = 1 << 7;
const H7_CR_SNB_SHIFT: u32 = 8;
const H7_CR_SNB_MASK: u32 = 0b111 << H7_CR_SNB_SHIFT;

const H7_SR_BSY: u32 = 1 << 0;
const H7_SR_QW: u32 = 1 << 2;
/// End-of-program, cleared through CCR like the error flags.
const H7_SR_EOP: u32 = 1 << 16;
/// Every error flag SR reports for a write or erase; the same bit positions clear them in CCR.
///
/// RM0399 4.9.5 lists two more that are deliberately NOT here, and the omission is a scope rather
/// than an oversight: `SNECCERR1` (25) and `DBECCERR1` (26) are ECC flags raised by a READ (4.7.7),
/// and this mask is consulted after a PROGRAM or an ERASE. Folding a read's flags into an
/// operation's verdict would let one report failure for the other's condition.
pub(crate) const H7_SR_ERRORS: u32 =
    (1 << 17) | (1 << 18) | (1 << 19) | (1 << 21) | (1 << 22) | (1 << 23) | (1 << 24);

/// The programming granule: 256 bits.
pub const STM32H7_FLASH_WORD: usize = 32;
/// One erase sector.
pub const STM32H7_SECTOR: u32 = 128 * 1024;
/// Where bank 1 is mapped for execution.
pub const STM32H7_FLASH_BASE: u32 = 0x0800_0000;
/// Where bank 2 begins on a 2 MB part.
pub const STM32H7_BANK2_BASE: u32 = 0x0810_0000;

/// `DBGMCU_IDC`, the register that answers about ST rather than about Arm (RM0399 "DBGMCU
/// registers", address offset `0x000`).
///
/// **RM0399 GIVES THE DBGMCU TWO BASE ADDRESSES AND THIS IS THE ONE A PROBE CAN USE.** It is
/// "accessible to the debugger via the APB-D bus at base address 0xE00E1000" and "also accessible by
/// both processor cores at base address 0x5C001000". A memory read through an AHB-AP is a bus access
/// with the cores' view of the map, so the core-visible address is the one that answers.
///
/// The reset value RM0399 states is `0xX00X6450`, so bits 15:12 read `0b0110` here as they do on the
/// L0 -- the same shape, at a different address, which is exactly why the address is not shared.
pub const STM32H7_DBGMCU_IDC: u32 = 0x5C00_1000;

/// The `DEV_ID` values RM0399 lists for the parts this driver was written from, with what each names.
///
/// **ONE ENTRY, BECAUSE THE MANUAL LISTS ONE.** RM0399 covers H745/H747/H755/H757 and gives a single
/// device id for the whole group, so this is not an abbreviated list -- an id outside it is a part
/// RM0399 does not describe, and this driver's register model is not known to hold there.
pub const STM32H7_PARTS: &[(u32, &str)] = &[(
    0x450,
    "an STM32H745/755 or STM32H747/757 -- the DEV_ID, which every part in that group answers, not this board",
)];

/// Names the error bits a failed operation left in `SR`, so a failure says which rule was broken
/// rather than "it did not work".
///
/// Split out and tested because these seven conditions are the whole diagnostic value of the status
/// register, and a wrong bit number turns a precise complaint into a confidently wrong one.
fn h7_error_text(sr: u32) -> Option<&'static str> {
    if sr & (1 << 17) != 0 {
        return Some("STM32H7 flash write protection error (WRPERR) -- the sector is write protected");
    }
    if sr & (1 << 18) != 0 {
        return Some("STM32H7 flash programming sequence error (PGSERR) -- PG was not set, or a flash word was not written in one go");
    }
    if sr & (1 << 19) != 0 {
        return Some("STM32H7 flash strobe error (STRBERR) -- the same bytes were written twice into one flash word");
    }
    if sr & (1 << 21) != 0 {
        return Some("STM32H7 flash inconsistency error (INCERR) -- a flash word was not written completely");
    }
    if sr & (1 << 22) != 0 {
        return Some("STM32H7 flash write/erase error (OPERR)");
    }
    if sr & (1 << 23) != 0 {
        return Some("STM32H7 flash read protection error (RDPERR)");
    }
    if sr & (1 << 24) != 0 {
        return Some("STM32H7 flash secure error (RDSERR)");
    }
    None
}

/// Which bank an execution address lives in, as a register-block offset.
fn h7_bank_offset(address: u32) -> u32 {
    if address >= STM32H7_BANK2_BASE { H7_BANK_STRIDE } else { 0 }
}

/// The address of `register` in the register block of the bank holding `address`.
fn h7_reg(address: u32, register: u32) -> u32 {
    H7_FLASH_BASE + h7_bank_offset(address) + register
}

/// The sector index WITHIN its own bank -- bank 2's first sector is 0, not 8.
fn h7_sector_of(address: u32) -> u32 {
    let bank_base = if address >= STM32H7_BANK2_BASE { STM32H7_BANK2_BASE } else { STM32H7_FLASH_BASE };
    (address - bank_base) / STM32H7_SECTOR
}

/// The first address after the sector containing `address` -- where a chunk must stop.
///
/// Programming is chunked at SECTOR boundaries rather than by a fixed size, and that is a
/// correctness requirement rather than tidiness: the sector is the erase unit, so it is the unit a
/// failed chunk can be recovered by, AND a sector never spans the two banks -- whose control,
/// status and lock registers are entirely separate.
fn h7_sector_end(address: u32) -> u32 {
    let bank_base = if address >= STM32H7_BANK2_BASE { STM32H7_BANK2_BASE } else { STM32H7_FLASH_BASE };
    bank_base + (h7_sector_of(address) + 1) * STM32H7_SECTOR
}

/// Waits for the bank holding `address` to go idle, then reports any error it left behind.
///
/// IT WAITS ON `QW` AS WELL AS `BSY`, which is the reason this is not the F4's routine with
/// different constants -- see the module note.
///
/// On [`FlashWait::BeforeOperation`] the latched flags are cleared and NOT reported -- see the
/// enum, which records the board this distinction was measured on.
pub(crate) fn h7_wait_idle<A: TargetAccess>(
    target: &mut A,
    address: u32,
    phase: FlashWait,
) -> Result<(), ProbeError> {
    let sr_at = h7_reg(address, H7_SR);
    for _ in 0..200_000 {
        let sr = target.read_word(sr_at)?;
        if sr & (H7_SR_BSY | H7_SR_QW) != 0 {
            continue;
        }
        if phase == FlashWait::BeforeOperation {
            if sr & (H7_SR_ERRORS | H7_SR_EOP) != 0 {
                target.write_word(h7_reg(address, H7_CCR), H7_SR_ERRORS | H7_SR_EOP)?;
            }
            return Ok(());
        }
        if let Some(text) = h7_error_text(sr) {
            target.write_word(h7_reg(address, H7_CCR), H7_SR_ERRORS | H7_SR_EOP)?;
            return Err(ProbeError::Device(text));
        }
        if sr & H7_SR_EOP != 0 {
            target.write_word(h7_reg(address, H7_CCR), H7_SR_EOP)?;
        }
        return Ok(());
    }
    Err(ProbeError::Timeout("STM32H7 flash controller busy"))
}

/// STM32H7 embedded-flash programming, added to ANY [`TargetAccess`] probe -- so an ST-Link drives
/// it exactly as a CMSIS-DAP probe does. Halt the core before erasing or writing.
pub trait Stm32H7Flash {
    /// Unlocks the control register of the bank holding `address`. Idempotent.
    ///
    /// PER BANK: unlocking bank 1 leaves bank 2 locked. And the key sequence is checked for
    /// having already been done, because RM0399 is explicit that performing it TWICE locks the
    /// register until the next system reset -- so a "harmless" repeat bricks the session.
    fn h7_unlock_flash(&mut self, address: u32) -> Result<(), ProbeError>;
    /// Re-locks the control register of the bank holding `address`.
    fn h7_lock_flash(&mut self, address: u32) -> Result<(), ProbeError>;
    /// Erases the 128 KB sector containing `address`.
    fn h7_erase_sector(&mut self, address: u32) -> Result<(), ProbeError>;
    /// Programs `data` from `address`, which must be 32-byte aligned and lie in erased flash. A
    /// trailing partial flash word is padded with `0xFF`.
    fn h7_program(&mut self, address: u32, data: &[u8]) -> Result<(), ProbeError>;
}

impl<A: TargetAccess> Stm32H7Flash for A {
    fn h7_unlock_flash(&mut self, address: u32) -> Result<(), ProbeError> {
        let cr_at = h7_reg(address, H7_CR);
        if self.read_word(cr_at)? & H7_CR_LOCK == 0 {
            return Ok(());
        }
        let keyr = h7_reg(address, H7_KEYR);
        self.write_word(keyr, H7_KEY1)?;
        self.write_word(keyr, H7_KEY2)?;
        if self.read_word(cr_at)? & H7_CR_LOCK != 0 {
            return Err(ProbeError::Device("STM32H7 flash stayed locked after the key sequence"));
        }
        Ok(())
    }

    fn h7_lock_flash(&mut self, address: u32) -> Result<(), ProbeError> {
        let cr_at = h7_reg(address, H7_CR);
        let cr = self.read_word(cr_at)?;
        self.write_word(cr_at, cr | H7_CR_LOCK)
    }

    fn h7_erase_sector(&mut self, address: u32) -> Result<(), ProbeError> {
        h7_wait_idle(self, address, FlashWait::BeforeOperation)?;
        let cr_at = h7_reg(address, H7_CR);
        let snb = (h7_sector_of(address) << H7_CR_SNB_SHIFT) & H7_CR_SNB_MASK;
        self.write_word(cr_at, H7_CR_SER | H7_CR_PSIZE_WORD | snb)?;
        self.write_word(cr_at, H7_CR_SER | H7_CR_PSIZE_WORD | snb | H7_CR_START)?;
        h7_wait_idle(self, address, FlashWait::AfterOperation)?;
        self.write_word(cr_at, H7_CR_PSIZE_WORD)
    }

    fn h7_program(&mut self, address: u32, data: &[u8]) -> Result<(), ProbeError> {
        if address as usize % STM32H7_FLASH_WORD != 0 {
            return Err(ProbeError::Device("STM32H7 programming starts on a 32-byte flash-word boundary"));
        }
        let mut padded = data.to_vec();
        while padded.len() % STM32H7_FLASH_WORD != 0 {
            padded.push(0xff);
        }
        let words: Vec<u32> = padded
            .chunks(4)
            .map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
            .collect();

        let mut at = address;
        let mut rest: &[u32] = &words;
        let mut prepared: Option<u32> = None;
        while !rest.is_empty() {
            let cr_at = h7_reg(at, H7_CR);
            if prepared != Some(cr_at) {
                if let Some(previous) = prepared {
                    let cr = self.read_word(previous)?;
                    self.write_word(previous, cr & !H7_CR_PG)?;
                }
                if self.read_word(cr_at)? & H7_CR_LOCK != 0 {
                    return Err(ProbeError::Device(
                        "STM32H7 programming reached a locked flash bank -- unlock the bank holding the end of the image as well as the one holding its start",
                    ));
                }
                h7_wait_idle(self, at, FlashWait::BeforeOperation)?;
                self.write_word(cr_at, H7_CR_PG | H7_CR_PSIZE_WORD)?;
                prepared = Some(cr_at);
            }

            let room = ((h7_sector_end(at) - at) / 4) as usize;
            let take = rest.len().min(room);
            self.write_words(at, &rest[..take])?;
            h7_wait_idle(self, at, FlashWait::AfterOperation)?;
            at += (take * 4) as u32;
            rest = &rest[take..];
        }

        if let Some(cr_at) = prepared {
            let cr = self.read_word(cr_at)?;
            self.write_word(cr_at, cr & !H7_CR_PG)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bank_split_follows_the_execution_address() {
        assert_eq!(h7_bank_offset(0x0800_0000), 0);
        assert_eq!(h7_bank_offset(0x080f_ffff), 0);
        assert_eq!(h7_bank_offset(0x0810_0000), H7_BANK_STRIDE);
        assert_eq!(h7_reg(0x0800_0000, H7_CR), 0x5200_200c);
        assert_eq!(h7_reg(0x0810_0000, H7_CR), 0x5200_210c);
    }

    #[test]
    fn every_write_error_bit_is_named() {
        for bit in [17u32, 18, 19, 21, 22, 23, 24] {
            assert!(h7_error_text(1 << bit).is_some(), "SR bit {bit} must be named");
        }
        assert!(h7_error_text(0).is_none());
        assert!(h7_error_text(H7_SR_BSY | H7_SR_QW).is_none());
        assert!(h7_error_text(1 << 20).is_none());
    }

    #[test]
    fn the_sector_number_is_within_its_bank_and_fits_the_field() {
        assert_eq!(h7_sector_of(0x0800_0000), 0);
        assert_eq!(h7_sector_of(0x0802_0000), 1);
        assert_eq!(h7_sector_of(0x080e_0000), 7);
        assert_eq!(h7_sector_of(0x0810_0000), 0);
        assert_eq!(h7_sector_of(0x0812_0000), 1);
        for sector in [0u32, 1, 7] {
            let field = (sector << H7_CR_SNB_SHIFT) & H7_CR_SNB_MASK;
            assert_eq!(field >> H7_CR_SNB_SHIFT, sector, "sector {sector} must survive the field");
        }
    }

    #[test]
    fn a_chunk_stops_at_the_sector_it_started_in() {
        assert_eq!(h7_sector_end(0x0800_0000), 0x0802_0000);
        assert_eq!(h7_sector_end(0x0801_ffff), 0x0802_0000);
        assert_eq!(h7_sector_end(0x0802_0000), 0x0804_0000);
        assert_eq!(h7_sector_end(0x080e_0000), 0x0810_0000);
        assert_eq!(h7_sector_end(0x080f_ffff), 0x0810_0000);
        assert_eq!(h7_sector_end(0x0810_0000), 0x0812_0000);
        assert_eq!(h7_sector_end(0x081e_0000), 0x0820_0000);
    }

    #[test]
    fn no_chunk_boundary_can_fall_inside_a_flash_word() {
        assert_eq!(STM32H7_SECTOR as usize % STM32H7_FLASH_WORD, 0);
        for at in [0x0800_0000u32, 0x0802_0000, 0x080e_0000, 0x0810_0000] {
            assert_eq!(h7_sector_end(at) as usize % STM32H7_FLASH_WORD, 0);
        }
    }

    #[test]
    fn the_two_banks_never_share_a_control_register() {
        assert_ne!(h7_reg(0x080e_0000, H7_CR), h7_reg(0x0810_0000, H7_CR));
        assert_ne!(h7_reg(0x080e_0000, H7_SR), h7_reg(0x0810_0000, H7_SR));
        assert_ne!(h7_reg(0x080e_0000, H7_KEYR), h7_reg(0x0810_0000, H7_KEYR));
        assert_ne!(h7_reg(0x080e_0000, H7_CCR), h7_reg(0x0810_0000, H7_CCR));
    }

    #[test]
    fn programming_refuses_an_address_that_is_not_flash_word_aligned() {
        assert_eq!(0x0800_0000u32 as usize % STM32H7_FLASH_WORD, 0);
        assert_ne!(0x0800_0004u32 as usize % STM32H7_FLASH_WORD, 0);
        assert_ne!(0x0800_0010u32 as usize % STM32H7_FLASH_WORD, 0);
    }
}
