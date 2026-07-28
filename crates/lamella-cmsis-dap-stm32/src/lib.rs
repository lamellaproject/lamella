//! STMicroelectronics STM32 flash programming over a Lamella debug probe.

use lamella_probe_core::{CallFrame, ProbeError, TargetAccess};

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

/// STM32F4 embedded-flash programming, added to a CMSIS-DAP [`TargetAccess`] probe. Halt the core before
/// erasing or writing so it is not fetching from flash during the operation, and program only
/// erased (0xFF) flash.
pub trait Stm32F4Flash {
    /// Unlocks `FLASH_CR` for erase/program (writes the two `FLASH_KEYR` keys). Idempotent: the
    /// keys have no effect if the controller is already unlocked.
    fn unlock_flash(&mut self) -> Result<(), ProbeError>;
    /// Re-locks `FLASH_CR`.
    fn lock_flash(&mut self) -> Result<(), ProbeError>;
    /// Erases flash `sector` (its size is part-dependent: sector 0-3 = 16 KB, 4 = 64 KB, 5+ =
    /// 128 KB on a 1 MB F4). The controller must be unlocked.
    fn erase_sector(&mut self, sector: u32) -> Result<(), ProbeError>;
    /// Programs consecutive 32-bit `words` to flash from `address` (which, with its span, must lie
    /// in already-erased sectors). The controller must be unlocked.
    fn program_words(&mut self, address: u32, words: &[u32]) -> Result<(), ProbeError>;
}

impl<A: TargetAccess> Stm32F4Flash for A {
    fn unlock_flash(&mut self) -> Result<(), ProbeError> {
        self.write_word(FLASH_KEYR, KEY1)?;
        self.write_word(FLASH_KEYR, KEY2)
    }

    fn lock_flash(&mut self) -> Result<(), ProbeError> {
        let cr = self.read_word(FLASH_CR)?;
        self.write_word(FLASH_CR, cr | CR_LOCK)
    }

    fn erase_sector(&mut self, sector: u32) -> Result<(), ProbeError> {
        wait_not_busy(self)?;
        let base = CR_PSIZE_X32 | CR_SER | (sector << CR_SNB_SHIFT);
        self.write_word(FLASH_CR, base)?;
        self.write_word(FLASH_CR, base | CR_STRT)?;
        wait_not_busy(self)?;
        self.write_word(FLASH_CR, CR_PSIZE_X32)
    }

    fn program_words(&mut self, address: u32, words: &[u32]) -> Result<(), ProbeError> {
        wait_not_busy(self)?;
        self.write_word(FLASH_CR, CR_PSIZE_X32 | CR_PG)?;
        for (index, &word) in words.iter().enumerate() {
            self.write_word(address + (index as u32) * 4, word)?;
            wait_not_busy(self)?;
        }
        self.write_word(FLASH_CR, CR_PSIZE_X32)
    }
}

const F0_FLASH_BASE: u32 = 0x4002_2000;
const F0_KEYR: u32 = F0_FLASH_BASE + 0x04;
const F0_SR: u32 = F0_FLASH_BASE + 0x0c;
const F0_CR: u32 = F0_FLASH_BASE + 0x10;
const F0_AR: u32 = F0_FLASH_BASE + 0x14;
const F0_KEY1: u32 = 0x4567_0123;
const F0_KEY2: u32 = 0xcdef_89ab;
const F0_CR_PER: u32 = 1 << 1;
const F0_CR_STRT: u32 = 1 << 6;
const F0_CR_LOCK: u32 = 1 << 7;
const F0_SR_BSY: u32 = 1 << 0;
const F0_SR_PGERR: u32 = 1 << 2;
const F0_SR_WRPRTERR: u32 = 1 << 4;
const F0_SR_EOP: u32 = 1 << 5;

/// Page size on the parts this drives (RM0091 Table 5: 2 KB pages; an F091 has 128 of them).
pub const STM32F0_PAGE: u32 = 2048;
/// Where main flash is mapped.
pub const STM32F0_FLASH_BASE: u32 = 0x0800_0000;

/// The programming loader, assembled from `f0-loader.s` in this directory (`arm-none-eabi-as
/// -mcpu=cortex-m0`), kept beside it so the machine code is reproducible rather than hand-encoded.
///
/// It exists because of a PORTABILITY constraint, not a performance one. These controllers program
/// 16 bits at a time, and a probe cannot be relied on to issue a genuine 16-bit BUS cycle -- an
/// ST-Link has no 16-bit access at all, so `write_halfword` there is two byte writes. Running the
/// stores ON THE CORE makes the access width the core's business, and the driver then behaves
/// identically through every probe family.
const F0_LOADER: [u32; 14] = [
    0x2501_691c, 0x611c_432c, 0xd00c_2a00, 0x800c_8804, 0x2601_68dd, 0xd1fb_4235, 0x4235_2614,
    0x3002_d105, 0x3a01_3102, 0x2000_e7f0, 0x0028_e000, 0x2501_691c, 0x611c_43ac, 0x46c0_4770,
];

/// Where the loader, its scratch and its buffer live in target RAM while programming.
///
/// The whole window is clobbered, so the core must be halted and is not resumable afterward without
/// a reset. Chosen to sit at the base of SRAM, which every STM32F0 maps at 0x2000_0000 with at least
/// 16 KB -- an F091 has 32.
const F0_TRAP: u32 = 0x2000_0000;
const F0_LOADER_ADDR: u32 = 0x2000_0010;
const F0_BUFFER: u32 = 0x2000_0100;
const F0_STACK_TOP: u32 = 0x2000_2000;
/// Bytes staged per loader invocation. One page at a time keeps RAM use modest and matches the
/// erase granule, so a caller reasoning in pages sees one buffer fill per page.
const F0_CHUNK: usize = STM32F0_PAGE as usize;

/// STM32 F0 embedded-flash programming (RM0091). See the scope note above before assuming another
/// family shares this controller -- the L0 does not.
///
/// Halt the core before erasing or writing: programming runs a loader in target RAM, which clobbers
/// the low SRAM window and leaves the core unable to resume without a reset.
pub trait Stm32F0Flash {
    /// Unlocks `FLASH_CR` for erase and program (the two-key sequence). Idempotent.
    fn f0_unlock_flash(&mut self) -> Result<(), ProbeError>;
    /// Re-locks `FLASH_CR`.
    fn f0_lock_flash(&mut self) -> Result<(), ProbeError>;
    /// Erases the 2 KB page containing `address`.
    fn f0_erase_page(&mut self, address: u32) -> Result<(), ProbeError>;
    /// Programs `data` to `address`, which must be half-word aligned, as must the length. The pages
    /// spanned must already be erased.
    fn f0_program(&mut self, address: u32, data: &[u8]) -> Result<(), ProbeError>;
}

impl<A: TargetAccess> Stm32F0Flash for A {
    fn f0_unlock_flash(&mut self) -> Result<(), ProbeError> {
        self.write_word(F0_KEYR, F0_KEY1)?;
        self.write_word(F0_KEYR, F0_KEY2)?;
        if self.read_word(F0_CR)? & F0_CR_LOCK != 0 {
            return Err(ProbeError::Device("STM32F0 flash stayed locked after the key sequence"));
        }
        Ok(())
    }

    fn f0_lock_flash(&mut self) -> Result<(), ProbeError> {
        let cr = self.read_word(F0_CR)?;
        self.write_word(F0_CR, cr | F0_CR_LOCK)
    }

    fn f0_erase_page(&mut self, address: u32) -> Result<(), ProbeError> {
        f0_wait_idle(self)?;
        self.write_word(F0_CR, F0_CR_PER)?;
        self.write_word(F0_AR, address & !(STM32F0_PAGE - 1))?;
        self.write_word(F0_CR, F0_CR_PER | F0_CR_STRT)?;
        f0_wait_idle(self)?;
        self.write_word(F0_CR, 0)
    }

    fn f0_program(&mut self, address: u32, data: &[u8]) -> Result<(), ProbeError> {
        if address % 2 != 0 || data.len() % 2 != 0 {
            return Err(ProbeError::Device("STM32F0 programming is half-word granular"));
        }
        f0_wait_idle(self)?;
        self.write_words(F0_LOADER_ADDR, &F0_LOADER)?;

        let frame = CallFrame::new(F0_STACK_TOP, F0_TRAP);
        let mut address = address;
        for chunk in data.chunks(F0_CHUNK) {
            let mut words = Vec::with_capacity(chunk.len().div_ceil(4));
            for group in chunk.chunks(4) {
                let mut buf = [0xffu8; 4];
                buf[..group.len()].copy_from_slice(group);
                words.push(u32::from_le_bytes(buf));
            }
            self.write_words(F0_BUFFER, &words)?;

            let halfwords = (chunk.len() / 2) as u32;
            let status = self.call_target(
                F0_LOADER_ADDR | 1,
                &[F0_BUFFER, address, halfwords, F0_FLASH_BASE],
                &frame,
            )?;
            if status != 0 {
                return Err(if status & F0_SR_WRPRTERR != 0 {
                    ProbeError::Device("STM32F0 program hit write protection (WRPRTERR)")
                } else {
                    ProbeError::Device("STM32F0 programming error (PGERR) -- is the page erased?")
                });
            }
            address += chunk.len() as u32;
        }
        Ok(())
    }
}

/// Waits for the controller to go idle, then reports any error the last operation left latched.
/// Error flags are write-1-to-clear and persist, so they are cleared here rather than left to
/// poison the next operation's status read.
fn f0_wait_idle<A: TargetAccess>(target: &mut A) -> Result<(), ProbeError> {
    for _ in 0..100_000 {
        let sr = target.read_word(F0_SR)?;
        if sr & F0_SR_BSY != 0 {
            continue;
        }
        if sr & (F0_SR_PGERR | F0_SR_WRPRTERR | F0_SR_EOP) != 0 {
            target.write_word(F0_SR, F0_SR_PGERR | F0_SR_WRPRTERR | F0_SR_EOP)?;
        }
        if sr & F0_SR_WRPRTERR != 0 {
            return Err(ProbeError::Device("STM32F0 flash write protection error (WRPRTERR)"));
        }
        if sr & F0_SR_PGERR != 0 {
            return Err(ProbeError::Device("STM32F0 flash programming error (PGERR)"));
        }
        return Ok(());
    }
    Err(ProbeError::Timeout("STM32F0 flash controller busy"))
}

/// Polls `FLASH_SR` until the controller reports not busy.
fn wait_not_busy<A: TargetAccess>(target: &mut A) -> Result<(), ProbeError> {
    for _ in 0..100_000 {
        if target.read_word(FLASH_SR)? & SR_BSY == 0 {
            return Ok(());
        }
    }
    Err(ProbeError::Timeout("STM32 flash busy"))
}
