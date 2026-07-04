//! STMicroelectronics STM32 flash programming over a Lamella CMSIS-DAP debug probe.

use lamella_cmsis_dap::{Dap, DapError, Transport};

const FLASH_KEYR: u32 = 0x4002_3C04;
const FLASH_SR: u32 = 0x4002_3C0C;
const FLASH_CR: u32 = 0x4002_3C10;
const KEY1: u32 = 0x4567_0123;
const KEY2: u32 = 0xCDEF_89AB;
const SR_BSY: u32 = 1 << 16;
const CR_PG: u32 = 1 << 0;
const CR_SER: u32 = 1 << 1;
const CR_SNB_SHIFT: u32 = 3;
const CR_PSIZE_X32: u32 = 0b10 << 8;
const CR_STRT: u32 = 1 << 16;
const CR_LOCK: u32 = 1 << 31;

/// STM32F4 embedded-flash programming, added to a CMSIS-DAP [`Dap`] probe. Halt the core before
/// erasing or writing so it is not fetching from flash during the operation, and program only
/// erased (0xFF) flash.
pub trait Stm32F4Flash {
    /// Unlocks `FLASH_CR` for erase/program (writes the two `FLASH_KEYR` keys). Idempotent: the
    /// keys have no effect if the controller is already unlocked.
    fn unlock_flash(&mut self) -> Result<(), DapError>;
    /// Re-locks `FLASH_CR`.
    fn lock_flash(&mut self) -> Result<(), DapError>;
    /// Erases flash `sector` (its size is part-dependent: sector 0-3 = 16 KB, 4 = 64 KB, 5+ =
    /// 128 KB on a 1 MB F4). The controller must be unlocked.
    fn erase_sector(&mut self, sector: u32) -> Result<(), DapError>;
    /// Programs consecutive 32-bit `words` to flash from `address` (which, with its span, must lie
    /// in already-erased sectors). The controller must be unlocked.
    fn program_words(&mut self, address: u32, words: &[u32]) -> Result<(), DapError>;
}

impl<T: Transport> Stm32F4Flash for Dap<T> {
    fn unlock_flash(&mut self) -> Result<(), DapError> {
        self.write_word(FLASH_KEYR, KEY1)?;
        self.write_word(FLASH_KEYR, KEY2)
    }

    fn lock_flash(&mut self) -> Result<(), DapError> {
        let cr = self.read_word(FLASH_CR)?;
        self.write_word(FLASH_CR, cr | CR_LOCK)
    }

    fn erase_sector(&mut self, sector: u32) -> Result<(), DapError> {
        wait_not_busy(self)?;
        let base = CR_PSIZE_X32 | CR_SER | (sector << CR_SNB_SHIFT);
        self.write_word(FLASH_CR, base)?;
        self.write_word(FLASH_CR, base | CR_STRT)?;
        wait_not_busy(self)?;
        self.write_word(FLASH_CR, CR_PSIZE_X32)
    }

    fn program_words(&mut self, address: u32, words: &[u32]) -> Result<(), DapError> {
        wait_not_busy(self)?;
        self.write_word(FLASH_CR, CR_PSIZE_X32 | CR_PG)?;
        for (index, &word) in words.iter().enumerate() {
            self.write_word(address + (index as u32) * 4, word)?;
            wait_not_busy(self)?;
        }
        self.write_word(FLASH_CR, CR_PSIZE_X32)
    }
}

/// Polls `FLASH_SR` until the controller reports not busy.
fn wait_not_busy<T: Transport>(dap: &mut Dap<T>) -> Result<(), DapError> {
    for _ in 0..100_000 {
        if dap.read_word(FLASH_SR)? & SR_BSY == 0 {
            return Ok(());
        }
    }
    Err(DapError::Timeout("STM32 flash busy"))
}
