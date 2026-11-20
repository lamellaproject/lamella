//! The board's settings: the last [`SETTINGS_SECTORS`] sectors of its flash, where the board keeps what
//! it stores, such as the Wi-Fi network it joins, which `lamella wifi set` writes through a debug
//! probe or while the board waits in BOOTSEL. A deploy never reaches them, and a firmware update --
//! which writes only the firmware's own pages -- leaves them as they were. Their place is stated once,
//! here: the C# interpreter firmware reads and writes them through `rp2350_flash.rs`, and the Python
//! one reads them.

// `BOARD_FLASH_BYTES`, which `build.rs` reads from the board's facts for the board this build
// is for.
include!(concat!(env!("OUT_DIR"), "/board_flash.rs"));

/// One past the last byte of the board's flash. The RP2350 has no internal flash -- it executes in
/// place from an external QSPI part -- so this is a fact of the board rather than of the chip, and
/// it is read from the board's own facts: 4 MB on the Pico 2 and Pico 2 W, and 16 MB on the Pimoroni
/// Pico Plus 2 and Plus 2 W.
pub const FLASH_END: usize = 0x1000_0000 + BOARD_FLASH_BYTES;

/// The QSPI flash's erase sector, which the bootrom API enforces (RP2350 datasheet 5.4.8.9).
pub const SECTOR_BYTES: usize = 4096;

/// The board's settings: the last `SETTINGS_SECTORS` sectors of the flash, each its own erase unit,
/// so one can be erased while the other holds what it replaces.
pub const SETTINGS_SECTORS: usize = 2;

/// Where the settings begin: [`SETTINGS_SECTORS`] sectors below the end of the flash.
pub const SETTINGS_BASE: usize = FLASH_END - SETTINGS_SECTORS * SECTOR_BYTES;

/// Settings sector `sector` as the flash reads now.
#[cfg(feature = "cyw43")]
pub fn settings_sector(sector: usize) -> &'static [u8] {
    unsafe { core::slice::from_raw_parts((SETTINGS_BASE + sector * SECTOR_BYTES) as *const u8, SECTOR_BYTES) }
}
