//! RP2040 (Raspberry Pi Pico, Pico W) support over the generic `lamella-cmsis-dap` host: select
//! the chip's core-0 debug port on its shared multi-drop SWD bus, address its ADIv5 MEM-AP, and
//! program QSPI flash by calling the chip's own bootrom flash API on the halted core -- no flash
//! controller driver on the host.

use lamella_cmsis_dap::{Dap, Transport};
use lamella_probe_core::{ArmDap, CallFrame, ProbeError, TargetAccess, TargetAccessExt};

/// The XIP flash window base -- where a firmware image boots from, and the address a read-back
/// reads through.
///
/// **NOT what the bootrom's erase and program functions take.** Those take an offset from the start
/// of the flash device; see [`flash_image`].
pub const XIP_BASE: u32 = 0x1000_0000;

/// The `DPIDR` every RP2040 debug port answers.
///
/// **THIS SEPARATES THE GENERATIONS AND NOTHING FINER.** An RP2350 answers `0x4c013477`, so this
/// tells a Pico from a Pico 2 -- which is the confusion that erases a board, because the two accept
/// each other's probes and connectors. It does NOT tell a Pico from a Pico W, nor one Pico from
/// another: they carry the same die and answer identically.
pub const RP2040_DPIDR: u32 = 0x0bc1_2477;

/// The core-0 debug port's address on the shared SWD bus (datasheet 2.3.4.1). The top four bits are
/// an instance id a multichip application may change.
pub const CORE0_TARGET_ID: u32 = 0x0100_2927;
/// The core-1 debug port's address on the shared SWD bus.
pub const CORE1_TARGET_ID: u32 = 0x1100_2927;
/// The rescue debug port's address on the shared SWD bus.
///
/// WARNING: selecting this port and then powering it up RESETS THE CHIP -- that is what a rescue
/// port is for. Nothing here selects it; it is named so a caller reading a `DPIDR` back knows which
/// port answered.
pub const RESCUE_TARGET_ID: u32 = 0xf100_2927;

/// The erase granule the bootrom API enforces (datasheet 2.8.3.1.3): `_flash_range_erase` requires
/// a 4096-byte-aligned offset and a count that is a multiple of 4096.
pub const SECTOR_BYTES: usize = 4096;
/// The program granule the bootrom API enforces (datasheet 2.8.3.1.3): `flash_range_program`
/// requires a 256-byte-aligned offset and a count that is a multiple of 256.
pub const PAGE_BYTES: usize = 256;

/// How much flash the bootrom's restored XIP mode can address.
///
/// `_flash_enter_cmd_xip` sets up a 03h serial read with **24 address bits** (datasheet 2.8.3.1.3)
/// -- the widely-supported configuration a debugger asks for so freshly programmed bytes are
/// visible without knowing what flash device is fitted. Past this an address aliases, so it bounds
/// what [`flash_image`] can honestly claim to have verified.
pub const XIP_ADDRESSABLE: usize = 16 * 1024 * 1024;

const BOOTROM_MAGIC_ADDR: u32 = 0x10;
const BOOTROM_MAGIC: u32 = 0x01_75_4d;
const BOOTROM_FUNC_TABLE_PTR: u32 = 0x14;
/// The ROM is 16 KB at address 0 (datasheet 2.6), which bounds every pointer read out of it.
const ROM_END: u32 = 0x4000;
/// How many table entries to read before concluding the walk found no terminator.
const MAX_ROM_ENTRIES: usize = 256;

const STAGE_BASE: u32 = 0x2000_1000;
const STAGE_MAX: usize = 192 * 1024;
const CALL_SP: u32 = 0x2004_0000;
const CALL_TRAP: u32 = 0x2000_0000;

const _: () = {
    assert!(CALL_TRAP < STAGE_BASE, "the trap word sits below the staging window");
    assert!(STAGE_BASE as usize + STAGE_MAX < CALL_SP as usize, "staging ends below the stack top");
    assert!(STAGE_MAX % PAGE_BYTES == 0, "a staged slice is a whole number of program pages");
};

const DEMCR: u32 = 0xe000_edfc;
const DEMCR_VC_CORERESET: u32 = 1 << 0;
const AIRCR: u32 = 0xe000_ed0c;
const AIRCR_SYSRESETREQ: u32 = 0x05fa_0004;
const DHCSR: u32 = 0xe000_edf0;
const DHCSR_DEBUGEN: u32 = 0xa05f_0001;

/// `PSM_FRCE_OFF` (datasheet 2.13): writing a block's bit holds it in reset, clearing it lets the
/// power-on state machine bring the block back up.
const PSM_FRCE_OFF: u32 = 0x4001_0004;
/// `PSM_FRCE_OFF.PROC1`.
const PSM_PROC1: u32 = 1 << 16;

/// `_flash_range_erase`'s block-erase hint: the size the block command erases, and the command
/// itself. The bootrom uses the larger erase where a whole block falls inside the range and falls
/// back to sector erases at the edges, so this is a speed hint and not a granularity the caller
/// has to align to (datasheet 2.8.3.1.3 states both, and its own worked sequence passes these).
const ERASE_BLOCK_SIZE: u32 = 1 << 16;
const ERASE_BLOCK_CMD: u32 = 0xd8;

/// Connect to the RP2040 over SWD and return core 0's `DPIDR`: wake the bus, give the probe a
/// WAIT-retry budget, select the core-0 debug port out of the several on the bus, and open its
/// MEM-AP.
///
/// Unlike the rest of this crate, bring-up is NOT generic over [`TargetAccess`]: waking the bus,
/// configuring the probe's WAIT-retry budget and addressing one port on a multi-drop bus are wire-
/// and probe-level operations that a high-level probe performs inside its own connect command
/// rather than exposing. So this one function is bound to the CMSIS-DAP/ARM stack; everything below
/// it -- reset, ROM lookup, flash -- consumes only the neutral seam.
///
/// # The `DPIDR` is checked before the port is powered up, and the order is the safety
///
/// Until a port is addressed every port on the bus is tristated, so a failure to select is
/// indistinguishable from a target that is not there -- and a debug power-up request aimed at the
/// RESCUE port is a chip reset. The selection's own `DPIDR` read is therefore the evidence that the
/// intended port is listening, and it happens before [`ArmDap::init_mem`] asks for power.
///
/// # Errors
/// The wire failing to come up, no port answering the selection, or a port answering that is not
/// an RP2040's.
pub fn connect<T: Transport>(target: &mut ArmDap<Dap<T>>) -> Result<u32, ProbeError> {
    target.inner_mut().connect_swd_from_dormant()?;
    target.inner_mut().configure_transfers(0, 64, 0)?;
    let idcode = target.inner_mut().select_multidrop_target(CORE0_TARGET_ID)?;
    if idcode != RP2040_DPIDR {
        return Err(ProbeError::Device("the debug port that answered is not an RP2040's"));
    }
    target.init_mem()?;
    Ok(idcode)
}

/// Reset core 0 and halt it at its reset vector, so nothing is executing out of the
/// execute-in-place window when the flash sequence switches that window off.
///
/// # Why `SYSRESETREQ` rather than the reset line, on this part specifically
///
/// The core's `SYSRESETREQ` resets the processor core and not the debug domain (datasheet 2.4.2.9),
/// so the debug port keeps its multi-drop selection across it. **A reset driven through the chip's
/// reset pin goes to the power-on state machine, which resets the debug subsystem too** -- and on a
/// bus where every port tristates until it is addressed, that leaves the port unselected and every
/// access afterwards answering nothing. So the generic reset-and-halt, which drives the reset line
/// first, is deliberately not what this uses.
///
/// # Errors
/// The core failing to report itself halted within the poll budget.
pub fn reset_halt<A: TargetAccess>(target: &mut A) -> Result<(), ProbeError> {
    target.write_word(DHCSR, DHCSR_DEBUGEN)?;
    target.write_word(DEMCR, DEMCR_VC_CORERESET)?;
    let _ = target.write_word(AIRCR, AIRCR_SYSRESETREQ);
    let mut halted = false;
    for _ in 0..4000 {
        if let Ok(true) = target.is_halted() {
            halted = true;
            break;
        }
    }
    let _ = target.write_word(DEMCR, 0);
    if halted { Ok(()) } else { Err(ProbeError::Timeout("reset-halt: core did not halt")) }
}

/// The bootrom's public function table, read off the target and checked against what the datasheet
/// says is in it.
///
/// # Why this is read and validated rather than looked up
///
/// The datasheet publishes the table's ADDRESS (a halfword pointer at `0x14`), the CODE of every
/// function in it, and the existence of a `rom_table_lookup()` helper behind the pointer at `0x18`
/// -- but **neither the layout of a table entry nor the helper's parameters**, deferring both to the
/// vendor SDK's source. This crate is written from the published document, so it does neither: it
/// reads the table and requires it to decode, and refuses if it does not.
///
/// # What makes the decode evidence rather than a guess
///
/// A pointer table of halfwords admits one shape -- pairs of (code, address) -- and the check is
/// that the WHOLE table decodes that way and agrees with facts the datasheet does publish:
///
/// - every entry's code is two printable characters and every entry's address lands inside the
///   16 KB ROM, all the way to a terminator;
/// - `_debug_trampoline` and `_debug_trampoline_end` are both present, and the datasheet says the
///   second is *the address of the final `BKPT #0` instruction* of the first -- so the two must be
///   a few bytes apart in that order, **and the halfword at the second must actually be a `BKPT
///   #0`**. That last one is read off the chip: it is the table's own claim about ROM checked
///   against the ROM.
///
/// The addresses are Arm function pointers and so carry the Thumb bit -- they are odd, and the bit
/// is masked off before reading and left on when calling, which is what `call_target` wants.
///
/// A layout that decoded to different addresses would have to place a `BKPT` at a plausible
/// distance from a plausible neighbour by accident. Nothing is called until all of it holds.
pub struct RomFunctions {
    entries: Vec<(u16, u16)>,
}

impl RomFunctions {
    /// Read and validate the table.
    ///
    /// # Errors
    /// A missing or wrong bootrom magic, a table pointer outside ROM, a table that does not decode,
    /// or the trampoline control failing.
    pub fn read<A: TargetAccess>(target: &mut A) -> Result<Self, ProbeError> {
        if target.read_word(BOOTROM_MAGIC_ADDR)? & 0x00ff_ffff != BOOTROM_MAGIC {
            return Err(ProbeError::Device("no RP2040 bootrom magic at 0x10"));
        }
        let table = u32::from(read_halfword(target, BOOTROM_FUNC_TABLE_PTR)?);
        if table == 0 || table >= ROM_END {
            return Err(ProbeError::Device("the bootrom function table pointer is outside ROM"));
        }

        let available = ((ROM_END - table) / 2) as usize;
        let raw = read_halfwords(target, table, available.min(MAX_ROM_ENTRIES * 2))?;

        let mut entries = Vec::new();
        let mut terminated = false;
        for pair in raw.chunks_exact(2) {
            let (code, address) = (pair[0], pair[1]);
            if code == 0 {
                terminated = true;
                break;
            }
            let printable = code.to_le_bytes().iter().all(|b| (0x20..=0x7e).contains(b));
            if !printable || address == 0 || u32::from(address) >= ROM_END {
                return Err(ProbeError::Device(
                    "the bootrom function table does not decode as (code, address) entries",
                ));
            }
            entries.push((code, address));
        }
        if !terminated {
            return Err(ProbeError::Device("the bootrom function table has no terminator"));
        }

        let functions = RomFunctions { entries };
        functions.check_trampoline_control(target)?;
        Ok(functions)
    }

    /// The datasheet's own statement about two of these entries, checked against the ROM.
    fn check_trampoline_control<A: TargetAccess>(&self, target: &mut A) -> Result<(), ProbeError> {
        const TRAMPOLINE_MAX_BYTES: u16 = 64;
        const BKPT_0: u16 = 0xbe00;

        let start = self.lookup(b'D', b'T').ok_or(ProbeError::Device(
            "the bootrom function table decoded but does not contain _debug_trampoline",
        ))?;
        let end = self.lookup(b'D', b'E').ok_or(ProbeError::Device(
            "the bootrom function table decoded but does not contain _debug_trampoline_end",
        ))?;
        if end <= start || end - start > TRAMPOLINE_MAX_BYTES {
            return Err(ProbeError::Device(
                "the decoded _debug_trampoline addresses are not a function and its own end",
            ));
        }
        if read_halfword(target, u32::from(end) & !1)? != BKPT_0 {
            return Err(ProbeError::Device(
                "the decoded _debug_trampoline_end does not point at a BKPT, so the table layout \
                 read here is not the layout this bootrom uses",
            ));
        }
        Ok(())
    }

    /// The address of the function with this two-character code, if the table carries one.
    #[must_use]
    pub fn lookup(&self, c1: u8, c2: u8) -> Option<u16> {
        let wanted = u16::from(c1) | (u16::from(c2) << 8);
        self.entries.iter().find(|(code, _)| *code == wanted).map(|(_, address)| *address)
    }

    /// [`lookup`](Self::lookup), failing by name when the bootrom does not carry the function.
    ///
    /// # Errors
    /// The code not being in the table.
    pub fn require(&self, c1: u8, c2: u8, what: &'static str) -> Result<u32, ProbeError> {
        self.lookup(c1, c2).map(u32::from).ok_or(ProbeError::Device(what))
    }

    /// How many functions the table carries -- the one number that says the walk found a table
    /// rather than a plausible-looking start of one.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the table is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Every (code, address) pair, in table order -- for a tool that reports what it found.
    pub fn entries(&self) -> impl Iterator<Item = (u16, u16)> + '_ {
        self.entries.iter().copied()
    }
}

/// Resolve one bootrom function by its two-character code, reading and validating the table.
///
/// [`RomFunctions::read`] once and [`RomFunctions::lookup`] repeatedly where more than one function
/// is wanted: the table costs a read of the whole ROM tail, and asking for six functions one at a
/// time pays for it six times.
///
/// # Errors
/// Whatever [`RomFunctions::read`] refuses on, or the code not being in the table.
pub fn rom_function<A: TargetAccess>(target: &mut A, c1: u8, c2: u8) -> Result<u16, ProbeError> {
    RomFunctions::read(target)?
        .lookup(c1, c2)
        .ok_or(ProbeError::Device("that function is not in the bootrom table"))
}

/// Flash `image` to the start of QSPI flash, verify it by reading back through the restored
/// execute-in-place window, and leave the core halted for the caller to reset.
///
/// `log` receives progress lines.
///
/// # The addresses, because two of them are not the same number
///
/// The image lands at [`XIP_BASE`] and that is where the read-back reads it from. The bootrom's
/// erase and program functions take an OFFSET FROM THE START OF FLASH, so they are called with 0
/// and with the offsets inside the image -- not with the window base. **BOTH of those calls take
/// the offset, and the two numbers differ by 256 MB**, so passing the window base to either aims a
/// quarter of a gigabyte past the start of the flash device.
///
/// # The second core
///
/// `_flash_exit_xip` leaves the flash interface unable to serve execute-in-place reads until
/// `_flash_enter_cmd_xip` puts it back, so any core fetching from that window in between stalls
/// mid-instruction. Reset-halting core 0 does not stop core 1 -- `SYSRESETREQ` resets only the core
/// that wrote it -- so core 1 is held in reset through the power-on state machine for the duration
/// and released before the caller boots the image, which leaves it where a cold boot leaves it:
/// out of reset, in the bootrom, waiting to be launched.
///
/// # Errors
/// An empty image, a bootrom that does not validate, a failing erase or program, or a read-back
/// that does not match what was written.
pub fn flash_image<A: TargetAccess>(
    target: &mut A,
    image: &[u8],
    mut log: impl FnMut(&str),
) -> Result<(), ProbeError> {
    if image.is_empty() {
        return Err(ProbeError::Device("image empty"));
    }
    if image.len() > XIP_ADDRESSABLE {
        return Err(ProbeError::Device(
            "image larger than the 16 MB the bootrom's XIP read command can address",
        ));
    }

    log("reading the bootrom function table...");
    let rom = RomFunctions::read(target)?;
    let connect_internal_flash =
        rom.require(b'I', b'F', "this bootrom has no _connect_internal_flash")?;
    let flash_exit_xip = rom.require(b'E', b'X', "this bootrom has no _flash_exit_xip")?;
    let flash_range_erase = rom.require(b'R', b'E', "this bootrom has no _flash_range_erase")?;
    let flash_range_program = rom.require(b'R', b'P', "this bootrom has no flash_range_program")?;
    let flash_flush_cache = rom.require(b'F', b'C', "this bootrom has no _flash_flush_cache")?;
    let flash_enter_cmd_xip = rom.require(b'C', b'X', "this bootrom has no _flash_enter_cmd_xip")?;
    log(&format!("  {} functions; erase at {flash_range_erase:#06x}", rom.len()));

    log("reset-halt core 0...");
    reset_halt(target)?;

    hold_core1(target, true)?;
    let outcome = program_and_verify(
        target,
        image,
        FlashRom {
            connect_internal_flash,
            flash_exit_xip,
            flash_range_erase,
            flash_range_program,
            flash_flush_cache,
            flash_enter_cmd_xip,
        },
        &mut log,
    );
    let released = hold_core1(target, false);
    outcome?;
    released?;
    log("flashed + verified; the core is halted, ready to be reset into the image.");
    Ok(())
}

/// The six bootrom entry points the flash sequence calls, resolved once.
struct FlashRom {
    connect_internal_flash: u32,
    flash_exit_xip: u32,
    flash_range_erase: u32,
    flash_range_program: u32,
    flash_flush_cache: u32,
    flash_enter_cmd_xip: u32,
}

fn program_and_verify<A: TargetAccess>(
    target: &mut A,
    image: &[u8],
    rom: FlashRom,
    log: &mut impl FnMut(&str),
) -> Result<(), ProbeError> {
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
    target.call_target(rom.connect_internal_flash, &[], &frame)?;
    target.call_target(rom.flash_exit_xip, &[], &frame)?;

    let erase_len = image.len().div_ceil(SECTOR_BYTES) * SECTOR_BYTES;
    log(&format!("_flash_range_erase {erase_len} bytes at flash offset 0..."));
    target.call_target(
        rom.flash_range_erase,
        &[0, erase_len as u32, ERASE_BLOCK_SIZE, ERASE_BLOCK_CMD],
        &slow,
    )?;

    let stage_words = STAGE_MAX / 4;
    for (index, slice) in words.chunks(stage_words).enumerate() {
        let offset = (index * STAGE_MAX) as u32;
        log(&format!(
            "staging {} bytes at {STAGE_BASE:#010x}, flash_range_program at flash offset {offset:#x}...",
            slice.len() * 4
        ));
        target.write_words(STAGE_BASE, slice)?;
        target.call_target(
            rom.flash_range_program,
            &[offset, STAGE_BASE, (slice.len() * 4) as u32],
            &slow,
        )?;
    }

    target.call_target(rom.flash_flush_cache, &[], &frame)?;
    target.call_target(rom.flash_enter_cmd_xip, &[], &frame)?;

    log("verifying (full read-back over XIP)...");
    let readback = target.read_words(XIP_BASE, words.len())?;
    if readback != words {
        let first_bad = readback.iter().zip(&words).position(|(a, b)| a != b).unwrap_or(0);
        log(&format!("VERIFY FAILED at word {first_bad} (byte offset {:#x})", first_bad * 4));
        return Err(ProbeError::Device("flash verify mismatch"));
    }
    Ok(())
}

/// Hold core 1 in reset, or release it.
///
/// Read-modify-write rather than a bare store: `PSM_FRCE_OFF` carries a bit per block and the
/// others are nobody's business here.
///
/// # Errors
/// The register read or write failing.
pub fn hold_core1<A: TargetAccess>(target: &mut A, held: bool) -> Result<(), ProbeError> {
    let current = target.read_word(PSM_FRCE_OFF)?;
    let wanted = if held { current | PSM_PROC1 } else { current & !PSM_PROC1 };
    target.write_word(PSM_FRCE_OFF, wanted)
}

/// One halfword out of target memory, from a possibly unaligned address.
fn read_halfword<A: TargetAccess>(target: &mut A, address: u32) -> Result<u16, ProbeError> {
    let word = target.read_word(address & !3)?;
    Ok(if address & 2 != 0 { (word >> 16) as u16 } else { word as u16 })
}

/// `count` consecutive halfwords, in ONE batched word read rather than one round trip each.
///
/// The ROM table is several hundred halfwords and a probe round trip is milliseconds, so reading it
/// a halfword at a time is the difference between a lookup that costs nothing and one that is
/// noticeable in a flash's wall clock.
fn read_halfwords<A: TargetAccess>(
    target: &mut A,
    start: u32,
    count: usize,
) -> Result<Vec<u16>, ProbeError> {
    let skip = usize::from(start & 2 != 0);
    let words = target.read_words(start & !3, (count + skip).div_ceil(2))?;
    let mut out = Vec::with_capacity(words.len() * 2);
    for word in words {
        out.push(word as u16);
        out.push((word >> 16) as u16);
    }
    out.drain(..skip);
    out.truncate(count);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bootrom_magic_is_the_rp2040s_and_not_its_successors() {
        assert_eq!(BOOTROM_MAGIC.to_le_bytes()[..3], [b'M', b'u', 0x01]);
    }

    #[test]
    fn the_generations_are_told_apart_by_their_debug_ports() {
        const RP2350_DPIDR: u32 = 0x4c01_3477;
        assert_ne!(RP2040_DPIDR, RP2350_DPIDR);
    }

    #[test]
    fn the_multidrop_addresses_are_distinct() {
        let ports = [CORE0_TARGET_ID, CORE1_TARGET_ID, RESCUE_TARGET_ID];
        for (i, a) in ports.iter().enumerate() {
            for b in &ports[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    #[test]
    fn a_function_code_packs_its_first_character_low() {
        let table = RomFunctions { entries: vec![(0x4552, 0x1234)] };
        assert_eq!(table.lookup(b'R', b'E'), Some(0x1234));
        assert_eq!(table.lookup(b'E', b'R'), None);
    }
}
