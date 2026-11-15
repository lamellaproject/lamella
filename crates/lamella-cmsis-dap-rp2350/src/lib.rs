//! RP2350 (Raspberry Pi Pico 2) support over the generic `lamella-cmsis-dap` host: connect to
//! the chip's dormant-boot SWD port, address its ADIv6 MEM-AP, and program QSPI flash by calling
//! the chip's own bootrom flash API on the halted core -- no flash controller driver on the host.

use lamella_cmsis_dap::{Dap, Transport};
use lamella_flash_backend::progress::Moment;
use lamella_probe_core::{ArmDap, CallFrame, ProbeError, TargetAccess, TargetAccessExt};

/// The XIP flash window base -- where a firmware image boots from.
pub const XIP_BASE: u32 = 0x1000_0000;

/// DP `SELECT` for the core-0 AHB MEM-AP: AP base `0x2000` + the ADIv6 MEM-AP register file at
/// `0xd00` (so `TAR`/`DRW`/`CSW` decode at their usual offsets).
pub const CORE0_MEM_AP_SELECT: u32 = 0x2d00;

/// The `DPIDR` an RP2350's SW-DP answers. It is Arm's port, so a part from another vendor could answer
/// the same word, and the port's `TARGETID` is what names the chip.
pub const DPIDR: u32 = 0x4c01_3477;

/// DP `SELECT` for the RP-AP (datasheet 3.5.10.1). The RP-AP is always powered: reaching it needs no
/// debug power-up request, and the switched core domain may be down.
pub const RP_AP_SELECT: u32 = 0x8_0000;

/// The RP-AP's `DBG_POW_STATE_SWCORE` register (datasheet 3.5.10.1, table 99): the state of the power
/// sequencer for the switched core domain.
pub const RP_AP_DBG_POW_STATE_SWCORE: u8 = 0x08;

/// `DBG_POW_STATE_SWCORE.IS_PU`: the switched core domain has finished powering up.
pub const POW_STATE_IS_PU: u32 = 1 << 8;

/// `SYSINFO.CHIP_ID` (datasheet 12.15.1): the JEDEC identifier of the chip, with its revision.
pub const SYSINFO_CHIP_ID: u32 = 0x4000_0000;

/// `SYSINFO.PACKAGE_SEL` (datasheet table 1308): bit 0 is 0 for QFN80, the RP2350B, and 1 for QFN60,
/// the RP2350A.
pub const SYSINFO_PACKAGE_SEL: u32 = 0x4000_0004;

/// `OTP_DATA_BASE`, the ECC-corrected alias of the OTP array (datasheet 13): rows `0x000..0x003` hold
/// the 64-bit chip id, one 32-bit read per two rows, low word first.
pub const OTP_CHIP_ID: u32 = 0x4013_0000;

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
/// several times over. Calls stop at 64 KiB boundaries, so a whole block inside a differing span
/// takes one block erase where that command is enabled.
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

/// Compare `image` with QSPI flash (the XIP base), rewrite only sectors that differ via the
/// bootrom, verify the whole image through the coherent XIP window, and reset the chip to boot it.
/// `log` receives progress lines. Returns the words the verify read: the whole page-padded image,
/// as the flash holds it now, so a caller can compare them against its own copy of the image
/// without reading the flash a second time.
///
/// The erase goes a 4 KiB sector at a time, which every flash device takes. [`flash_image_with`]
/// lets a caller that knows the device erase by 64 KiB block instead.
///
/// It paces its progress by the standard library's clock, so a WebAssembly build for a web page,
/// whose standard library has none, has no `flash_image`: it gives [`flash_image_with_clock`] its
/// host's clock instead.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
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
///
/// Like [`flash_image`], it is not in a WebAssembly build for a web page.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
pub fn flash_image_with<A: TargetAccess>(
    target: &mut A,
    image: &[u8],
    erase: Erase,
    as_found: &mut D8hAsFound,
    log: impl FnMut(&str),
) -> Result<Vec<u32>, ProbeError> {
    flash_image_with_clock(target, image, erase, as_found, log, std::time::Instant::now)
}

/// [`flash_image_with`], pacing its chunk lines and timing its compare by the monotonic clock
/// `now` reads: [`Instant::now`](std::time::Instant::now), or a host's time since a fixed origin,
/// as a [`Duration`](std::time::Duration), where the standard library has no clock.
pub fn flash_image_with_clock<A: TargetAccess, T: Moment>(
    target: &mut A,
    image: &[u8],
    erase: Erase,
    as_found: &mut D8hAsFound,
    log: impl FnMut(&str),
    now: impl FnMut() -> T,
) -> Result<Vec<u32>, ProbeError> {
    let whole = Span { offset: 0, bytes: image, header: 0 };
    let mut written = flash_spans_with_clock(target, &[whole], &[], erase, as_found, log, now)?;
    Ok(written.pop().map(|span| span.read).unwrap_or_default())
}

/// One span of a write: bytes that belong in the flash from [`offset`](Self::offset), compared with
/// the flash, erased and programmed a 4 KiB sector at a time.
///
/// A span covers whole sectors. The bytes past its end in its last sector are erased padding, as
/// they are for a whole image, so two spans of one write never share a sector.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span<'a> {
    /// Where the first byte belongs, in bytes from the start of the flash: a sector's start.
    pub offset: u32,
    /// The bytes, at least one.
    pub bytes: &'a [u8],
    /// How many bytes at the span's start are its directory, in whole sectors, or 0 for a span
    /// with none.
    ///
    /// When any sector of a span with a directory differs from the flash, the directory's sectors
    /// are erased before every other sector of the span, and programmed only after every other
    /// sector of the span has been programmed and read back. A write stopped anywhere on the way
    /// leaves the directory erased, so nothing can read it as describing bytes that are not whole.
    pub header: u32,
}

/// What a write does with one [`Span`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpanWrite {
    /// The 4 KiB sectors erased: none when the flash already held the span.
    pub sectors_erased: usize,
    /// The bytes programmed: none when the flash already held the span.
    pub bytes_programmed: usize,
    /// The span's words, page-padded, as the flash holds them once the write is done: the
    /// comparison's read of a span the flash already held, and the read-back of one it wrote.
    pub read: Vec<u32>,
}

/// Writes `spans` in one session on the halted core, in the order given, and resets the chip to
/// run.
///
/// The flash is compared with every span first. Then each span that differs has the sectors that
/// differ erased and programmed, and is read back whole, before the next span is touched. A span
/// the flash already holds is erased and programmed nowhere. Nothing outside the spans' sectors is
/// erased or programmed.
///
/// `keep` names flash that no span may reach, in bytes from the flash's start. A span that reaches
/// it, two spans that share a sector, a span that does not start on a sector or passes the end of
/// the flash's 16 MiB, and a directory that is not whole sectors inside its span are each refused
/// before anything is reset.
///
/// `erase` and `as_found` are [`flash_image_with`]'s. Returns what was done with each span, in the
/// order given.
///
/// Like [`flash_image`], it is not in a WebAssembly build for a web page.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
pub fn flash_spans_with<A: TargetAccess>(
    target: &mut A,
    spans: &[Span<'_>],
    keep: &[core::ops::Range<u32>],
    erase: Erase,
    as_found: &mut D8hAsFound,
    log: impl FnMut(&str),
) -> Result<Vec<SpanWrite>, ProbeError> {
    flash_spans_with_clock(target, spans, keep, erase, as_found, log, std::time::Instant::now)
}

/// [`flash_spans_with`], pacing its chunk lines and timing its comparisons by the monotonic clock
/// `now` reads, as [`flash_image_with_clock`] does.
pub fn flash_spans_with_clock<A: TargetAccess, T: Moment>(
    target: &mut A,
    spans: &[Span<'_>],
    keep: &[core::ops::Range<u32>],
    erase: Erase,
    as_found: &mut D8hAsFound,
    mut log: impl FnMut(&str),
    mut now: impl FnMut() -> T,
) -> Result<Vec<SpanWrite>, ProbeError> {
    if spans.is_empty() || spans.iter().any(|span| span.bytes.is_empty()) {
        return Err(ProbeError::Timeout("image empty"));
    }
    refuse_unwritable(spans, keep, &mut log)?;

    log("reset-halt into the secure boot context...");
    reset_halt(target)?;
    log("secure setup (stack limits + ACCESSCTRL + RCP)...");
    secure_setup(target)?;

    let connect_internal_flash = rom_function(target, b'I', b'F')?;
    let flash_exit_xip = rom_function(target, b'E', b'X')?;
    let flash_op = rom_function(target, b'F', b'O')?;
    let flash_flush_cache = rom_function(target, b'F', b'C')?;

    claim_flash_lock(target)?;

    let words: Vec<Vec<u32>> = spans.iter().map(|span| page_padded_words(span.bytes)).collect();

    let frame = CallFrame::new(CALL_SP, CALL_TRAP);
    let slow = CallFrame { poll_tries: FLASH_OP_POLLS, ..frame };
    log("connect_internal_flash + flash_exit_xip...");
    target.call_target(u32::from(connect_internal_flash), &[], &frame)?;
    target.call_target(u32::from(flash_exit_xip), &[], &frame)?;

    target.call_target(u32::from(flash_flush_cache), &[], &frame)?;
    let several = spans.len() > 1;
    let mut present = Vec::with_capacity(spans.len());
    let mut changed = Vec::with_capacity(spans.len());
    for (span, wanted) in spans.iter().zip(&words) {
        let erase_len = sector_len(span.bytes.len());
        let compared_at = now();
        let read = target.read_words(XIP_BASE + span.offset, erase_len / 4)?;
        let runs = changed_sectors(wanted, &read);
        let different: usize = runs.iter().map(|run| run.len()).sum();
        let at = if several { format!(" at {:#010x}", XIP_BASE + span.offset) } else { String::new() };
        log(&format!(
            "compared {erase_len} bytes{at} in {:.3} s; {} of {} sectors differ",
            now().since(compared_at).as_secs_f64(),
            different / SECTOR_BYTES,
            erase_len / SECTOR_BYTES
        ));
        present.push(read);
        changed.push(runs);
    }
    if changed.iter().all(Vec::is_empty) {
        if as_found.0 == Some(false) {
            let mut held = HeldD8h { target: &mut *target, bit: None };
            allow_block_erase(&mut held, as_found, &mut log)?;
            held.release()?;
        }
        target.write_word(BOOTLOCK_FLASH_OP, 0)?;
        log(if several {
            "the flash already holds every span; resetting the chip without erasing or programming."
        } else if spans.first().is_some_and(|span| span.header > 0) {
            "the flash already holds these bytes; resetting the chip without erasing or programming."
        } else {
            "image already matches; resetting to boot it without erasing or programming."
        });
        target.reset_and_run()?;
        return Ok(present
            .into_iter()
            .zip(&words)
            .map(|(mut read, wanted)| {
                read.truncate(wanted.len());
                SpanWrite { sectors_erased: 0, bytes_programmed: 0, read }
            })
            .collect());
    }
    let plans: Vec<SpanPlan> = spans.iter().zip(&changed).map(|(span, runs)| SpanPlan::of(span, runs)).collect();
    let total: usize = plans.iter().zip(&words).map(|(plan, words)| programmed_len(&plan.runs(), words)).sum();
    let mut chunk_lines = None;
    let mut written = 0;
    let mut erasing_said = false;
    let mut done = Vec::with_capacity(spans.len());
    for (((span, plan), wanted), compared) in spans.iter().zip(&plans).zip(&words).zip(present) {
        if plan.is_held() {
            let mut read = compared;
            read.truncate(wanted.len());
            done.push(SpanWrite { sectors_erased: 0, bytes_programmed: 0, read });
            continue;
        }
        let erased_runs: Vec<core::ops::Range<usize>> = plan
            .runs()
            .iter()
            .map(|run| span.offset as usize + run.start..span.offset as usize + run.end)
            .collect();
        let different: usize = erased_runs.iter().map(|run| run.len()).sum();
        let mut held = HeldD8h { target: &mut *target, bit: None };
        let by_block = match erase {
            Erase::Sectors => Ok(false),
            Erase::Blocks => allow_block_erase(&mut held, as_found, &mut log),
        };
        let erased = by_block.and_then(|by_block| {
            let by_block = if by_block { ", each whole 64 KiB block by one block erase" } else { "" };
            log(&format!(
                "flash_op ERASE {different} differing bytes from {:#010x}, {ERASE_CALL_BYTES} bytes a \
                 call{by_block}...",
                XIP_BASE + span.offset
            ));
            if !erasing_said {
                erasing_said = true;
                log(lamella_flash_backend::progress::ERASING);
            }
            erase_spans(&mut *held, flash_op, &erased_runs, &slow, &mut log)
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

        let chunk_lines = chunk_lines.get_or_insert_with(|| lamella_flash_backend::progress::ChunkLines::new(now()));
        let mut chunk = |bytes: usize| {
            written += bytes;
            chunk_lines.completed(written, total, now())
        };
        for run in &plan.body {
            program_run(&mut *target, flash_op, &slow, span.offset, wanted, run, &mut chunk, &mut log)?;
        }
        target.call_target(u32::from(flash_flush_cache), &[], &frame)?;
        log("verifying (full read-back over XIP)...");
        let header = plan.directory.as_ref().map_or(0, |directory| directory.end);
        let body = match &wanted[header / 4..] {
            [] => Vec::new(),
            rest => read_back(&mut *target, span.offset + header as u32, rest, &mut log)?,
        };
        let read = match &plan.directory {
            None => body,
            Some(directory) => {
                program_run(&mut *target, flash_op, &slow, span.offset, wanted, directory, &mut chunk, &mut log)?;
                target.call_target(u32::from(flash_flush_cache), &[], &frame)?;
                log("verifying the directory, programmed last...");
                let mut read = read_back(&mut *target, span.offset, &wanted[..header / 4], &mut log)?;
                read.extend(body);
                read
            }
        };
        done.push(SpanWrite {
            sectors_erased: different / SECTOR_BYTES,
            bytes_programmed: programmed_len(&plan.runs(), wanted),
            read,
        });
    }

    target.write_word(BOOTLOCK_FLASH_OP, 0)?;
    log(lamella_flash_backend::progress::COMPLETE);
    log("flashed + verified; resetting to boot the image.");
    target.reset_and_run()?;
    Ok(done)
}

/// What a write does to one span, given the runs of its sectors that differ from the flash, in
/// bytes from the span's start.
struct SpanPlan {
    /// The directory's bytes, rewritten whenever any sector of the span differs: erased before the
    /// body and programmed after it. `None` for a span with no directory, or one the flash holds.
    directory: Option<core::ops::Range<usize>>,
    /// The differing runs past the directory, in address order.
    body: Vec<core::ops::Range<usize>>,
}

impl SpanPlan {
    /// The plan for `span`, whose sectors in `runs` differ from the flash.
    fn of(span: &Span<'_>, runs: &[core::ops::Range<usize>]) -> SpanPlan {
        let header = span.header as usize;
        if runs.is_empty() {
            return SpanPlan { directory: None, body: Vec::new() };
        }
        let body = runs
            .iter()
            .filter_map(|run| {
                let start = run.start.max(header);
                (start < run.end).then_some(start..run.end)
            })
            .collect();
        SpanPlan { directory: (header > 0).then_some(0..header), body }
    }

    /// Whether the flash already holds the span, so nothing of it is erased or programmed.
    fn is_held(&self) -> bool {
        self.directory.is_none() && self.body.is_empty()
    }

    /// The runs a write erases and programs: the directory's first, then the body's, which is also
    /// their address order. They are erased in this order and programmed body first.
    fn runs(&self) -> Vec<core::ops::Range<usize>> {
        self.directory.iter().cloned().chain(self.body.iter().cloned()).collect()
    }
}

/// Refuses `spans` that a write cannot keep inside their own sectors, before anything is reset: a
/// span that does not start on a sector or passes the end of the flash's 16 MiB, a directory that
/// is not whole sectors inside its span, a span that reaches `keep`, and two spans that share a
/// sector. A line to `log` names the span and the place.
fn refuse_unwritable(
    spans: &[Span<'_>],
    keep: &[core::ops::Range<u32>],
    log: &mut dyn FnMut(&str),
) -> Result<(), ProbeError> {
    let sector = SECTOR_BYTES as u64;
    let mut reaches: Vec<(u64, u64)> = Vec::with_capacity(spans.len());
    for span in spans {
        let start = u64::from(span.offset);
        let end = start + sector_len(span.bytes.len()) as u64;
        let named = format!("the span at {start:#010x} ({} bytes, to {end:#010x})", span.bytes.len());
        if start % sector != 0 {
            log(&format!("{named} does not start on a 4 KiB sector"));
            return Err(ProbeError::Device("a span starts on a 4 KiB sector's start"));
        }
        if end > u64::from(CHIP_SELECT_BYTES) {
            log(&format!("{named} passes {CHIP_SELECT_BYTES:#010x}, the end of the flash's 16 MiB"));
            return Err(past_the_flash());
        }
        if u64::from(span.header) % sector != 0 || span.header as usize > span.bytes.len() {
            log(&format!("{named} has a directory of {} bytes", span.header));
            return Err(ProbeError::Device("a span's directory is whole 4 KiB sectors inside the span"));
        }
        if let Some(kept) = keep.iter().find(|kept| start < u64::from(kept.end) && u64::from(kept.start) < end) {
            log(&format!(
                "{named} reaches {:#010x} to {:#010x}, which this write keeps",
                kept.start, kept.end
            ));
            return Err(ProbeError::Device("a span reaches flash this write keeps"));
        }
        reaches.push((start, end));
    }
    reaches.sort_unstable();
    if let Some(pair) = reaches.windows(2).find(|pair| pair[0].1 > pair[1].0) {
        log(&format!(
            "the span at {:#010x} reaches {:#010x}, where the span at {:#010x} starts",
            pair[0].0, pair[0].1, pair[1].0
        ));
        return Err(ProbeError::Device("two spans share a 4 KiB sector"));
    }
    Ok(())
}

/// `bytes` as little-endian words, padded with erased bytes to a whole number of 256-byte pages.
fn page_padded_words(bytes: &[u8]) -> Vec<u32> {
    let mut words: Vec<u32> = bytes
        .chunks(4)
        .map(|chunk| {
            let mut word = [0xFFu8; 4];
            word[..chunk.len()].copy_from_slice(chunk);
            u32::from_le_bytes(word)
        })
        .collect();
    words.resize(bytes.len().div_ceil(PAGE_BYTES) * (PAGE_BYTES / 4), 0xFFFF_FFFF);
    words
}

/// The bytes of the whole sectors `len` bytes from a sector's start reach.
fn sector_len(len: usize) -> usize {
    len.div_ceil(SECTOR_BYTES) * SECTOR_BYTES
}

/// How many bytes programming `runs` of a span whose page-padded words are `words` writes: the
/// runs, each cut at the end of the padded span, since the erased tail past it needs no program.
fn programmed_len(runs: &[core::ops::Range<usize>], words: &[u32]) -> usize {
    runs.iter().map(|run| run.end.min(words.len() * 4).saturating_sub(run.start)).sum()
}

/// Programs `run`, bytes from the start of the span at `offset` whose page-padded words are
/// `words`, in staging-window slices. Each slice's length goes to `chunk` once it is programmed,
/// and the progress line `chunk` answers, if any, to `log`.
#[allow(clippy::too_many_arguments)]
fn program_run<A: TargetAccess>(
    target: &mut A,
    flash_op: u16,
    slow: &CallFrame,
    offset: u32,
    words: &[u32],
    run: &core::ops::Range<usize>,
    chunk: &mut dyn FnMut(usize) -> Option<String>,
    log: &mut dyn FnMut(&str),
) -> Result<(), ProbeError> {
    let end = run.end.min(words.len() * 4);
    if end <= run.start {
        return Ok(());
    }
    for (index, slice) in words[run.start / 4..end / 4].chunks(STAGE_MAX / 4).enumerate() {
        let at = offset + (run.start + index * STAGE_MAX) as u32;
        log(&format!(
            "staging {} bytes at {STAGE_BASE:#010x}, flash_op PROGRAM at +{at:#x}...",
            slice.len() * 4
        ));
        target.write_words(STAGE_BASE, slice)?;
        let status = target.call_target(
            u32::from(flash_op),
            &[CFLASH_SECURE | CFLASH_OP_PROGRAM, XIP_BASE + at, (slice.len() * 4) as u32, STAGE_BASE],
            slow,
        )?;
        if status != 0 {
            log(&format!("flash_op program status = {} ({status:#010x})", status as i32));
            return Err(ProbeError::Timeout("flash_op program returned an error status"));
        }
        if let Some(line) = chunk(slice.len() * 4) {
            log(&line);
        }
    }
    Ok(())
}

/// Reads back the span at `offset` whose page-padded words are `words`, and fails at the first word
/// the flash does not hold, naming it.
fn read_back<A: TargetAccess>(
    target: &mut A,
    offset: u32,
    words: &[u32],
    log: &mut dyn FnMut(&str),
) -> Result<Vec<u32>, ProbeError> {
    let readback = target.read_words(XIP_BASE + offset, words.len())?;
    if readback != words {
        let first_bad = readback.iter().zip(words).position(|(a, b)| a != b).unwrap_or(0);
        log(&format!(
            "VERIFY FAILED at word {first_bad} (byte offset {:#x})",
            offset as usize + first_bad * 4
        ));
        return Err(ProbeError::Timeout("flash verify mismatch"));
    }
    Ok(readback)
}

/// The bytes of the flash a [`FlashSession`] reaches: chip select 0, the board's own flash.
/// `flash_op` addresses two chip selects of 16 MiB each, told apart by address bits 24 and up, and
/// an operation made without address translation stays inside one (datasheet 5.4.8.9).
const CHIP_SELECT_BYTES: u32 = 0x0100_0000;

/// The board's flash at offsets from its start, through the chip's own boot ROM flash API on the
/// halted core: reads, sector erases and page programs, for data kept apart from the program, such
/// as the settings a board stores.
///
/// [`open`](Self::open) reads the boot ROM's magic before it changes anything, so a probe wired to
/// another part is refused with that part untouched. Then it resets the chip into its secure boot
/// context and takes the flash out of execute-in-place, as a whole-image write does. The boot ROM
/// leaves the flash readable through the XIP window between its operations, and each erase and
/// program here flushes the XIP cache after it, so a read sees the flash as it now is (datasheet
/// 5.4.8.9). [`close`](Self::close) releases the boot ROM's flash lock and starts the chip again,
/// and a session dropped without closing does the same, as far as the probe still answers.
pub struct FlashSession<'a, A: TargetAccess> {
    target: &'a mut A,
    flash_op: u16,
    flash_flush_cache: u16,
    closed: bool,
}

impl<'a, A: TargetAccess> FlashSession<'a, A> {
    /// Opens the flash of the RP2350 that `target` reaches.
    ///
    /// # Errors
    /// A part whose boot ROM is not an RP2350's, refused before anything is reset; and any step of
    /// the preparation, after which the chip is started again.
    pub fn open(target: &'a mut A) -> Result<Self, ProbeError> {
        if target.read_word(BOOTROM_MAGIC_ADDR)? & 0x00ff_ffff != BOOTROM_MAGIC {
            return Err(ProbeError::Device(
                "the part is not an RP2350: its boot ROM does not begin with an RP2350's magic",
            ));
        }
        reset_halt(target)?;
        let mut session = FlashSession { target, flash_op: 0, flash_flush_cache: 0, closed: false };
        session.prepare()?;
        Ok(session)
    }

    /// The secure context, the four boot ROM functions, the flash lock, and the flash taken out of
    /// execute-in-place, as a whole-image write prepares them.
    fn prepare(&mut self) -> Result<(), ProbeError> {
        secure_setup(self.target)?;
        let connect_internal_flash = rom_function(self.target, b'I', b'F')?;
        let flash_exit_xip = rom_function(self.target, b'E', b'X')?;
        self.flash_op = rom_function(self.target, b'F', b'O')?;
        self.flash_flush_cache = rom_function(self.target, b'F', b'C')?;
        claim_flash_lock(self.target)?;
        let frame = CallFrame::new(CALL_SP, CALL_TRAP);
        self.target.call_target(u32::from(connect_internal_flash), &[], &frame)?;
        self.target.call_target(u32::from(flash_exit_xip), &[], &frame)?;
        self.flush()
    }

    /// Flushes the XIP cache, so the next read through the XIP window sees the flash as it is.
    fn flush(&mut self) -> Result<(), ProbeError> {
        let frame = CallFrame::new(CALL_SP, CALL_TRAP);
        self.target.call_target(u32::from(self.flash_flush_cache), &[], &frame)?;
        Ok(())
    }

    /// Calls `flash_op` with `args`, and fails with the boot ROM's own reason for a status other
    /// than 0.
    fn flash_op(&mut self, args: [u32; 4]) -> Result<(), ProbeError> {
        let slow = CallFrame { poll_tries: FLASH_OP_POLLS, ..CallFrame::new(CALL_SP, CALL_TRAP) };
        match self.target.call_target(u32::from(self.flash_op), &args, &slow)? {
            0 => Ok(()),
            status => Err(ProbeError::Device(flash_op_refusal(status))),
        }
    }

    /// The `len` bytes at `offset`.
    ///
    /// # Errors
    /// A span past the end of the flash, and a read the probe did not complete.
    pub fn read(&mut self, offset: u32, len: usize) -> Result<Vec<u8>, ProbeError> {
        let length = u32::try_from(len).map_err(|_| past_the_flash())?;
        let end = within_the_flash(offset, length)?;
        let first = offset & !3;
        let words = self.target.read_words(XIP_BASE + first, (end.div_ceil(4) * 4 - first) as usize / 4)?;
        let skip = (offset - first) as usize;
        Ok(words.iter().flat_map(|word| word.to_le_bytes()).skip(skip).take(len).collect())
    }

    /// Erases the `len` bytes at `offset`: whole 4 KiB sectors, from a sector's start.
    ///
    /// # Errors
    /// A span that is not whole sectors from a sector's start, or that passes the end of the flash,
    /// refused before the boot ROM is called; an erase the boot ROM refused, and a call that did not
    /// complete.
    pub fn erase(&mut self, offset: u32, len: u32) -> Result<(), ProbeError> {
        let sector = SECTOR_BYTES as u32;
        if len == 0 || !offset.is_multiple_of(sector) || !len.is_multiple_of(sector) {
            return Err(ProbeError::Device("an erase covers whole 4 KiB sectors, from a sector's start"));
        }
        let end = within_the_flash(offset, len)?;
        for (address, bytes) in erase_calls(offset as usize..end as usize) {
            self.flash_op([CFLASH_SECURE | CFLASH_OP_ERASE, address, bytes, 0])?;
        }
        self.flush()
    }

    /// Programs `page` at `offset`, a 256-byte page's start, where the flash reads erased.
    ///
    /// # Errors
    /// An offset that is not a page's start, or past the end of the flash, refused before the boot
    /// ROM is called; a program the boot ROM refused, and a call that did not complete.
    pub fn program(&mut self, offset: u32, page: &[u8; PAGE_BYTES]) -> Result<(), ProbeError> {
        if !offset.is_multiple_of(PAGE_BYTES as u32) {
            return Err(ProbeError::Device("a program starts at a 256-byte page's start"));
        }
        within_the_flash(offset, PAGE_BYTES as u32)?;
        let mut words: Vec<u32> =
            page.chunks_exact(4).map(|bytes| u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])).collect();
        let staged = self.target.write_words(STAGE_BASE, &words);
        words.fill(0);
        core::hint::black_box(&words);
        staged?;
        self.flash_op([CFLASH_SECURE | CFLASH_OP_PROGRAM, XIP_BASE + offset, PAGE_BYTES as u32, STAGE_BASE])?;
        self.flush()
    }

    /// Releases the boot ROM's flash lock and starts the chip again.
    ///
    /// # Errors
    /// Either step the probe did not complete; the other is still made.
    pub fn close(mut self) -> Result<(), ProbeError> {
        self.closed = true;
        self.leave()
    }

    /// Releases the boot ROM's flash lock (a write unclaims it), so the starting program finds boot
    /// RAM as a power-on boot leaves it, then starts the chip again.
    fn leave(&mut self) -> Result<(), ProbeError> {
        let released = self.target.write_word(BOOTLOCK_FLASH_OP, 0);
        let started = self.target.reset_and_run();
        released.and(started)
    }
}

impl<A: TargetAccess> Drop for FlashSession<'_, A> {
    fn drop(&mut self) {
        if !self.closed {
            let _ = self.leave();
        }
    }
}

/// Claims the boot ROM's flash lock for the calls that follow. The read is the claim. The core
/// halted at reset runs nothing, so a claim that fails means a program before the reset left the
/// lock claimed, and it is released and claimed again.
fn claim_flash_lock<A: TargetAccess>(target: &mut A) -> Result<(), ProbeError> {
    if target.read_word(BOOTLOCK_FLASH_OP)? == 0 {
        target.write_word(BOOTLOCK_FLASH_OP, 0)?;
        let _ = target.read_word(BOOTLOCK_FLASH_OP)?;
    }
    Ok(())
}

/// The end of the `len` bytes at `offset`, when they lie in the flash a [`FlashSession`] reaches.
fn within_the_flash(offset: u32, len: u32) -> Result<u32, ProbeError> {
    offset.checked_add(len).filter(|&end| end <= CHIP_SELECT_BYTES).ok_or_else(past_the_flash)
}

/// The refusal of a span past the first chip select's 16 MiB.
fn past_the_flash() -> ProbeError {
    ProbeError::Device("the span passes the end of the flash's 16 MiB")
}

/// What a `flash_op` status other than 0 says, in the terms of the boot ROM's error codes
/// (`bootrom_constants.h`).
fn flash_op_refusal(status: u32) -> &'static str {
    match status.cast_signed() {
        -4 => "the boot ROM did not permit the flash operation there: a partition's permissions forbid it",
        -10 => "the boot ROM refused the flash operation's address as outside the flash",
        -11 => "the boot ROM refused the flash operation's alignment",
        -19 => "the boot ROM refused the flash operation: its flash lock was not held",
        _ => "the boot ROM's flash operation returned an error",
    }
}

/// Contiguous byte spans of differing 4 KiB sectors, including the last sector's erased padding.
fn changed_sectors(wanted: &[u32], present: &[u32]) -> Vec<core::ops::Range<usize>> {
    let mut spans: Vec<core::ops::Range<usize>> = Vec::new();
    for (index, sector) in present.chunks(SECTOR_BYTES / 4).enumerate() {
        let start = index * SECTOR_BYTES;
        if sector.iter().enumerate().all(|(at, word)| {
            *word == wanted.get(start / 4 + at).copied().unwrap_or(u32::MAX)
        }) {
            continue;
        }
        if let Some(last) = spans.last_mut() && last.end == start {
            last.end += SECTOR_BYTES;
        } else {
            spans.push(start..start + SECTOR_BYTES);
        }
    }
    spans
}

/// Erase the differing sector spans by [`erase_calls`]' bounded calls to `flash_op`.
fn erase_spans<A: TargetAccess>(
    target: &mut A,
    flash_op: u16,
    spans: &[core::ops::Range<usize>],
    frame: &CallFrame,
    log: &mut dyn FnMut(&str),
) -> Result<(), ProbeError> {
    let calls: Vec<_> = spans.iter().flat_map(|span| erase_calls(span.clone())).collect();
    let count = calls.len();
    let erase_len: usize = spans.iter().map(|span| span.len()).sum();
    let mut erased = 0;
    for (index, (address, bytes)) in calls.into_iter().enumerate() {
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
        erased += bytes as usize;
        if erased.is_multiple_of(ERASE_PROGRESS_BYTES) && erased < erase_len {
            log(&format!("erased {erased} of {erase_len} bytes..."));
        }
    }
    Ok(())
}

/// Erase calls covering `span` exactly, split at 64 KiB boundaries so a full block stays one call.
fn erase_calls(span: core::ops::Range<usize>) -> impl ExactSizeIterator<Item = (u32, u32)> {
    (span.start / ERASE_CALL_BYTES..span.end.div_ceil(ERASE_CALL_BYTES)).map(move |index| {
        let offset = (index * ERASE_CALL_BYTES).max(span.start);
        let end = ((index + 1) * ERASE_CALL_BYTES).min(span.end);
        (XIP_BASE + offset as u32, (end - offset) as u32)
    })
}

#[cfg(test)]
mod tests {
    /// An unaligned differing span preserves its neighbours and still permits whole block erases.
    #[test]
    fn sparse_erase_calls_stop_at_block_boundaries() {
        assert_eq!(super::erase_calls(4096..3 * 65536 + 8192).collect::<Vec<_>>(), [
            (0x1000_1000, 65536 - 4096), (0x1001_0000, 65536),
            (0x1002_0000, 65536), (0x1003_0000, 8192),
        ]);
    }

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
            let calls: Vec<(u32, u32)> = super::erase_calls(0..erase_len).collect();
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
