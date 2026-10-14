//! A PSRAM on the RP2350's QSPI memory interface chip select 1, kept through a flash operation.
//!
//! A flash erase or program through the bootrom, and the XIP setup that follows it, disturb such a
//! PSRAM twice. The bootrom's XIP setup writes window 0's timing, read format and read command over
//! window 1's, so every PSRAM read then returns one value wherever it reads. And the operation ends
//! in the bootrom's `flash_flush_cache`, which invalidates every XIP cache line without writing it
//! back, so a PSRAM write still held in the cache is lost with its line. [`keeping_psram`] runs an
//! operation so that neither happens: it saves window 1's five settings and cleans the cache before
//! the operation, and writes the settings back after it.
//!
//! Every image that writes the flash at run time on such a board takes its operations through it:
//! this file is the board crate's module, and a firmware that does not link the board crate takes it
//! by `#[path]`. It reads and writes the chip through [`Words`], so a test runs it against a model.
//! On a board without a PSRAM it changes nothing: window 1 is unused, and flash lines are never dirty.

/// Window 1's settings: M1_TIMING, M1_RFMT, M1_RCMD, M1_WFMT and M1_WCMD, the five words at the QSPI
/// memory interface's base + 0x20 (RP2350 datasheet 12.14).
pub const WINDOW1_SETTINGS: usize = 0x400d_0020;
/// How many words [`WINDOW1_SETTINGS`] spans.
pub const WINDOW1_WORDS: usize = 5;

/// The XIP cache's maintenance alias (datasheet 4.4): an operation in the address's three low bits,
/// the way in bit 13 and the line in bits 12:3.
pub const CACHE_MAINTENANCE: usize = 0x1800_0000;
/// The maintenance operation that writes a line back if it is dirty: clean by set and way.
pub const CLEAN_BY_SET_WAY: usize = 1;
/// The cache's ways.
pub const CACHE_WAYS: usize = 2;
/// The lines in each of the cache's ways.
pub const CACHE_LINES_PER_WAY: usize = 1024;

/// Word reads and writes at the chip's addresses.
#[allow(unsafe_code)]
pub trait Words {
    /// Reads the word at `address`.
    ///
    /// # Safety
    /// `address` is one of the registers or aliases this module names.
    unsafe fn read(&mut self, address: usize) -> u32;
    /// Writes `value` to the word at `address`.
    ///
    /// # Safety
    /// As [`Words::read`].
    unsafe fn write(&mut self, address: usize, value: u32);
}

/// The chip itself: volatile accesses at its addresses.
pub struct Chip;

#[allow(unsafe_code)]
impl Words for Chip {
    unsafe fn read(&mut self, address: usize) -> u32 {
        // SAFETY: the caller names one of this module's registers.
        unsafe { core::ptr::read_volatile(address as *const u32) }
    }
    unsafe fn write(&mut self, address: usize, value: u32) {
        // SAFETY: as above.
        unsafe { core::ptr::write_volatile(address as *mut u32, value) }
    }
}

/// Runs `flash_operation`, a bootrom flash erase or program together with the XIP setup after it,
/// so that a PSRAM on chip select 1 keeps both its contents and its reads: window 1's settings are
/// saved and the XIP cache is cleaned first, and the settings are written back after. Answers what
/// the operation answers.
#[allow(unsafe_code)]
pub fn keeping_psram<W: Words, T>(words: &mut W, flash_operation: impl FnOnce() -> T) -> T {
    let mut settings = [0u32; WINDOW1_WORDS];
    for (index, setting) in settings.iter_mut().enumerate() {
        // SAFETY: window 1's settings registers.
        *setting = unsafe { words.read(WINDOW1_SETTINGS + 4 * index) };
    }
    for way in 0..CACHE_WAYS {
        for line in 0..CACHE_LINES_PER_WAY {
            // SAFETY: the cache's maintenance alias.
            unsafe { words.write(CACHE_MAINTENANCE | (way << 13) | (line << 3) | CLEAN_BY_SET_WAY, 0) };
        }
    }
    let answer = flash_operation();
    for (index, &setting) in settings.iter().enumerate() {
        // SAFETY: window 1's settings registers.
        unsafe { words.write(WINDOW1_SETTINGS + 4 * index, setting) };
    }
    answer
}
