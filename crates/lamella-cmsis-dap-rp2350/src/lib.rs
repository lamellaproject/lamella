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
    target.inner_mut().configure_transfers(0, 64, 0)?;
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
    let read16 = |target: &mut A, addr: u32| -> Result<u16, ProbeError> {
        let word = target.read_word(addr & !3)?;
        Ok(if addr & 2 != 0 { (word >> 16) as u16 } else { word as u16 })
    };
    if target.read_word(BOOTROM_MAGIC_ADDR)? & 0x00ff_ffff != BOOTROM_MAGIC {
        return Err(ProbeError::Timeout("RP2350 bootrom magic not found"));
    }
    let table = read16(target, BOOTROM_ROMTABLE_PTR)?;
    let wanted = u16::from(c1) | (u16::from(c2) << 8);
    let mut entry = u32::from(table);
    for _ in 0..512 {
        let tag = read16(target, entry)?;
        if tag == 0 {
            break;
        }
        let flags = read16(target, entry + 2)?;
        let mut value_at = entry + 4;
        if tag == wanted && flags & RT_FLAG_FUNC_ARM_SEC != 0 {
            let lower = flags & (RT_FLAG_FUNC_ARM_SEC - 1);
            value_at += 2 * u32::from(lower.count_ones() as u16);
            return read16(target, value_at);
        }
        entry = value_at + 2 * flags.count_ones();
    }
    Err(ProbeError::Timeout("bootrom function not in the ROM table"))
}

/// Flash `image` to the start of QSPI flash (the XIP base) via the bootrom, verify it by
/// reading back through the coherent (cache-flushed) XIP window, and reset the chip to boot it.
/// `log` receives progress lines.
pub fn flash_image<A: TargetAccess>(
    target: &mut A,
    image: &[u8],
    mut log: impl FnMut(&str),
) -> Result<(), ProbeError> {
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
    let slow = CallFrame { poll_tries: 100_000, ..frame };
    log("connect_internal_flash + flash_exit_xip...");
    target.call_target(u32::from(connect_internal_flash), &[], &frame)?;
    target.call_target(u32::from(flash_exit_xip), &[], &frame)?;

    let erase_len = image.len().div_ceil(SECTOR_BYTES) * SECTOR_BYTES;
    log(&format!("flash_op ERASE {erase_len} bytes at {XIP_BASE:#010x}..."));
    let status = target.call_target(
        u32::from(flash_op),
        &[CFLASH_SECURE | CFLASH_OP_ERASE, XIP_BASE, erase_len as u32, 0],
        &slow,
    )?;
    if status != 0 {
        log(&format!("flash_op erase status = {} ({status:#010x})", status as i32));
        return Err(ProbeError::Timeout("flash_op erase returned an error status"));
    }

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
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn stub_words_decode_to_the_documented_instructions() {
        assert_eq!(0x880a_f380u32.to_le_bytes(), [0x80, 0xf3, 0x0a, 0x88]);
        assert_eq!(0x4770_6011u32.to_le_bytes(), [0x11, 0x60, 0x70, 0x47]);
        assert_eq!(0xbe00_2780u32.to_le_bytes()[2..], [0x00, 0xbe]);
    }
}
