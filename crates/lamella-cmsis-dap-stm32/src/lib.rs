//! STMicroelectronics STM32 flash programming over a Lamella debug probe.

//! The H7 lives in its own module rather than beside the others, because its controller shares
//! almost nothing with them -- a 32-byte write granule, a queue flag that is not `BSY`, and two
//! banks with two register sets. Keeping it here would have invited exactly the constant-swapping
//! that makes a flash algorithm subtly wrong.
//! The C0 is likewise its own module, and for a reason that reads as a coincidence until it
//! bites: it shares the F0's 2 KB page and NOT its programming granule (64-bit double words against
//! the F0's 16 bits). Two families that agree on the easy fact and differ on the hard one are
//! exactly the pair worth keeping apart.
mod c0;
pub use c0::{STM32C0_DOUBLE_WORD, STM32C0_FLASH_BASE, STM32C0_PAGE, Stm32C0Flash};

/// The U5 gets its own module again: 8 KB pages, a 128-bit quad-word granule, two banks selected
/// by a `BKER` bit, and TrustZone-aliased registers. Four families in this crate now have four
/// different programming granules -- 16, 64, 128 and 256 bits -- which is the fact most likely to be
/// copied from a neighbour and the one that only fails on silicon.
mod u5;
pub use u5::{
    STM32U5_FLASH_BASE, STM32U5_FLASH_SIZE_REG, STM32U5_PAGE, STM32U5_MAX_PAGES_PER_BANK, STM32U5_QUAD_WORD, Stm32U5Flash,
};

mod h7;
pub use h7::{
    STM32H7_BANK2_BASE, STM32H7_FLASH_BASE, STM32H7_FLASH_WORD, STM32H7_SECTOR, Stm32H7Flash,
};

use lamella_probe_core::{CallFrame, ProbeError, TargetAccess};

/// **Where each ST family keeps the flash size the FACTORY programmed into it.**
///
/// A host tool cannot see how much flash the part in front of it has. It can be told on the command
/// line, and then it is being told what somebody believes rather than what is true -- which is how
/// an image gets erased and programmed past the end of an array, one page at a time, reporting
/// success on every page that happened to exist. **The part knows, and every one of these families
/// has a register that says so**, factory-programmed and read-only.
///
/// Each address is read out of that family's own reference manual rather than inferred from a
/// neighbour, because they share no pattern at all:
///
/// | family | register | manual |
/// |---|---|---|
/// | STM32F0 | `0x1FFF_F7CC` | RM0091 33.2, "Flash memory size data register" |
/// | STM32F4 | `0x1FFF_7A22` | RM0090 39.2, "Flash size" |
/// | STM32F7 | `0x1FF0_F442` | RM0385 41.2, "Flash size" |
/// | STM32H7 | `0x1FF1_E880` | RM0399 64.2, "Flash size" |
/// | STM32L0 | `0x1FF8_007C` | RM0377 and RM0367 34.1.1, "Flash size register" |
/// | STM32U5 | `0x0BFA_07A0` | RM0456 76.2, "Flash size data register" |
///
pub const STM32F0_FLASH_SIZE_REG: u32 = 0x1FFF_F7CC;
/// See [`STM32F0_FLASH_SIZE_REG`].
pub const STM32F4_FLASH_SIZE_REG: u32 = 0x1FFF_7A22;
/// See [`STM32F0_FLASH_SIZE_REG`].
pub const STM32F7_FLASH_SIZE_REG: u32 = 0x1FF0_F442;
/// See [`STM32F0_FLASH_SIZE_REG`].
pub const STM32H7_FLASH_SIZE_REG: u32 = 0x1FF1_E880;
/// See [`STM32F0_FLASH_SIZE_REG`].
pub const STM32L0_FLASH_SIZE_REG: u32 = 0x1FF8_007C;

/// Reads one of the [`STM32F0_FLASH_SIZE_REG`] family of registers and returns the size in BYTES.
///
/// **The register is sixteen bits and three of these addresses are not word aligned** -- the F4's
/// ends in `0x22` and the F7's in `0x42`. A 32-bit read of an unaligned address is not a way to get
/// the halfword: the MEM-AP either faults it or answers for a different address. So the aligned word
/// containing it is read and the correct half taken, which is also why this is a function rather
/// than six call sites doing their own shift.
///
/// Refuses a reading of zero or `0xffff`. Those are what an unmapped read and a floating bus
/// produce, and both would otherwise decode as a plausible-looking part -- zero flash, or 64 MB.
pub fn stm32_flash_size_bytes<A: TargetAccess>(
    target: &mut A,
    register: u32,
) -> Result<u32, ProbeError> {
    let word = target.read_word(register & !3)?;
    let kb = if register & 2 != 0 { (word >> 16) & 0xffff } else { word & 0xffff };
    if kb == 0 || kb == 0xffff {
        return Err(ProbeError::Device(
            "the flash-size register reads blank -- is this the right part for --part?",
        ));
    }
    Ok(kb * 1024)
}

/// Which of the two questions a flash status-register poll is asking.
///
/// **They are two different questions.** An error bit in an STM32 flash status register is
/// write-1-to-clear, so it outlives the host tool that caused it: a board can be attached already
/// holding a flag from a session that has ended. Whether that flag is an error depends entirely on
/// which question is being asked of it.
///
/// Reporting a latched flag before anything has been commanded names an operation that never ran.
/// It is also self-erasing, because the reporting path clears the flags on its way out -- so the
/// retry succeeds, and **a guard that fails once and then passes protects nothing while costing one
/// confusing failure**. That is the shape that gets blamed on the probe or the cable.
///
/// The distinction is made at the CALL SITE rather than inside the poll, so that reading an erase
/// or program routine shows which question each of its waits is asking.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum FlashWait {
    /// Before anything has been commanded: wait for the controller, then DISCARD whatever the
    /// previous session left latched. A stale flag is not this operation's error.
    BeforeOperation,
    /// After an operation has been commanded: wait for it to finish, then report what it latched
    /// as that operation's error.
    AfterOperation,
}

const FLASH_KEYR: u32 = 0x4002_3C04;
const FLASH_SR: u32 = 0x4002_3C0C;
const FLASH_CR: u32 = 0x4002_3C10;
const KEY1: u32 = 0x4567_0123;
const KEY2: u32 = 0xCDEF_89AB;
const SR_BSY: u32 = 1 << 16;
const CR_PG: u32 = 1 << 0;
const CR_SER: u32 = 1 << 1;
const CR_SNB_SHIFT: u32 = 3;
/// The largest sector index `FLASH_CR.SNB` can carry on any part this trait drives.
///
/// The field is FOUR bits on an F4/F74x ([6:3]) and FIVE on an F76x ([7:3], RM0410), which needs
/// them for a dual-bank part's 24 sectors. This is the wider of the two, so it bounds the register
/// rather than the part -- see the refusal in `erase_sector` for why a bound belongs here at all.
const SNB_MAX: u32 = 0b1_1111;
const CR_PSIZE_X32: u32 = 0b10 << 8;
const CR_STRT: u32 = 1 << 16;
const CR_LOCK: u32 = 1 << 31;

/// STM32F4 embedded-flash programming, added to ANY [`TargetAccess`] probe. Halt the core before
/// erasing or writing so it is not fetching from flash during the operation, and program only
/// erased (0xFF) flash.
///
/// **NOT CMSIS-DAP-ONLY, whatever this crate is called.** The impl below is blanket over
/// `TargetAccess`, so an ST-Link gets these methods too -- `lamella_stlink::StLink` implements that
/// trait directly, and `lamella-stlink`'s `stlink-flash` example is the composition with no glue.
/// This doc previously said "a CMSIS-DAP `TargetAccess` probe", which together with the crate's name
/// read as a transport binding that does not exist, and cost a lane a bug report for a gap that was
/// not there. **A name that under-claims fails in the direction where nobody files a bug**, because
/// the reader assumes the limit is real and works around it.
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
        if sector > SNB_MAX {
            return Err(ProbeError::Device(
                "STM32 sector index does not fit the FLASH_CR SNB field -- refusing rather than erasing a different sector",
            ));
        }
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

/// The sector sizes of an STM32F4 with 1 MB of flash, in order from sector 0: four of 16 KB, one
/// of 64 KB, then seven of 128 KB (RM0090).
///
/// THE TABLE HAS TO DESCRIBE THE WHOLE PART, AND A SHORT ONE IS A LIVE UNDER-ERASE.
/// `sectors_covering` saturates at the end of the table, so an image reaching past the last entry
/// erases the sectors the table knows about, is told those were all of them, and then programs its
/// tail into flash that was never erased. Nothing reports it: the trigger is simply "the firmware
/// grew past what the table covers".
///
/// A 2 MB dual-bank F4 (the F429ZI among them) has a SECOND bank of twelve more sectors that this
/// table does not describe. An image inside the first megabyte is unaffected; a larger one needs
/// the bank-2 geometry added rather than this table extended.
pub const STM32F4_SECTOR_SIZES: [usize; 12] = [
    16 * 1024, 16 * 1024, 16 * 1024, 16 * 1024,
    64 * 1024,
    128 * 1024, 128 * 1024, 128 * 1024, 128 * 1024, 128 * 1024, 128 * 1024, 128 * 1024,
];

/// The sector sizes of an STM32F74x/F75x with 1 MB of flash, in order from sector 0 (RM0385).
///
/// THE F7 SHARES THE F4'S FLASH CONTROLLER AND NOT ITS GEOMETRY -- the only place the two
/// diverge, and the easiest to miss. Verified against RM0385 rather than assumed: the register
/// block sits at the same base with the same offsets (`ACR/KEYR/OPTKEYR/SR/CR` =
/// `0x00/0x04/0x08/0x0C/0x10`), the same two keys, the same `CR` bit positions (`PG` 0, `SER` 1,
/// `MER` 2, `SNB` [6:3], `PSIZE` [9:8], `STRT` 16, `LOCK` 31) and `SR.BSY` at 16. **So
/// [`Stm32F4Flash`] IS the F7's sequence**, and is reused deliberately rather than copied under a
/// second name. What differs is the geometry: an F4 starts with four 16 KB sectors and tails in
/// 128 KB; an F7 starts with four of 32 KB and tails in 256 KB.
///
/// Getting this wrong in the SMALL direction is the dangerous one -- too few sectors erased
/// leaves part of an image programmed into flash that was never erased, which corrupts or fails
/// per word rather than failing up front.
pub const STM32F7_SECTOR_SIZES: [usize; 8] = [
    32 * 1024, 32 * 1024, 32 * 1024, 32 * 1024, 128 * 1024, 256 * 1024, 256 * 1024, 256 * 1024,
];

/// The sector sizes of an STM32F76x/F77x with 2 MB of flash **in single-bank mode**, in order from
/// sector 0: four of 32 KB, one of 128 KB, then seven of 256 KB (RM0410).
///
/// THE GEOMETRY OF THIS PART IS NOT FIXED BY ITS PART NUMBER -- IT IS SET BY AN OPTION BIT, AND
/// THAT IS THE WHOLE FINDING. `FLASH_OPTCR.nDBANK` (bit 29) selects between this layout and a
/// DUAL-bank one of two 1 MB banks, each 4x16 KB + 1x64 KB + 7x128 KB. Same silicon, same order
/// code, two different sector maps. **A table chosen from the part number would be right or wrong
/// depending on a bit nobody looked at**, which is the same shape as a chip id being unable to tell
/// a populated board from a bare one.
///
/// A caller can tell which layout it is holding without guessing: `nDBANK` SET in `FLASH_OPTCR`
/// (`0xffffaafd` on a part in single-bank mode) selects this table, and the flash-size register at
/// `0x1ff0f442` gives the total in KB -- `0x0800` is 2048 KB, which this table's entries sum to.
///
/// The DUAL-bank layout is deliberately NOT added here yet. It needs its own sector NUMBERING
/// (RM0410 says dual-bank numbering differs from single-bank), and a table without the bit that
/// selects it would be an invitation to pick the wrong one.
pub const STM32F76X_SECTOR_SIZES_SINGLE_BANK: [usize; 12] = [
    32 * 1024, 32 * 1024, 32 * 1024, 32 * 1024,
    128 * 1024,
    256 * 1024, 256 * 1024, 256 * 1024, 256 * 1024, 256 * 1024, 256 * 1024, 256 * 1024,
];

/// `FLASH_OPTCR`, whose bit 29 (`nDBANK`) decides which of the two F76x geometries applies.
pub const STM32F7_OPTCR: u32 = 0x4002_3C14;
/// `FLASH_OPTCR.nDBANK`: SET means single bank, CLEAR means dual.
pub const STM32F7_OPTCR_NDBANK: u32 = 1 << 29;

/// How many sectors from 0 an image of `len` bytes spans, given a family's sector `sizes`.
///
/// Saturates at `sizes.len()`: a part with more flash than the table describes is a table that
/// needs extending, and quietly erasing fewer sectors than an image needs is exactly the failure
/// this exists to prevent.
#[must_use]
pub fn sectors_covering(len: usize, sizes: &[usize]) -> u32 {
    let mut covered = 0usize;
    for (index, size) in sizes.iter().enumerate() {
        covered += size;
        if covered >= len {
            return index as u32 + 1;
        }
    }
    sizes.len() as u32
}

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
        f0_wait_idle(self, FlashWait::BeforeOperation)?;
        self.write_word(F0_CR, F0_CR_PER)?;
        self.write_word(F0_AR, address & !(STM32F0_PAGE - 1))?;
        self.write_word(F0_CR, F0_CR_PER | F0_CR_STRT)?;
        f0_wait_idle(self, FlashWait::AfterOperation)?;
        self.write_word(F0_CR, 0)
    }

    fn f0_program(&mut self, address: u32, data: &[u8]) -> Result<(), ProbeError> {
        if address % 2 != 0 || data.len() % 2 != 0 {
            return Err(ProbeError::Device("STM32F0 programming is half-word granular"));
        }
        f0_wait_idle(self, FlashWait::BeforeOperation)?;
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
///
/// On [`FlashWait::BeforeOperation`] the latched flags are cleared and NOT reported -- see the
/// enum, which records the board this distinction was measured on.
fn f0_wait_idle<A: TargetAccess>(target: &mut A, phase: FlashWait) -> Result<(), ProbeError> {
    for _ in 0..100_000 {
        let sr = target.read_word(F0_SR)?;
        if sr & F0_SR_BSY != 0 {
            continue;
        }
        if sr & (F0_SR_PGERR | F0_SR_WRPRTERR | F0_SR_EOP) != 0 {
            target.write_word(F0_SR, F0_SR_PGERR | F0_SR_WRPRTERR | F0_SR_EOP)?;
        }
        if phase == FlashWait::BeforeOperation {
            return Ok(());
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

const L0_BASE: u32 = 0x4002_2000;
const L0_PECR: u32 = L0_BASE + 0x04;
const L0_PEKEYR: u32 = L0_BASE + 0x0c;
const L0_PRGKEYR: u32 = L0_BASE + 0x10;
const L0_SR: u32 = L0_BASE + 0x18;
const L0_PEKEY1: u32 = 0x89ab_cdef;
const L0_PEKEY2: u32 = 0x0203_0405;
const L0_PRGKEY1: u32 = 0x8c9d_aebf;
const L0_PRGKEY2: u32 = 0x1314_1516;
const L0_PECR_PELOCK: u32 = 1 << 0;
const L0_PECR_PRGLOCK: u32 = 1 << 1;
/// `PECR.PROG`: selects the program memory, for a page erase or a half-page program.
const L0_PECR_PROG: u32 = 1 << 3;
/// `PECR.ERASE`: the operation is an erase rather than a program.
const L0_PECR_ERASE: u32 = 1 << 9;
const L0_SR_BSY: u32 = 1 << 0;
const L0_SR_EOP: u32 = 1 << 1;
const L0_SR_WRPERR: u32 = 1 << 8;
const L0_SR_PGAERR: u32 = 1 << 9;
const L0_SR_SIZERR: u32 = 1 << 10;
const L0_SR_OPTVERR: u32 = 1 << 11;
const L0_SR_RDERR: u32 = 1 << 13;
const L0_SR_NOTZEROERR: u32 = 1 << 16;
const L0_SR_FWWERR: u32 = 1 << 17;
const L0_SR_ERRORS: u32 = L0_SR_WRPERR
    | L0_SR_PGAERR
    | L0_SR_OPTVERR
    | L0_SR_RDERR
    | L0_SR_SIZERR
    | L0_SR_NOTZEROERR
    | L0_SR_FWWERR;

/// Where an STM32L0's program flash is mapped, and the address a programming write is aimed at.
pub const STM32L0_FLASH_BASE: u32 = 0x0800_0000;
/// The STM32L0 erase granularity: one page of 128 bytes, which RM0377 also calls 32 words.
///
/// **Checked on the dual-bank part rather than carried over to it.** The 128-byte page is stated
/// for the L0x1 in RM0377's glossary and 3.3.4, and a category 5 device is twice the size of
/// anything else in the family with a second bank besides -- both good reasons for a page to grow.
/// It does not: RM0367 3.3.1 says "a page is composed of 32 words (or 128 bytes)" for the whole
/// L0x3 line, and its Table 6 lays out the 192 KB category 5 part as pages 0 to 1535 of 128 bytes
/// each, which multiplies back to exactly 192 KB.
///
/// **And the two banks are contiguous, so nothing here has to know about them.** Table 6 puts
/// Bank 1 at `0x08000000` to `0x08017FFF` (pages 0 to 767, sectors 0 to 23) and Bank 2 immediately
/// after it at `0x08018000` to `0x0802FFFF` (pages 768 to 1535, sectors 24 to 47). A linear
/// erase-and-program from the flash base crosses that boundary without noticing it, which is why
/// [`Stm32L0Flash`] has no bank parameter. The banks are
/// addressable separately for three things this crate does not do: `PECR.PARALLELBANK`
/// programming, the `SYSCFG_CFGR1.UFB` swap that maps Bank 2 at the flash base, and the
/// `FLASH_OPTR.BFB2` boot mechanism.
pub const STM32L0_PAGE: u32 = 128;

/// **What an ERASED word reads as on this part, and it is not what the rest of this tree assumes.**
///
/// STM32 F0/F4/F7, the SAM D and E families, the nRF5x and the RP2350 all erase flash to all-ones,
/// so "is this erased?" is written as a comparison against `0xffffffff` throughout. **The STM32L0
/// erases to ZERO** (RM0377 3.3.4: a program raises `NOTZEROERR` when "the user tries to write a
/// value in a word which is not zero", and outside category 3 devices the write is then not
/// performed at all). A blank-check or a post-erase verify carried over from any other part reads
/// a correctly erased L0 as full, and a correctly erased L0 page looks like erased-and-then-zeroed
/// data to a tool that does not know the difference.
pub const STM32L0_ERASED_WORD: u32 = 0x0000_0000;

/// Where an STM32 F0 or L0 keeps `DBGMCU_IDCODE`, the register that names the die.
///
/// **A debug-port IDCODE names the port's design and not the part behind it** -- every M0-class ST
/// part answers `0x0bc11477`, an L0 and a C0 and a SAM D11 alike -- so a tool that
/// asks the wire what it is reaching gets an answer about Arm rather than about ST. This register
/// is the one that answers about ST: `DEV_ID` in bits 11:0, `REV_ID` in bits 31:16, and bits 15:12
/// reading `0b0110` on the L0 (RM0367 33.4.1, RM0377 27.4.1).
///
/// It is at a peripheral address on the F0 and the L0 and at `0xE0042000` on an F4 or F7, which is
/// why this constant names the family it belongs to rather than pretending to be general.
///
/// Read over SWD with nothing enabled first, and with the core still running: RM0367 33.4.1 says
/// the code "is accessible by the software debug port (two pins) or by the user software", and a
/// NUCLEO-L073RZ answered `0x20006447` before anything halted it or enabled a clock.
///
/// **That distinction is worth stating, because `SYSCFG_CFGR1` on the same part reads `0x00000000`
/// when its clock is off** -- a reassuring value that means nothing -- and the two registers are
/// one paragraph apart in a reader's head.
pub const STM32L0_DBGMCU_IDCODE: u32 = 0x4001_5800;

/// The ST product CATEGORY an STM32L0 reports about itself, and it is a safety-relevant reading
/// rather than a label.
///
/// ST divides the L0 into product categories that share this crate's whole register model -- base,
/// offsets, keys, `PECR` bits, `SR` bits, the 128-byte page and the erased value -- and differ on
/// what the memory does when a program hits a word that is not zero. On a **category 3** part the
/// write is performed anyway, ORing old with new "both for data and ECC", and the cell can no
/// longer be read back correctly; on every other category it is discarded (RM0367 3.3.4). So the
/// same mistake is a diagnostic on one part and unrecoverable data loss on its sibling, and this
/// reading is the only thing that says which one is on the wire.
///
/// **`DEV_ID` names the CATEGORY, not the line.** `0x417` is category 3 whether the part is an
/// L051 (RM0377's L0x1) or an L053 (RM0367's L0x3), so this cannot tell those two apart and does
/// not claim to -- but the category is exactly the fact the paragraph above turns on. Values from
/// RM0377 27.4.1, which lists all four, cross-checked against RM0367 33.4.1, which lists the two
/// the L0x3 line has.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stm32L0Category {
    /// `DEV_ID` 0x457. The only category with RM0377 2.4's empty check.
    One,
    /// `DEV_ID` 0x425.
    Two,
    /// `DEV_ID` 0x417. **The category where a program to an unerased word is PERFORMED** rather
    /// than discarded (RM0367 3.3.4).
    Three,
    /// `DEV_ID` 0x447. Dual bank, and the only category carrying `PECR.NZDISABLE` and
    /// `PECR.PARALLELBANK` (RM0367 3.7.2).
    Five,
}

impl Stm32L0Category {
    /// Decodes `DEV_ID` -- the low twelve bits of [`STM32L0_DBGMCU_IDCODE`].
    ///
    /// `None` is "this is not an id RM0377 or RM0367 lists", which on an `--part l0` run means the
    /// part in front of the tool is not an L0 at all. It is deliberately not "some other L0": ST
    /// numbers the categories 1, 2, 3 and 5, with no category 4, so an unlisted id is a different
    /// family rather than a gap in this list.
    pub fn from_dev_id(dev_id: u32) -> Option<Self> {
        match dev_id & 0xfff {
            0x457 => Some(Self::One),
            0x425 => Some(Self::Two),
            0x417 => Some(Self::Three),
            0x447 => Some(Self::Five),
            _ => None,
        }
    }

    /// Whether a program to a word that is not zero is CARRIED OUT on this category rather than
    /// discarded -- true for category 3 alone (RM0367 3.3.4).
    ///
    /// The true answer is the dangerous one: what lands is the OR of old and new including the ECC,
    /// and the manual says a read-back "might not return the old value, the new one or the ORed
    /// values". A retry cannot repair it, because the ECC is already wrong.
    pub fn program_to_unerased_word_is_performed(&self) -> bool {
        matches!(self, Self::Three)
    }

    /// The category's name as ST writes it, for a tool reporting what it read.
    pub fn name(&self) -> &'static str {
        match self {
            Self::One => "category 1",
            Self::Two => "category 2",
            Self::Three => "category 3",
            Self::Five => "category 5",
        }
    }
}

/// Reads [`STM32L0_DBGMCU_IDCODE`] and returns `(DEV_ID, REV_ID)`.
///
/// Refuses an all-zero or all-ones reading. Those are what an unmapped read and an undriven bus
/// produce, and `0x000` would otherwise decode as "no category I recognize" -- a true statement
/// about a reading that never happened, which is the failure mode
/// [`stm32_flash_size_bytes`] refuses for the same reason.
pub fn stm32l0_dev_id<A: TargetAccess>(target: &mut A) -> Result<(u32, u32), ProbeError> {
    let word = target.read_word(STM32L0_DBGMCU_IDCODE)?;
    if word == 0 || word == 0xffff_ffff {
        return Err(ProbeError::Device(
            "DBGMCU_IDCODE reads blank -- the debug port answered but the part did not",
        ));
    }
    Ok((word & 0xfff, word >> 16))
}

/// STM32L0x1 and STM32L0x3 flash programming through the debug port.
///
/// Programming is word at a time straight over SWD, which costs one flash write (RM0377 and RM0367
/// agree on `Tprog`, 3.2 ms) plus a status poll per word. That is the whole cost model on a part of
/// this size and it needs no code running on the target; the half-page mode that would beat it
/// requires the programming loop to execute from RAM, which is the F0 path's RAM loader stub rather
/// than this.
///
/// # Which parts this has been run on
///
/// Driven on silicon against three parts from different lines and different product categories,
/// each doing erase, program, read-back verify, reset, and executing the image afterwards:
///
/// * **STM32L011** (`DEV_ID` 0x457, RM0377, category 1), 16 KB, on a NUCLEO-L011K4.
/// * **STM32L053** (`DEV_ID` 0x417, RM0367, category 3), 64 KB, on a NUCLEO-L053R8.
/// * **STM32L073** (`DEV_ID` 0x447, RM0367, category 5), 192 KB dual bank, on a NUCLEO-L073RZ.
///
/// **That is every category this driver claims, and category 5 needed no code.** The L073 took the
/// same sequence the L011 was written for: same base, same keys, same `PECR` bits, same 128-byte
/// page, same erased value. Its bank boundary was crossed by one `l0_program` call spanning
/// `0x08017FF8` to `0x08018008` -- two words in Bank 1 and two in Bank 2 -- which read back
/// unchanged, against a control doing the same thing across an ordinary page join mid-bank.
///
/// Category 2 (`DEV_ID` 0x425) is the one nobody has read. It is not claimed here.
///
/// # A program to an unerased word behaves differently by category
///
/// ST divides the L0 into product categories, and the two manuals cover an overlapping set: RM0377
/// documents the STM32L0x1 across categories 1, 2, 3 and 5, and RM0367 the STM32L0x3, whose
/// `STM32L053x` and `STM32L063x` are category 3 and whose `STM32L073x` and `STM32L083x` are
/// category 5 (RM0367 Table 1). Every register fact is common to all of them. This is not.
///
/// Writing a word whose current content is not zero raises `NOTZEROERR` everywhere, but what the
/// memory then does splits in two (RM0367 3.3.4, "Program a single word to Flash program memory"):
///
/// * **Category 3 -- the write is PERFORMED.** What lands is the bitwise OR of the old and the new
///   value, "both for data and ECC", and the manual warns that a read-back "might not return the old
///   value, the new one or the ORed values" because the ECC no longer matches the data. Nothing is
///   aborted and nothing is rolled back.
/// * **Every other category -- the write is DISCARDED.** The operation aborts and the cell keeps
///   what it held.
///
/// So the same mistake is a clean refusal on an L011 or an L073 and silent, unreadable corruption on
/// an L053 -- same code path, same status bit. That is why [`Stm32L0Flash::l0_program`] requires an
/// erase first and skips words that are already zero rather than treating either as an
/// optimization: on a category 3 part those checks are the only thing between an out-of-order write
/// and a word that can no longer be read back correctly at all.
///
/// Category 5 additionally carries a `PECR.NZDISABLE` bit that suppresses the check entirely and a
/// `PECR.PARALLELBANK` bit for its second bank. Neither exists on category 3, and this driver sets
/// neither.
///
/// # An erased category 1 part boots the bootloader, and programming it does not undo that
///
/// RM0377 2.4 carries a section headed "Empty check (category 1 devices only)": the part keeps an
/// internal flag that is **set when the word at `0x08000000` reads `0x00000000`**, and while it is
/// set the system memory is selected as the boot area instead of the flash. Because an erased cell
/// on this family reads zero, **every virgin or freshly erased category 1 L0 latches it.**
///
/// The flag is updated *only* when the option bytes are loaded, so programming the part does not
/// clear it: RM0377 is explicit that "only a power-on reset or setting `OBL_LAUNCH` bit in
/// `FLASH_CR` register can clear this flag after programming a virgin device". A system reset --
/// which is what a debug probe issues -- leaves it set.
///
/// **The part still runs the image**, and the same section says why: with the flag set the
/// bootloader is entered, and "the bootloader code switches the boot memory mapping to Flash
/// program memory and performs a jump to the user code it hosts". So the observable effect of a
/// deploy over SWD is a boot detour through system memory, not a part that fails to start -- which
/// matches an STM32L011 that executed its image after a probe-issued reset.
///
/// **The empty check itself is category 1 only.** RM0367 describes no such mechanism for the
/// STM32L0x3 -- the phrase does not occur in the manual -- so neither an L053 nor an L073 keeps
/// that flag.
///
/// # But a category 5 part has a bootloader detour of its own, on a different trigger
///
/// **Do not read the paragraph above as "an L073 always boots straight from flash".** RM0367 3.3.2
/// gives category 5 a dual-bank boot mechanism the other categories do not have, gated on
/// `FLASH_OPTR.BFB2` (bit 23). With `BFB2` set and `BOOT0` low the part maps system memory at zero
/// and runs the bootloader for roughly 440 us, which checks Bank 2 for valid code, then Bank 1,
/// and jumps to whichever it finds -- the same "runs anyway, by way of a detour" shape as the
/// category 1 empty check, reached through an option byte instead of a latched flag.
///
/// Two differences matter to a programmer:
///
/// * **`BFB2` is 0 in ST's production option-byte value** (RM0367 3.7.8 gives `FLASH_OPTR`
///   `0x807000AA`), so the mechanism is off unless somebody turned it on. It is a reading to take,
///   not a hazard to assume -- and nothing this crate does sets it.
/// * **"Valid" is defined differently.** The empty check asks whether `0x08000000` reads zero; the
///   dual-bank boot asks whether the word at the bank's start "points to a valid address (stack top
///   address)". A zero initial stack pointer fails both, which is one more reason the refusal in
///   `stlink-flash` is worth its line, but a NON-zero nonsense SP fails only the second.
///
/// **The case that does not recover is an image whose first word is zero.** The programmer skips
/// words that are already zero (see [`Stm32L0Flash::l0_program`]), so a zero initial stack pointer
/// leaves `0x08000000` reading as erased, the flag latches at every option-byte load, and no number
/// of reprogrammings changes it. A zero initial stack pointer is an invalid Cortex-M vector table
/// regardless; on a category 1 L0 it is also unbootable in a way that survives the fix.
pub trait Stm32L0Flash {
    /// Runs both unlock sequences: `PEKEYR` to clear `PECR.PELOCK`, then `PRGKEYR` to clear
    /// `PECR.PRGLOCK`. Each is confirmed by reading the bit back rather than assumed, because the
    /// failure mode is a register locked until the next reset.
    fn l0_unlock_flash(&mut self) -> Result<(), ProbeError>;
    /// Re-locks `PECR` by setting `PELOCK`, which locks `PRGLOCK` with it.
    fn l0_lock_flash(&mut self) -> Result<(), ProbeError>;
    /// Erases the 128-byte page containing `address`.
    fn l0_erase_page(&mut self, address: u32) -> Result<(), ProbeError>;
    /// Programs `data` to `address`; both must be word aligned, and the pages spanned must already
    /// be erased -- this part refuses a write to a word that is not zero.
    fn l0_program(&mut self, address: u32, data: &[u8]) -> Result<(), ProbeError>;
}

impl<A: TargetAccess> Stm32L0Flash for A {
    fn l0_unlock_flash(&mut self) -> Result<(), ProbeError> {
        if self.read_word(L0_PECR)? & L0_PECR_PELOCK != 0 {
            self.write_word(L0_PEKEYR, L0_PEKEY1)?;
            self.write_word(L0_PEKEYR, L0_PEKEY2)?;
            if self.read_word(L0_PECR)? & L0_PECR_PELOCK != 0 {
                return Err(ProbeError::Device("STM32L0 PECR stayed locked after the PEKEY sequence"));
            }
        }
        if self.read_word(L0_PECR)? & L0_PECR_PRGLOCK != 0 {
            self.write_word(L0_PRGKEYR, L0_PRGKEY1)?;
            self.write_word(L0_PRGKEYR, L0_PRGKEY2)?;
            if self.read_word(L0_PECR)? & L0_PECR_PRGLOCK != 0 {
                return Err(ProbeError::Device(
                    "STM32L0 program memory stayed locked after the PRGKEY sequence",
                ));
            }
        }
        Ok(())
    }

    fn l0_lock_flash(&mut self) -> Result<(), ProbeError> {
        let pecr = self.read_word(L0_PECR)?;
        self.write_word(L0_PECR, pecr | L0_PECR_PELOCK)
    }

    /// RM0377 3.3.4 "Erase a page in Flash program memory": with `ERASE` and `PROG` both set, a
    /// word write to any address in the page erases that page. The value written is ignored.
    fn l0_erase_page(&mut self, address: u32) -> Result<(), ProbeError> {
        l0_wait_idle(self, FlashWait::BeforeOperation)?;
        let pecr = self.read_word(L0_PECR)?;
        self.write_word(L0_PECR, pecr | L0_PECR_ERASE | L0_PECR_PROG)?;
        self.write_word(address & !(STM32L0_PAGE - 1), 0)?;
        let result = l0_wait_idle(self, FlashWait::AfterOperation);
        let pecr = self.read_word(L0_PECR)?;
        self.write_word(L0_PECR, pecr & !(L0_PECR_ERASE | L0_PECR_PROG))?;
        result
    }

    /// RM0377 3.3.4 "Write a word in the Flash program memory": with `PELOCK` and `PRGLOCK` clear
    /// and no other `PECR` bit set, the word is written to its address directly.
    fn l0_program(&mut self, address: u32, data: &[u8]) -> Result<(), ProbeError> {
        if address % 4 != 0 || data.len() % 4 != 0 {
            return Err(ProbeError::Device("STM32L0 programming is word granular"));
        }
        l0_wait_idle(self, FlashWait::BeforeOperation)?;
        for (index, group) in data.chunks(4).enumerate() {
            let mut buf = [0u8; 4];
            buf.copy_from_slice(group);
            let word = u32::from_le_bytes(buf);
            if word == STM32L0_ERASED_WORD {
                continue;
            }
            self.write_word(address + index as u32 * 4, word)?;
            l0_wait_idle(self, FlashWait::AfterOperation)?;
        }
        Ok(())
    }
}

/// Polls `FLASH_SR` until the controller is idle, then reports the first error it latched.
///
/// The error bits are write-1-to-clear and are cleared here before returning, so one failed page
/// does not report itself again on the next operation.
///
/// On [`FlashWait::BeforeOperation`] the latched flags are cleared and NOT reported -- see the
/// enum, which records the board this distinction was measured on.
fn l0_wait_idle<A: TargetAccess>(target: &mut A, phase: FlashWait) -> Result<(), ProbeError> {
    for _ in 0..100_000 {
        let sr = target.read_word(L0_SR)?;
        if sr & L0_SR_BSY != 0 {
            continue;
        }
        if sr & (L0_SR_ERRORS | L0_SR_EOP) != 0 {
            target.write_word(L0_SR, L0_SR_ERRORS | L0_SR_EOP)?;
        }
        if phase == FlashWait::BeforeOperation {
            return Ok(());
        }
        if sr & L0_SR_WRPERR != 0 {
            return Err(ProbeError::Device("STM32L0 flash write protection error (WRPERR)"));
        }
        if sr & L0_SR_NOTZEROERR != 0 {
            return Err(ProbeError::Device(
                "STM32L0 wrote a word that was not erased (NOTZEROERR) -- erase the page first",
            ));
        }
        if sr & L0_SR_PGAERR != 0 {
            return Err(ProbeError::Device("STM32L0 flash alignment error (PGAERR)"));
        }
        if sr & L0_SR_SIZERR != 0 {
            return Err(ProbeError::Device("STM32L0 flash size error (SIZERR)"));
        }
        if sr & L0_SR_FWWERR != 0 {
            return Err(ProbeError::Device(
                "STM32L0 write/erase was aborted to serve a fetch (FWWERR) -- retry it",
            ));
        }
        if sr & L0_SR_RDERR != 0 {
            return Err(ProbeError::Device(
                "STM32L0 read of a PcROP-protected sector (RDERR) -- the data read back is zeros",
            ));
        }
        if sr & L0_SR_OPTVERR != 0 {
            return Err(ProbeError::Device(
                "STM32L0 option bytes failed to load (OPTVERR) -- the part reset mid-operation",
            ));
        }
        return Ok(());
    }
    Err(ProbeError::Timeout("STM32L0 flash controller busy"))
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

#[cfg(test)]
mod geometry_tests {
    use super::*;

    /// THE TWO FAMILIES DISAGREE FOR EVERY IMAGE SIZE THAT MATTERS, which is why the table is a
    /// parameter rather than a constant baked into a caller. A 200 KB serve image needs SIX sectors
    /// on an F4 and only FIVE on an F7 -- and the dangerous direction is using the F7's answer on an
    /// F4, which leaves the tail of the image in flash that was never erased.
    #[test]
    fn the_f4_and_f7_geometries_are_not_interchangeable() {
        let image = 200 * 1024;
        assert_eq!(sectors_covering(image, &STM32F4_SECTOR_SIZES), 6);
        assert_eq!(sectors_covering(image, &STM32F7_SECTOR_SIZES), 5);
        assert_ne!(
            sectors_covering(image, &STM32F4_SECTOR_SIZES),
            sectors_covering(image, &STM32F7_SECTOR_SIZES),
            "if these ever agree the test has stopped discriminating"
        );
    }

    #[test]
    fn a_sector_boundary_is_covered_by_that_sector_and_not_the_next() {
        assert_eq!(sectors_covering(32 * 1024, &STM32F7_SECTOR_SIZES), 1);
        assert_eq!(sectors_covering(32 * 1024 + 1, &STM32F7_SECTOR_SIZES), 2);
        assert_eq!(sectors_covering(16 * 1024, &STM32F4_SECTOR_SIZES), 1);
        assert_eq!(sectors_covering(16 * 1024 + 1, &STM32F4_SECTOR_SIZES), 2);
    }

    #[test]
    fn an_image_larger_than_the_table_saturates_rather_than_wrapping() {
        let huge = 4 * 1024 * 1024;
        assert_eq!(sectors_covering(huge, &STM32F7_SECTOR_SIZES), STM32F7_SECTOR_SIZES.len() as u32);
        assert_eq!(sectors_covering(huge, &STM32F4_SECTOR_SIZES), STM32F4_SECTOR_SIZES.len() as u32);
    }

    /// THIS TEST FOUND A REAL UNDER-ERASE THE MOMENT IT WAS WRITTEN, and the reason it could is
    /// that it checks each table against the PART rather than against the other table. Every other
    /// test here compares the two geometries to each other, and a matched pair of wrong tables
    /// would satisfy all of them perfectly.
    ///
    /// What it caught: the F4 table had EIGHT entries summing to 512 KB, so `sectors_covering`
    /// saturated there and an image over 512 KB would have had its tail programmed into flash that
    /// was never erased. It had never fired because no image flashed to an F4 had yet exceeded
    /// half a megabyte.
    #[test]
    fn the_tables_describe_the_flash_the_parts_actually_have() {
        assert_eq!(
            STM32F4_SECTOR_SIZES.iter().sum::<usize>(),
            1024 * 1024,
            "a 1 MB F4 is 4x16K + 64K + 7x128K = twelve sectors"
        );
        assert_eq!(
            STM32F7_SECTOR_SIZES.iter().sum::<usize>(),
            1024 * 1024,
            "a 1 MB F7 is 4x32K + 128K + 3x256K = eight sectors"
        );
        assert_eq!(
            STM32F76X_SECTOR_SIZES_SINGLE_BANK.iter().sum::<usize>(),
            2048 * 1024,
            "a 2 MB F76x in single-bank mode is 4x32K + 128K + 7x256K = twelve sectors"
        );
        assert_eq!(
            STM32F76X_SECTOR_SIZES_SINGLE_BANK.len(),
            STM32F7_SECTOR_SIZES.len() + 4,
            "the F76x has four more sectors than the F74x, not a different shape"
        );
        assert_eq!(STM32F7_OPTCR_NDBANK, 1 << 29);
    }

    /// AN OUT-OF-RANGE SECTOR INDEX MUST BE REFUSED, AND MASKING WOULD BE WORSE THAN NOTHING.
    /// `SNB` is five bits at most, so sector 32 masks to **0** -- erasing the vector table the part
    /// boots from, while every call reports success. This asserts the arithmetic that makes that
    /// outcome reachable, so nobody "simplifies" the refusal into a mask.
    #[test]
    fn a_sector_index_too_large_would_wrap_onto_sector_zero() {
        assert_eq!((32u32 << CR_SNB_SHIFT) >> CR_SNB_SHIFT & SNB_MAX, 0, "32 wraps to sector 0");
        assert_eq!((33u32 << CR_SNB_SHIFT) >> CR_SNB_SHIFT & SNB_MAX, 1);
        for sector in [0u32, 11, 23, SNB_MAX] {
            assert!(sector <= SNB_MAX);
            assert_eq!((sector << CR_SNB_SHIFT) >> CR_SNB_SHIFT, sector, "sector {sector} survives");
        }
        assert!(STM32F4_SECTOR_SIZES.len() as u32 - 1 <= SNB_MAX);
        assert!(STM32F76X_SECTOR_SIZES_SINGLE_BANK.len() as u32 * 2 - 1 <= SNB_MAX);
        assert_ne!(STM32F4_SECTOR_SIZES.len(), STM32F7_SECTOR_SIZES.len());
    }
}

#[cfg(test)]
mod l0_tests {
    use super::*;
    use lamella_cmsis_dap::testing::{Mock, echo};
    use lamella_cmsis_dap::{Dap, proto};
    use lamella_probe_core::ArmDap;

    fn ack() -> Vec<u8> {
        echo(proto::cmd::TRANSFER, &[0x01, 0x01])
    }

    /// A MEM-AP read reply carrying `value`.
    fn word(value: u32) -> Vec<u8> {
        let b = value.to_le_bytes();
        vec![proto::cmd::TRANSFER, 0x01, 0x01, b[0], b[1], b[2], b[3]]
    }

    /// `FLASH_SR` with nothing busy and nothing latched.
    fn idle() -> Vec<u8> {
        word(0)
    }

    /// The address a `TAR` write named, taken from the packet the mock recorded.
    fn tar(sent: &[Vec<u8>], index: usize) -> u32 {
        u32::from_le_bytes(sent[index][4..8].try_into().unwrap())
    }

    /// The value a `DRW` write carried.
    fn drw(sent: &[Vec<u8>], index: usize) -> u32 {
        u32::from_le_bytes(sent[index][4..8].try_into().unwrap())
    }

    #[test]
    fn unlock_writes_both_key_pairs_to_their_own_registers() {
        let replies = vec![
            ack(), word(L0_PECR_PELOCK | L0_PECR_PRGLOCK),
            ack(), ack(),
            ack(), ack(),
            ack(), word(L0_PECR_PRGLOCK),
            ack(), word(L0_PECR_PRGLOCK),
            ack(), ack(),
            ack(), ack(),
            ack(), word(0),
        ];
        let mut target = ArmDap::new(Dap::new(Mock::new(replies)));
        target.l0_unlock_flash().unwrap();
        let sent = &target.inner().transport().sent;
        assert_eq!(tar(sent, 2), 0x4002_200c, "FLASH_PEKEYR is base + 0x0c");
        assert_eq!(drw(sent, 3), 0x89ab_cdef, "PEKEY1");
        assert_eq!(drw(sent, 5), 0x0203_0405, "PEKEY2");
        assert_eq!(tar(sent, 10), 0x4002_2010, "FLASH_PRGKEYR is base + 0x10");
        assert_eq!(drw(sent, 11), 0x8c9d_aebf, "PRGKEY1");
        assert_eq!(drw(sent, 13), 0x1314_1516, "PRGKEY2");
    }

    #[test]
    fn a_lock_that_survives_its_key_sequence_is_reported_not_ignored() {
        let replies = vec![
            ack(), word(L0_PECR_PELOCK),
            ack(), ack(),
            ack(), ack(),
            ack(), word(L0_PECR_PELOCK),
        ];
        let mut target = ArmDap::new(Dap::new(Mock::new(replies)));
        assert!(target.l0_unlock_flash().is_err());
    }

    #[test]
    fn erase_sets_erase_and_prog_writes_the_page_then_clears_pecr() {
        let replies = vec![
            ack(), idle(),
            ack(), word(0),
            ack(), ack(),
            ack(), ack(),
            ack(), idle(),
            ack(), word(L0_PECR_ERASE | L0_PECR_PROG),
            ack(), ack(),
        ];
        let mut target = ArmDap::new(Dap::new(Mock::new(replies)));
        target.l0_erase_page(0x0800_1084).unwrap();
        let sent = &target.inner().transport().sent;
        assert_eq!(tar(sent, 4), 0x4002_2004, "FLASH_PECR is base + 0x04");
        assert_eq!(drw(sent, 5), 0x0000_0208, "PECR.ERASE (bit 9) | PECR.PROG (bit 3)");
        assert_eq!(tar(sent, 6), 0x0800_1080, "the write is aimed at the page base");
        assert_eq!(drw(sent, 13), 0, "ERASE and PROG are cleared afterwards");
    }

    #[test]
    fn an_erase_that_errors_still_leaves_pecr_neutral() {
        let replies = vec![
            ack(), idle(),
            ack(), word(0),
            ack(), ack(),
            ack(), ack(),
            ack(), word(L0_SR_WRPERR),
            ack(), ack(),
            ack(), word(L0_PECR_ERASE | L0_PECR_PROG),
            ack(), ack(),
        ];
        let mut target = ArmDap::new(Dap::new(Mock::new(replies)));
        assert!(target.l0_erase_page(0x0800_0000).is_err());
        let sent = &target.inner().transport().sent;
        assert_eq!(drw(sent, 15), 0, "ERASE and PROG cleared despite the error");
    }

    #[test]
    fn program_writes_each_word_to_its_own_address() {
        let replies = vec![
            ack(), idle(),
            ack(), ack(), ack(), idle(),
            ack(), ack(), ack(), idle(),
        ];
        let mut target = ArmDap::new(Dap::new(Mock::new(replies)));
        target.l0_program(0x0800_0200, &[0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88]).unwrap();
        let sent = &target.inner().transport().sent;
        assert_eq!(tar(sent, 2), 0x0800_0200);
        assert_eq!(drw(sent, 3), 0x4433_2211);
        assert_eq!(tar(sent, 6), 0x0800_0204);
        assert_eq!(drw(sent, 7), 0x8877_6655);
    }

    #[test]
    fn a_zero_word_is_skipped_because_an_erased_cell_already_holds_it() {
        let replies = vec![
            ack(), idle(),
            ack(), ack(), ack(), idle(),
        ];
        let mut target = ArmDap::new(Dap::new(Mock::new(replies)));
        target.l0_program(0x0800_0000, &[0, 0, 0, 0, 0xaa, 0xbb, 0xcc, 0xdd]).unwrap();
        let sent = &target.inner().transport().sent;
        assert_eq!(tar(sent, 2), 0x0800_0004, "the zero word at +0 was skipped");
        assert_eq!(drw(sent, 3), 0xddcc_bbaa);
    }

    #[test]
    fn programming_is_refused_unless_address_and_length_are_word_aligned() {
        let mut target = ArmDap::new(Dap::new(Mock::new(vec![])));
        assert!(target.l0_program(0x0800_0002, &[0; 4]).is_err());
        assert!(target.l0_program(0x0800_0000, &[0; 3]).is_err());
    }

    /// Every error flag the manual names, not every error flag this crate happened to decode.
    ///
    /// RM0377 3.5 and RM0367 3.5 both list seven in one sentence -- "RDERR, WRPERR, PGAERR,
    /// OPTVERR, SIZERR, FWWERR, NOTZEROERR" -- and this loop is the place that has to grow when a
    /// reading turns up a flag nothing reports. A flag left out is not a silent pass: it is a
    /// failed operation that returns `Ok`.
    #[test]
    fn each_status_error_bit_is_reported_as_itself() {
        let mut covered = 0u32;
        for (bit, needle) in [
            (L0_SR_WRPERR, "WRPERR"),
            (L0_SR_NOTZEROERR, "NOTZEROERR"),
            (L0_SR_PGAERR, "PGAERR"),
            (L0_SR_SIZERR, "SIZERR"),
            (L0_SR_FWWERR, "FWWERR"),
            (L0_SR_RDERR, "RDERR"),
            (L0_SR_OPTVERR, "OPTVERR"),
        ] {
            let replies = vec![ack(), word(bit), ack(), ack()];
            let mut target = ArmDap::new(Dap::new(Mock::new(replies)));
            let error = l0_wait_idle(&mut target, FlashWait::AfterOperation)
                .expect_err("an error bit must not read as idle");
            assert!(
                format!("{error}").contains(needle),
                "SR bit {bit:#x} reported as {error}, which does not name {needle}"
            );
            covered |= bit;
        }
        assert_eq!(
            covered, L0_SR_ERRORS,
            "the reported bits and the cleared bits have drifted apart: {:#x} vs {L0_SR_ERRORS:#x}",
            covered
        );
    }

    #[test]
    fn the_erased_word_is_zero_on_this_part_alone() {
        assert_eq!(STM32L0_ERASED_WORD, 0);
        assert_ne!(STM32L0_ERASED_WORD, 0xffff_ffff);
        assert_eq!(STM32L0_PAGE, 32 * 4);
        assert_eq!((L0_PECR, L0_PEKEYR, L0_PRGKEYR, L0_SR), (0x4002_2004, 0x4002_200c, 0x4002_2010, 0x4002_2018));
        assert_eq!(L0_SR_ERRORS, (1 << 8) | (1 << 9) | (1 << 10) | (1 << 11) | (1 << 13) | (1 << 16) | (1 << 17));
        assert_eq!(L0_SR_ERRORS.count_ones(), 7, "RM0377 3.5 and RM0367 3.5 both name seven");
        assert_eq!(L0_SR_ERRORS & (L0_SR_BSY | L0_SR_EOP), 0);
        assert_eq!((L0_PECR_PELOCK, L0_PECR_PRGLOCK, L0_PECR_PROG, L0_PECR_ERASE), (1, 2, 8, 0x200));
        assert_eq!((L0_SR_BSY, L0_SR_EOP, L0_SR_WRPERR), (1, 2, 0x100));
        assert_eq!((L0_SR_PGAERR, L0_SR_SIZERR, L0_SR_NOTZEROERR, L0_SR_FWWERR), (0x200, 0x400, 0x1_0000, 0x2_0000));
        assert_eq!(STM32L0_FLASH_BASE, 0x0800_0000);
    }

    /// THE SECOND MANUAL, PINNED SEPARATELY FROM THE FIRST.
    ///
    /// This driver claims two reference manuals, so each states its own literals here rather than
    /// one asserting that the other agrees. If the books ever disagree, these two rows disagree
    /// first.
    ///
    /// Sourced to RM0367 section by section -- 3.7.2 for `FLASH_PECR` and its bits, 3.7.4 and 3.7.5
    /// for the key registers, 3.7.7 for `FLASH_SR`, 3.3.4 for the page and the erased value, and
    /// the memory map for the base. Every value below was read off those pages, not off the
    /// constants it is checking.
    #[test]
    fn rm0367_states_the_same_register_block_as_rm0377() {
        assert_eq!(L0_BASE, 0x4002_2000);
        assert_eq!(L0_PECR, 0x4002_2000 + 0x04);
        assert_eq!(L0_PEKEYR, 0x4002_2000 + 0x0c);
        assert_eq!(L0_PRGKEYR, 0x4002_2000 + 0x10);
        assert_eq!(L0_SR, 0x4002_2000 + 0x18);
        assert_eq!((L0_PEKEY1, L0_PEKEY2), (0x89AB_CDEF, 0x0203_0405));
        assert_eq!((L0_PRGKEY1, L0_PRGKEY2), (0x8C9D_AEBF, 0x1314_1516));
        assert_eq!(L0_PECR_PELOCK, 1 << 0);
        assert_eq!(L0_PECR_PRGLOCK, 1 << 1);
        assert_eq!(L0_PECR_PROG, 1 << 3);
        assert_eq!(L0_PECR_ERASE, 1 << 9);
        assert_eq!(L0_SR_BSY, 1 << 0);
        assert_eq!(L0_SR_EOP, 1 << 1);
        assert_eq!(L0_SR_WRPERR, 1 << 8);
        assert_eq!(L0_SR_PGAERR, 1 << 9);
        assert_eq!(L0_SR_SIZERR, 1 << 10);
        assert_eq!(L0_SR_NOTZEROERR, 1 << 16);
        assert_eq!(L0_SR_FWWERR, 1 << 17);
        assert_eq!(STM32L0_PAGE, 32 * 4);
        assert_eq!(STM32L0_ERASED_WORD, 0);
    }

    /// The reset values RM0367 states for the two registers this driver reads first, checked
    /// against what a NUCLEO-L053R8 actually answered before anything was written to it.
    ///
    /// This is the row that would catch a base address off by a peripheral: a wrong base still reads
    /// SOMETHING, and two registers matching their documented reset values at the documented offsets
    /// is what makes "the block is where the manual says" a measurement rather than an assumption.
    #[test]
    fn the_l053_answered_the_documented_reset_values() {
        let measured_pecr = 0x0000_0007;
        assert_eq!(measured_pecr & L0_PECR_PELOCK, L0_PECR_PELOCK, "a reset part is locked");
        assert_eq!(measured_pecr & L0_PECR_PRGLOCK, L0_PECR_PRGLOCK, "and program memory with it");
        let measured_sr = 0x0000_000c;
        assert_eq!(measured_sr & L0_SR_BSY, 0, "a reset controller is not busy");
        assert_eq!(measured_sr & L0_SR_EOP, 0, "and has completed nothing");
        assert_eq!(measured_sr & L0_SR_ERRORS, 0, "and has latched no error");
    }
}

/// **THE ONE RULE, ASSERTED AT EVERY ONE OF ITS FIVE IMPLEMENTATIONS.**
///
/// A status-register poll answers two different questions ([`FlashWait`]) and this crate answered
/// both with the second one's rule in all five families at once -- the shape where a rule with
/// several implementations gains a new case in none of them. So the discrimination is tested per
/// family and per error bit, not once at whichever site the defect happened to be found on.
///
/// It is a BEHAVIOURAL test rather than a comparison of constants with themselves: each family's
/// poll is driven over a mock probe whose status register reports one latched error, and the two
/// phases are required to disagree about it. Flip either arm of any `phase ==` check and a row here
/// goes red.
#[cfg(test)]
mod flash_wait_phase_tests {
    use super::*;
    use lamella_cmsis_dap::testing::{Mock, echo};
    use lamella_cmsis_dap::{Dap, proto};
    use lamella_probe_core::ArmDap;

    fn ack() -> Vec<u8> {
        echo(proto::cmd::TRANSFER, &[0x01, 0x01])
    }

    fn word(value: u32) -> Vec<u8> {
        let b = value.to_le_bytes();
        vec![proto::cmd::TRANSFER, 0x01, 0x01, b[0], b[1], b[2], b[3]]
    }

    /// One status read reporting `latched`, then the write that clears it.
    fn probe_reporting(latched: u32) -> ArmDap<Dap<Mock>> {
        ArmDap::new(Dap::new(Mock::new(vec![ack(), word(latched), ack(), ack()])))
    }

    /// Every individual error bit a family's status register can latch.
    fn bits_of(mask: u32) -> Vec<u32> {
        (0..32).map(|shift| 1u32 << shift).filter(|bit| mask & bit != 0).collect()
    }

    #[test]
    fn a_stale_error_blocks_no_operation_but_a_fresh_one_fails_its_own() {
        type Poll = fn(&mut ArmDap<Dap<Mock>>, FlashWait) -> Result<(), ProbeError>;
        let families: [(&str, u32, Poll); 5] = [
            ("F0", F0_SR_PGERR | F0_SR_WRPRTERR, f0_wait_idle),
            ("L0", L0_SR_ERRORS, l0_wait_idle),
            ("C0", c0::C0_SR_ERRORS, c0::c0_wait_idle),
            ("U5", u5::U5_SR_ERRORS, u5::u5_wait_idle),
            ("H7", h7::H7_SR_ERRORS, |target, phase| {
                h7::h7_wait_idle(target, STM32H7_FLASH_BASE, phase)
            }),
        ];

        for (family, mask, poll) in families {
            assert_ne!(mask, 0, "{family} decodes no error bits, so this row proves nothing");
            for bit in bits_of(mask) {
                let mut target = probe_reporting(bit);
                assert!(
                    poll(&mut target, FlashWait::AfterOperation).is_err(),
                    "{family} status bit {bit:#x} must fail the operation that latched it",
                );

                let mut target = probe_reporting(bit);
                assert!(
                    poll(&mut target, FlashWait::BeforeOperation).is_ok(),
                    "{family} status bit {bit:#x} is stale before an operation and must not fail it",
                );
            }
        }
    }

    /// The measured case, kept as its own row because it is the one that actually happened.
    ///
    /// A NUCLEO-U5A5ZJ-Q read `FLASH_NSSR = 0x00000080` on three consecutive reads before anything
    /// had written to it, and `stlink-flash --part u5` failed its first page erase. The literal is
    /// the value off that board, not a reference to the constant it is testing.
    #[test]
    fn the_u5a5_arrived_holding_pgserr_and_that_must_not_fail_the_next_erase() {
        let measured_nssr = 0x0000_0080;
        let mut target = probe_reporting(measured_nssr);
        assert!(
            u5::u5_wait_idle(&mut target, FlashWait::BeforeOperation).is_ok(),
            "the board this was measured on could not be programmed on a first run",
        );
        let mut target = probe_reporting(measured_nssr);
        assert!(
            u5::u5_wait_idle(&mut target, FlashWait::AfterOperation).is_err(),
            "PGSERR raised BY an operation is still that operation's failure",
        );
    }
}

/// THE FLASH-SIZE REGISTERS, PINNED TO THEIR MANUALS, AND THE ALIGNMENT THAT MAKES THEM AWKWARD.
///
/// Six families, six unrelated addresses, and no pattern to derive one from another -- so each is
/// written out here as a literal against the manual it came from. Three of them are HALFWORD
/// aligned, which is the part a reader is most likely to get wrong twice: once by reading a 32-bit
/// word at an odd-halfword address, and once by taking the wrong half of the aligned word.
#[cfg(test)]
mod flash_size_register_tests {
    use super::*;
    use lamella_cmsis_dap::testing::{Mock, echo};
    use lamella_cmsis_dap::{Dap, proto};
    use lamella_probe_core::ArmDap;

    fn probe_returning(value: u32) -> ArmDap<Dap<Mock>> {
        let b = value.to_le_bytes();
        ArmDap::new(Dap::new(Mock::new(vec![
            echo(proto::cmd::TRANSFER, &[0x01, 0x01]),
            vec![proto::cmd::TRANSFER, 0x01, 0x01, b[0], b[1], b[2], b[3]],
        ])))
    }

    #[test]
    fn each_family_register_is_the_address_its_manual_prints() {
        assert_eq!(STM32F0_FLASH_SIZE_REG, 0x1FFF_F7CC, "RM0091 33.2");
        assert_eq!(STM32F4_FLASH_SIZE_REG, 0x1FFF_7A22, "RM0090 39.2");
        assert_eq!(STM32F7_FLASH_SIZE_REG, 0x1FF0_F442, "RM0385 41.2");
        assert_eq!(STM32H7_FLASH_SIZE_REG, 0x1FF1_E880, "RM0399 64.2");
        assert_eq!(STM32L0_FLASH_SIZE_REG, 0x1FF8_007C, "RM0377 / RM0367 34.1.1");
        assert_eq!(STM32U5_FLASH_SIZE_REG, 0x0BFA_07A0, "RM0456 76.2");

        let all = [
            STM32F0_FLASH_SIZE_REG, STM32F4_FLASH_SIZE_REG, STM32F7_FLASH_SIZE_REG,
            STM32H7_FLASH_SIZE_REG, STM32L0_FLASH_SIZE_REG, STM32U5_FLASH_SIZE_REG,
        ];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a, b, "two families share a flash-size register address");
            }
        }
    }

    /// THE HALVES, AND THEY ARE THE WHOLE REASON THIS IS A FUNCTION.
    ///
    /// The F4 and F7 registers sit at `...22` and `...42`: bit 1 set, so the halfword lives in the
    /// UPPER half of the aligned word below. Reading the lower half there returns whatever precedes
    /// the register -- a plausible number, in kilobytes, about a part that does not exist.
    #[test]
    fn the_upper_and_lower_halfword_cases_do_not_agree() {
        let word = 0x0800_0040;
        let mut lower = probe_returning(word);
        assert_eq!(stm32_flash_size_bytes(&mut lower, STM32L0_FLASH_SIZE_REG).unwrap(), 64 * 1024);
        let mut upper = probe_returning(word);
        assert_eq!(stm32_flash_size_bytes(&mut upper, STM32F4_FLASH_SIZE_REG).unwrap(), 2048 * 1024);

        for register in [STM32F4_FLASH_SIZE_REG, STM32F7_FLASH_SIZE_REG] {
            assert_ne!(register & 2, 0, "{register:#x} is an UPPER-halfword register");
        }
        for register in [STM32F0_FLASH_SIZE_REG, STM32H7_FLASH_SIZE_REG, STM32L0_FLASH_SIZE_REG, STM32U5_FLASH_SIZE_REG] {
            assert_eq!(register & 2, 0, "{register:#x} is a LOWER-halfword register");
        }
        for register in [STM32F4_FLASH_SIZE_REG, STM32F7_FLASH_SIZE_REG] {
            assert_eq!((register & !3) % 4, 0);
        }
    }

    /// The measured parts, so the decode is anchored to silicon and not only to arithmetic.
    #[test]
    fn the_boards_on_this_bench_decode_to_what_they_are() {
        let mut l053 = probe_returning(0x038f_0040);
        assert_eq!(stm32_flash_size_bytes(&mut l053, STM32L0_FLASH_SIZE_REG).unwrap(), 64 * 1024);
        let mut u5a5 = probe_returning(0xffff_1000);
        assert_eq!(stm32_flash_size_bytes(&mut u5a5, STM32U5_FLASH_SIZE_REG).unwrap(), 4096 * 1024);
        let mut l011 = probe_returning(0x038f_0010);
        assert_eq!(stm32_flash_size_bytes(&mut l011, STM32L0_FLASH_SIZE_REG).unwrap(), 16 * 1024);
    }

    /// A blank read is refused, because both spellings of it decode as a believable part.
    #[test]
    fn a_blank_register_is_refused_rather_than_believed() {
        let mut zero = probe_returning(0);
        assert!(stm32_flash_size_bytes(&mut zero, STM32L0_FLASH_SIZE_REG).is_err());
        let mut ones = probe_returning(0xffff_ffff);
        assert!(stm32_flash_size_bytes(&mut ones, STM32L0_FLASH_SIZE_REG).is_err(), "0xffff KB is 64 MB");
    }
}

/// The DBGMCU identity decode, pinned to the two manuals rather than to this crate's own constants.
///
/// Every value below is typed out of RM0377 27.4.1 and RM0367 33.4.1 by hand. The first version of
/// the L0 register tests compared the crate's constants with themselves and passed with the `ERASE`
/// bit deliberately moved from 9 to 8, which is the whole reason these are literals.
#[cfg(test)]
mod l0_identity_tests {
    use super::*;
    use lamella_cmsis_dap::testing::{Mock, echo};
    use lamella_cmsis_dap::{Dap, proto};
    use lamella_probe_core::ArmDap;

    fn probe_returning(value: u32) -> ArmDap<Dap<Mock>> {
        let b = value.to_le_bytes();
        ArmDap::new(Dap::new(Mock::new(vec![
            echo(proto::cmd::TRANSFER, &[0x01, 0x01]),
            vec![proto::cmd::TRANSFER, 0x01, 0x01, b[0], b[1], b[2], b[3]],
        ])))
    }

    #[test]
    fn the_register_is_the_address_both_manuals_print() {
        assert_eq!(STM32L0_DBGMCU_IDCODE, 0x4001_5800, "RM0377 27.4.1 and RM0367 33.4.1");
        assert_ne!(STM32L0_DBGMCU_IDCODE, 0xE004_2000);
    }

    #[test]
    fn each_category_is_the_dev_id_its_manual_lists() {
        assert_eq!(Stm32L0Category::from_dev_id(0x457), Some(Stm32L0Category::One));
        assert_eq!(Stm32L0Category::from_dev_id(0x425), Some(Stm32L0Category::Two));
        assert_eq!(Stm32L0Category::from_dev_id(0x417), Some(Stm32L0Category::Three));
        assert_eq!(Stm32L0Category::from_dev_id(0x447), Some(Stm32L0Category::Five));
        assert_eq!(Stm32L0Category::from_dev_id(0x000), None);
        assert_eq!(Stm32L0Category::from_dev_id(0xfff), None);
        assert_eq!(Stm32L0Category::from_dev_id(0x440), None);
    }

    /// **The consequence of a mistake, not the label**, and it is true of exactly one category.
    #[test]
    fn only_category_three_carries_out_a_program_to_an_unerased_word() {
        assert!(Stm32L0Category::Three.program_to_unerased_word_is_performed(), "RM0367 3.3.4");
        for category in [Stm32L0Category::One, Stm32L0Category::Two, Stm32L0Category::Five] {
            assert!(
                !category.program_to_unerased_word_is_performed(),
                "{} discards the write; only category 3 performs it",
                category.name()
            );
        }
    }

    #[test]
    fn the_boards_on_this_bench_decode_to_their_category() {
        let mut l073 = probe_returning(0x2008_6447);
        let (dev_id, rev_id) = stm32l0_dev_id(&mut l073).unwrap();
        assert_eq!(dev_id, 0x447);
        assert_eq!(rev_id, 0x2008);
        assert_eq!(Stm32L0Category::from_dev_id(dev_id), Some(Stm32L0Category::Five));

        let mut l053 = probe_returning(0x1038_6417);
        let (dev_id, rev_id) = stm32l0_dev_id(&mut l053).unwrap();
        assert_eq!(dev_id, 0x417);
        assert_eq!(rev_id, 0x1038);
        assert_eq!(Stm32L0Category::from_dev_id(dev_id), Some(Stm32L0Category::Three));
    }

    /// **RM0367's Table 173 does not let a REV_ID stand in for a category**, so nothing here reads
    /// one as though it could. `0x1000` is Rev A in BOTH columns; `0x1008`, `0x1018` and `0x1038`
    /// are category 3 only; `0x2000` and `0x2008` are category 5 only.
    #[test]
    fn the_reserved_nibble_is_not_part_of_the_device_id() {
        let mut probe = probe_returning(0x2008_6447);
        let (dev_id, _) = stm32l0_dev_id(&mut probe).unwrap();
        assert_eq!(dev_id & 0xf000, 0, "DEV_ID is twelve bits, not sixteen");
        assert_eq!(0x2008_6447u32 >> 12 & 0xf, 0b0110, "RM0367 33.4.1: reserved, read 0b0110");
    }

    #[test]
    fn a_blank_reading_is_refused_rather_than_decoded_as_no_category() {
        let mut zero = probe_returning(0);
        assert!(stm32l0_dev_id(&mut zero).is_err(), "0x000 is not a category, it is a failed read");
        let mut ones = probe_returning(0xffff_ffff);
        assert!(stm32l0_dev_id(&mut ones).is_err());
    }
}
