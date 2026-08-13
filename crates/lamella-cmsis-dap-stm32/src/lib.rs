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
    STM32U5_FLASH_BASE, STM32U5_PAGE, STM32U5_MAX_PAGES_PER_BANK, STM32U5_QUAD_WORD, Stm32U5Flash,
};

mod h7;
pub use h7::{
    STM32H7_BANK2_BASE, STM32H7_FLASH_BASE, STM32H7_FLASH_WORD, STM32H7_SECTOR, Stm32H7Flash,
};

use lamella_probe_core::{CallFrame, ProbeError, TargetAccess};

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
        f0_wait_idle(self)?;
        self.write_word(F0_CR, F0_CR_PER)?;
        self.write_word(F0_AR, address & !(STM32F0_PAGE - 1))?;
        self.write_word(F0_CR, F0_CR_PER | F0_CR_STRT)?;
        f0_wait_idle(self)?;
        self.write_word(F0_CR, 0)
    }

    fn f0_program(&mut self, address: u32, data: &[u8]) -> Result<(), ProbeError> {
        if address % 2 != 0 || data.len() % 2 != 0 {
            return Err(ProbeError::Device("STM32F0 programming is half-word granular"));
        }
        f0_wait_idle(self)?;
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
fn f0_wait_idle<A: TargetAccess>(target: &mut A) -> Result<(), ProbeError> {
    for _ in 0..100_000 {
        let sr = target.read_word(F0_SR)?;
        if sr & F0_SR_BSY != 0 {
            continue;
        }
        if sr & (F0_SR_PGERR | F0_SR_WRPRTERR | F0_SR_EOP) != 0 {
            target.write_word(F0_SR, F0_SR_PGERR | F0_SR_WRPRTERR | F0_SR_EOP)?;
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
