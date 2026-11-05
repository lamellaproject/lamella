//! RP2350 (Raspberry Pi Pico 2) support over the generic `lamella-cmsis-dap` host: connect to
//! the chip's dormant-boot SWD port, address its ADIv6 MEM-AP, and program QSPI flash by calling
//! the chip's own bootrom flash API on the halted core -- no flash controller driver on the host.

use lamella_cmsis_dap::{Dap, Transport};
use lamella_probe_core::{ArmDap, CallFrame, ProbeError, TargetAccess, TargetAccessExt};

/// The XIP flash window base -- where a firmware image boots from.
pub const XIP_BASE: u32 = 0x1000_0000;

/// DP `SELECT` for the core-0 AHB MEM-AP: AP base `0x2000` + the ADIv6 MEM-AP register file at
/// `0xd00` (so `TAR`/`DRW`/`CSW` decode at their usual offsets).
pub const CORE0_MEM_AP_SELECT: u32 = 0x2d00;

/// The erase granule the bootrom API enforces (datasheet 5.4.8.9).
pub const SECTOR_BYTES: usize = 4096;
/// The program granule the bootrom API enforces (datasheet 5.4.8.9).
pub const PAGE_BYTES: usize = 256;

const BOOTROM_MAGIC_ADDR: u32 = 0x10;
const BOOTROM_MAGIC: u32 = 0x02_75_4d;
const BOOTROM_ROMTABLE_PTR: u32 = 0x14;
/// ROM-table entry flag: the Secure Arm function pointer (bootrom_constants.h).
const RT_FLAG_FUNC_ARM_SEC: u16 = 0x0004;
/// ROM-table entry flag: a data item, whose value is the item's address in the ROM
/// (bootrom_constants.h).
const RT_FLAG_DATA: u16 = 0x0040;

/// Boot RAM, the 1 kB of SRAM the bootrom keeps its state in (datasheet 4.3). The flash API's
/// copy of `FLASH_DEVINFO` is one of the things it holds.
const BOOT_RAM: core::ops::Range<u32> = 0x400e_0000..0x400e_0400;
/// The atomic bitmask-set alias of a peripheral register block (datasheet 2.1.3), which boot RAM
/// supports as other peripherals do (4.3): a write there sets only the bits that are 1 in the
/// value written, and leaves every other bit of the word as it was.
const ATOMIC_SET: u32 = 0x2000;
/// The atomic bitmask-clear alias, the set alias's counterpart: a write there clears only the bits
/// that are 1 in the value written.
const ATOMIC_CLEAR: u32 = 0x3000;
/// `FLASH_DEVINFO`'s `D8H_ERASE_SUPPORTED` bit (datasheet 13.10, Table 1392). While it is set, the
/// flash API erases a whole 64 KiB block with one D8h command wherever the block lies inside the
/// span it was asked to erase.
const DEVINFO_D8H_ERASE_SUPPORTED: u32 = 1 << 7;

/// How [`flash_image_with`] erases the span an image covers before programming it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Erase {
    /// One 4 KiB sector erase (20h) at a time. This is what the bootrom does unless it is told the
    /// device takes more, and every 25-series flash device takes it.
    Sectors,
    /// A 64 KiB block erase (D8h) wherever a whole block lies inside the span, and 4 KiB sectors
    /// at its unaligned end.
    ///
    /// Only for a flash device whose own datasheet specifies the D8h command. The bootrom uses
    /// that command only while its boot-RAM copy of `FLASH_DEVINFO` says the device takes it, and
    /// a boot leaves that unset unless the chip's OTP says otherwise (datasheet 5.4.8.5). So the
    /// write sets the bit for its erase and clears it again before anything is programmed, however
    /// the erase ends. The erase still never reaches outside the span: the bootrom takes a block
    /// only where the whole block is inside it (5.4.8.9).
    Blocks,
}

/// `flash_op` flags (5.4.8.9): storage address space (bit 0 = 0), Secure permissions
/// (bits 9:8 = 1), operation in bits 17:16.
const CFLASH_SECURE: u32 = 0x100;
const CFLASH_OP_ERASE: u32 = 0x0 << 16;
const CFLASH_OP_PROGRAM: u32 = 0x1 << 16;

const STUB_BASE: u32 = 0x2000_8000;
const STAGE_BASE: u32 = 0x2001_0000;
const STAGE_MAX: usize = 320 * 1024;
const CALL_SP: u32 = 0x2008_0000;
const CALL_TRAP: u32 = 0x2000_0000;

/// The most bytes one `flash_op` erase call covers: sixteen sectors, one 64 KiB block.
///
/// **THE IMAGE IS ERASED IN CALLS OF THIS SIZE RATHER THAN IN ONE, BECAUSE AN ERASE TAKES LONGER
/// THE MORE BYTES IT COVERS WHILE A CALL IS POLLED FOR ITS RETURN A FIXED NUMBER OF TIMES.**
/// `flash_op` erases 4 KiB sectors one at a time unless the boot RAM's `FLASH_DEVINFO` says the
/// flash device takes the 64 KiB block erase (datasheet 5.4.8.9), and a sector erase is specified
/// at up to 400 ms on the 16 MB parts these boards carry (the W25Q128JV's tSE: 45 ms typical). So
/// one call takes at most 6.4 s whatever the image's size, and [`FLASH_OP_POLLS`] covers that
/// several times over. Every call starts on a 64 KiB boundary, so where the block erase is enabled
/// a whole call is one block.
const ERASE_CALL_BYTES: usize = 64 * 1024;

/// How many times a `flash_op` call is polled for its return before it is given up as hung.
///
/// A poll is one read through the probe, so the time this spans is the probe's: about 40 s
/// through a Raspberry Pi Debug Probe. Every call is sized so that its slowest specified case is
/// a small part of that: an erase by [`ERASE_CALL_BYTES`] (16 sectors at up to 400 ms, 6.4 s) and
/// a program by [`STAGE_MAX`] (1,280 pages at up to 3 ms, 3.8 s).
const FLASH_OP_POLLS: u32 = 100_000;

/// How often the erase says how far it has got, in bytes. A whole-board image takes more than a
/// minute to erase, and a minute without a word reads as a hang.
const ERASE_PROGRESS_BYTES: usize = 1024 * 1024;

/// Boot-RAM BOOTLOCK1 = the bootrom's `LOCK_FLASH_OP` (datasheet 5.4.4 + the boot-RAM register
/// map): read to claim (returns `1 << 1` on success, 0 if already claimed), write to unclaim.
/// A firmware built with SDK-style bootrom locking claims `LOCK_ENABLE` (BOOTLOCK7), and boot
/// RAM SURVIVES `SYSRESETREQ` -- so after such a firmware has run once, every `flash_op`
/// without this lock answers `BOOTROM_ERROR_LOCK_REQUIRED` (-19), debugger included.
const BOOTLOCK_FLASH_OP: u32 = 0x400e_0810;

const DEMCR: u32 = 0xe000_edfc;
const DEMCR_TRCENA: u32 = 1 << 24;
const DEMCR_VC_CORERESET: u32 = 1 << 0;
const AIRCR: u32 = 0xe000_ed0c;
const AIRCR_SYSRESETREQ: u32 = 0x05fa_0004;
const DHCSR: u32 = 0xe000_edf0;
const DHCSR_DEBUGEN: u32 = 0xa05f_0001;

/// The SWD clock a write through this part's flash route runs its transfers at, once [`connect`]
/// has woken the part at [`FALLBACK_SWD_CLOCK_HZ`] and read its `DPIDR` there.
///
/// The part's SW-DP runs reliably up to 50 MHz, with 10 MHz typical (datasheet, SWCLK pin
/// characteristics), but the wiring between probe and part decides what a link carries, and the
/// datasheet's advice for unreliable SWD is a lower clock. So a write that meets a transfer fault at
/// this rate is made again, whole and from the start, at [`FALLBACK_SWD_CLOCK_HZ`].
pub const SWD_CLOCK_HZ: u32 = 10_000_000;

/// The SWD clock [`connect`] wakes the part at, and the one a write that met a transfer fault at
/// [`SWD_CLOCK_HZ`] is made again at.
pub const FALLBACK_SWD_CLOCK_HZ: u32 = lamella_cmsis_dap::CONNECT_CLOCK_HZ;

/// How many times the probe retries a transfer the part answers WAIT, at [`FALLBACK_SWD_CLOCK_HZ`].
/// AP accesses around a reset, and while the bootrom flash helpers hold the bus, legitimately answer
/// WAIT.
const WAIT_RETRIES: u16 = 64;

/// [`WAIT_RETRIES`] at `clock_hz`. The budget is counted in retries, and the wire time of a retry
/// shrinks as the clock rises, so it is scaled with the clock to span at least the time it spans at
/// [`FALLBACK_SWD_CLOCK_HZ`]: a WAIT the part answers for that long is still ridden out at a faster
/// clock, and one it answers for longer is a fault there as it is at the slower one.
fn wait_retries_at(clock_hz: u32) -> u16 {
    let scale = (clock_hz / FALLBACK_SWD_CLOCK_HZ).max(1);
    u16::try_from(u32::from(WAIT_RETRIES).saturating_mul(scale)).unwrap_or(u16::MAX)
}

/// Runs the session's transfers at `clock_hz` from here on: the clock, and the probe's WAIT budget
/// scaled with it to span at least the time it spans at [`FALLBACK_SWD_CLOCK_HZ`]. Bound to the
/// CMSIS-DAP stack, as [`connect`] is, and for its reason.
pub fn set_swd_clock<T: Transport>(
    target: &mut ArmDap<Dap<T>>,
    clock_hz: u32,
) -> Result<(), ProbeError> {
    target.inner_mut().set_clock(clock_hz)?;
    target.inner_mut().configure_transfers(0, wait_retries_at(clock_hz), 0)?;
    Ok(())
}

/// Connect to the RP2350 over SWD and return its DPIDR: wake the dormant DP (harmless when it
/// is already awake), give the probe a WAIT-retry budget, and select the core-0 MEM-AP.
///
/// Unlike the rest of this crate, bring-up is NOT generic over [`TargetAccess`]: waking a dormant
/// DP and configuring the probe's WAIT-retry budget are wire- and probe-level operations that a
/// high-level probe (ST-Link, J-Link) performs inside its own connect command rather than exposing,
/// and the core-0 MEM-AP is addressed the ADIv6 way. So this one function is bound to the
/// CMSIS-DAP/ARM stack; everything below it -- reset, secure setup, flash -- consumes only the
/// neutral seam.
pub fn connect<T: Transport>(target: &mut ArmDap<Dap<T>>) -> Result<u32, ProbeError> {
    target.inner_mut().connect_swd_from_dormant()?;
    target.inner_mut().configure_transfers(0, WAIT_RETRIES, 0)?;
    let idcode = match target.read_idcode() {
        Ok(idcode) => idcode,
        Err(ProbeError::Ack(_)) => {
            target.inner_mut().abort_stalled_transaction()?;
            target.read_idcode()?
        }
        Err(error) => return Err(error),
    };
    target.init_mem_select(CORE0_MEM_AP_SELECT)?;
    Ok(idcode)
}

/// Reset the chip and halt core 0 at its (secure) reset vector: `DEMCR.VC_CORERESET` catches the
/// reset, so the bootrom flash functions later run in the clean Secure boot context (`MSPLIM` at
/// its reset 0, secure stack valid). `SYSRESETREQ` resets core + system but not the debug
/// domain, so the DP/AP selection persists.
pub fn reset_halt<A: TargetAccess>(target: &mut A) -> Result<(), ProbeError> {
    target.write_word(DHCSR, DHCSR_DEBUGEN)?;
    target.write_word(DEMCR, DEMCR_TRCENA | DEMCR_VC_CORERESET)?;
    let _ = target.write_word(AIRCR, AIRCR_SYSRESETREQ);
    let mut halted = false;
    for _ in 0..4000 {
        if let Ok(true) = target.is_halted() {
            halted = true;
            break;
        }
    }
    let _ = target.write_word(DEMCR, DEMCR_TRCENA);
    if halted { Ok(()) } else { Err(ProbeError::Timeout("reset-halt: core did not halt")) }
}

/// Prepare the Secure context the bootrom flash functions need, by running two stubs ON THE
/// CORE (so every access is made from its Secure state). Instruction words are hand-assembled
/// from the Armv8-M encodings; the register facts are RP2350 datasheet 10.6.3 (ACCESSCTRL) and
/// 3.6 (RCP).
pub fn secure_setup<A: TargetAccess>(target: &mut A) -> Result<(), ProbeError> {
    let frame = CallFrame::new(CALL_SP, CALL_TRAP);

    let stub_a: [u32; 3] = [0x880a_f380, 0x880b_f380, 0x4770_6011];
    for (i, word) in stub_a.iter().enumerate() {
        target.write_word(STUB_BASE + (i as u32) * 4, *word)?;
    }
    target.call_target(STUB_BASE, &[0, 0xacce_0001, 0x4006_0008], &frame)?;

    let stub_b: [u32; 7] = [
        0x0100_f24c,
        0x6001_4804,
        0xf710_fe30,
        0x2201_d203,
        0xfc43_2300,
        0xbe00_2780,
        0xe000_ed88,
    ];
    for (i, word) in stub_b.iter().enumerate() {
        target.write_word(STUB_BASE + (i as u32) * 4, *word)?;
    }
    target.call_target(STUB_BASE, &[], &frame)?;
    Ok(())
}

/// Resolve a bootrom function: check the magic, then walk the ROM table for `code` with the
/// Secure-Arm flag. A pure memory-read walk (entry = tag u16, flags u16, then one u16 value per
/// set flag bit in ascending order), so nothing executes on the target before [`secure_setup`].
pub fn rom_function<A: TargetAccess>(target: &mut A, c1: u8, c2: u8) -> Result<u16, ProbeError> {
    rom_entry(target, c1, c2, RT_FLAG_FUNC_ARM_SEC, "bootrom function not in the ROM table")
}

/// Resolve a bootrom data item: its address in the ROM, found by the same walk as
/// [`rom_function`] under the data flag (datasheet 5.4.1).
pub fn rom_data<A: TargetAccess>(target: &mut A, c1: u8, c2: u8) -> Result<u16, ProbeError> {
    rom_entry(target, c1, c2, RT_FLAG_DATA, "bootrom data item not in the ROM table")
}

/// The ROM-table walk behind [`rom_function`] and [`rom_data`]: the value `flag` carries in the
/// entry for the two-character `code`, or `missing` when no such entry has that flag.
fn rom_entry<A: TargetAccess>(
    target: &mut A,
    c1: u8,
    c2: u8,
    flag: u16,
    missing: &'static str,
) -> Result<u16, ProbeError> {
    if target.read_word(BOOTROM_MAGIC_ADDR)? & 0x00ff_ffff != BOOTROM_MAGIC {
        return Err(ProbeError::Timeout("RP2350 bootrom magic not found"));
    }
    let table = read_halfword_of_word(target, BOOTROM_ROMTABLE_PTR)?;
    let wanted = u16::from(c1) | (u16::from(c2) << 8);
    let mut entry = u32::from(table);
    for _ in 0..512 {
        let tag = read_halfword_of_word(target, entry)?;
        if tag == 0 {
            break;
        }
        let flags = read_halfword_of_word(target, entry + 2)?;
        let mut value_at = entry + 4;
        if tag == wanted && flags & flag != 0 {
            let lower = flags & (flag - 1);
            value_at += 2 * u32::from(lower.count_ones() as u16);
            return read_halfword_of_word(target, value_at);
        }
        entry = value_at + 2 * flags.count_ones();
    }
    Err(ProbeError::Timeout(missing))
}

/// The halfword at `address`, read as the aligned word that holds it, since a word read is the one
/// access every probe makes the same way.
fn read_halfword_of_word<A: TargetAccess>(target: &mut A, address: u32) -> Result<u16, ProbeError> {
    let word = target.read_word(address & !3)?;
    Ok(if address & 2 != 0 { (word >> 16) as u16 } else { word as u16 })
}

/// The 32-bit value at `address` in the ROM, which may sit on a halfword boundary there.
fn read_rom_u32<A: TargetAccess>(target: &mut A, address: u32) -> Result<u32, ProbeError> {
    if address & 3 == 0 {
        return target.read_word(address);
    }
    let low = read_halfword_of_word(target, address)?;
    let high = read_halfword_of_word(target, address + 2)?;
    Ok(u32::from(low) | (u32::from(high) << 16))
}

/// The boot-RAM word that holds the 16-bit `FLASH_DEVINFO` at `pointer`, and the mask of its D8h
/// bit within that word. `None` unless `pointer` is a halfword inside boot RAM.
fn devinfo_d8h_bit(pointer: u32) -> Option<(u32, u32)> {
    if !BOOT_RAM.contains(&pointer) || pointer & 1 != 0 {
        return None;
    }
    let shift = (pointer & 2) * 8;
    Some((pointer & !3, DEVINFO_D8H_ERASE_SUPPORTED << shift))
}

/// The D8h bit a write set in boot RAM's `FLASH_DEVINFO`, which it clears again once its erase is
/// done.
struct BlockErase {
    word: u32,
    mask: u32,
}

impl BlockErase {
    /// Clear the D8h bit again through the atomic clear alias, so what runs after the erase finds
    /// `FLASH_DEVINFO` as the write found it.
    fn withdraw<A: TargetAccess>(&self, target: &mut A) -> Result<(), ProbeError> {
        target.write_word(self.word + ATOMIC_CLEAR, self.mask)?;
        if target.read_word(self.word)? & self.mask != 0 {
            return Err(ProbeError::Timeout("boot RAM's FLASH_DEVINFO kept the D8h bit"));
        }
        Ok(())
    }
}

/// Boot RAM's D8h bit as one write first found it, before setting it: carried by the caller from
/// one attempt of that write to the next.
///
/// A write clears the bit it set on its way out, whatever stops it. A clear that fails leaves the
/// bit set, and the reset-halt that begins the write's next attempt stops the core before the
/// bootrom sets `FLASH_DEVINFO` up afresh, so that attempt finds the bit set. Handed this record,
/// it takes that bit for the one the earlier attempt set: it erases by block and then clears it.
/// Without the record it would take the bit for the chip's own setting and leave it set.
///
/// A write made once passes a fresh record, `D8hAsFound::default()`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct D8hAsFound(Option<bool>);

/// Boot RAM's D8h bit while a write holds it set for its erase. [`release`](Self::release) clears
/// it and says whether that worked; a write that unwinds before then clears it as it goes.
struct HeldD8h<'a, A: TargetAccess> {
    target: &'a mut A,
    bit: Option<BlockErase>,
}

impl<A: TargetAccess> HeldD8h<'_, A> {
    /// Clears the bit when one is held.
    fn release(mut self) -> Result<(), ProbeError> {
        match self.bit.take() {
            Some(bit) => bit.withdraw(self.target),
            None => Ok(()),
        }
    }
}

impl<A: TargetAccess> Drop for HeldD8h<'_, A> {
    fn drop(&mut self) {
        if let Some(bit) = self.bit.take() {
            let _ = bit.withdraw(self.target);
        }
    }
}

impl<A: TargetAccess> std::ops::Deref for HeldD8h<'_, A> {
    type Target = A;
    fn deref(&self) -> &A {
        self.target
    }
}

impl<A: TargetAccess> std::ops::DerefMut for HeldD8h<'_, A> {
    fn deref_mut(&mut self) -> &mut A {
        self.target
    }
}

/// Let this write's erase use the 64 KiB block command: find the flash API's `FLASH_DEVINFO`
/// through the ROM table's 'F','D' item (datasheet 5.4.8.5) and set its D8h bit through boot RAM's
/// atomic set alias, which touches that bit alone.
///
/// Returns whether the erase goes by block. `held` holds the bit from just before the set, so it
/// is cleared whatever fails after that. The erase goes a sector at a time, as before, when the
/// pointer does not lead into boot RAM or the bit would not set. A bit the write's first attempt
/// found set (the chip's OTP says the device takes D8h) is left as it is, and the bootrom erases
/// by block anyway. A bit found set by a later attempt, where `as_found` says the first found it
/// clear, is the one an earlier attempt set and could not clear, so `held` takes it.
fn allow_block_erase<A: TargetAccess>(
    held: &mut HeldD8h<'_, A>,
    as_found: &mut D8hAsFound,
    log: &mut dyn FnMut(&str),
) -> Result<bool, ProbeError> {
    let literal = rom_data(&mut **held, b'F', b'D')?;
    let pointer = read_rom_u32(&mut **held, u32::from(literal))?;
    let Some((word, mask)) = devinfo_d8h_bit(pointer) else {
        log(&format!(
            "FLASH_DEVINFO's pointer {pointer:#010x} is not in boot RAM, so the erase goes a 4 KiB \
             sector at a time"
        ));
        return Ok(false);
    };
    let before = held.read_word(word)?;
    let set_now = before & mask != 0;
    let set_at_first = *as_found.0.get_or_insert(set_now);
    if set_now {
        if set_at_first {
            log("FLASH_DEVINFO already allows the 64 KiB block erase");
        } else {
            log(
                "FLASH_DEVINFO still has the D8h bit an earlier attempt of this write set, so \
                 it is cleared after the erase",
            );
            held.bit = Some(BlockErase { word, mask });
        }
        return Ok(true);
    }
    held.bit = Some(BlockErase { word, mask });
    held.write_word(word + ATOMIC_SET, mask)?;
    let after = held.read_word(word)?;
    if after == before {
        held.bit = None;
        log("FLASH_DEVINFO did not take the D8h bit, so the erase goes a 4 KiB sector at a time");
        return Ok(false);
    }
    if after != before | mask {
        return Err(ProbeError::Timeout("boot RAM's FLASH_DEVINFO changed beyond its D8h bit"));
    }
    Ok(true)
}

/// Flash `image` to the start of QSPI flash (the XIP base) via the bootrom, verify it by
/// reading back through the coherent (cache-flushed) XIP window, and reset the chip to boot it.
/// `log` receives progress lines. Returns the words the verify read: the whole page-padded image,
/// as the flash holds it now, so a caller can compare them against its own copy of the image
/// without reading the flash a second time.
///
/// The erase goes a 4 KiB sector at a time, which every flash device takes. [`flash_image_with`]
/// lets a caller that knows the device erase by 64 KiB block instead.
pub fn flash_image<A: TargetAccess>(
    target: &mut A,
    image: &[u8],
    log: impl FnMut(&str),
) -> Result<Vec<u32>, ProbeError> {
    flash_image_with(target, image, Erase::Sectors, &mut D8hAsFound::default(), log)
}

/// [`flash_image`], with the erase made the way `erase` says.
///
/// `as_found` is the D8h bit as this write's first attempt found it (see [`D8hAsFound`]): a fresh
/// record for a first attempt, and the one an earlier attempt filled for each attempt after it. A
/// bit this write set is cleared before anything is programmed, whatever stops the erase; a
/// failed clear fails the write, and when the write already failed, a line says so.
pub fn flash_image_with<A: TargetAccess>(
    target: &mut A,
    image: &[u8],
    erase: Erase,
    as_found: &mut D8hAsFound,
    mut log: impl FnMut(&str),
) -> Result<Vec<u32>, ProbeError> {
    if image.is_empty() {
        return Err(ProbeError::Timeout("image empty"));
    }

    log("reset-halt into the secure boot context...");
    reset_halt(target)?;
    log("secure setup (stack limits + ACCESSCTRL + RCP)...");
    secure_setup(target)?;

    let connect_internal_flash = rom_function(target, b'I', b'F')?;
    let flash_exit_xip = rom_function(target, b'E', b'X')?;
    let flash_op = rom_function(target, b'F', b'O')?;
    let flash_flush_cache = rom_function(target, b'F', b'C')?;

    if target.read_word(BOOTLOCK_FLASH_OP)? == 0 {
        target.write_word(BOOTLOCK_FLASH_OP, 0)?;
        let _ = target.read_word(BOOTLOCK_FLASH_OP)?;
    }

    let mut words: Vec<u32> = image
        .chunks(4)
        .map(|c| {
            let mut w = [0xFFu8; 4];
            w[..c.len()].copy_from_slice(c);
            u32::from_le_bytes(w)
        })
        .collect();
    words.resize(image.len().div_ceil(PAGE_BYTES) * (PAGE_BYTES / 4), 0xFFFF_FFFF);

    let frame = CallFrame::new(CALL_SP, CALL_TRAP);
    let slow = CallFrame { poll_tries: FLASH_OP_POLLS, ..frame };
    log("connect_internal_flash + flash_exit_xip...");
    target.call_target(u32::from(connect_internal_flash), &[], &frame)?;
    target.call_target(u32::from(flash_exit_xip), &[], &frame)?;

    let erase_len = image.len().div_ceil(SECTOR_BYTES) * SECTOR_BYTES;
    let mut held = HeldD8h { target: &mut *target, bit: None };
    let by_block = match erase {
        Erase::Sectors => Ok(false),
        Erase::Blocks => allow_block_erase(&mut held, as_found, &mut log),
    };
    let erased = by_block.and_then(|by_block| {
        let by_block = if by_block { ", each whole 64 KiB block by one block erase" } else { "" };
        log(&format!(
            "flash_op ERASE {erase_len} bytes at {XIP_BASE:#010x}, {ERASE_CALL_BYTES} bytes a \
             call{by_block}..."
        ));
        erase_span(&mut *held, flash_op, erase_len, &slow, &mut log)
    });
    let released = held.release();
    if let Err(why) = erased {
        if let Err(clear) = released {
            log(&format!(
                "FLASH_DEVINFO's D8h bit could not be cleared ({clear}); the chip's next boot sets \
                 FLASH_DEVINFO up afresh"
            ));
        }
        return Err(why);
    }
    released?;

    let stage_words = STAGE_MAX / 4;
    for (index, slice) in words.chunks(stage_words).enumerate() {
        let offset = (index * STAGE_MAX) as u32;
        log(&format!(
            "staging {} bytes at {STAGE_BASE:#010x}, flash_op PROGRAM at +{offset:#x}...",
            slice.len() * 4
        ));
        target.write_words(STAGE_BASE, slice)?;
        let status = target.call_target(
            u32::from(flash_op),
            &[
                CFLASH_SECURE | CFLASH_OP_PROGRAM,
                XIP_BASE + offset,
                (slice.len() * 4) as u32,
                STAGE_BASE,
            ],
            &slow,
        )?;
        if status != 0 {
            log(&format!("flash_op program status = {} ({status:#010x})", status as i32));
            return Err(ProbeError::Timeout("flash_op program returned an error status"));
        }
    }

    target.call_target(u32::from(flash_flush_cache), &[], &frame)?;
    log("verifying (full read-back over XIP)...");
    let readback = target.read_words(XIP_BASE, words.len())?;
    if readback != words {
        let first_bad = readback.iter().zip(&words).position(|(a, b)| a != b).unwrap_or(0);
        log(&format!("VERIFY FAILED at word {first_bad} (byte offset {:#x})", first_bad * 4));
        return Err(ProbeError::Timeout("flash verify mismatch"));
    }

    target.write_word(BOOTLOCK_FLASH_OP, 0)?;
    log("flashed + verified; resetting to boot the image.");
    target.reset_and_run()?;
    Ok(readback)
}

/// Erase the first `erase_len` bytes of flash by [`erase_calls`]' calls to `flash_op`, saying how
/// far it has got.
fn erase_span<A: TargetAccess>(
    target: &mut A,
    flash_op: u16,
    erase_len: usize,
    frame: &CallFrame,
    log: &mut dyn FnMut(&str),
) -> Result<(), ProbeError> {
    let calls = erase_calls(erase_len);
    let count = calls.len();
    for (index, (address, bytes)) in calls.enumerate() {
        let call = format!(
            "flash_op ERASE call {} of {count} ({bytes} bytes at {address:#010x})",
            index + 1
        );
        let args = [CFLASH_SECURE | CFLASH_OP_ERASE, address, bytes, 0];
        let status = target
            .call_target(u32::from(flash_op), &args, frame)
            .inspect_err(|error| log(&format!("{call} did not complete: {error}")))?;
        if status != 0 {
            log(&format!("{call} returned status {} ({status:#010x})", status as i32));
            return Err(ProbeError::Timeout("flash_op erase returned an error status"));
        }
        let erased = (address - XIP_BASE) as usize + bytes as usize;
        if erased.is_multiple_of(ERASE_PROGRESS_BYTES) && erased < erase_len {
            log(&format!("erased {erased} of {erase_len} bytes..."));
        }
    }
    Ok(())
}

/// The `flash_op` erase calls that erase the first `erase_len` bytes of flash, in address order:
/// each an XIP address on a 64 KiB boundary and a byte count no larger than [`ERASE_CALL_BYTES`].
fn erase_calls(erase_len: usize) -> impl ExactSizeIterator<Item = (u32, u32)> {
    (0..erase_len.div_ceil(ERASE_CALL_BYTES)).map(move |index| {
        let offset = index * ERASE_CALL_BYTES;
        (XIP_BASE + offset as u32, (erase_len - offset).min(ERASE_CALL_BYTES) as u32)
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn stub_words_decode_to_the_documented_instructions() {
        assert_eq!(0x880a_f380u32.to_le_bytes(), [0x80, 0xf3, 0x0a, 0x88]);
        assert_eq!(0x4770_6011u32.to_le_bytes(), [0x11, 0x60, 0x70, 0x47]);
        assert_eq!(0xbe00_2780u32.to_le_bytes()[2..], [0x00, 0xbe]);
    }

    #[test]
    fn the_wait_budget_spans_at_least_its_time_at_the_fallback_clock() {
        assert_eq!(super::wait_retries_at(super::FALLBACK_SWD_CLOCK_HZ), 64);
        assert_eq!(super::wait_retries_at(4_000_000), 256);
        assert_eq!(
            super::wait_retries_at(super::SWD_CLOCK_HZ),
            64 * (super::SWD_CLOCK_HZ / super::FALLBACK_SWD_CLOCK_HZ) as u16
        );
        assert_eq!(super::wait_retries_at(500_000), 64, "never fewer than at the fallback clock");
        assert_eq!(super::wait_retries_at(u32::MAX), u16::MAX);
    }

    /// The erase calls cover the image's sectors exactly, in address order and without a gap: each
    /// a whole number of sectors starting on a 64 KiB boundary, and none larger than one block.
    #[test]
    fn the_erase_calls_cover_the_image_a_block_at_a_time() {
        use super::{ERASE_CALL_BYTES, SECTOR_BYTES, XIP_BASE};

        for erase_len in [4096, 65536, 65536 + 4096, 6_504_448, 16 * 1024 * 1024] {
            let calls: Vec<(u32, u32)> = super::erase_calls(erase_len).collect();
            assert_eq!(calls.len(), erase_len.div_ceil(ERASE_CALL_BYTES), "{erase_len}");
            let mut next = XIP_BASE;
            for &(address, bytes) in &calls {
                assert_eq!(address, next, "{erase_len}: in order, with no gap");
                assert_eq!((address - XIP_BASE) as usize % ERASE_CALL_BYTES, 0, "{address:#x}");
                assert!(bytes > 0 && bytes as usize <= ERASE_CALL_BYTES, "{bytes} at {address:#x}");
                assert_eq!(bytes as usize % SECTOR_BYTES, 0, "{bytes} at {address:#x}");
                next += bytes;
            }
            assert_eq!((next - XIP_BASE) as usize, erase_len, "the image's sectors and no more");
        }
    }

    /// **NO CALL OUTGROWS ITS POLL BUDGET**: the slowest erase call and the slowest program call
    /// the flash device is specified for each take a fifth or less of the time the budget spans
    /// through the probe, so a probe that polls five times faster still waits them out.
    #[test]
    fn every_flash_op_call_fits_its_poll_budget_five_times_over() {
        const SECTOR_ERASE_MAX_US: u64 = 400_000;
        const PAGE_PROGRAM_MAX_US: u64 = 3_000;
        const BLOCK_ERASE_MAX_US: u64 = 2_000_000;
        const POLL_US: u64 = 400;

        let budget_us = u64::from(super::FLASH_OP_POLLS) * POLL_US;
        let erase_us = (super::ERASE_CALL_BYTES / super::SECTOR_BYTES) as u64 * SECTOR_ERASE_MAX_US;
        let program_us = (super::STAGE_MAX / super::PAGE_BYTES) as u64 * PAGE_PROGRAM_MAX_US;
        assert!(erase_us * 5 <= budget_us, "an erase call: {erase_us} us of {budget_us}");
        assert!(program_us * 5 <= budget_us, "a program call: {program_us} us of {budget_us}");
        assert!(
            BLOCK_ERASE_MAX_US * 5 <= budget_us,
            "a call erased as one block: {BLOCK_ERASE_MAX_US} us of {budget_us}"
        );
    }

    /// The D8h bit is found in the boot-RAM word that holds `FLASH_DEVINFO`, in whichever half the
    /// pointer names, and a pointer that is not a halfword inside boot RAM names no bit at all.
    #[test]
    fn the_d8h_bit_is_in_the_half_of_the_boot_ram_word_the_pointer_names() {
        use super::devinfo_d8h_bit;

        assert_eq!(devinfo_d8h_bit(0x400e_0010), Some((0x400e_0010, 0x0000_0080)), "the low half");
        assert_eq!(devinfo_d8h_bit(0x400e_0012), Some((0x400e_0010, 0x0080_0000)), "the high half");
        assert_eq!(devinfo_d8h_bit(0x400e_0000), Some((0x400e_0000, 0x80)), "its first word");
        assert_eq!(devinfo_d8h_bit(0x400e_03fe), Some((0x400e_03fc, 0x0080_0000)), "its last half");
        assert_eq!(devinfo_d8h_bit(0x400e_0011), None, "not a halfword");
        assert_eq!(devinfo_d8h_bit(0x400e_0400), None, "past boot RAM");
        assert_eq!(devinfo_d8h_bit(0x400e_0810), None, "the boot locks are beside boot RAM");
        assert_eq!(devinfo_d8h_bit(0x2000_0000), None, "main SRAM");
        assert_eq!(devinfo_d8h_bit(0), None, "a null pointer");
    }

    /// The faster clock goes out with its WAIT budget, in that order, and nothing else is sent.
    #[test]
    fn a_faster_clock_is_set_with_its_wait_budget() {
        use lamella_cmsis_dap::proto;
        use lamella_cmsis_dap::testing::{Mock, echo};

        let mut target = lamella_probe_core::ArmDap::new(lamella_cmsis_dap::Dap::new(Mock::new(vec![
            echo(proto::cmd::SWJ_CLOCK, &[proto::DAP_OK]),
            echo(proto::cmd::TRANSFER_CONFIGURE, &[proto::DAP_OK]),
        ])));
        super::set_swd_clock(&mut target, 4_000_000).unwrap();
        assert_eq!(
            target.inner().transport().sent,
            vec![
                vec![0x11, 0x00, 0x09, 0x3d, 0x00],
                vec![0x04, 0x00, 0x00, 0x01, 0x00, 0x00],
            ]
        );
    }
}
