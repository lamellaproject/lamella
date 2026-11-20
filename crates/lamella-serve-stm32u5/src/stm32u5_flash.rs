//! The STM32U5 embedded-flash controller (RM0456 Rev 7 section 7), driven from firmware for
//! self-programming: unlock, page erase, quad-word program. Register-only and unsafe, so it lives in
//! the bare-metal shim and the interpreter crates stay `forbid(unsafe_code)`. It is the sequence the
//! host route runs through a probe (`lamella-cmsis-dap-stm32`'s U5 routines), run here by the
//! target on itself, so a serve firmware can receive a deployed image into its own flash and run it
//! in place.
//!
//! Bank 2 only. RM0456 Table 53 organizes the STM32U5A5's 4 MB as two banks of 2 MB, each of 256
//! pages of 8 KB numbered from 0 within the bank: bank 1 from 0x0800 0000, bank 2 from 0x0820 0000.
//! The firmware runs from bank 1, and nothing here can name a page of it: an erase takes a bank-2
//! page number, and a program refuses an address outside bank 2. Section 7.3.5: an ongoing flash
//! operation "does not block the CPU as long as the CPU does not access the same flash memory bank",
//! so an erase or a program never stalls the code performing it.
//!
//! Before anything is erased or programmed, [`Stm32U5Flash::banks_as_mapped`] asks the part whether
//! its banks are where this map puts them: with TrustZone on, the secure watermarks decide which
//! pages the nonsecure registers driven here may reach; swapped banks would put bank 1 at the
//! window's addresses; and a part with less flash has its bank 2 elsewhere.
//!
//! The deploy window is the linker script's (`_simage`, `_image_len`), and its first page is derived
//! from that base rather than written a second time here.
//!
//! Reading back what was programmed is safe because the instruction cache and the data cache are
//! off at reset (RM0456 sections 8.7.1 and 9.7.1) and this firmware turns neither on, so a CPU read
//! of freshly programmed flash sees the new data with no cache maintenance.

use core::ptr::{read_volatile, write_volatile};

const FLASH_REGISTERS: usize = 0x4002_2000;
const FLASH_ACR: usize = FLASH_REGISTERS;
const ACR_LATENCY_MASK: u32 = 0xF;
const FLASH_NSKEYR: usize = FLASH_REGISTERS + 0x08;
const FLASH_NSSR: usize = FLASH_REGISTERS + 0x20;
const FLASH_NSCR: usize = FLASH_REGISTERS + 0x28;
const FLASH_OPTR: usize = FLASH_REGISTERS + 0x40;
const FLASH_SIZE_DATA: usize = 0x0BFA_07A0;

const KEY1: u32 = 0x4567_0123;
const KEY2: u32 = 0xCDEF_89AB;

const SR_EOP: u32 = 1 << 0;
const SR_ERRORS: u32 = (1 << 1) | (1 << 3) | (1 << 4) | (1 << 5) | (1 << 6) | (1 << 7) | (1 << 13);
const SR_BSY: u32 = 1 << 16;
const SR_WDW: u32 = 1 << 17;
const CR_PG: u32 = 1 << 0;
const CR_PER: u32 = 1 << 1;
const CR_PNB_SHIFT: u32 = 3;
const CR_BKER: u32 = 1 << 11;
const CR_STRT: u32 = 1 << 16;
const CR_LOCK: u32 = 1 << 31;
const OPTR_TZEN: u32 = 1 << 31;
const OPTR_SWAP_BANK: u32 = 1 << 20;

/// The flash this map is for, in Kbytes: the STM32U5A5ZJ's 4 MB.
const FLASH_KBYTES: u32 = 4096;
/// Bank 2's base on a 4 MB part (RM0456 Table 53).
const BANK2_BASE: usize = 0x0820_0000;
/// The pages in one bank of a 4 MB part, numbered 0 to 255 (RM0456 Table 53).
const PAGES_PER_BANK: u32 = 256;
pub use crate::stm32u5_deploy::{PAGE, QUAD_WORD};

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
/// registers are Device memory, and this core may let a buffered write to the first pass a read of
/// the second, so the words of a quad-word are fenced before the status register is read.
fn data_barrier() {
    unsafe { core::arch::asm!("dsb", options(nostack, preserves_flags)) };
}

/// Sets the flash read latency to `wait_states`, and waits until the controller reports it in
/// force: RM0456 section 7.3.3's first two steps before raising the core clock are to program
/// LATENCY and then check, by reading FLASH_ACR back, that the new count is used.
pub fn set_wait_states(wait_states: u32) {
    let acr = read_reg(FLASH_ACR);
    write_reg(FLASH_ACR, (acr & !ACR_LATENCY_MASK) | (wait_states & ACR_LATENCY_MASK));
    while read_reg(FLASH_ACR) & ACR_LATENCY_MASK != wait_states & ACR_LATENCY_MASK {}
}

/// The STM32U5A5's embedded-flash controller, through its nonsecure registers. Unlock before an
/// erase or a program, and program only erased quad-words of bank 2.
pub struct Stm32U5Flash;

impl Stm32U5Flash {
    /// Whether the part's banks are where this map puts them, the only layout this driver erases
    /// and programs by: TrustZone off, where the secure flash registers read as zero and ignore
    /// writes (RM0456 section 7.5.1), so the nonsecure ones driven here are the only set in force;
    /// the banks not swapped, so bank 2 is at 0x0820 0000; and 4 MB of flash, so each bank is 2 MB.
    pub fn banks_as_mapped() -> bool {
        let optr = read_reg(FLASH_OPTR);
        optr & (OPTR_TZEN | OPTR_SWAP_BANK) == 0 && read_reg(FLASH_SIZE_DATA) & 0xFFFF == FLASH_KBYTES
    }

    /// Waits until the controller is idle and its write buffer empty. RM0456 section 7.3.7 asks
    /// that the wait for BSY to clear begin only once BSY is set or WDW is clear, and polling both
    /// together does that: WDW clears as BSY sets, when the fourth word of a quad-word arrives.
    fn wait_idle() {
        while read_reg(FLASH_NSSR) & (SR_BSY | SR_WDW) != 0 {}
    }

    /// Waits until the controller is idle, then clears the flags a previous operation left, so the
    /// flags read after the next operation are that operation's own. RM0456 sections 7.3.6 and 7.3.7
    /// begin each sequence there: with an error flag left set, the next operation sets PGSERR.
    fn wait_ready() {
        Self::wait_idle();
        write_reg(FLASH_NSSR, SR_EOP | SR_ERRORS);
    }

    /// Writes the two keys to unlock FLASH_NSCR, only while it is locked. Returns whether it is
    /// unlocked.
    pub fn unlock(&mut self) -> bool {
        if read_reg(FLASH_NSCR) & CR_LOCK != 0 {
            write_reg(FLASH_NSKEYR, KEY1);
            write_reg(FLASH_NSKEYR, KEY2);
        }
        read_reg(FLASH_NSCR) & CR_LOCK == 0
    }

    /// Locks FLASH_NSCR again.
    pub fn lock(&mut self) {
        write_reg(FLASH_NSCR, read_reg(FLASH_NSCR) | CR_LOCK);
    }

    /// Erases page `page` of bank 2, by RM0456 section 7.3.6's page erase: idle and the error flags
    /// clear, then PER with the page and its bank, then STRT, then wait. Returns whether the
    /// controller reported no error; a page number outside the bank erases nothing.
    pub fn erase_bank2_page(&mut self, page: u32) -> bool {
        if page >= PAGES_PER_BANK {
            return false;
        }
        Self::wait_ready();
        let select = CR_PER | CR_BKER | (page << CR_PNB_SHIFT);
        write_reg(FLASH_NSCR, select);
        write_reg(FLASH_NSCR, select | CR_STRT);
        data_barrier();
        Self::wait_idle();
        let clean = read_reg(FLASH_NSSR) & SR_ERRORS == 0;
        write_reg(FLASH_NSCR, 0);
        clean
    }

    /// Programs `bytes` from `addr` (quad-word aligned, in bank 2) one quad-word at a time, by
    /// RM0456 section 7.3.7's sequence, padding a trailing partial quad-word with 0xFF, which leaves
    /// those bytes erased. Only an erased quad-word is programmed: the controller refuses to program
    /// an address twice between erases, and this stops before trying. Successive calls must not
    /// split a quad-word, which holds while chunks are 16-byte aligned in offset and length. Returns
    /// whether every quad-word was erased beforehand and the controller reported no error; the
    /// caller reads the bytes back as well.
    pub fn program_quad_words(&mut self, addr: usize, bytes: &[u8]) -> bool {
        let in_bank2 = addr >= BANK2_BASE
            && addr
                .checked_add(bytes.len())
                .is_some_and(|end| end <= BANK2_BASE + PAGES_PER_BANK as usize * PAGE);
        if !in_bank2 || addr % QUAD_WORD != 0 {
            return false;
        }
        Self::wait_ready();
        write_reg(FLASH_NSCR, CR_PG);
        let mut clean = true;
        let mut offset = 0;
        while offset < bytes.len() {
            let at = addr + offset;
            let erased = unsafe { core::slice::from_raw_parts(at as *const u8, QUAD_WORD) }
                .iter()
                .all(|&byte| byte == 0xFF);
            if !erased {
                clean = false;
                break;
            }
            let mut quad = [0xFFu8; QUAD_WORD];
            let n = core::cmp::min(QUAD_WORD, bytes.len() - offset);
            quad[..n].copy_from_slice(&bytes[offset..offset + n]);
            for (index, word) in quad.chunks_exact(4).enumerate() {
                write_reg(at + 4 * index, u32::from_le_bytes([word[0], word[1], word[2], word[3]]));
            }
            data_barrier();
            Self::wait_idle();
            if read_reg(FLASH_NSSR) & SR_ERRORS != 0 {
                clean = false;
                break;
            }
            offset += QUAD_WORD;
        }
        write_reg(FLASH_NSCR, 0);
        clean
    }

    /// The bank-2 pages that `len` bytes from the window's base reach, as a range of page numbers.
    /// Empty when the window is not where this map can erase it, which the caller treats as a
    /// refusal rather than erasing by a guess.
    pub fn window_pages(len: usize) -> core::ops::Range<u32> {
        let base = image_base();
        if base < BANK2_BASE || (base - BANK2_BASE) % PAGE != 0 || len > image_len() {
            return 0..0;
        }
        let first = ((base - BANK2_BASE) / PAGE) as u32;
        let last = first + len.div_ceil(PAGE) as u32;
        if last > PAGES_PER_BANK {
            return 0..0;
        }
        first..last
    }
}
