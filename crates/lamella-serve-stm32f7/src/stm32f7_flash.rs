//! The STM32F7 embedded-flash controller, driven from firmware for self-programming: unlock, sector
//! erase, word program. Register-only and unsafe, so it lives in the bare-metal shim and the
//! interpreter crates stay `forbid(unsafe_code)`. It is the sequence the host route runs through a
//! probe (`lamella-cmsis-dap-stm32`'s F4 primitives), run here by the target on itself, so a serve
//! firmware can receive a deployed image into its own flash and run it in place. RM0410 Rev 5 gives
//! the STM32F769 and RM0385 Rev 9 the STM32F746 the same register block, keys and `FLASH_CR` fields
//! (sections 3.3.4 and 3.7 of each), except that the F746's sector-number field is narrower.
//!
//! One sector map per part, chosen by the part feature as build.rs chooses the memory map:
//! - The STM32F769NI (`stm32f769`), in single-bank mode only. RM0410 section 3.3.1: with `nDBANK`
//!   set, the 2 Mbytes are one array of 4 sectors of 32 Kbytes, 1 of 128 Kbytes and 7 of 256
//!   Kbytes, so sectors 5 to 11 are uniform 256 Kbytes from 0x0804 0000. With `nDBANK` clear the
//!   same addresses are two banks of smaller sectors, and an erase by this map would take the wrong
//!   span.
//! - The STM32F746NG (`stm32f7`), which has one bank by construction. RM0385 section 3.3.1: its
//!   1 Mbyte is 4 sectors of 32 Kbytes, 1 of 128 Kbytes and 3 of 256 Kbytes, so sectors 5 to 7 are
//!   uniform 256 Kbytes from the same 0x0804 0000. Its FLASH_OPTCR has no bank bit (RM0385 section
//!   3.7.6), so the part is asked instead which part it is.
//!
//! On either part [`Stm32F7Flash::erases_by_this_map`] is asked before anything is erased.
//!
//! The deploy window is the linker script's (`_simage`, `_image_len`), and its first sector is
//! derived from that base rather than written a second time here.
//!
//! Two hardware facts make this safe from flash-resident code:
//! - The ART accelerator and the core's caches are off at reset, and this firmware turns neither
//!   on, so a CPU read of freshly programmed flash sees the new data with no cache maintenance.
//! - An erase or program operation stalls every read of the array until it completes (section
//!   3.3.3 of RM0410 and of RM0385). The firmware runs from the sectors below the window, 0 to 7 on
//!   the F769 and 0 to 5 on the F746, so its next fetch waits for the operation and resumes; nothing
//!   erases the code that is running.

use core::ptr::{read_volatile, write_volatile};

const FLASH_KEYR: usize = 0x4002_3C04;
const FLASH_SR: usize = 0x4002_3C0C;
const FLASH_CR: usize = 0x4002_3C10;
#[cfg(feature = "stm32f769")]
const FLASH_OPTCR: usize = 0x4002_3C14;
const KEY1: u32 = 0x4567_0123;
const KEY2: u32 = 0xCDEF_89AB;
const SR_BSY: u32 = 1 << 16;
const SR_EOP: u32 = 1 << 0;
const SR_ERRORS: u32 = (1 << 7) | (1 << 6) | (1 << 5) | (1 << 4) | (1 << 1);
const CR_PG: u32 = 1 << 0;
const CR_SER: u32 = 1 << 1;
const CR_SNB_SHIFT: u32 = 3;
const CR_PSIZE_X32: u32 = 0b10 << 8;
const CR_STRT: u32 = 1 << 16;
const CR_LOCK: u32 = 1 << 31;
#[cfg(feature = "stm32f769")]
const OPTCR_NDBANK: u32 = 1 << 29;

/// The die identity the part reports about itself (RM0410 Rev 5 section 44.6.1, RM0385 Rev 9
/// section 40.6.1): `DEV_ID` in bits 11:0, `REV_ID` in 31:16, read in one 32-bit access, readable
/// "by the user software". The STM32F746's map check reads it, and a firmware reports it.
pub const DBGMCU_IDCODE: usize = 0xE004_2000;
#[cfg(not(feature = "stm32f769"))]
const DEV_ID_MASK: u32 = 0xFFF;
#[cfg(not(feature = "stm32f769"))]
const DEV_ID_STM32F74X_F75X: u32 = 0x449;
#[cfg(not(feature = "stm32f769"))]
const FLASH_SIZE_KB: usize = 0x1FF0_F442;
#[cfg(not(feature = "stm32f769"))]
const FLASH_KB_WITH_SECTORS_6_AND_7: u16 = 1024;

/// The sectors from 5 to the part's last are uniform 256 Kbytes from here, on both parts (section
/// 3.3.1 of RM0410 and of RM0385).
const UNIFORM_SECTORS_BASE: usize = 0x0804_0000;
const FIRST_UNIFORM_SECTOR: u32 = 5;
/// The part's last sector: 11 in the STM32F769's single-bank map (RM0410 section 3.3.1), and 7 on
/// the STM32F746, whose FLASH_CR allows no other (RM0385 section 3.7.5).
#[cfg(feature = "stm32f769")]
const LAST_SECTOR: u32 = 11;
#[cfg(not(feature = "stm32f769"))]
const LAST_SECTOR: u32 = 7;
/// The erase granule over the deploy window: one 256-Kbyte sector.
pub const SECTOR_256K: usize = 256 * 1024;

unsafe extern "C" {
    static _simage: u8;
    static _image_len: u8;
}

/// The deployed-image window's base, as the linker script placed it.
pub fn image_base() -> usize {
    (&raw const _simage) as usize
}

/// How many bytes the deployed-image window spans.
pub fn image_len() -> usize {
    (&raw const _image_len) as usize
}

fn write_reg(addr: usize, val: u32) {
    unsafe { write_volatile(addr as *mut u32, val) };
}

fn read_reg(addr: usize) -> u32 {
    unsafe { read_volatile(addr as *const u32) }
}

/// Completes every write before the next access. The array is Normal memory and the flash
/// registers are Device memory, and this core may let a buffered write to the first pass a read
/// of the second, so a program operation is fenced before its BSY flag is read.
fn data_barrier() {
    unsafe { core::arch::asm!("dsb", options(nostack, preserves_flags)) };
}

/// The F7 embedded-flash controller. Unlock before an erase or a program, and program only
/// erased flash, a word at a time at word-aligned addresses.
pub struct Stm32F7Flash;

impl Stm32F7Flash {
    /// Whether the part's sectors are the ones this build's map erases by: on the STM32F769, that
    /// the array is in single-bank mode. Asked before anything is erased or programmed; a part that
    /// answers no is refused rather than erased by a guess.
    #[cfg(feature = "stm32f769")]
    pub fn erases_by_this_map() -> bool {
        read_reg(FLASH_OPTCR) & OPTCR_NDBANK != 0
    }

    /// Whether the part's sectors are the ones this build's map erases by: on the STM32F746, which
    /// has one bank by construction, that the part is the one RM0385's map describes, an
    /// STM32F74x/F75x by its device ID, with 1 Mbyte by its flash-size register, so that sectors 6
    /// and 7 are there. Asked before anything is erased or programmed; a part that answers no is
    /// refused rather than erased by a guess.
    #[cfg(not(feature = "stm32f769"))]
    pub fn erases_by_this_map() -> bool {
        let dev_id = read_reg(DBGMCU_IDCODE) & DEV_ID_MASK;
        let flash_kb = unsafe { read_volatile(FLASH_SIZE_KB as *const u16) };
        dev_id == DEV_ID_STM32F74X_F75X && flash_kb == FLASH_KB_WITH_SECTORS_6_AND_7
    }

    /// Waits until the controller is idle (RM0410 sections 3.3.7 and 3.3.8, and RM0385 sections
    /// 3.3.6 and 3.3.7, begin each sequence there), then clears the flags a previous operation
    /// left, so the flags read after the next operation are that operation's own.
    fn wait_ready() {
        while read_reg(FLASH_SR) & SR_BSY != 0 {}
        write_reg(FLASH_SR, SR_EOP | SR_ERRORS);
    }

    /// Writes the two keys to unlock FLASH_CR. Keys written to an unlocked register are ignored.
    pub fn unlock(&mut self) {
        if read_reg(FLASH_CR) & CR_LOCK != 0 {
            write_reg(FLASH_KEYR, KEY1);
            write_reg(FLASH_KEYR, KEY2);
        }
    }

    /// Locks FLASH_CR again.
    pub fn lock(&mut self) {
        write_reg(FLASH_CR, read_reg(FLASH_CR) | CR_LOCK);
    }

    /// Erases one sector: select SER and SNB, start, wait, then clear SER. Returns whether the
    /// controller reported no error.
    pub fn erase_sector(&mut self, sector: u32) -> bool {
        Self::wait_ready();
        let select = CR_PSIZE_X32 | CR_SER | (sector << CR_SNB_SHIFT);
        write_reg(FLASH_CR, select);
        write_reg(FLASH_CR, select | CR_STRT);
        data_barrier();
        while read_reg(FLASH_SR) & SR_BSY != 0 {}
        let clean = read_reg(FLASH_SR) & SR_ERRORS == 0;
        write_reg(FLASH_CR, CR_PSIZE_X32);
        clean
    }

    /// Programs `bytes` from `addr` (word aligned, in erased flash) as 32-bit words, padding a
    /// trailing partial word with 0xFF, which leaves those bytes erased. Successive calls must not
    /// split a word, which holds while chunks are 4-byte aligned in offset and length. Returns
    /// whether the controller reported no error; the caller reads the bytes back as well.
    pub fn program_bytes(&mut self, addr: usize, bytes: &[u8]) -> bool {
        Self::wait_ready();
        write_reg(FLASH_CR, CR_PSIZE_X32 | CR_PG);
        let mut clean = true;
        let mut offset = 0;
        while offset < bytes.len() {
            let mut word = [0xFFu8; 4];
            let n = core::cmp::min(4, bytes.len() - offset);
            word[..n].copy_from_slice(&bytes[offset..offset + n]);
            write_reg(addr + offset, u32::from_le_bytes(word));
            data_barrier();
            while read_reg(FLASH_SR) & SR_BSY != 0 {}
            if read_reg(FLASH_SR) & SR_ERRORS != 0 {
                clean = false;
                break;
            }
            offset += 4;
        }
        write_reg(FLASH_CR, CR_PSIZE_X32);
        clean
    }

    /// The sector holding `addr`, for an address in the uniform 256-Kbyte span.
    fn sector_at(addr: usize) -> u32 {
        FIRST_UNIFORM_SECTOR + ((addr - UNIFORM_SECTORS_BASE) / SECTOR_256K) as u32
    }

    /// The window's sectors that `len` bytes from its base reach, as a range of sector numbers.
    /// Empty when the window is not where this map can erase it, which the caller treats as a
    /// refusal rather than erasing by a guess.
    pub fn window_sectors(len: usize) -> core::ops::Range<u32> {
        let base = image_base();
        if base < UNIFORM_SECTORS_BASE || (base - UNIFORM_SECTORS_BASE) % SECTOR_256K != 0 || len > image_len() {
            return 0..0;
        }
        let first = Self::sector_at(base);
        let last = first + len.div_ceil(SECTOR_256K) as u32;
        if last > LAST_SECTOR + 1 {
            return 0..0;
        }
        first..last
    }
}
