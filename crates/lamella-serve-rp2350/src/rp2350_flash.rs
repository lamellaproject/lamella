//! The RP2350 deployed-image flash region, programmed through the
//! chip's bootrom flash API -- the QSPI flash controller (QMI) is never driven directly.
//!
//! Facts from the RP2350 datasheet (5.4, "Bootrom APIs"):
//! - The bootrom publishes its functions through a lookup helper whose 16-bit pointer sits at
//!   `0x16` (behind the `'M','u',0x02` magic at `0x10`); Secure Arm entries are selected with the
//!   `RT_FLAG_FUNC_ARM_SEC` mask. Codes used here: `IF` connect_internal_flash, `EX`
//!   flash_exit_xip, `FO` flash_op, `FC` flash_flush_cache.
//! - `flash_op(flags, addr, size, buf)` (5.4.8.9) is the checked high-level erase/program/read:
//!   erases are 4096-byte-sector granular, programs 256-byte-page granular, `addr` is an XIP
//!   window address, and the flags select the operation (bits 17:16), the security level
//!   (bits 9:8, Secure here) and the address space (bit 0, storage -- no runtime translation).
//! - The documented call sequence is connect_internal_flash -> flash_exit_xip -> flash_op(s) ->
//!   flash_flush_cache -> re-run the XIP setup function the bootrom saved in boot RAM. On the
//!   RP2350, XIP reads keep working in a basic serial mode between operations, but during a
//!   program/erase the QMI is in direct mode and any XIP fetch bus-errors -- so the sequence up
//!   to the cache flush runs from a RAM-resident function ([`flash_ops_in_ram`]), and the
//!   restore step executes the boot-RAM setup copied to RAM (5.4.8.14: the bootrom saves the
//!   mode/clkdiv it discovered at flash scan as a callable function at the start of boot RAM).
//!
//! The region: [`IMAGE_BASE`] (2 MB into the flash, clear of the firmware at `0x10000000`)
//! running to [`FLASH_END`], the end of the board's flash -- every byte of the part the firmware
//! does not occupy, so no capacity is declared away.

extern crate alloc;

use alloc::vec::Vec;

/// The deployed-image region: XIP window address and length. Disjoint from the firmware, which
/// links from `0x10000000`, so programming it never touches running code.
///
/// That disjointness is enforced rather than assumed: `build.rs` reads this base and hands the
/// firmware's link `IMAGE_BASE - 0x10000000` as its FLASH length, so a firmware that would reach
/// into this region fails to link instead of being silently overwritten by the first deploy.
/// Moving this constant moves the ceiling with it.
///
/// Two megabytes, because a firmware carrying the full runtime, its resident corlib and the
/// radio images of a Pico 2 W needs more than the first one.
pub const IMAGE_BASE: usize = 0x1020_0000;

// `BOARD_FLASH_BYTES`, which `build.rs` reads from the board's facts for the board this build
// is for.
include!(concat!(env!("OUT_DIR"), "/board_flash.rs"));

/// One past the last byte of the board's flash. The RP2350 has no internal flash -- it executes in
/// place from an external QSPI part -- so this is a fact of the board rather than of the chip, and
/// it is read from the board's own facts: 4 MB on the Pico 2 and Pico 2 W, where the deploy window
/// is 2 MB, and 16 MB on the Pimoroni Pico Plus 2 and Plus 2 W, where it is 14 MB.
pub const FLASH_END: usize = 0x1000_0000 + BOARD_FLASH_BYTES;

/// The deploy window's length: from [`IMAGE_BASE`] to the end of the flash.
///
/// It is derived from the two rather than stated, so when the firmware outgrows the flash below
/// the base and `IMAGE_BASE` moves up, the window shrinks to match. Nothing else uses the flash
/// above the base: a Pico 2 W's radio images are built into the firmware image, below it, and no
/// partition table or filesystem names these addresses.
pub const IMAGE_LEN: usize = FLASH_END - IMAGE_BASE;

/// QSPI flash geometry the bootrom API enforces (RP2350 datasheet 5.4.8.9).
pub const SECTOR_BYTES: usize = 4096;
pub const PAGE_BYTES: usize = 256;

// Bootrom well-known layout (5.4.1, table 453) and ROM-table lookup flags.
const BOOTROM_MAGIC_ADDR: usize = 0x10;
const BOOTROM_MAGIC: u32 = 0x02_75_4d; // 'M', 'u', 0x02 (little-endian low 24 bits)
const BOOTROM_TABLE_LOOKUP_PTR: usize = 0x16;
const RT_FLAG_FUNC_ARM_SEC: u32 = 0x0004;

const fn rom_code(c1: u8, c2: u8) -> u32 {
    ((c2 as u32) << 8) | c1 as u32
}

/// `flash_op` flags: address space = storage (bit 0 = 0), security level = Secure (bits 9:8),
/// operation in bits 17:16.
const CFLASH_SECURE: u32 = 0x100;
const CFLASH_OP_ERASE: u32 = 0x0 << 16;
const CFLASH_OP_PROGRAM: u32 = 0x1 << 16;

type RomTableLookup = unsafe extern "C" fn(u32, u32) -> u32;
type RomFnVoid = extern "C" fn();
type RomFlashOp = extern "C" fn(u32, u32, u32, u32) -> i32;

/// The resolved bootrom flash entry points. Resolution happens once, at construction; a ROM
/// whose magic or table entries are missing yields an unavailable sink whose programs fail
/// cleanly (the firmware then reports the deploy as failed rather than faulting).
pub struct Rp2350Flash {
    connect_internal_flash: Option<RomFnVoid>,
    flash_exit_xip: Option<RomFnVoid>,
    flash_op: Option<RomFlashOp>,
    flash_flush_cache: Option<RomFnVoid>,
}

fn rom_lookup(code: u32) -> u32 {
    let magic = unsafe { core::ptr::read_volatile(BOOTROM_MAGIC_ADDR as *const u32) };
    if magic & 0x00ff_ffff != BOOTROM_MAGIC {
        return 0;
    }
    let helper = unsafe { core::ptr::read_volatile(BOOTROM_TABLE_LOOKUP_PTR as *const u16) };
    if helper == 0 {
        return 0;
    }
    let lookup: RomTableLookup = unsafe { core::mem::transmute(helper as usize) };
    unsafe { lookup(code, RT_FLAG_FUNC_ARM_SEC) }
}

impl Rp2350Flash {
    pub fn new() -> Self {
        let fetch_void = |c1, c2| -> Option<RomFnVoid> {
            let addr = rom_lookup(rom_code(c1, c2));
            if addr == 0 { None } else { Some(unsafe { core::mem::transmute::<usize, RomFnVoid>(addr as usize) }) }
        };
        let flash_op = {
            let addr = rom_lookup(rom_code(b'F', b'O'));
            if addr == 0 { None } else { Some(unsafe { core::mem::transmute::<usize, RomFlashOp>(addr as usize) }) }
        };
        Self {
            connect_internal_flash: fetch_void(b'I', b'F'),
            flash_exit_xip: fetch_void(b'E', b'X'),
            flash_op,
            flash_flush_cache: fetch_void(b'F', b'C'),
        }
    }

    /// Run an erase and/or program batch through the bootrom, then restore the fast XIP mode.
    /// Returns the first non-zero bootrom status, or 0.
    fn run_ops(&self, erase_addr: u32, erase_len: u32, program_addr: u32, program_src: u32, program_len: u32) -> i32 {
        let (Some(cif), Some(cex), Some(cfo), Some(cfc)) = (
            self.connect_internal_flash,
            self.flash_exit_xip,
            self.flash_op,
            self.flash_flush_cache,
        ) else {
            return -1;
        };
        let status = flash_ops_in_ram(cif, cex, cfo, cfc, erase_addr, erase_len, program_addr, program_src, program_len);
        restore_xip_mode();
        status
    }
}

/// The whole mutating sequence, executing from RAM: while a `flash_op` program/erase runs, the
/// QMI is in direct mode and an XIP fetch would bus-error, and the state between
/// `connect_internal_flash` and `flash_exit_xip` is likewise not guaranteed fetchable -- so no
/// instruction of this function may live in flash. The `.data` placement makes the startup code
/// copy it to RAM with the initialized data. It calls nothing but the ROM entry points it is
/// handed (ROM execution is always safe) and uses only immediates, so it drags no flash-resident
/// `.rodata` or libcalls into the window.
#[unsafe(link_section = ".data.rp2350_flash_ops")]
#[inline(never)]
extern "C" fn flash_ops_in_ram(
    connect_internal_flash: RomFnVoid,
    flash_exit_xip: RomFnVoid,
    flash_op: RomFlashOp,
    flash_flush_cache: RomFnVoid,
    erase_addr: u32,
    erase_len: u32,
    program_addr: u32,
    program_src: u32,
    program_len: u32,
) -> i32 {
    connect_internal_flash();
    flash_exit_xip();
    let mut status = 0;
    if erase_len != 0 {
        status = flash_op(CFLASH_SECURE | CFLASH_OP_ERASE, erase_addr, erase_len, 0);
    }
    if status == 0 && program_len != 0 {
        status = flash_op(CFLASH_SECURE | CFLASH_OP_PROGRAM, program_addr, program_len, program_src);
    }
    // Always flush: the cache may hold lines for just-mutated addresses, and the flush also
    // returns the QSPI chip-select forcing to normal (datasheet note on flash_flush_cache).
    flash_flush_cache();
    status
}

/// Restore the XIP mode the bootrom discovered at flash scan: copy its saved setup function
/// (the first 64 words of boot RAM, per the SDK's documented convention for this chip) into RAM
/// and execute it. Between `flash_op`s the QMI is already in a working basic serial XIP mode, so
/// this only restores speed -- running it from RAM keeps the reconfiguration window free of
/// flash fetches.
fn restore_xip_mode() {
    const BOOTRAM_BASE: usize = 0x400e_0000;
    const SETUP_WORDS: usize = 64;
    static mut SETUP_COPY: [u32; SETUP_WORDS] = [0; SETUP_WORDS];
    unsafe {
        let src = BOOTRAM_BASE as *const u32;
        let dst = core::ptr::addr_of_mut!(SETUP_COPY).cast::<u32>();
        let mut i = 0;
        while i < SETUP_WORDS {
            core::ptr::write_volatile(dst.add(i), core::ptr::read_volatile(src.add(i)));
            i += 1;
        }
        core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
        let setup: extern "C" fn() = core::mem::transmute(dst as usize | 1); // Thumb entry
        setup();
    }
}

/// Round `len` up to a whole number of `unit` bytes.
fn round_up(len: usize, unit: usize) -> usize {
    len.div_ceil(unit) * unit
}

/// Pad `bytes` to a whole number of flash pages with `0xFF` (the erased state, so the padding
/// programs no bits), staging in RAM as the bootrom requires.
fn page_padded(bytes: &[u8]) -> Vec<u8> {
    let mut staged = Vec::with_capacity(round_up(bytes.len(), PAGE_BYTES));
    staged.extend_from_slice(bytes);
    staged.resize(round_up(bytes.len(), PAGE_BYTES), 0xFF);
    staged
}

impl lamella_runner::FlashSink for Rp2350Flash {
    fn image_slice(&self) -> &'static [u8] {
        unsafe { core::slice::from_raw_parts(IMAGE_BASE as *const u8, IMAGE_LEN) }
    }

    fn resident_corlib(&self) -> Option<&'static [u8]> {
        crate::serve::resident_corlib()
    }

    fn erase(&mut self) {
        let _ = self.run_ops(IMAGE_BASE as u32, SECTOR_BYTES as u32, 0, 0, 0);
    }

    fn program(&mut self, image: &[u8]) -> bool {
        if image.is_empty() || image.len() > IMAGE_LEN {
            return false;
        }
        let staged = page_padded(image);
        let status = self.run_ops(
            IMAGE_BASE as u32,
            round_up(image.len(), SECTOR_BYTES) as u32,
            IMAGE_BASE as u32,
            staged.as_ptr() as u32,
            staged.len() as u32,
        );
        let flashed = unsafe { core::slice::from_raw_parts(IMAGE_BASE as *const u8, image.len()) };
        status == 0 && flashed == image
    }

    fn program_chunk(&mut self, offset: usize, chunk: &[u8], total: usize) -> bool {
        if total > IMAGE_LEN || offset + chunk.len() > IMAGE_LEN || offset % PAGE_BYTES != 0 {
            return false;
        }
        let staged = page_padded(chunk);
        // Each chunk erases the sectors that start inside it, so one chunk's erase is bounded by
        // the chunk's own length however large the image is. Erasing the whole image on the first
        // chunk made that one exchange as long as the whole image's erase, and a few megabytes of
        // it can run past the seconds a host waits for one acknowledgement. The host sends
        // ascending, page-aligned chunks from offset 0, so a sector a chunk begins partway into
        // was erased by the chunk before it.
        let erase_from = round_up(offset, SECTOR_BYTES);
        let erase_to = round_up(offset + chunk.len(), SECTOR_BYTES);
        let status = self.run_ops(
            (IMAGE_BASE + erase_from) as u32,
            (erase_to - erase_from) as u32,
            (IMAGE_BASE + offset) as u32,
            staged.as_ptr() as u32,
            staged.len() as u32,
        );
        let flashed =
            unsafe { core::slice::from_raw_parts((IMAGE_BASE + offset) as *const u8, chunk.len()) };
        status == 0 && flashed == chunk
    }
}
